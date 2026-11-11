//! Keeps the model's confined commands from making git run a program later,
//! outside the sandbox.
//!
//! The OS sandbox denies writes to every protected repo's `hooks`. A repo's
//! config can't be closed the same way, since `git push -u`, `git remote
//! add` and `git config` write it. So gz saves each protected config before
//! a confined command and, when the command added or changed a key that
//! makes git run a program ([`KEYS_THAT_RUN_PROGRAMS`]), puts the file back
//! as it was and says so. Other changes (remotes, branches, `user.*`) stay.
//!
//! A repo the command creates itself (`git init`) was not protected while
//! it ran, so after the command its hooks (other than git's `*.sample`
//! templates) are deleted and those keys removed from its config.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Repos this many folders below an open folder are protected too (the
/// open folder itself is 0).
const MAX_REPO_DEPTH: usize = 4;

/// Most folders looked at per open folder when finding repos: it runs
/// before every command, so a huge tree must not slow each one down.
const MAX_FOLDERS_VISITED: usize = 2_000;

/// Folders never searched for repos: dependencies and build output, often
/// huge and holding no repo of the user's. Dot-folders are skipped too.
const SKIPPED_FOLDERS: &[&str] = &["node_modules", "target", ".venv", "vendor"];

/// Git's own hook templates, which never run.
const SAMPLE_HOOK_SUFFIX: &str = ".sample";

/// How much older than the command a git dir may look and still count as
/// made by it: some file systems keep times to the second.
const CREATED_SLACK: Duration = Duration::from_secs(2);

/// Git config keys whose value git runs as a program (or that pull in
/// more config). `*` stands for any subsection, or for any name where it is
/// the last part. An `alias.*` counts only when it runs a shell command
/// (`!…`) or starts with an option such as `-c`, which can set any of the
/// others for one git call.
pub const KEYS_THAT_RUN_PROGRAMS: &[&str] = &[
    "core.hooksPath",
    "core.fsmonitor",
    "core.sshCommand",
    "core.pager",
    "core.editor",
    "core.askPass",
    "core.gitProxy",
    "sequence.editor",
    "diff.external",
    "alias.*",
    "pager.*",
    "filter.*.clean",
    "filter.*.smudge",
    "filter.*.process",
    "diff.*.textconv",
    "diff.*.command",
    "difftool.*.cmd",
    "merge.*.driver",
    "mergetool.*.cmd",
    "remote.*.uploadpack",
    "remote.*.receivepack",
    "credential.helper",
    "credential.*.helper",
    "gpg.program",
    "gpg.*.program",
    "include.path",
    "includeIf.*.path",
];

/// One repo's git dir under an open folder: its hooks are closed to
/// commands and its config files watched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDir {
    pub hooks: PathBuf,
    /// `config`, and `config.worktree`, which git also reads when the repo
    /// turns it on.
    pub configs: Vec<PathBuf>,
}

/// The git dirs of the repos at or below `roots` (real paths), up to
/// [`MAX_REPO_DEPTH`] folders down. A `.git` file (a worktree or
/// submodule) counts when the git dir it points to is inside a root.
pub fn find_git_dirs(roots: &[PathBuf]) -> Vec<GitDir> {
    let mut found: Vec<GitDir> = Vec::new();
    for root in roots {
        for repo_dir in repo_dirs_below(root) {
            let Some(git_dir) = git_dir_of(&repo_dir, roots) else {
                continue;
            };
            // Worktrees share their main repo's hooks; each adds its own
            // `config.worktree`.
            match found.iter_mut().find(|known| known.hooks == git_dir.hooks) {
                Some(known) => {
                    for config in git_dir.configs {
                        if !known.configs.contains(&config) {
                            known.configs.push(config);
                        }
                    }
                }
                None => found.push(git_dir),
            }
        }
    }
    found
}

/// Folders at or below `root` that hold a `.git`, breadth first.
fn repo_dirs_below(root: &Path) -> Vec<PathBuf> {
    let mut repos = Vec::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), 0)]);
    let mut visited = 0;
    while let Some((dir, depth)) = queue.pop_front() {
        visited += 1;
        if visited > MAX_FOLDERS_VISITED {
            break;
        }
        if dir.join(".git").symlink_metadata().is_ok() {
            repos.push(dir.clone());
        }
        if depth == MAX_REPO_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // `file_type` doesn't follow links: a linked folder is not walked.
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_dir && !name.starts_with('.') && !SKIPPED_FOLDERS.contains(&name.as_ref()) {
                queue.push_back((entry.path(), depth + 1));
            }
        }
    }
    repos
}

/// The git dir `repo_dir/.git` stands for, when it lies inside `roots`
/// (one outside them is not writable by commands anyway).
fn git_dir_of(repo_dir: &Path, roots: &[PathBuf]) -> Option<GitDir> {
    let dot_git = repo_dir.join(".git");
    let own = if dot_git.is_file() {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let target = text.trim().strip_prefix("gitdir:")?.trim();
        repo_dir.join(target).canonicalize().ok()?
    } else {
        dot_git.canonicalize().ok()?
    };
    // A linked worktree's hooks and config live in the main repo's git dir.
    let common = match std::fs::read_to_string(own.join("commondir")) {
        Ok(text) => own.join(text.trim()).canonicalize().ok()?,
        Err(_) => own.clone(),
    };
    if !roots.iter().any(|root| common.starts_with(root)) {
        return None;
    }
    Some(GitDir {
        hooks: common.join("hooks"),
        configs: vec![common.join("config"), own.join("config.worktree")],
    })
}

/// The protected repos as they were before a command, to compare and
/// restore after it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitWatch {
    /// Each config and its bytes; `None` when it did not exist.
    saved: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// The hooks folders of the repos that existed.
    known_hooks: Vec<PathBuf>,
    /// The open folders, searched again for repos the command made.
    roots: Vec<PathBuf>,
    /// When the command started; `None` treats nothing as new.
    started: Option<SystemTime>,
}

impl GitWatch {
    /// Saves the configs of `git_dirs`, found in `roots`, as they are now.
    /// One that is not a plain file (a link) is not watched, since it could
    /// not be put back.
    pub fn save(git_dirs: &[GitDir], roots: &[PathBuf]) -> Self {
        let saved = git_dirs
            .iter()
            .flat_map(|dir| dir.configs.iter())
            .filter_map(|path| match read_plain(path) {
                ConfigFile::Missing => Some((path.clone(), None)),
                ConfigFile::Plain(bytes) => Some((path.clone(), Some(bytes))),
                ConfigFile::NotPlain => None,
            })
            .collect();
        Self {
            saved,
            known_hooks: git_dirs.iter().map(|dir| dir.hooks.clone()).collect(),
            roots: roots.to_vec(),
            started: Some(SystemTime::now()),
        }
    }

    /// Puts back each config the command changed so that git would run a
    /// program, cleans the repos it made, and says what and why.
    pub fn restore_risky_changes(&self) -> Reverts {
        let reverted = self
            .saved
            .iter()
            .filter_map(|(path, before)| {
                let (why, undone) = risky_change(before.as_deref(), &read_plain(path))?;
                let restored = restore(path, before.as_deref()).map_err(|err| err.to_string());
                Some(Reverted {
                    config: path.clone(),
                    why,
                    undone,
                    restored,
                })
            })
            .collect();
        Reverts {
            reverted,
            cleaned: self.clean_new_repos(),
        }
    }

    /// Repos (and worktree configs) that appeared during the command: the
    /// sandbox could not guard them, so what would run a program goes.
    fn clean_new_repos(&self) -> Vec<Cleaned> {
        if self.roots.is_empty() {
            return Vec::new();
        }
        find_git_dirs(&self.roots)
            .into_iter()
            .filter_map(|dir| self.clean(&dir))
            .collect()
    }

    fn clean(&self, dir: &GitDir) -> Option<Cleaned> {
        let git_dir = dir.hooks.parent()?.to_path_buf();
        let is_new_repo = !self.known_hooks.contains(&dir.hooks) && self.made_since_start(&git_dir);
        let mut cleaned = Cleaned {
            git_dir,
            ..Cleaned::default()
        };
        if is_new_repo {
            remove_hooks(&dir.hooks, &mut cleaned);
        }
        for config in &dir.configs {
            let was_watched = self.saved.iter().any(|(path, _)| path == config);
            let is_new = is_new_repo || (!was_watched && self.made_since_start(config));
            if is_new {
                strip_program_keys(config, &mut cleaned);
            }
        }
        (!cleaned.is_empty()).then_some(cleaned)
    }

    /// Whether `path` was created after the command started; a file
    /// system that can't tell never counts, so the user's own files stay.
    fn made_since_start(&self, path: &Path) -> bool {
        let Some(started) = self.started else {
            return false;
        };
        let created = path.symlink_metadata().and_then(|meta| meta.created());
        created.is_ok_and(|created| created + CREATED_SLACK >= started)
    }
}

/// Deletes every hook in `hooks` but git's `*.sample` templates; a hooks
/// link goes as a whole, without touching what it points to.
fn remove_hooks(hooks: &Path, cleaned: &mut Cleaned) {
    let Ok(meta) = hooks.symlink_metadata() else {
        return;
    };
    if !meta.is_dir() {
        match remove_any(hooks) {
            Ok(()) => cleaned.removed.push("its hooks link".into()),
            Err(err) => cleaned
                .problems
                .push(format!("could not remove {} ({err})", hooks.display())),
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(hooks) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_template = name.ends_with(SAMPLE_HOOK_SUFFIX)
            && entry.file_type().is_ok_and(|kind| kind.is_file());
        if is_template {
            continue;
        }
        match remove_any(&entry.path()) {
            Ok(()) => cleaned.removed.push(format!("the hook {name}")),
            Err(err) => cleaned
                .problems
                .push(format!("could not remove the hook {name} ({err})")),
        }
    }
}

/// Drops the entries of `config` that make git run a program, keeping
/// the rest of the file as the command wrote it. A config gz can't read,
/// or a link in its place, is removed whole.
fn strip_program_keys(config: &Path, cleaned: &mut Cleaned) {
    let file = config
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let text = match read_plain(config) {
        ConfigFile::Missing => return,
        ConfigFile::Plain(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        ConfigFile::NotPlain => {
            match remove_any(config) {
                Ok(()) => cleaned.removed.push(format!("its {file}, a link")),
                Err(err) => cleaned
                    .problems
                    .push(format!("could not remove {} ({err})", config.display())),
            }
            return;
        }
    };
    let (kept, removed) = match without_program_keys(&text) {
        Ok(stripped) => stripped,
        Err(why) => {
            match remove_any(config) {
                Ok(()) => cleaned
                    .removed
                    .push(format!("its {file}, which gz could not read ({why})")),
                Err(err) => cleaned
                    .problems
                    .push(format!("could not remove {} ({err})", config.display())),
            }
            return;
        }
    };
    if removed.is_empty() {
        return;
    }
    match std::fs::write(config, kept) {
        Ok(()) => cleaned.removed.extend(removed),
        Err(err) => cleaned.problems.push(format!(
            "could not remove {} from {} ({err})",
            removed.join(", "),
            config.display()
        )),
    }
}

/// `text` without its entries that make git run a program (each from the
/// start of its line through its end), and those entries' names.
fn without_program_keys(text: &str) -> Result<(String, Vec<String>), String> {
    let chars: Vec<char> = text.chars().collect();
    let mut cut: Vec<Range<usize>> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for entry in parse_entries(text)? {
        let Some(pattern) = matching_pattern(&entry.key) else {
            continue;
        };
        if !value_runs_program(&entry.key, entry.value.as_deref()) {
            continue;
        }
        cut.push(whole_line(&chars, entry.span));
        let name = key_name(&entry.key, pattern);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let kept = chars
        .iter()
        .enumerate()
        .filter(|(at, _)| !cut.iter().any(|range| range.contains(at)))
        .map(|(_, c)| c)
        .collect();
    Ok((kept, names))
}

/// `span` widened back over the indent before it, when only indent sits
/// between it and the start of its line (a key after a `[section]` on the
/// same line keeps the header).
fn whole_line(chars: &[char], span: Range<usize>) -> Range<usize> {
    let mut start = span.start;
    while start > 0 && matches!(chars[start - 1], ' ' | '\t') {
        start -= 1;
    }
    if start == 0 || chars[start - 1] == '\n' {
        start..span.end
    } else {
        span
    }
}

enum ConfigFile {
    Missing,
    Plain(Vec<u8>),
    /// A link or a folder in its place.
    NotPlain,
}

fn read_plain(path: &Path) -> ConfigFile {
    match path.symlink_metadata() {
        Err(_) => ConfigFile::Missing,
        Ok(meta) if !meta.is_file() => ConfigFile::NotPlain,
        Ok(_) => match std::fs::read(path) {
            Ok(bytes) => ConfigFile::Plain(bytes),
            Err(_) => ConfigFile::NotPlain,
        },
    }
}

/// Why `after` must not stay, given the config was `before`, and the
/// command's other changes that putting `before` back undoes.
fn risky_change(before: Option<&[u8]>, after: &ConfigFile) -> Option<(Why, Vec<String>)> {
    let after = match after {
        ConfigFile::Missing => return None,
        ConfigFile::NotPlain => return Some((Why::Replaced, Vec::new())),
        ConfigFile::Plain(bytes) if Some(bytes.as_slice()) == before => return None,
        ConfigFile::Plain(bytes) => bytes,
    };
    let after = match parse(&String::from_utf8_lossy(after)) {
        Ok(entries) => entries,
        // Git may still read what gz can't, so it isn't left in place.
        Err(why) => return Some((Why::Unreadable(why), Vec::new())),
    };
    // The config as it was parsed when the command started; a file that
    // was already unreadable compares as empty, so every risky key counts.
    let before = before
        .and_then(|bytes| parse(&String::from_utf8_lossy(bytes)).ok())
        .unwrap_or_default();
    let keys = keys_that_now_run_programs(&before, &after);
    if keys.is_empty() {
        return None;
    }
    let undone = other_changed_keys(&before, &after);
    Some((Why::Keys(keys), undone))
}

/// Keys added, changed or removed between `before` and `after` that run
/// no program: restoring `before` undoes them too.
fn other_changed_keys(before: &Entries, after: &Entries) -> Vec<String> {
    let mut keys: Vec<&ConfigKey> = before.keys().chain(after.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|key| before.get(*key) != after.get(*key))
        .filter(|key| !runs_program(key, after.get(*key)))
        .map(ConfigKey::to_string)
        .collect()
}

fn runs_program(key: &ConfigKey, values: Option<&Vec<Option<String>>>) -> bool {
    matching_pattern(key).is_some()
        && values.is_some_and(|values| {
            values
                .iter()
                .any(|value| value_runs_program(key, value.as_deref()))
        })
}

fn restore(path: &Path, before: Option<&[u8]>) -> std::io::Result<()> {
    let is_plain = path.symlink_metadata().is_ok_and(|meta| meta.is_file());
    if !is_plain {
        // Writing through a link would change the file it points to.
        remove_any(path)?;
    }
    match before {
        Some(bytes) => std::fs::write(path, bytes),
        None => remove_any(path),
    }
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    let Ok(meta) = path.symlink_metadata() else {
        return Ok(());
    };
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Why gz put a config back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// These keys were added or changed.
    Keys(Vec<String>),
    /// The new file could not be read as git config.
    Unreadable(String),
    /// The file was replaced by a link or a folder.
    Replaced,
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::Keys(keys) => write!(f, "it set {}", keys.join(", ")),
            Why::Unreadable(why) => write!(f, "gz could not read the new file ({why})"),
            Why::Replaced => write!(f, "it replaced the file with a link"),
        }
    }
}

/// One config gz put back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reverted {
    pub config: PathBuf,
    pub why: Why,
    /// The command's other changes to it that the restore undid.
    pub undone: Vec<String>,
    /// `Err` when the old file could not be written back.
    pub restored: Result<(), String>,
}

/// A repo the command made, and what gz took out of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cleaned {
    pub git_dir: PathBuf,
    /// Hooks and config keys removed, in words (`the hook pre-commit`,
    /// `core.hooksPath`).
    pub removed: Vec<String>,
    /// What gz could not remove, and why.
    pub problems: Vec<String>,
}

impl Cleaned {
    fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.problems.is_empty()
    }

    fn summary(&self) -> String {
        let mut text = format!("In {}, which the command created", self.git_dir.display());
        if !self.removed.is_empty() {
            text.push_str(&format!(", gz removed {}", self.removed.join(", ")));
        }
        if !self.problems.is_empty() {
            let joiner = if self.removed.is_empty() {
                ", gz "
            } else {
                "; it "
            };
            text.push_str(&format!("{joiner}{}", self.problems.join("; ")));
        }
        text.push_str(": git would run them outside the sandbox.");
        text
    }
}

/// What gz put back or removed after one command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reverts {
    pub reverted: Vec<Reverted>,
    pub cleaned: Vec<Cleaned>,
}

impl Reverts {
    pub fn is_empty(&self) -> bool {
        self.reverted.is_empty() && self.cleaned.is_empty()
    }

    /// What the model reads in the command's result.
    pub fn model_note(&self) -> String {
        let reverted = self.reverted.iter().map(|reverted| {
            let mut line = match &reverted.restored {
                Ok(()) => format!(
                    "gz restored {} to how it was before this command: {}, which makes git \
                     run a program later, outside the sandbox.",
                    reverted.config.display(),
                    reverted.why
                ),
                Err(err) => format!(
                    "{}: {}, which makes git run a program outside the sandbox, and gz could \
                     not restore it ({err}).",
                    reverted.config.display(),
                    reverted.why
                ),
            };
            if reverted.restored.is_ok() && !reverted.undone.is_empty() {
                line.push_str(&format!(
                    " Restoring it also undid this command's other changes there: {}; redo \
                     those in a separate command if they are still needed.",
                    reverted.undone.join(", ")
                ));
            }
            line
        });
        let lines: Vec<String> = reverted
            .chain(self.cleaned.iter().map(Cleaned::summary))
            .collect();
        format!(
            "[{} Do not try another way around it; tell the user. If they meant it, they can \
             run the command themselves with !command.]",
            lines.join(" ")
        )
    }

    /// What the user sees as a notice.
    pub fn user_notice(&self) -> String {
        let reverted = self.reverted.iter().map(|reverted| {
            let mut line = match &reverted.restored {
                Ok(()) => format!(
                    "gz reverted {}: the model's command {}, and git would run that program \
                     outside the sandbox.",
                    reverted.config.display(),
                    reverted.why
                ),
                Err(err) => format!(
                    "Check {} before you run git there: the model's command {}, and gz could \
                     not put the old file back ({err}).",
                    reverted.config.display(),
                    reverted.why
                ),
            };
            if reverted.restored.is_ok() && !reverted.undone.is_empty() {
                line.push_str(&format!(
                    " That also undid its {}.",
                    reverted.undone.join(", ")
                ));
            }
            line
        });
        let lines: Vec<String> = reverted
            .chain(self.cleaned.iter().map(Cleaned::summary))
            .collect();
        format!(
            "{} If you meant it, run the command yourself with !command.",
            lines.join(" ")
        )
    }
}

/// A config key: section and name lowercased, as git compares them; the
/// subsection keeps its case.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ConfigKey {
    section: String,
    subsection: Option<String>,
    name: String,
}

impl fmt::Display for ConfigKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.subsection {
            Some(sub) => write!(f, "{}.{sub}.{}", self.section, self.name),
            None => write!(f, "{}.{}", self.section, self.name),
        }
    }
}

/// Each key's values in file order; `None` is a bare key (true).
type Entries = BTreeMap<ConfigKey, Vec<Option<String>>>;

/// The risky keys `after` has that `before` did not have with the same
/// values, named as [`KEYS_THAT_RUN_PROGRAMS`] spells them.
fn keys_that_now_run_programs(before: &Entries, after: &Entries) -> Vec<String> {
    after
        .iter()
        .filter_map(|(key, values)| {
            let pattern = matching_pattern(key)?;
            let runs = values
                .iter()
                .any(|value| value_runs_program(key, value.as_deref()));
            let is_new = before.get(key) != Some(values);
            (runs && is_new).then(|| key_name(key, pattern))
        })
        .collect()
}

/// `key` as [`KEYS_THAT_RUN_PROGRAMS`] spells it, or in full for a
/// wildcard entry.
fn key_name(key: &ConfigKey, pattern: &str) -> String {
    if pattern.contains('*') {
        key.to_string()
    } else {
        pattern.to_string()
    }
}

fn matching_pattern(key: &ConfigKey) -> Option<&'static str> {
    KEYS_THAT_RUN_PROGRAMS.iter().copied().find(|pattern| {
        let parts: Vec<&str> = pattern.split('.').collect();
        match parts.as_slice() {
            [section, name] => {
                key.subsection.is_none()
                    && section.eq_ignore_ascii_case(&key.section)
                    && (*name == "*" || name.eq_ignore_ascii_case(&key.name))
            }
            [section, _, name] => {
                key.subsection.is_some()
                    && section.eq_ignore_ascii_case(&key.section)
                    && name.eq_ignore_ascii_case(&key.name)
            }
            _ => false,
        }
    })
}

fn value_runs_program(key: &ConfigKey, value: Option<&str>) -> bool {
    if key.section != "alias" {
        return true;
    }
    value.is_some_and(|alias| alias.starts_with('!') || alias.starts_with('-'))
}

/// Parses git config text the way git reads it: `[section]`, `[section
/// "subsection"]` and the old `[section.subsection]` headers, `key = value`
/// lines (a bare key is true), `#`/`;` comments, quotes, escapes and `\`
/// line continuations.
fn parse(text: &str) -> Result<Entries, String> {
    let mut entries = Entries::new();
    for entry in parse_entries(text)? {
        entries.entry(entry.key).or_default().push(entry.value);
    }
    Ok(entries)
}

/// One `key = value` of a config file.
struct ParsedEntry {
    key: ConfigKey,
    value: Option<String>,
    /// Where it sits in the text, in characters: from the key through the
    /// end of its (continued) line, newline included.
    span: Range<usize>,
}

/// The entries of `text` in file order; see [`parse`].
fn parse_entries(text: &str) -> Result<Vec<ParsedEntry>, String> {
    let mut reader = Reader {
        chars: text.chars().collect(),
        at: 0,
        line: 1,
    };
    let mut entries = Vec::new();
    let mut section: Option<(String, Option<String>)> = None;
    while let Some(c) = reader.skip_whitespace() {
        match c {
            '#' | ';' => reader.skip_line(),
            '[' => section = Some(reader.header()?),
            c if c.is_ascii_alphabetic() => {
                let Some((section, subsection)) = &section else {
                    return Err(reader.error("a key before any [section]"));
                };
                let start = reader.at;
                let (name, value) = reader.entry()?;
                // A bare key leaves its newline unread.
                if reader.peek() == Some('\n') {
                    reader.next();
                }
                entries.push(ParsedEntry {
                    key: ConfigKey {
                        section: section.clone(),
                        subsection: subsection.clone(),
                        name,
                    },
                    value,
                    span: start..reader.at,
                });
            }
            _ => return Err(reader.error("an unexpected character")),
        }
    }
    Ok(entries)
}

struct Reader {
    chars: Vec<char>,
    at: usize,
    line: usize,
}

impl Reader {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn error(&self, what: &str) -> String {
        format!("{what} on line {}", self.line)
    }

    /// Skips whitespace and returns the next character, unread.
    fn skip_whitespace(&mut self) -> Option<char> {
        while self.peek()?.is_whitespace() {
            self.next();
        }
        self.peek()
    }

    fn skip_line(&mut self) {
        while let Some(c) = self.next() {
            if c == '\n' {
                return;
            }
        }
    }

    fn header(&mut self) -> Result<(String, Option<String>), String> {
        self.next(); // `[`
        let mut name = String::new();
        let subsection = loop {
            match self.next() {
                Some(']') => break None,
                Some(c) if c.is_ascii_alphanumeric() || c == '-' || c == '.' => name.push(c),
                Some(' ' | '\t') => break Some(self.quoted_subsection()?),
                _ => return Err(self.error("a bad section header")),
            }
        };
        if name.is_empty() {
            return Err(self.error("an empty section name"));
        }
        let name = name.to_ascii_lowercase();
        Ok(match subsection {
            Some(sub) => (name, Some(sub)),
            // `[section.sub]`: the old spelling, compared without case.
            None => match name.split_once('.') {
                Some((section, sub)) => (section.to_string(), Some(sub.to_string())),
                None => (name, None),
            },
        })
    }

    /// `"sub"]` after a section name and a space.
    fn quoted_subsection(&mut self) -> Result<String, String> {
        if self.skip_whitespace() != Some('"') {
            return Err(self.error("a bad section header"));
        }
        self.next();
        let mut sub = String::new();
        loop {
            match self.next() {
                Some('"') => break,
                Some('\\') => match self.next() {
                    Some('\n') | None => return Err(self.error("a bad section header")),
                    Some(c) => sub.push(c),
                },
                Some('\n') | None => return Err(self.error("a bad section header")),
                Some(c) => sub.push(c),
            }
        }
        match self.next() {
            Some(']') => Ok(sub),
            _ => Err(self.error("a bad section header")),
        }
    }

    /// `name [= value]` up to the end of its (continued) line.
    fn entry(&mut self) -> Result<(String, Option<String>), String> {
        let mut name = String::new();
        while let Some(c) = self
            .peek()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        {
            name.push(c.to_ascii_lowercase());
            self.next();
        }
        while matches!(self.peek(), Some(' ' | '\t' | '\r')) {
            self.next();
        }
        match self.peek() {
            Some('=') => {
                self.next();
                self.value().map(|value| (name, Some(value)))
            }
            None | Some('\n') => Ok((name, None)),
            Some('#' | ';') => {
                self.skip_line();
                Ok((name, None))
            }
            Some(_) => Err(self.error("a bad key")),
        }
    }

    fn value(&mut self) -> Result<String, String> {
        let mut value = String::new();
        let mut pending_space = String::new();
        let mut quoted = false;
        loop {
            let Some(c) = self.next() else {
                return if quoted {
                    Err(self.error("an unclosed quote"))
                } else {
                    Ok(value)
                };
            };
            match c {
                '\n' if quoted => return Err(self.error("an unclosed quote")),
                '\n' => return Ok(value),
                '#' | ';' if !quoted => {
                    self.skip_line();
                    return Ok(value);
                }
                c if c.is_whitespace() && !quoted => {
                    // Leading and trailing space is dropped; inner space kept.
                    if !value.is_empty() {
                        pending_space.push(c);
                    }
                }
                '"' => {
                    value.push_str(&std::mem::take(&mut pending_space));
                    quoted = !quoted;
                }
                '\\' => {
                    value.push_str(&std::mem::take(&mut pending_space));
                    match self.next() {
                        Some('\n') => {}
                        Some('n') => value.push('\n'),
                        Some('t') => value.push('\t'),
                        Some('b') => value.push('\u{8}'),
                        Some(c @ ('\\' | '"')) => value.push(c),
                        _ => return Err(self.error("a bad escape")),
                    }
                }
                c => {
                    value.push_str(&std::mem::take(&mut pending_space));
                    value.push(c);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::sandbox::tests::temp_dir;

    fn key(text: &str) -> ConfigKey {
        let parts: Vec<&str> = text.split('.').collect();
        match parts.as_slice() {
            [section, name] => ConfigKey {
                section: section.to_string(),
                subsection: None,
                name: name.to_string(),
            },
            [section, sub, name] => ConfigKey {
                section: section.to_string(),
                subsection: Some(sub.to_string()),
                name: name.to_string(),
            },
            _ => panic!("{text}"),
        }
    }

    fn risky(before: &str, after: &str) -> Vec<String> {
        keys_that_now_run_programs(&parse(before).unwrap(), &parse(after).unwrap())
    }

    #[test]
    fn the_parser_reads_headers_quotes_comments_and_continuations() {
        let text = "# top\n[core]\n\tbare = true\n\tHooksPath = \" x\\\"y\" ; comment\n\
                    [remote \"Origin\"] url = git@h:r.git\n[Branch.Main]\n  merge = a\\\n b\n\
                    [user]\n  flag\n";
        let entries = parse(text).unwrap();
        assert_eq!(
            entries[&key("core.hookspath")],
            vec![Some(" x\"y".to_string())]
        );
        assert_eq!(
            entries[&key("remote.Origin.url")],
            vec![Some("git@h:r.git".to_string())]
        );
        assert_eq!(
            entries[&key("branch.main.merge")],
            vec![Some("a b".to_string())]
        );
        assert_eq!(entries[&key("user.flag")], vec![None]);
        assert!(parse("key = v").is_err(), "a key needs a section");
        assert!(parse("[core]\n x = \"open\n").is_err());
    }

    #[test]
    fn only_new_or_changed_keys_that_run_programs_count() {
        let before = "[core]\n\teditor = vim\n[user]\n\tname = a\n";
        assert_eq!(
            risky(before, "[core]\n\teditor = vim\n\thooksPath = /x\n"),
            vec!["core.hooksPath"]
        );
        assert_eq!(
            risky(before, "[core]\n\teditor = vim\n"),
            Vec::<String>::new()
        );
        assert_eq!(
            risky(
                before,
                "[core]\n\teditor = vim\n[user]\n\tname = b\n[remote \"o\"]\n\turl = u\n"
            ),
            Vec::<String>::new(),
            "remotes and user.* stay"
        );
        assert_eq!(
            risky(before, "[core]\n\teditor = evil\n"),
            vec!["core.editor"]
        );
        assert_eq!(
            risky(
                "",
                "[filter \"lfs\"]\n\tclean = run\n[includeIf \"gitdir:/x\"]\n\tpath = p\n"
            ),
            vec!["filter.lfs.clean", "includeif.gitdir:/x.path"]
        );
    }

    #[test]
    fn stripping_drops_only_the_lines_of_keys_that_run_programs() {
        let text = "[core]\n\trepositoryformatversion = 0\n\thooksPath = /x\n\
                    [alias \"\"] st = status\n[alias] x = !sh \\\n  evil\n\
                    [remote \"o\"]\n\turl = u\n\tfsmonitor\n";
        let (kept, names) = without_program_keys(text).unwrap();
        assert_eq!(names, vec!["core.hooksPath", "alias.x"]);
        assert_eq!(
            kept,
            "[core]\n\trepositoryformatversion = 0\n[alias \"\"] st = status\n[alias] \
             [remote \"o\"]\n\turl = u\n\tfsmonitor\n",
            "a key sharing its header's line keeps the header"
        );
        assert!(keys_that_now_run_programs(&Entries::new(), &parse(&kept).unwrap()).is_empty());
    }

    #[test]
    fn an_alias_counts_only_when_it_runs_a_shell_or_sets_options() {
        assert_eq!(
            risky(
                "",
                "[alias]\n\tst = status\n\tx = !sh evil\n\ty = -c core.pager=evil log\n"
            ),
            vec!["alias.x", "alias.y"]
        );
    }

    fn repo_with_config(name: &str) -> (PathBuf, PathBuf) {
        let root = temp_dir(name);
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        let config = root.join(".git/config");
        std::fs::write(&config, "[core]\n\tbare = false\n").unwrap();
        (root, config)
    }

    #[test]
    fn a_risky_change_is_put_back_and_a_harmless_one_kept() {
        let (root, config) = repo_with_config("guard-restore");
        let roots = [root.clone()];
        let watch = GitWatch::save(&find_git_dirs(&roots), &roots);
        std::fs::write(
            &config,
            "[core]\n\tbare = false\n\tfsmonitor = ./x\n[user]\n\tname = y\n",
        )
        .unwrap();
        let reverts = watch.restore_risky_changes();
        assert_eq!(reverts.reverted.len(), 1, "{reverts:?}");
        assert_eq!(
            reverts.reverted[0].why,
            Why::Keys(vec!["core.fsmonitor".into()])
        );
        assert_eq!(reverts.reverted[0].undone, vec!["user.name".to_string()]);
        assert!(reverts.model_note().contains("user.name"));
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "[core]\n\tbare = false\n"
        );
        let notice = reverts.user_notice();
        assert!(
            notice.contains("core.fsmonitor") && notice.contains("!command"),
            "{notice}"
        );

        let kept = "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = u\n";
        std::fs::write(&config, kept).unwrap();
        assert!(watch.restore_risky_changes().is_empty());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), kept);
    }

    #[cfg(unix)]
    #[test]
    fn a_config_replaced_by_a_link_is_put_back_as_a_file() {
        let (root, config) = repo_with_config("guard-link");
        let elsewhere = root.join("elsewhere.cfg");
        std::fs::write(&elsewhere, "[user]\n\tname = x\n").unwrap();
        let roots = [root.clone()];
        let watch = GitWatch::save(&find_git_dirs(&roots), &roots);
        std::fs::remove_file(&config).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &config).unwrap();
        let reverts = watch.restore_risky_changes();
        assert_eq!(reverts.reverted[0].why, Why::Replaced);
        assert!(config.symlink_metadata().unwrap().is_file());
        assert_eq!(
            std::fs::read_to_string(&elsewhere).unwrap(),
            "[user]\n\tname = x\n",
            "the link's target is left alone"
        );
    }

    #[test]
    fn nested_repos_and_worktrees_inside_the_folder_are_found() {
        let root = temp_dir("guard-find");
        for repo in [
            "a/b/c",
            "node_modules/dep",
            ".hidden/r",
            "a/b/c/d/e/too-deep",
        ] {
            std::fs::create_dir_all(root.join(repo).join(".git")).unwrap();
        }
        // A worktree whose git dir lives in the open folder's own repo.
        std::fs::create_dir_all(root.join(".git/worktrees/wt")).unwrap();
        std::fs::write(root.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        std::fs::create_dir_all(root.join("wt")).unwrap();
        std::fs::write(root.join("wt/.git"), "gitdir: ../.git/worktrees/wt\n").unwrap();
        // One pointing outside the open folders.
        let outside = temp_dir("guard-outside");
        std::fs::create_dir_all(outside.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("linked")).unwrap();
        std::fs::write(
            root.join("linked/.git"),
            format!("gitdir: {}\n", outside.join(".git").display()),
        )
        .unwrap();

        let found = find_git_dirs(std::slice::from_ref(&root));
        let hooks: Vec<PathBuf> = found.iter().map(|dir| dir.hooks.clone()).collect();
        assert_eq!(
            hooks,
            vec![root.join(".git/hooks"), root.join("a/b/c/.git/hooks")],
            "the worktree shares the root repo's hooks; skipped and deep ones are not found"
        );
        assert_eq!(
            found[0].configs,
            vec![
                root.join(".git/config"),
                root.join(".git/config.worktree"),
                root.join(".git/worktrees/wt/config.worktree")
            ]
        );
    }
}
