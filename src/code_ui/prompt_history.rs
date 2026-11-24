//! Prompts the user sent, per project folder: recalled with ↑/↓ and found
//! with Ctrl+R, in this session and the next ones.
//!
//! Each folder's history lives in a private file under the gz config
//! folder (`code_history/<sha256 of the folder>.jsonl`), one JSON string
//! per line, appended as prompts are sent and trimmed to the newest
//! [`MAX_ENTRIES`] when it grows past twice that. Pasted images are never
//! stored: their `[Image #N]` chips are taken out, and a prompt longer than
//! [`MAX_STORED_CHARS`] stays in this session only.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Prompts kept per folder.
pub const MAX_ENTRIES: usize = 1_000;
/// Longer prompts are recalled in this session but never written to disk.
pub const MAX_STORED_CHARS: usize = 10_000;
const FOLDER: &str = "code_history";

/// The prompts sent in one folder, oldest first, with the ↑/↓ position.
#[derive(Debug, Default)]
pub struct PromptHistory {
    entries: Vec<String>,
    /// Where new prompts are appended; `None` keeps them in memory only.
    file: Option<PathBuf>,
    /// The entry ↑/↓ shows, `None` while editing a fresh draft.
    pos: Option<usize>,
    /// What was in the input before ↑ was first pressed, for ↓ past the end.
    draft: String,
}

impl PromptHistory {
    /// The history of `folder`, read from the gz config folder. Without a
    /// config folder (or in tests) it lives in memory only.
    pub fn for_folder(folder: &Path) -> Result<Self> {
        match default_root() {
            Some(root) => Self::open(&root.join(file_name(folder))),
            None => Ok(Self::default()),
        }
    }

    /// The history kept in `file` (created on the first prompt).
    pub fn open(file: &Path) -> Result<Self> {
        let entries = match std::fs::read_to_string(file) {
            Ok(text) => parse(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", file.display()));
            }
        };
        let read = entries.len();
        let entries = newest(entries);
        if read > MAX_ENTRIES * 2 {
            rewrite(file, &entries)?;
        }
        Ok(Self {
            entries,
            file: Some(file.to_path_buf()),
            pos: None,
            draft: String::new(),
        })
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Records a sent prompt (image chips taken out; an immediate repeat is
    /// kept once) and saves it. Browsing starts over from the newest.
    pub fn push(&mut self, prompt: &str) -> Result<()> {
        self.pos = None;
        self.draft.clear();
        let text = without_image_chips(prompt);
        if text.is_empty() || self.entries.last() == Some(&text) {
            return Ok(());
        }
        self.entries.push(text.clone());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        match &self.file {
            Some(file) if text.chars().count() <= MAX_STORED_CHARS => append(file, &text),
            _ => Ok(()),
        }
    }

    /// One step through the history from the input's `current` text:
    /// `back` is ↑ (older). Returns the text to show, or `None` when there
    /// is nothing further that way. ↓ past the newest brings back the draft.
    pub fn step(&mut self, back: bool, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let last = self.entries.len() - 1;
        // Text changed since it was recalled (edited, cleared): browsing
        // starts over from the newest, with that text as the draft.
        if self
            .pos
            .is_some_and(|i| self.entries.get(i).map(String::as_str) != Some(current))
        {
            self.reset_browsing();
        }
        let next = match (self.pos, back) {
            (None, true) => {
                self.draft = current.to_string();
                Some(last)
            }
            (None, false) => return None,
            (Some(0), true) => return None,
            (Some(i), true) => Some(i - 1),
            (Some(i), false) if i >= last => None,
            (Some(i), false) => Some(i + 1),
        };
        self.pos = next;
        Some(match next {
            Some(i) => self.entries[i].clone(),
            None => std::mem::take(&mut self.draft),
        })
    }

    /// True while ↑/↓ is showing an entry and `current` is still that
    /// entry unedited: further arrows keep browsing.
    pub fn is_browsing(&self, current: &str) -> bool {
        self.pos
            .is_some_and(|i| self.entries.get(i).map(String::as_str) == Some(current))
    }

    /// Forgets the ↑/↓ position (the user edited the text or cleared it).
    pub fn reset_browsing(&mut self) {
        self.pos = None;
        self.draft.clear();
    }

    /// The newest entry older than `before` (all entries when `None`) that
    /// contains `query`, ignoring case; an empty query matches any entry.
    pub fn find(&self, query: &str, before: Option<usize>) -> Option<usize> {
        let end = before.unwrap_or(self.entries.len()).min(self.entries.len());
        let needle = query.to_lowercase();
        (0..end)
            .rev()
            .find(|&i| needle.is_empty() || self.entries[i].to_lowercase().contains(&needle))
    }

    /// The newest entry older than `at` that matches `query` and reads
    /// differently from entry `at`, so repeated Ctrl+R never shows the same
    /// text twice in a row.
    pub fn find_older_distinct(&self, query: &str, at: usize) -> Option<usize> {
        let shown = self.entries.get(at)?;
        let mut before = at;
        loop {
            let older = self.find(query, Some(before))?;
            if &self.entries[older] != shown {
                return Some(older);
            }
            before = older;
        }
    }

    #[cfg(test)]
    pub fn in_memory(entries: &[&str]) -> Self {
        Self {
            entries: entries.iter().map(|e| e.to_string()).collect(),
            ..Self::default()
        }
    }
}

/// `<gz config folder>/code_history`; tests never write the real one.
fn default_root() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let config = crate::config::config_path().ok()?;
    Some(config.parent()?.join(FOLDER))
}

/// One file per folder, named by a hash so no path text leaks into names.
fn file_name(folder: &Path) -> String {
    let folder = folder
        .canonicalize()
        .unwrap_or_else(|_| folder.to_path_buf());
    let digest = Sha256::digest(folder.to_string_lossy().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex}.jsonl")
}

/// Every entry in the file; lines that aren't JSON strings (a torn write,
/// a hand edit) are skipped rather than losing the whole history.
fn parse(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .filter(|entry| !entry.is_empty())
        .collect()
}

fn newest(mut entries: Vec<String>) -> Vec<String> {
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
    entries
}

fn line(entry: &str) -> String {
    let mut line = serde_json::Value::String(entry.to_string()).to_string();
    line.push('\n');
    line
}

fn rewrite(file: &Path, entries: &[String]) -> Result<()> {
    let text: String = entries.iter().map(|e| line(e)).collect();
    crate::assistant::private_file::write_private(file, &text)
}

/// Appends one entry; the file is created owner-only.
fn append(file: &Path, entry: &str) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut handle = options
        .open(file)
        .with_context(|| format!("cannot open {}", file.display()))?;
    handle
        .write_all(line(entry).as_bytes())
        .with_context(|| format!("cannot write {}", file.display()))
}

/// `prompt` without `[Image #N]` chips: the images they stand for belong to
/// one message and are never kept.
fn without_image_chips(prompt: &str) -> String {
    let mut out = String::with_capacity(prompt.len());
    let mut rest = prompt;
    while let Some(start) = rest.find("[Image #") {
        let tail = &rest[start + "[Image #".len()..];
        let digits = tail.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 || !tail[digits..].starts_with(']') {
            out.push_str(&rest[..start + 1]);
            rest = &rest[start + 1..];
            continue;
        }
        out.push_str(&rest[..start]);
        rest = &tail[digits + 1..];
        if out.is_empty() || out.ends_with(char::is_whitespace) {
            rest = rest.strip_prefix(' ').unwrap_or(rest);
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        crate::assistant::sandbox::tests::temp_dir(&format!("history-{name}")).join("h.jsonl")
    }

    #[test]
    fn prompts_survive_into_the_next_session() {
        let file = temp_file("persist");
        let mut first = PromptHistory::open(&file).unwrap();
        first.push("explain main.rs").unwrap();
        first.push("line one\nline two").unwrap();
        first.push("line one\nline two").unwrap();
        let mut next = PromptHistory::open(&file).unwrap();
        assert_eq!(next.entries(), ["explain main.rs", "line one\nline two"]);
        assert_eq!(next.step(true, "").as_deref(), Some("line one\nline two"));
        assert_eq!(
            next.step(true, "line one\nline two").as_deref(),
            Some("explain main.rs")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_history_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let file = temp_file("private");
        PromptHistory::open(&file).unwrap().push("hi").unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn image_chips_are_never_stored() {
        let file = temp_file("chips");
        let mut history = PromptHistory::open(&file).unwrap();
        history
            .push("[Image #1] what is in [Image #2] this?")
            .unwrap();
        history.push("[Image #3]").unwrap();
        history.push("keep [Image #x] and [Image] text").unwrap();
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(!saved.contains("[Image #1]"), "{saved}");
        assert_eq!(
            PromptHistory::open(&file).unwrap().entries(),
            ["what is in this?", "keep [Image #x] and [Image] text"]
        );
    }

    #[test]
    fn the_file_stays_bounded() {
        let file = temp_file("bounded");
        let lines: String = (0..MAX_ENTRIES * 2 + 5)
            .map(|i| line(&format!("prompt {i}")))
            .collect();
        std::fs::write(&file, lines).unwrap();
        let history = PromptHistory::open(&file).unwrap();
        assert_eq!(history.entries().len(), MAX_ENTRIES);
        assert_eq!(
            history.entries().last().map(String::as_str),
            Some(format!("prompt {}", MAX_ENTRIES * 2 + 4).as_str())
        );
        let on_disk = std::fs::read_to_string(&file).unwrap().lines().count();
        assert_eq!(on_disk, MAX_ENTRIES, "trimmed on open");

        let mut history = PromptHistory::open(&file).unwrap();
        let huge = "x".repeat(MAX_STORED_CHARS + 1);
        history.push(&huge).unwrap();
        assert_eq!(
            history.entries().last(),
            Some(&huge),
            "recalled this session"
        );
        let reopened = PromptHistory::open(&file).unwrap();
        assert_ne!(reopened.entries().last(), Some(&huge), "never written");
    }

    #[test]
    fn a_torn_line_does_not_lose_the_rest() {
        let file = temp_file("torn");
        std::fs::write(&file, "\"first\"\n{not json\n\"second\"\n\"thi").unwrap();
        assert_eq!(
            PromptHistory::open(&file).unwrap().entries(),
            ["first", "second"]
        );
    }

    #[test]
    fn down_past_the_newest_restores_the_draft() {
        let mut history = PromptHistory::in_memory(&["a", "b"]);
        assert_eq!(history.step(false, "draft"), None);
        assert_eq!(history.step(true, "draft").as_deref(), Some("b"));
        assert_eq!(history.step(true, "b").as_deref(), Some("a"));
        assert_eq!(history.step(true, "a"), None, "nothing older");
        assert_eq!(history.step(false, "a").as_deref(), Some("b"));
        assert_eq!(history.step(false, "b").as_deref(), Some("draft"));
        assert_eq!(history.step(false, "draft"), None);
    }

    #[test]
    fn editing_a_recalled_prompt_starts_browsing_over() {
        let mut history = PromptHistory::in_memory(&["a", "b", "c"]);
        assert_eq!(history.step(true, "").as_deref(), Some("c"));
        assert_eq!(history.step(true, "c").as_deref(), Some("b"));
        assert_eq!(
            history.step(true, "").as_deref(),
            Some("c"),
            "cleared: newest again"
        );
        assert_eq!(history.step(true, "c").as_deref(), Some("b"));
        assert_eq!(
            history.step(true, "b!").as_deref(),
            Some("c"),
            "edited: newest again"
        );
        assert_eq!(
            history.step(false, "c").as_deref(),
            Some("b!"),
            "the edit is the draft"
        );
    }

    #[test]
    fn search_finds_older_matches_ignoring_case() {
        let history = PromptHistory::in_memory(&["Fix the TEST", "run tests", "deploy"]);
        assert_eq!(history.find("test", None), Some(1));
        assert_eq!(history.find("test", Some(1)), Some(0));
        assert_eq!(history.find("test", Some(0)), None);
        assert_eq!(history.find("", None), Some(2));
        assert_eq!(history.find("nope", None), None);
    }

    #[test]
    fn each_folder_has_its_own_file() {
        assert_ne!(
            file_name(Path::new("/a/one")),
            file_name(Path::new("/a/two"))
        );
        assert!(file_name(Path::new("/a/one")).ends_with(".jsonl"));
        assert!(!file_name(Path::new("/secret/project")).contains("secret"));
    }
}
