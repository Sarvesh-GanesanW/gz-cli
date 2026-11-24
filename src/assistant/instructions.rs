//! Instruction files, in the order they go out as
//! `clientContext.instructions`:
//!
//! 1. the user's personal `instructions.md` next to the gz config;
//! 2. `GZ.md`, `GZ.local.md` (personal to this checkout), `AGENTS.md` and
//!    `CLAUDE.md` walking up from the current folder to the home folder (or
//!    to the top open folder outside home), nearest first;
//! 3. rules in `.gz/rules/` of those folders that have no `paths:`;
//! 4. files picked up on demand, in the order they were: instruction files
//!    of a subfolder once the model reads or edits something in it, and
//!    `.gz/rules` files whose `paths:` globs match a file it touched.
//!
//! Every file's `@path` imports are appended to its entry. Files are read
//! again for every turn, so an edit applies to the next message; entries
//! picked up on demand are added at the end so earlier ones keep their
//! place (and the server's prompt cache).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::assistant::instruction_imports::{self, Reach, ReadRule};
use crate::assistant::instruction_rules::{self, Rule, RULES_DIR};
use crate::assistant::local_context::Limits;
use crate::assistant::sandbox::Sandbox;

/// Project instruction files, in the order read within one folder.
pub const PROJECT_FILES: [&str; 4] = ["GZ.md", LOCAL_FILE, "AGENTS.md", "CLAUDE.md"];
/// The personal, uncommitted instruction file of a checkout.
pub const LOCAL_FILE: &str = "GZ.local.md";
const USER_FILE: &str = "instructions.md";
/// Bytes read from one file at most; far more than any limit keeps.
const MAX_READ_BYTES: u64 = 1024 * 1024;

/// Whose instructions a file holds (the contract's `source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    User,
    Project,
}

/// Why an entry is sent, for `/instructions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// gz's own environment snapshot.
    Environment,
    /// The personal `instructions.md`.
    Personal,
    /// In the current folder or a folder above it.
    Folder,
    /// A `.gz/rules` file without `paths:` in the current folder or above.
    Rule,
    /// In a subfolder the model read or edited `touched` in.
    Subfolder { touched: String },
    /// A rule whose `globs` matched `touched`.
    MatchedRule { touched: String, globs: Vec<String> },
}

impl Why {
    /// A short phrase for `/instructions`: "gz worked on src/api/x.rs".
    pub fn describe(&self) -> String {
        match self {
            Why::Environment => "taken when the session started".into(),
            Why::Personal => "your personal instructions".into(),
            Why::Folder => "in this folder or above".into(),
            Why::Rule => "rule for every file".into(),
            Why::Subfolder { touched } => format!("gz worked on {touched}"),
            Why::MatchedRule { touched, globs } => {
                format!("rule for {} · matched {touched}", globs.join(", "))
            }
        }
    }
}

/// One instruction entry as it goes to the chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub source: Source,
    /// Where it is, shown as `~/…` under home.
    pub shown: String,
    /// What is sent: the file's text and its imports, within the limit.
    pub text: String,
    /// The file's size on disk, for `/instructions`.
    pub bytes: u64,
    /// The file's own text was cut to fit.
    pub truncated: bool,
    pub why: Why,
    /// Imported files included in `text`, as shown paths.
    pub imports: Vec<String>,
    /// What was left out of this entry, one line each (`@x.md: not found`).
    pub dropped: Vec<String>,
    /// Something to tell the user about the file (a `GZ.local.md` git
    /// would commit, or one not trusted as theirs and why).
    pub note: Option<String>,
}

impl Instruction {
    /// An entry gz writes itself (the environment snapshot): sent as the
    /// user's, named by `label` rather than a file, cut like a file.
    pub fn generated(label: &str, text: &str, limit: usize) -> Self {
        let (text_sent, truncated) = cut(text, limit);
        Self {
            source: Source::User,
            shown: label.to_string(),
            bytes: text.len() as u64,
            text: text_sent,
            truncated,
            why: Why::Environment,
            imports: Vec::new(),
            dropped: Vec::new(),
            note: None,
        }
    }
}

/// A file gz found but did not send, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeftOut {
    pub shown: String,
    pub why: String,
}

/// The entries to send and the files left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub sent: Vec<Instruction>,
    pub left_out: Vec<LeftOut>,
}

/// The personal instructions file: `<gz config folder>/instructions.md`.
pub fn user_file() -> Result<PathBuf> {
    let config = crate::config::config_path()?;
    let dir = config.parent().context("config path has no folder")?;
    Ok(dir.join(USER_FILE))
}

/// The instruction files that apply in the sandbox's current folder,
/// without any picked up on demand (see [`find`]).
#[cfg(test)]
pub fn discover(sandbox: &Sandbox, user: Option<&Path>, limits: &Limits) -> Vec<Instruction> {
    find(sandbox, user, limits, &OnDemand::default()).sent
}

/// Every instruction entry for the next request: at most
/// `limits.instruction_files`, each within `limits.instruction_chars`, the
/// same text only once, never a secret file; the rest in `left_out`.
pub fn find(
    sandbox: &Sandbox,
    user: Option<&Path>,
    limits: &Limits,
    on_demand: &OnDemand,
) -> Found {
    find_under_rules(sandbox, user, limits, on_demand, &|_| None)
}

/// [`find`], leaving out project files and imports a deny or ask read
/// rule covers (`read_rule` names the rule): they would reach the model
/// without the prompt the rule calls for.
pub fn find_under_rules(
    sandbox: &Sandbox,
    user: Option<&Path>,
    limits: &Limits,
    on_demand: &OnDemand,
    read_rule: ReadRule<'_>,
) -> Found {
    let mut found = Found::default();
    let candidates = candidates(sandbox, user, on_demand, &mut found.left_out);
    let mut seen_texts: Vec<String> = Vec::new();
    for candidate in candidates {
        let shown = sandbox.display_folder(&candidate.path);
        if let Some(rule) = candidate.covering_rule(read_rule) {
            found.left_out.push(LeftOut {
                shown,
                why: format!("the rule `{rule}` covers it, so gz doesn't send it unasked"),
            });
            continue;
        }
        let Some((text, bytes)) = candidate.text(sandbox, &mut found.left_out) else {
            continue;
        };
        if text.trim().is_empty() || seen_texts.contains(&text) {
            continue;
        }
        if found.sent.len() >= limits.instruction_files {
            found.left_out.push(LeftOut {
                shown,
                why: format!(
                    "the chat server takes at most {} entries; this one is past them",
                    limits.instruction_files
                ),
            });
            continue;
        }
        let (source, note) = candidate.standing();
        let reach = match source {
            Source::User => Reach::Anywhere,
            Source::Project => Reach::Confined,
        };
        let read = |path: &Path| read_instruction(sandbox, path).map(|(text, _)| text);
        let imports =
            instruction_imports::collect(sandbox, &candidate.path, &text, reach, &read, read_rule);
        let composed = compose(sandbox, &text, &imports, limits.instruction_chars);
        seen_texts.push(text);
        found.sent.push(Instruction {
            source,
            shown,
            text: composed.text,
            bytes,
            truncated: composed.truncated,
            why: candidate.why,
            imports: composed.imports,
            dropped: composed.dropped,
            note,
        });
    }
    found
}

/// A file that may be sent.
struct Candidate {
    path: PathBuf,
    source: Source,
    why: Why,
    is_rule: bool,
}

impl Candidate {
    fn file(path: PathBuf, why: Why) -> Self {
        Self {
            source: if why == Why::Personal {
                Source::User
            } else {
                Source::Project
            },
            path,
            why,
            is_rule: false,
        }
    }

    /// Whose the file is, and what `/instructions` should say about it: a
    /// `GZ.local.md` is the user's only while [`local_file_standing`] says
    /// so (asked only for a file that exists, since it runs git).
    fn standing(&self) -> (Source, Option<String>) {
        let is_local_file = self.path.file_name().is_some_and(|n| n == LOCAL_FILE);
        if !is_local_file || self.why == Why::Personal {
            return (self.source, None);
        }
        match local_file_standing(&self.path, self.why == Why::Folder) {
            Standing::Personal { hint } => (Source::User, hint),
            Standing::Shared { why } => (
                Source::Project,
                Some(format!("{LOCAL_FILE} sent as project instructions: {why}")),
            ),
        }
    }

    /// The deny or ask read rule covering a project file; the user's own
    /// file is theirs to send.
    fn covering_rule(&self, read_rule: ReadRule<'_>) -> Option<String> {
        if self.why == Why::Personal {
            return None;
        }
        let real = self.path.canonicalize().ok()?;
        read_rule(&real)
    }

    fn rule(path: PathBuf, why: Why) -> Self {
        Self {
            path,
            source: Source::Project,
            why,
            is_rule: true,
        }
    }

    /// The text to send (a rule's body), read now; a rule that no longer
    /// parses is reported in `left_out`.
    fn text(&self, sandbox: &Sandbox, left_out: &mut Vec<LeftOut>) -> Option<(String, u64)> {
        let (text, bytes) = read_instruction(sandbox, &self.path)?;
        if !self.is_rule {
            return Some((text, bytes));
        }
        let project = rule_project(&self.path)?;
        match instruction_rules::parse(&project, &self.path, &text) {
            Ok(rule) => Some((rule.body, bytes)),
            Err(err) => {
                left_out.push(LeftOut {
                    shown: sandbox.display_folder(&self.path),
                    why: format!("{err:#}"),
                });
                None
            }
        }
    }
}

/// The folder holding `.gz` for a rule file at `<project>/.gz/rules/…`.
fn rule_project(rule: &Path) -> Option<PathBuf> {
    rule.ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == RULES_DIR[0]))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

fn candidates(
    sandbox: &Sandbox,
    user: Option<&Path>,
    on_demand: &OnDemand,
    left_out: &mut Vec<LeftOut>,
) -> Vec<Candidate> {
    let walk = folder_walk(sandbox);
    let mut out: Vec<Candidate> = user
        .map(|path| Candidate::file(path.to_path_buf(), Why::Personal))
        .into_iter()
        .collect();
    for dir in &walk {
        out.extend(
            PROJECT_FILES
                .iter()
                .map(|name| Candidate::file(project_file(dir, name), Why::Folder)),
        );
    }
    for dir in &walk {
        let (rules, bad) = rules_in(sandbox, dir);
        report_bad_rules(sandbox, bad, left_out);
        out.extend(
            rules
                .into_iter()
                .filter(Rule::is_always)
                .map(|rule| Candidate::rule(rule.file, Why::Rule)),
        );
    }
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let known: Vec<PathBuf> = out.iter().map(|c| canonical(&c.path)).collect();
    for activation in on_demand.activations() {
        if known.contains(&activation.file) {
            continue;
        }
        out.push(match activation.is_rule {
            true => Candidate::rule(activation.file, activation.why),
            false => Candidate::file(activation.file, activation.why),
        });
    }
    out
}

fn report_bad_rules(
    sandbox: &Sandbox,
    bad: Vec<instruction_rules::BadRule>,
    left_out: &mut Vec<LeftOut>,
) {
    left_out.extend(bad.into_iter().map(|rule| LeftOut {
        shown: sandbox.display_folder(&rule.file),
        why: rule.why,
    }));
}

fn rules_in(sandbox: &Sandbox, project: &Path) -> (Vec<Rule>, Vec<instruction_rules::BadRule>) {
    let read = |path: &Path| read_instruction(sandbox, path).map(|(text, _)| text);
    instruction_rules::in_folder(project, &read)
}

/// The current folder, then each folder above it up to where the walk
/// stops.
fn folder_walk(sandbox: &Sandbox) -> Vec<PathBuf> {
    let cwd = sandbox.cwd();
    let stop = walk_stop(sandbox, &cwd);
    let mut out = Vec::new();
    let mut folder = Some(cwd.as_path());
    while let Some(dir) = folder {
        out.push(dir.to_path_buf());
        if dir == stop {
            break;
        }
        folder = match crate::cmd::code_worktree::main_checkout_of(dir) {
            // A `--worktree` session skips the main checkout it sits in:
            // its files belong to a different working copy.
            Some(checkout) if checkout != stop && checkout.starts_with(&stop) => checkout.parent(),
            Some(_) => break,
            None => dir.parent(),
        };
    }
    out
}

/// Home when the folder is inside it, else the outermost open folder that
/// holds it.
fn walk_stop(sandbox: &Sandbox, folder: &Path) -> PathBuf {
    outer_limit(sandbox, folder).unwrap_or_else(|| folder.to_path_buf())
}

/// Home or the outermost open folder holding `path`; `None` when neither
/// does.
fn outer_limit(sandbox: &Sandbox, path: &Path) -> Option<PathBuf> {
    if let Some(home) = sandbox.home().filter(|home| path.starts_with(home)) {
        return Some(home.to_path_buf());
    }
    sandbox
        .roots()
        .into_iter()
        .filter(|root| path.starts_with(root))
        .min_by_key(|root| root.components().count())
}

/// One entry's text with its imports, within `limit` characters.
struct Composed {
    text: String,
    truncated: bool,
    imports: Vec<String>,
    dropped: Vec<String>,
}

/// The file's text, then each import under a `[gz: imported from …]`
/// line. An import that doesn't fit is left out whole and named in a
/// closing note, so the model knows what it is missing; the result never
/// passes `limit` (the server refuses longer entries).
fn compose(
    sandbox: &Sandbox,
    text: &str,
    imports: &instruction_imports::Imports,
    limit: usize,
) -> Composed {
    let (base, truncated) = cut(text, limit);
    let mut dropped: Vec<String> = imports
        .skipped
        .iter()
        .map(|s| format!("{}: {}", s.mention, s.why))
        .collect();
    let mut kept: Vec<(String, String)> = Vec::new();
    let mut too_big: Vec<String> = Vec::new();
    let mut used = base.chars().count();
    for import in &imports.files {
        let shown = sandbox.display_folder(&import.path);
        let section = format!("\n\n[gz: imported from {shown}]\n{}", import.text);
        let size = section.chars().count();
        if !truncated && used + size <= limit {
            used += size;
            kept.push((shown, section));
        } else {
            too_big.push(shown);
        }
    }
    let mut note = size_note(&too_big);
    while used + note.chars().count() > limit {
        let Some((shown, section)) = kept.pop() else {
            // Not even the note fits: the entry goes without it, and
            // `/instructions` still lists what was left out.
            note.clear();
            break;
        };
        used -= section.chars().count();
        too_big.insert(0, shown);
        note = size_note(&too_big);
    }
    dropped.extend(
        too_big.iter().map(|shown| {
            format!("{shown}: imported, but past the {limit}-character limit per entry")
        }),
    );
    let mut out = base;
    for (_, section) in &kept {
        out.push_str(section);
    }
    out.push_str(&note);
    Composed {
        text: out,
        truncated,
        imports: kept.into_iter().map(|(shown, _)| shown).collect(),
        dropped,
    }
}

/// The closing note naming imports left out for size; empty when none.
fn size_note(left_out: &[String]) -> String {
    if left_out.is_empty() {
        return String::new();
    }
    format!(
        "\n\n[gz: imports left out to stay within the size limit: {}]",
        left_out.join(", ")
    )
}

/// Instruction files and rules picked up during the session, in the
/// order they were, shared by a session and its background subagents.
#[derive(Debug, Clone, Default)]
pub struct OnDemand {
    activations: Arc<Mutex<Vec<Activation>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Activation {
    file: PathBuf,
    why: Why,
    is_rule: bool,
}

impl OnDemand {
    fn lock(&self) -> MutexGuard<'_, Vec<Activation>> {
        self.activations.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn activations(&self) -> Vec<Activation> {
        self.lock().clone()
    }

    /// The model read or edited `path` (absolute, inside the sandbox): picks
    /// up instruction files of the subfolders between it and the current
    /// folder, and rules whose globs match it. Returns how many were new.
    pub fn touch(&self, sandbox: &Sandbox, path: &Path) -> usize {
        let Some(limit) = outer_limit(sandbox, path) else {
            return 0;
        };
        let start = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        let mut dirs: Vec<&Path> = Vec::new();
        for dir in start.ancestors() {
            if !dir.starts_with(&limit) {
                break;
            }
            dirs.push(dir);
            if dir == limit {
                break;
            }
        }
        let cwd = sandbox.cwd();
        let touched = sandbox.display(path);
        let mut new = Vec::new();
        for dir in dirs.into_iter().rev() {
            let covered = cwd.starts_with(dir);
            new.extend(activations_in(sandbox, dir, path, &touched, covered));
        }
        let mut held = self.lock();
        let before = held.len();
        for activation in new {
            if !held.iter().any(|a| a.file == activation.file) {
                held.push(activation);
            }
        }
        held.len() - before
    }

    /// Forgets everything picked up (a new conversation).
    pub fn clear(&self) {
        self.lock().clear();
    }
}

/// What `dir` adds once `path` in or below it was touched: its own
/// instruction files and always-rules unless the folder walk already
/// covers it, and rules whose globs match `path`.
fn activations_in(
    sandbox: &Sandbox,
    dir: &Path,
    path: &Path,
    touched: &str,
    covered: bool,
) -> Vec<Activation> {
    let subfolder = Why::Subfolder {
        touched: touched.to_string(),
    };
    let canonical = |p: &Path| p.canonicalize().ok().filter(|p| p.is_file());
    let mut out = Vec::new();
    if !covered {
        out.extend(PROJECT_FILES.iter().filter_map(|name| {
            canonical(&dir.join(name)).map(|file| Activation {
                file,
                why: subfolder.clone(),
                is_rule: false,
            })
        }));
    }
    let (rules, _) = rules_in(sandbox, dir);
    for rule in rules {
        let why = if rule.is_always() {
            if covered {
                continue;
            }
            subfolder.clone()
        } else if rule.matches(path) {
            Why::MatchedRule {
                touched: touched.to_string(),
                globs: rule.paths.clone(),
            }
        } else {
            continue;
        };
        if let Some(file) = canonical(&rule.file) {
            out.push(Activation {
                file,
                why,
                is_rule: true,
            });
        }
    }
    out
}

/// Whether a `GZ.local.md` is the user's own.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Standing {
    /// Sent as the user's; `hint` when git would still commit it.
    Personal { hint: Option<String> },
    /// Sent as project instructions, for the reason given.
    Shared { why: String },
}

/// A `GZ.local.md` is trusted as the user's (so it may import any file)
/// only when nothing suggests someone else wrote it: git doesn't track it
/// and its folder isn't one git ignores (a dependency or build output).
/// Outside a repository gz can't tell, so only one in the folder walk
/// (`in_walk`: the folder gz works in or above) counts.
fn local_file_standing(path: &Path, in_walk: bool) -> Standing {
    use crate::assistant::environment::{git_run, repo_root};
    let shared = |why: &str| Standing::Shared { why: why.into() };
    let Some(folder) = path.parent() else {
        return shared("it has no folder");
    };
    if repo_root(folder).is_none() {
        return if in_walk {
            Standing::Personal { hint: None }
        } else {
            shared("it is outside any git repository and outside the folder gz works in")
        };
    }
    // Exit codes: `ls-files --error-unmatch` 0 tracked, 1 not;
    // `check-ignore` 0 ignored, 1 not.
    let exit = |args: &[&str]| git_run(folder, args).and_then(|(code, _)| code);
    match exit(&["ls-files", "--error-unmatch", "--", LOCAL_FILE]) {
        Some(1) => {}
        Some(0) => return shared("the repository commits it, so someone else may have written it"),
        _ => return shared("gz couldn't ask git whether the repository commits it"),
    }
    match exit(&["check-ignore", "-q", "--", "."]) {
        Some(1) => {}
        Some(0) => return shared("it is in a folder git ignores, such as a dependency"),
        _ => return shared("gz couldn't ask git about its folder"),
    }
    let hint = (exit(&["check-ignore", "-q", "--", LOCAL_FILE]) != Some(0))
        .then(|| format!("{LOCAL_FILE} is personal: add it to .gitignore so it isn't committed"));
    Standing::Personal { hint }
}

/// `name` in `dir`; a worktree without its own `GZ.local.md` reads its main
/// checkout's.
fn project_file(dir: &Path, name: &str) -> PathBuf {
    match name {
        LOCAL_FILE => crate::cmd::code_worktree::personal_file(dir, name),
        _ => dir.join(name),
    }
}

/// The text of a readable, non-secret text file (following symlinks to
/// check where it really is) and its size; `None` otherwise.
fn read_instruction(sandbox: &Sandbox, path: &Path) -> Option<(String, u64)> {
    let real = path.canonicalize().ok()?;
    if !real.is_file() || sandbox.is_secret(&real) {
        return None;
    }
    let bytes = real.metadata().ok()?.len();
    let mut raw = Vec::new();
    std::fs::File::open(&real)
        .ok()?
        .take(MAX_READ_BYTES)
        .read_to_end(&mut raw)
        .ok()?;
    if raw.contains(&0) {
        return None;
    }
    Some((String::from_utf8_lossy(&raw).into_owned(), bytes))
}

/// The text cut to `limit` characters, the cut marked so the model knows
/// it saw only part of the file.
fn cut(text: &str, limit: usize) -> (String, bool) {
    let total = text.chars().count();
    if total <= limit {
        return (text.to_string(), false);
    }
    let note =
        format!("\n\n[gz: this file has {total} characters; only the first part is included]");
    let keep = limit.saturating_sub(note.chars().count());
    let mut out: String = text.chars().take(keep).collect();
    out.push_str(&note);
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::sandbox::tests::temp_dir;

    fn tree(name: &str) -> (PathBuf, Sandbox) {
        let home = temp_dir(name);
        std::fs::create_dir_all(home.join("code/app/src")).unwrap();
        let sandbox = Sandbox::with_home(&home, Some(&home)).unwrap();
        (home, sandbox)
    }

    fn shown(found: &[Instruction]) -> Vec<String> {
        found.iter().map(|i| i.shown.clone()).collect()
    }

    #[test]
    fn a_worktree_session_skips_the_main_checkout_it_sits_in() {
        let (home, sandbox) = tree("worktree-walk");
        let top = home.join("code/app/.gz/worktrees/w");
        std::fs::create_dir_all(top.join("src")).unwrap();
        std::fs::write(top.join(".git"), "gitdir: ../../../.git/worktrees/w\n").unwrap();
        std::fs::write(top.join("GZ.md"), "worktree copy").unwrap();
        std::fs::write(home.join("code/app/GZ.md"), "main checkout copy").unwrap();
        std::fs::write(home.join("code/AGENTS.md"), "above the repo").unwrap();
        sandbox.set_cwd(&top.join("src")).unwrap();
        let found = discover(&sandbox, None, &Limits::default());
        assert_eq!(
            shown(&found),
            ["~/code/app/.gz/worktrees/w/GZ.md", "~/code/AGENTS.md"]
        );
    }

    #[test]
    fn reads_the_user_file_then_project_files_nearest_first_up_to_home() {
        let (home, sandbox) = tree("order");
        let user = home.join("config/gz/instructions.md");
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(&user, "be brief").unwrap();
        std::fs::write(home.join("AGENTS.md"), "home rules").unwrap();
        std::fs::write(home.join("code/app/CLAUDE.md"), "app claude").unwrap();
        std::fs::write(home.join("code/app/GZ.md"), "app gz").unwrap();
        std::fs::write(home.join("code/app/src/AGENTS.md"), "src rules").unwrap();
        sandbox.set_cwd(&home.join("code/app/src")).unwrap();

        let found = discover(&sandbox, Some(&user), &Limits::default());
        assert_eq!(
            shown(&found),
            [
                "~/config/gz/instructions.md",
                "~/code/app/src/AGENTS.md",
                "~/code/app/GZ.md",
                "~/code/app/CLAUDE.md",
                "~/AGENTS.md",
            ]
        );
        assert_eq!(found[0].source, Source::User);
        assert!(found[1..].iter().all(|i| i.source == Source::Project));
        std::fs::write(home.join("code/app/GZ.md"), "edited").unwrap();
        let again = discover(&sandbox, Some(&user), &Limits::default());
        assert_eq!(again[2].text, "edited", "read again every turn");
    }

    #[test]
    fn stops_at_home_and_never_reads_above_it() {
        let outer = temp_dir("stop");
        let home = outer.join("home");
        std::fs::create_dir_all(home.join("code/app")).unwrap();
        std::fs::write(outer.join("AGENTS.md"), "above home").unwrap();
        std::fs::write(home.join("CLAUDE.md"), "home").unwrap();
        let sandbox = Sandbox::with_home(&home.join("code/app"), Some(&home)).unwrap();
        let found = discover(&sandbox, None, &Limits::default());
        assert_eq!(shown(&found), ["~/CLAUDE.md"], "parents up to home count");
    }

    #[test]
    fn outside_home_the_walk_stops_at_the_top_open_folder() {
        let (dir, _) = tree("outside");
        std::fs::write(dir.join("AGENTS.md"), "top").unwrap();
        std::fs::write(dir.join("code/AGENTS.md"), "middle").unwrap();
        let elsewhere = temp_dir("outside-home");
        let sandbox = Sandbox::with_home(&dir.join("code"), Some(&elsewhere)).unwrap();
        sandbox.set_cwd(&dir.join("code/app")).unwrap();
        let found = discover(&sandbox, None, &Limits::default());
        assert_eq!(found.len(), 1, "{:?}", shown(&found));
        assert_eq!(found[0].text, "middle");
    }

    #[test]
    fn caps_the_count_cuts_long_files_and_skips_duplicates() {
        let (home, sandbox) = tree("limits");
        std::fs::write(home.join("code/app/AGENTS.md"), "same").unwrap();
        std::fs::write(home.join("code/app/CLAUDE.md"), "same").unwrap();
        std::fs::write(home.join("code/app/GZ.md"), "y".repeat(500)).unwrap();
        std::fs::write(home.join("code/AGENTS.md"), "far").unwrap();
        sandbox.set_cwd(&home.join("code/app")).unwrap();
        let limits = Limits {
            instruction_files: 2,
            instruction_chars: 200,
            ..Limits::default()
        };
        let found = discover(&sandbox, None, &limits);
        assert_eq!(shown(&found), ["~/code/app/GZ.md", "~/code/app/AGENTS.md"]);
        assert!(found[0].truncated && found[0].text.chars().count() <= 200);
        assert!(found[0].text.contains("[gz: this file has 500 characters"));
        let all = discover(&sandbox, None, &Limits::default());
        assert_eq!(
            shown(&all),
            [
                "~/code/app/GZ.md",
                "~/code/app/AGENTS.md",
                "~/code/AGENTS.md"
            ],
            "identical CLAUDE.md is sent once"
        );
    }

    #[test]
    fn gz_local_md_is_sent_as_the_users_own_after_gz_md() {
        let (home, sandbox) = tree("local");
        std::fs::write(home.join("code/app/GZ.md"), "team").unwrap();
        std::fs::write(home.join("code/app/GZ.local.md"), "mine").unwrap();
        sandbox.set_cwd(&home.join("code/app")).unwrap();
        let found = discover(&sandbox, None, &Limits::default());
        assert_eq!(
            shown(&found),
            ["~/code/app/GZ.md", "~/code/app/GZ.local.md"]
        );
        assert_eq!(found[1].source, Source::User);
        assert_eq!(found[0].source, Source::Project);
    }

    #[test]
    fn imports_are_appended_and_reported_in_the_entry() {
        let (home, sandbox) = tree("imports");
        std::fs::create_dir_all(home.join("code/app/docs")).unwrap();
        std::fs::write(home.join("code/app/docs/style.md"), "Use tabs").unwrap();
        std::fs::write(
            home.join("code/app/GZ.md"),
            "Rules: @docs/style.md and @docs/gone.md",
        )
        .unwrap();
        sandbox.set_cwd(&home.join("code/app")).unwrap();
        let found = discover(&sandbox, None, &Limits::default());
        assert_eq!(
            found[0].text,
            "Rules: @docs/style.md and @docs/gone.md\n\n[gz: imported from ~/code/app/docs/style.md]\nUse tabs"
        );
        assert_eq!(found[0].imports, ["~/code/app/docs/style.md"]);
        assert_eq!(found[0].dropped, ["@docs/gone.md: not found"]);
    }

    #[test]
    fn imports_past_the_size_limit_are_named_not_silently_cut() {
        let (home, sandbox) = tree("import-size");
        std::fs::write(home.join("code/app/small.md"), "s".repeat(20)).unwrap();
        std::fs::write(home.join("code/app/big.md"), "b".repeat(400)).unwrap();
        std::fs::write(home.join("code/app/GZ.md"), "@small.md @big.md").unwrap();
        sandbox.set_cwd(&home.join("code/app")).unwrap();
        let limits = Limits {
            instruction_chars: 300,
            ..Limits::default()
        };
        let found = discover(&sandbox, None, &limits);
        let entry = &found[0];
        assert!(entry.text.chars().count() <= 300, "{}", entry.text);
        assert_eq!(entry.imports, ["~/code/app/small.md"]);
        assert!(entry
            .text
            .ends_with("[gz: imports left out to stay within the size limit: ~/code/app/big.md]"));
        assert!(entry.dropped[0].contains("big.md") && entry.dropped[0].contains("300"));
        let tight = Limits {
            instruction_chars: 18,
            ..Limits::default()
        };
        let found = discover(&sandbox, None, &tight);
        assert!(found[0].text.chars().count() <= 18, "never past the limit");
        assert_eq!(found[0].dropped.len(), 2);
    }

    #[test]
    fn rules_without_paths_apply_and_bad_ones_are_reported() {
        let (home, sandbox) = tree("rules");
        let rules = home.join("code/app/.gz/rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("always.md"), "---\ntitle: x\n---\nAlways this").unwrap();
        std::fs::write(
            rules.join("rust.md"),
            "---\npaths: \"*.rs\"\n---\nRust rule",
        )
        .unwrap();
        std::fs::write(rules.join("broken.md"), "---\npaths: [\n---\n").unwrap();
        sandbox.set_cwd(&home.join("code/app")).unwrap();
        let found = find(&sandbox, None, &Limits::default(), &OnDemand::default());
        assert_eq!(shown(&found.sent), ["~/code/app/.gz/rules/always.md"]);
        assert_eq!(
            found.sent[0].text, "Always this",
            "front matter is not sent"
        );
        assert_eq!(found.left_out.len(), 1);
        assert!(found.left_out[0].shown.ends_with("broken.md"));
    }

    #[test]
    fn touching_a_file_picks_up_its_subfolder_files_and_matching_rules_at_the_end() {
        let (home, sandbox) = tree("on-demand");
        let app = home.join("code/app");
        std::fs::create_dir_all(app.join("src/api")).unwrap();
        std::fs::create_dir_all(app.join(".gz/rules")).unwrap();
        std::fs::write(app.join("GZ.md"), "top").unwrap();
        std::fs::write(app.join("src/AGENTS.md"), "src rules").unwrap();
        std::fs::write(app.join("src/api/GZ.md"), "api rules").unwrap();
        std::fs::write(app.join("src/api/h.rs"), "fn x() {}").unwrap();
        std::fs::write(app.join("src/main.py"), "x").unwrap();
        std::fs::write(
            app.join(".gz/rules/rust.md"),
            "---\npaths: [\"src/**/*.rs\"]\n---\nRust rule",
        )
        .unwrap();
        sandbox.set_cwd(&app).unwrap();
        let on_demand = OnDemand::default();
        let names =
            |on_demand: &OnDemand| shown(&find(&sandbox, None, &Limits::default(), on_demand).sent);
        assert_eq!(names(&on_demand), ["~/code/app/GZ.md"]);

        assert_eq!(on_demand.touch(&sandbox, &app.join("src/main.py")), 1);
        assert_eq!(
            names(&on_demand),
            ["~/code/app/GZ.md", "~/code/app/src/AGENTS.md"]
        );
        assert_eq!(on_demand.touch(&sandbox, &app.join("src/api/h.rs")), 2);
        let found = find(&sandbox, None, &Limits::default(), &on_demand);
        assert_eq!(
            shown(&found.sent),
            [
                "~/code/app/GZ.md",
                "~/code/app/src/AGENTS.md",
                "~/code/app/.gz/rules/rust.md",
                "~/code/app/src/api/GZ.md",
            ],
            "earlier entries keep their place; new ones outermost folder first"
        );
        assert_eq!(found.sent[1].why.describe(), "gz worked on src/main.py");
        assert_eq!(
            found.sent[2].why,
            Why::MatchedRule {
                touched: "src/api/h.rs".into(),
                globs: vec!["src/**/*.rs".into()]
            }
        );
        assert_eq!(
            on_demand.touch(&sandbox, &app.join("src/api/h.rs")),
            0,
            "once each"
        );
        on_demand.clear();
        assert_eq!(names(&on_demand), ["~/code/app/GZ.md"]);
    }

    #[test]
    fn entries_past_the_server_count_are_reported_as_left_out() {
        let (home, sandbox) = tree("count");
        std::fs::write(home.join("code/app/GZ.md"), "a").unwrap();
        std::fs::write(home.join("code/app/AGENTS.md"), "b").unwrap();
        sandbox.set_cwd(&home.join("code/app")).unwrap();
        let limits = Limits {
            instruction_files: 1,
            ..Limits::default()
        };
        let found = find(&sandbox, None, &limits, &OnDemand::default());
        assert_eq!(shown(&found.sent), ["~/code/app/GZ.md"]);
        assert_eq!(found.left_out[0].shown, "~/code/app/AGENTS.md");
        assert!(found.left_out[0].why.contains("at most 1"));
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A home with `notes/private.md` and gz opened at `code/app`, so
    /// files under `~/notes` are outside the open folder.
    fn app_with_private_notes(name: &str) -> (PathBuf, PathBuf, Sandbox) {
        let (home, _) = tree(name);
        std::fs::create_dir_all(home.join("notes")).unwrap();
        std::fs::write(home.join("notes/private.md"), "PRIVATE NOTES").unwrap();
        let app = home.join("code/app");
        let sandbox = Sandbox::with_home(&app, Some(&home)).unwrap();
        (home, app, sandbox)
    }

    fn local_entry(found: &Found) -> &Instruction {
        found
            .sent
            .iter()
            .find(|i| i.shown.ends_with(LOCAL_FILE))
            .expect("GZ.local.md is sent")
    }

    #[test]
    fn an_untracked_gz_local_md_is_yours_with_a_gitignore_hint_until_ignored() {
        let (_, app, sandbox) = app_with_private_notes("local-hint");
        std::fs::write(app.join(LOCAL_FILE), "mine @~/notes/private.md").unwrap();
        let entry = || {
            local_entry(&find(
                &sandbox,
                None,
                &Limits::default(),
                &OnDemand::default(),
            ))
            .clone()
        };
        let outside_git = entry();
        assert_eq!(outside_git.source, Source::User);
        assert_eq!(outside_git.note, None, "not a repository");
        assert!(
            outside_git.text.contains("PRIVATE NOTES"),
            "yours may import anything"
        );
        git_in(&app, &["init", "-q"]);
        let untracked = entry();
        assert_eq!(untracked.source, Source::User);
        assert!(untracked.note.as_deref().unwrap().contains(".gitignore"));
        std::fs::write(app.join(".gitignore"), "GZ.local.md\n").unwrap();
        assert_eq!(entry().note, None);
    }

    #[test]
    fn a_committed_gz_local_md_is_project_text_and_cannot_import_private_files() {
        let (_, app, sandbox) = app_with_private_notes("local-committed");
        std::fs::write(app.join(LOCAL_FILE), "do this @~/notes/private.md").unwrap();
        git_in(&app, &["init", "-q"]);
        git_in(&app, &["add", LOCAL_FILE]);
        git_in(&app, &["commit", "-q", "-m", "ship a local file"]);
        let found = find(&sandbox, None, &Limits::default(), &OnDemand::default());
        let entry = local_entry(&found);
        assert_eq!(entry.source, Source::Project);
        assert!(!entry.text.contains("PRIVATE NOTES"), "{}", entry.text);
        assert!(entry.imports.is_empty());
        assert!(entry.note.as_deref().unwrap().contains("commits it"));
    }

    #[test]
    fn a_gz_local_md_in_an_ignored_dependency_folder_is_project_text() {
        let (_, app, sandbox) = app_with_private_notes("local-dependency");
        let package = app.join("node_modules/pkg");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("index.js"), "x").unwrap();
        std::fs::write(package.join(LOCAL_FILE), "obey @~/notes/private.md").unwrap();
        std::fs::write(app.join(".gitignore"), "node_modules\n").unwrap();
        git_in(&app, &["init", "-q"]);
        let on_demand = OnDemand::default();
        assert_eq!(on_demand.touch(&sandbox, &package.join("index.js")), 1);
        let found = find(&sandbox, None, &Limits::default(), &on_demand);
        let entry = local_entry(&found);
        assert_eq!(entry.source, Source::Project);
        assert!(!entry.text.contains("PRIVATE NOTES"), "{}", entry.text);
        assert!(entry.note.as_deref().unwrap().contains("git ignores"));
    }

    #[test]
    fn outside_git_only_a_gz_local_md_in_the_folder_walk_is_yours() {
        let (_, app, sandbox) = app_with_private_notes("local-no-git");
        let unpacked = app.join("unpacked");
        std::fs::create_dir_all(&unpacked).unwrap();
        std::fs::write(unpacked.join("a.txt"), "x").unwrap();
        std::fs::write(unpacked.join(LOCAL_FILE), "obey @~/notes/private.md").unwrap();
        let on_demand = OnDemand::default();
        on_demand.touch(&sandbox, &unpacked.join("a.txt"));
        let found = find(&sandbox, None, &Limits::default(), &on_demand);
        let entry = local_entry(&found);
        assert_eq!(entry.source, Source::Project);
        assert!(!entry.text.contains("PRIVATE NOTES"));
        assert!(entry
            .note
            .as_deref()
            .unwrap()
            .contains("outside any git repository"));
    }

    #[cfg(unix)]
    #[test]
    fn an_instruction_file_linked_to_a_secret_is_skipped() {
        let (home, sandbox) = tree("secret");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "PRIVATE").unwrap();
        std::os::unix::fs::symlink(home.join(".ssh/id_ed25519"), home.join("AGENTS.md")).unwrap();
        assert!(discover(&sandbox, None, &Limits::default()).is_empty());
    }
}
