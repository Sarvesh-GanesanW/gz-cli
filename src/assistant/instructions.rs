//! Instruction files: the user's personal
//! `instructions.md` next to the gz config, then `GZ.md`, `AGENTS.md` and
//! `CLAUDE.md` found walking up from the current folder to the home folder
//! (or to the top open folder outside home), nearest first. They are read
//! again for every turn, so an edit applies to the next message, and go out
//! as `clientContext.instructions`.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::assistant::local_context::Limits;
use crate::assistant::sandbox::Sandbox;

/// Project instruction files, in the order read within one folder.
pub const PROJECT_FILES: [&str; 3] = ["GZ.md", "AGENTS.md", "CLAUDE.md"];
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

/// One instruction file as it goes to the chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub source: Source,
    /// Where it is, shown as `~/…` under home.
    pub shown: String,
    /// What is sent: the file's text, cut to the limit with a note.
    pub text: String,
    /// The file's size on disk, for `/instructions`.
    pub bytes: u64,
    pub truncated: bool,
}

/// The personal instructions file: `<gz config folder>/instructions.md`.
pub fn user_file() -> Result<PathBuf> {
    let config = crate::config::config_path()?;
    let dir = config.parent().context("config path has no folder")?;
    Ok(dir.join(USER_FILE))
}

/// Every instruction file that applies in the sandbox's current folder:
/// the user's file first, then project files nearest first; at most
/// `limits.instruction_files`, each cut at `limits.instruction_chars`, the
/// same text only once, never a secret file.
pub fn discover(sandbox: &Sandbox, user: Option<&Path>, limits: &Limits) -> Vec<Instruction> {
    let candidates = user
        .map(|path| (Source::User, path.to_path_buf()))
        .into_iter()
        .chain(
            project_candidates(sandbox)
                .into_iter()
                .map(|path| (Source::Project, path)),
        );
    let mut found: Vec<Instruction> = Vec::new();
    let mut seen_texts: Vec<String> = Vec::new();
    for (source, path) in candidates {
        if found.len() >= limits.instruction_files {
            break;
        }
        let Some((text, bytes)) = read_instruction(sandbox, &path) else {
            continue;
        };
        if text.trim().is_empty() || seen_texts.contains(&text) {
            continue;
        }
        let (sent, truncated) = cut(&text, limits.instruction_chars);
        seen_texts.push(text);
        found.push(Instruction {
            source,
            shown: sandbox.display_folder(&path),
            text: sent,
            bytes,
            truncated,
        });
    }
    found
}

/// `GZ.md`, `AGENTS.md`, `CLAUDE.md` in the current folder, then in each
/// folder above it up to where the walk stops.
fn project_candidates(sandbox: &Sandbox) -> Vec<PathBuf> {
    let cwd = sandbox.cwd();
    let stop = walk_stop(sandbox, &cwd);
    let mut out = Vec::new();
    let mut folder = Some(cwd.as_path());
    while let Some(dir) = folder {
        out.extend(PROJECT_FILES.iter().map(|name| dir.join(name)));
        if dir == stop {
            break;
        }
        folder = dir.parent();
    }
    out
}

/// Home when the folder is inside it, else the outermost open folder that
/// holds it.
fn walk_stop(sandbox: &Sandbox, cwd: &Path) -> PathBuf {
    if let Some(home) = sandbox.home().filter(|home| cwd.starts_with(home)) {
        return home.to_path_buf();
    }
    sandbox
        .roots()
        .into_iter()
        .filter(|root| cwd.starts_with(root))
        .min_by_key(|root| root.components().count())
        .unwrap_or_else(|| cwd.to_path_buf())
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
