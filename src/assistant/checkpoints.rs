//! Edit history for `/rewind` and `/diff`: just before gz's own tools
//! (`edit_file`, `write_file`, `notebook_edit`, and the script helpers)
//! change a file, gz keeps what the file held, tagged with the user message
//! being answered. Changes made by commands (`run_command`, the user's own
//! `!commands`) go around gz's write path and are not kept.
//!
//! One history per conversation, under `<gz config folder>/code_edits/<id>/`:
//! `history.json` lists the messages and edits, `copies/<sha256>` holds the
//! earlier contents (owner-only). Background subagents write into their
//! main conversation's history by sharing its [`EditHistory`].

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::assistant::private_file::{write_private, write_private_bytes};
use crate::assistant::sandbox::Sandbox;
use crate::assistant::session::AssistantEvent;
use crate::assistant::tools::{self, Planned, ToolOutput};

/// Files larger than this are not copied; `/rewind` refuses them.
pub const MAX_KEPT_BYTES: u64 = 5 * 1024 * 1024;
/// Histories of conversations not edited for this long are deleted.
const STALE_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const FOLDER: &str = "code_edits";
const LOG_FILE: &str = "history.json";
const COPIES: &str = "copies";
const EXCERPT_CHARS: usize = 60;
/// The label for edits made before any message of this process (cannot
/// normally happen: subagents stop when the conversation changes).
const NO_MESSAGE: &str = "(no message)";

/// One conversation's edit history, shared by the main session, its
/// background subagents and the UI. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct EditHistory {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// `<config>/code_edits`; `None` keeps nothing (no config folder).
    root: Option<PathBuf>,
    conversation: String,
    /// The message being answered; saved with its first edit.
    current: Option<Turn>,
    /// The conversation's log, read on first use.
    log: Option<Log>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Log {
    turns: Vec<Turn>,
    edits: Vec<Edit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Turn {
    id: u64,
    excerpt: String,
    /// Unix milliseconds the message was sent.
    at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Edit {
    turn: u64,
    path: PathBuf,
    before: Before,
    /// SHA-256 of what gz wrote, to notice later changes made outside gz.
    after: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Before {
    /// The file did not exist: rewinding deletes it.
    Missing,
    /// Its content is in `copies/<sha>`.
    Kept { sha: String },
    /// No copy was kept (too large, unreadable); rewinding refuses it.
    NotKept { reason: String },
}

/// A message that led to file changes, for the `/rewind` picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEdits {
    pub turn: u64,
    pub excerpt: String,
    /// Unix milliseconds the message was sent.
    pub at: u64,
    /// Files gz changed while answering it.
    pub files: usize,
}

/// What rewinding to a message would do to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRewind {
    pub path: PathBuf,
    pub restore: Restore,
    /// The file differs from what gz last wrote (the user or a command
    /// changed it since).
    pub changed_since: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restore {
    /// Put the kept content back.
    Content,
    /// gz created it: delete it.
    Delete,
    /// No copy was kept; the reason says why.
    Impossible(String),
}

/// What a rewind did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rewound {
    /// The message rewound to (files are as they were before it).
    pub excerpt: String,
    pub restored: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    /// Changed since gz wrote them, left as they are by the user's choice.
    pub skipped: Vec<PathBuf>,
    /// No copy was kept, with why.
    pub not_kept: Vec<(PathBuf, String)>,
    /// Restoring failed, with why; their history is kept to try again.
    pub failed: Vec<(PathBuf, String)>,
}

impl Rewound {
    /// Files now back as they were.
    pub fn changed_files(&self) -> usize {
        self.restored.len() + self.deleted.len()
    }
}

/// One file's change since gz first touched it in this conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: PathBuf,
    pub change: Change,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Before and now, as text; `None` for a file that does not exist.
    Text {
        before: Option<String>,
        now: Option<String>,
    },
    /// Back to how it was.
    Unchanged,
    /// Either side is not text.
    Binary,
    /// The earlier content was not kept, with why.
    NotKept(String),
    /// The file is now over the size gz keeps, so no diff is computed.
    TooLarge,
}

impl EditHistory {
    /// The history of `conversation` in the gz config folder.
    pub fn open(conversation: &str) -> Self {
        Self::new(default_root(), conversation)
    }

    /// The history of `conversation` under `root` (a `code_edits` folder).
    #[cfg(test)]
    pub fn at(root: PathBuf, conversation: &str) -> Self {
        Self::new(Some(root), conversation)
    }

    /// A history that keeps nothing (scripts run outside a session).
    pub fn disabled() -> Self {
        Self::new(None, "")
    }

    fn new(root: Option<PathBuf>, conversation: &str) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                root,
                conversation: conversation.to_string(),
                current: None,
                log: None,
            })),
        }
    }

    /// Follows the session into another conversation; every clone follows.
    pub fn set_conversation(&self, conversation: &str) {
        let Ok(mut inner) = self.lock() else {
            return;
        };
        if inner.conversation != conversation {
            inner.conversation = conversation.to_string();
            inner.current = None;
            inner.log = None;
        }
    }

    /// Whether this history is kept anywhere.
    pub fn is_enabled(&self) -> bool {
        self.lock().is_ok_and(|inner| inner.root.is_some())
    }

    /// Notes that the user sent `prompt`: edits from now on belong to it.
    pub fn begin_turn(&self, prompt: &str) -> Result<()> {
        let mut inner = self.lock()?;
        if inner.root.is_none() {
            return Ok(());
        }
        let current_id = inner.current.as_ref().map_or(0, |t| t.id);
        let last_saved = inner.log()?.turns.iter().map(|t| t.id).max().unwrap_or(0);
        inner.current = Some(Turn {
            id: current_id.max(last_saved) + 1,
            excerpt: excerpt(prompt),
            at: crate::assistant::history::now_millis(),
        });
        Ok(())
    }

    /// The messages that led to file changes, newest first.
    pub fn messages(&self) -> Result<Vec<MessageEdits>> {
        let mut inner = self.lock()?;
        if inner.root.is_none() {
            return Ok(Vec::new());
        }
        let log = inner.log()?;
        let mut messages: Vec<MessageEdits> = log
            .turns
            .iter()
            .map(|turn| MessageEdits {
                turn: turn.id,
                excerpt: turn.excerpt.clone(),
                at: turn.at,
                files: log
                    .edits
                    .iter()
                    .filter(|e| e.turn == turn.id)
                    .map(|e| &e.path)
                    .collect::<HashSet<_>>()
                    .len(),
            })
            .filter(|m| m.files > 0)
            .collect();
        messages.sort_by(|a, b| b.turn.cmp(&a.turn));
        Ok(messages)
    }

    /// What rewinding to before message `turn` would do, file by file.
    pub fn plan_rewind(&self, turn: u64) -> Result<Vec<FileRewind>> {
        let mut inner = self.lock()?;
        let dir = inner.dir()?;
        Ok(plan(inner.log()?, turn, &dir))
    }

    /// Puts every file gz changed since message `turn` started back as it
    /// was before it. Files changed since gz wrote them are put back only
    /// with `overwrite_changed`. The rewound edits leave the history.
    pub fn rewind(&self, turn: u64, overwrite_changed: bool) -> Result<Rewound> {
        let mut inner = self.lock()?;
        let dir = inner.dir()?;
        let log = inner.log()?;
        let Some(excerpt) = log
            .turns
            .iter()
            .find(|t| t.id == turn)
            .map(|t| t.excerpt.clone())
        else {
            bail!("that message has no file changes left to rewind; run /rewind again to see the list");
        };
        let mut done = Rewound {
            excerpt,
            ..Rewound::default()
        };
        let mut keep: HashSet<PathBuf> = HashSet::new();
        for file in plan(log, turn, &dir) {
            let path = file.path.clone();
            match (&file.restore, file.changed_since && !overwrite_changed) {
                (Restore::Impossible(reason), _) => done.not_kept.push((path, reason.clone())),
                (_, true) => done.skipped.push(path),
                (restore, false) => match put_back(log, turn, &dir, &file.path, restore) {
                    Ok(()) if *restore == Restore::Delete => done.deleted.push(path),
                    Ok(()) => done.restored.push(path),
                    Err(err) => {
                        keep.insert(path.clone());
                        done.failed.push((path, format!("{err:#}")));
                    }
                },
            }
        }
        log.edits
            .retain(|e| e.turn < turn || keep.contains(&e.path));
        let still_used: HashSet<u64> = log.edits.iter().map(|e| e.turn).collect();
        log.turns.retain(|t| still_used.contains(&t.id));
        let log = log.clone();
        save(&dir, &log)?;
        remove_unused_copies(&dir, &log);
        Ok(done)
    }

    /// Every file gz changed in this conversation (or only `only`), from
    /// its content before gz first touched it to its content now, in the
    /// order gz first touched them.
    pub fn diffs(&self, only: Option<&Path>) -> Result<Vec<FileDiff>> {
        let mut inner = self.lock()?;
        if inner.root.is_none() {
            return Ok(Vec::new());
        }
        let dir = inner.dir()?;
        let log = inner.log()?;
        let mut seen = HashSet::new();
        let firsts: Vec<&Edit> = log
            .edits
            .iter()
            .filter(|e| only.is_none_or(|p| e.path == p))
            .filter(|e| seen.insert(e.path.clone()))
            .collect();
        Ok(firsts
            .into_iter()
            .map(|first| FileDiff {
                path: first.path.clone(),
                change: change_since(&dir, first),
            })
            .collect())
    }

    /// Keeps `snapshot` (taken just before gz wrote `written` over it).
    pub fn record(&self, snapshot: Snapshot, written: &str) -> Result<()> {
        let mut inner = self.lock()?;
        if inner.root.is_none() {
            return Ok(());
        }
        let dir = inner.dir()?;
        let before = match snapshot.content {
            Content::Missing => Before::Missing,
            Content::Bytes(bytes) => {
                let sha = sha256(&bytes);
                let copy = dir.join(COPIES).join(&sha);
                if !copy.exists() {
                    make_private_dir(&dir.join(COPIES))?;
                    write_private_bytes(&copy, &bytes)?;
                }
                Before::Kept { sha }
            }
            Content::NotKept(reason) => Before::NotKept { reason },
        };
        let turn = match inner.current.clone() {
            Some(turn) => turn,
            None => {
                let next = inner.log()?.turns.iter().map(|t| t.id).max().unwrap_or(0) + 1;
                let turn = Turn {
                    id: next,
                    excerpt: NO_MESSAGE.to_string(),
                    at: crate::assistant::history::now_millis(),
                };
                inner.current = Some(turn.clone());
                turn
            }
        };
        let log = inner.log()?;
        if !log.turns.iter().any(|t| t.id == turn.id) {
            log.turns.push(turn.clone());
        }
        log.edits.push(Edit {
            turn: turn.id,
            path: snapshot.path,
            before,
            after: sha256(written.as_bytes()),
        });
        let log = log.clone();
        save(&dir, &log)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        self.inner.lock().map_err(|_| {
            anyhow::anyhow!("the edit history broke after an earlier crash; restart gz")
        })
    }
}

impl Inner {
    /// This conversation's folder.
    fn dir(&self) -> Result<PathBuf> {
        let Some(root) = &self.root else {
            bail!(
                "gz has no config folder, so it keeps no edit history; set XDG_CONFIG_HOME or HOME"
            );
        };
        Ok(root.join(folder_name(&self.conversation)))
    }

    fn log(&mut self) -> Result<&mut Log> {
        if self.log.is_none() {
            let path = self.dir()?.join(LOG_FILE);
            let log = match std::fs::read_to_string(&path) {
                Ok(text) => serde_json::from_str(&text).with_context(|| {
                    format!(
                        "cannot read the edit history {}; delete it to start a new one",
                        path.display()
                    )
                })?,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Log::default(),
                Err(err) => {
                    return Err(err).with_context(|| format!("cannot read {}", path.display()))
                }
            };
            self.log = Some(log);
        }
        Ok(self.log.get_or_insert_with(Log::default))
    }
}

/// A file as it was just before gz wrote it.
pub struct Snapshot {
    path: PathBuf,
    content: Content,
}

enum Content {
    Missing,
    Bytes(Vec<u8>),
    NotKept(String),
}

impl Snapshot {
    pub fn take(path: &Path) -> Self {
        let content = match std::fs::metadata(path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Content::Missing,
            Err(err) => {
                Content::NotKept(format!("gz could not read it before changing it ({err})"))
            }
            Ok(meta) if meta.len() > MAX_KEPT_BYTES => Content::NotKept(format!(
                "it was over {} MB when gz changed it, so gz kept no copy",
                MAX_KEPT_BYTES / (1024 * 1024)
            )),
            Ok(_) => match std::fs::read(path) {
                Ok(bytes) => Content::Bytes(bytes),
                Err(err) => {
                    Content::NotKept(format!("gz could not read it before changing it ({err})"))
                }
            },
        };
        Self {
            path: path.to_path_buf(),
            content,
        }
    }
}

/// Runs an approved single tool call; a write's earlier content goes into
/// `history` first.
pub async fn run_tool(
    history: &EditHistory,
    sandbox: &Sandbox,
    planned: &Planned,
    on_event: &mut (dyn FnMut(AssistantEvent) + Send),
) -> ToolOutput {
    let Some((path, written)) = planned.written() else {
        return tools::run(sandbox, planned).await;
    };
    let snapshot = Snapshot::take(path);
    let out = tools::run(sandbox, planned).await;
    if !out.is_error {
        keep(history, snapshot, written, on_event);
    }
    out
}

/// [`run_tool`] for a script helper call.
pub fn run_helper(
    history: &EditHistory,
    sandbox: &Sandbox,
    planned: &Planned,
    on_event: &mut (dyn FnMut(AssistantEvent) + Send),
) -> Result<Value> {
    let Some((path, written)) = planned.written() else {
        return tools::run_helper(sandbox, planned);
    };
    let snapshot = Snapshot::take(path);
    let result = tools::run_helper(sandbox, planned)?;
    keep(history, snapshot, written, on_event);
    Ok(result)
}

fn keep(
    history: &EditHistory,
    snapshot: Snapshot,
    written: &str,
    on_event: &mut (dyn FnMut(AssistantEvent) + Send),
) {
    let path = snapshot.path.display().to_string();
    if let Err(err) = history.record(snapshot, written) {
        on_event(AssistantEvent::Notice(format!(
            "gz changed {path} but could not keep its earlier content, so /rewind cannot undo it: {err:#}"
        )));
    }
}

/// Deletes the histories of conversations not edited for 30 days.
/// Best effort: a history that can't be read or removed stays.
pub fn prune_stale() {
    if let Some(root) = default_root() {
        prune_stale_in(&root, SystemTime::now());
    }
}

fn prune_stale_in(root: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let modified = std::fs::metadata(dir.join(LOG_FILE))
            .or_else(|_| std::fs::metadata(&dir))
            .and_then(|meta| meta.modified());
        let is_stale = modified
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age > STALE_AFTER);
        if is_stale && dir.is_dir() {
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}

/// `<gz config folder>/code_edits`. Tests never write the real config
/// folder; they pass their own root.
fn default_root() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let config = crate::config::config_path().ok()?;
    Some(config.parent()?.join(FOLDER))
}

/// A safe folder name for a conversation id (ids are UUIDs, but they come
/// from the server and the command line).
fn folder_name(conversation: &str) -> String {
    let is_plain = !conversation.is_empty()
        && conversation.len() <= 100
        && conversation
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if is_plain {
        conversation.to_string()
    } else {
        sha256(conversation.as_bytes())
    }
}

/// For each file changed since `turn` started: the content before the
/// earliest of those changes, and whether the file still holds what gz
/// wrote last.
fn plan(log: &Log, turn: u64, dir: &Path) -> Vec<FileRewind> {
    let mut by_path: BTreeMap<&Path, (&Edit, &Edit)> = BTreeMap::new();
    for edit in log.edits.iter().filter(|e| e.turn >= turn) {
        by_path
            .entry(&edit.path)
            .and_modify(|(_, last)| *last = edit)
            .or_insert((edit, edit));
    }
    by_path
        .into_iter()
        .map(|(path, (first, last))| FileRewind {
            path: path.to_path_buf(),
            restore: match &first.before {
                Before::Missing => Restore::Delete,
                Before::Kept { sha } if dir.join(COPIES).join(sha).is_file() => Restore::Content,
                Before::Kept { .. } => Restore::Impossible(
                    "its kept copy is missing from gz's edit history".to_string(),
                ),
                Before::NotKept { reason } => Restore::Impossible(reason.clone()),
            },
            changed_since: !holds(path, &last.after),
        })
        .collect()
}

/// Whether `path` holds content with this SHA-256 (false when missing).
fn holds(path: &Path, sha: &str) -> bool {
    std::fs::read(path).is_ok_and(|bytes| sha256(&bytes) == sha)
}

fn put_back(log: &Log, turn: u64, dir: &Path, path: &Path, restore: &Restore) -> Result<()> {
    match restore {
        Restore::Delete => match std::fs::remove_file(path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                Err(err).context("cannot delete it")
            }
            _ => Ok(()),
        },
        Restore::Content => {
            let first = log
                .edits
                .iter()
                .find(|e| e.turn >= turn && e.path == path)
                .map(|e| &e.before);
            let Some(Before::Kept { sha }) = first else {
                bail!("gz kept no copy of it");
            };
            let bytes =
                std::fs::read(dir.join(COPIES).join(sha)).context("cannot read gz's copy of it")?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).context("cannot recreate its folder")?;
            }
            std::fs::write(path, bytes).context("cannot write it")
        }
        Restore::Impossible(reason) => bail!("{reason}"),
    }
}

fn change_since(dir: &Path, first: &Edit) -> Change {
    let before = match &first.before {
        Before::Missing => None,
        Before::Kept { sha } => match std::fs::read(dir.join(COPIES).join(sha)) {
            Ok(bytes) => Some(bytes),
            Err(_) => {
                return Change::NotKept("its kept copy is missing from gz's edit history".into())
            }
        },
        Before::NotKept { reason } => return Change::NotKept(reason.clone()),
    };
    let now = match std::fs::metadata(&first.path) {
        Ok(meta) if meta.len() > MAX_KEPT_BYTES => return Change::TooLarge,
        Ok(_) => std::fs::read(&first.path).ok(),
        Err(_) => None,
    };
    if before == now {
        return Change::Unchanged;
    }
    let as_text = |bytes: Option<Vec<u8>>| bytes.map(String::from_utf8).transpose();
    match (as_text(before), as_text(now)) {
        (Ok(before), Ok(now)) => Change::Text { before, now },
        _ => Change::Binary,
    }
}

fn save(dir: &Path, log: &Log) -> Result<()> {
    make_private_dir(dir)?;
    let text = serde_json::to_string(log).context("cannot encode the edit history")?;
    write_private(&dir.join(LOG_FILE), &text)
}

fn remove_unused_copies(dir: &Path, log: &Log) {
    let used: HashSet<&str> = log
        .edits
        .iter()
        .filter_map(|e| match &e.before {
            Before::Kept { sha } => Some(sha.as_str()),
            _ => None,
        })
        .collect();
    let Ok(entries) = std::fs::read_dir(dir.join(COPIES)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !used.contains(name.to_string_lossy().as_ref()) {
            std::fs::remove_file(entry.path()).ok();
        }
    }
}

fn make_private_dir(dir: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The first non-empty line of a message, shortened.
fn excerpt(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= EXCERPT_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(EXCERPT_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::assistant::sandbox::tests::{temp_dir, temp_workspace};
    use crate::assistant::tools::ToolName;
    use serde_json::json;

    /// A workspace and a history kept in its own temp folder.
    pub(crate) fn workspace(name: &str) -> (PathBuf, Sandbox, EditHistory) {
        let (dir, sandbox) = temp_workspace(name);
        let history = EditHistory::at(temp_dir(&format!("{name}-edits")), "conv-1");
        (dir, sandbox, history)
    }

    /// Runs `tool` like the session does, through the history.
    pub(crate) async fn run(
        history: &EditHistory,
        sandbox: &Sandbox,
        tool: ToolName,
        input: Value,
    ) -> ToolOutput {
        let planned = tools::plan(sandbox, tool, &input).unwrap();
        run_tool(history, sandbox, &planned, &mut |_| {}).await
    }

    pub(crate) async fn edit(
        history: &EditHistory,
        sandbox: &Sandbox,
        path: &str,
        old: &str,
        new: &str,
    ) {
        let input = json!({"path": path, "old_text": old, "new_text": new});
        assert!(
            !run(history, sandbox, ToolName::EditFile, input)
                .await
                .is_error
        );
    }

    pub(crate) async fn write(history: &EditHistory, sandbox: &Sandbox, path: &str, content: &str) {
        let input = json!({"path": path, "content": content});
        assert!(
            !run(history, sandbox, ToolName::WriteFile, input)
                .await
                .is_error
        );
    }

    fn read(path: PathBuf) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[tokio::test]
    async fn rewinding_restores_edits_and_deletes_created_files() {
        let (dir, sandbox, history) = workspace("rewind-basic");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        history.begin_turn("first change").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        history.begin_turn("second change\nwith details").unwrap();
        edit(&history, &sandbox, "a.txt", "two", "three").await;
        write(&history, &sandbox, "new/b.txt", "made\n").await;

        let messages = history.messages().unwrap();
        let excerpts: Vec<_> = messages
            .iter()
            .map(|m| (m.excerpt.as_str(), m.files))
            .collect();
        assert_eq!(excerpts, vec![("second change", 2), ("first change", 1)]);

        let done = history.rewind(messages[0].turn, false).unwrap();
        assert_eq!(read(dir.join("a.txt")), "two\n");
        assert!(!dir.join("new/b.txt").exists());
        assert_eq!(done.restored, vec![dir.join("a.txt")]);
        assert_eq!(done.deleted, vec![dir.join("new/b.txt")]);
        assert_eq!(
            history.messages().unwrap().len(),
            1,
            "the rewound message left"
        );

        history.rewind(messages[1].turn, false).unwrap();
        assert_eq!(read(dir.join("a.txt")), "one\n");
        assert!(history.messages().unwrap().is_empty());
    }

    #[tokio::test]
    async fn files_changed_outside_gz_are_kept_unless_overwriting() {
        let (dir, sandbox, history) = workspace("rewind-changed");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "bee\n").unwrap();
        history.begin_turn("change both").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        edit(&history, &sandbox, "b.txt", "bee", "wasp").await;
        std::fs::write(dir.join("a.txt"), "mine\n").unwrap();

        let turn = history.messages().unwrap()[0].turn;
        let plan = history.plan_rewind(turn).unwrap();
        let changed: Vec<_> = plan
            .iter()
            .filter(|f| f.changed_since)
            .map(|f| &f.path)
            .collect();
        assert_eq!(changed, vec![&dir.join("a.txt")]);

        let done = history.rewind(turn, false).unwrap();
        assert_eq!(read(dir.join("a.txt")), "mine\n", "not overwritten");
        assert_eq!(read(dir.join("b.txt")), "bee\n");
        assert_eq!(done.skipped, vec![dir.join("a.txt")]);
    }

    #[tokio::test]
    async fn overwriting_puts_changed_files_back_too() {
        let (dir, sandbox, history) = workspace("rewind-overwrite");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        history.begin_turn("change").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        std::fs::write(dir.join("a.txt"), "mine\n").unwrap();
        let turn = history.messages().unwrap()[0].turn;
        let done = history.rewind(turn, true).unwrap();
        assert_eq!(read(dir.join("a.txt")), "one\n");
        assert_eq!(done.restored, vec![dir.join("a.txt")]);
    }

    #[tokio::test]
    async fn large_files_are_noted_but_never_restored() {
        let (dir, sandbox, history) = workspace("rewind-large");
        let big = "x".repeat(MAX_KEPT_BYTES as usize + 1);
        std::fs::write(dir.join("big.txt"), &big).unwrap();
        history.begin_turn("shrink it").unwrap();
        write(&history, &sandbox, "big.txt", "small\n").await;
        let turn = history.messages().unwrap()[0].turn;
        let done = history.rewind(turn, true).unwrap();
        assert_eq!(read(dir.join("big.txt")), "small\n");
        assert_eq!(done.not_kept.len(), 1);
        assert!(
            done.not_kept[0].1.contains("over 5 MB"),
            "{:?}",
            done.not_kept
        );
    }

    #[tokio::test]
    async fn the_history_survives_a_new_handle_for_the_same_conversation() {
        let (dir, sandbox, history) = workspace("rewind-reopen");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        history.begin_turn("change").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        let root = history.lock().unwrap().root.clone().unwrap();

        let later = EditHistory::at(root.clone(), "conv-1");
        assert_eq!(later.messages().unwrap()[0].excerpt, "change");
        later.begin_turn("another").unwrap();
        assert!(
            later.messages().unwrap()[0].turn < later.lock().unwrap().current.as_ref().unwrap().id,
            "new messages number after the saved ones"
        );
        assert!(EditHistory::at(root, "other")
            .messages()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn diffs_run_from_before_gz_first_touched_a_file_to_now() {
        let (dir, sandbox, history) = workspace("rewind-diff");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        history.begin_turn("change").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        edit(&history, &sandbox, "a.txt", "two", "three").await;
        write(&history, &sandbox, "b.txt", "new\n").await;
        let diffs = history.diffs(None).unwrap();
        assert_eq!(diffs.len(), 2);
        assert_eq!(
            diffs[0].change,
            Change::Text {
                before: Some("one\n".into()),
                now: Some("three\n".into())
            }
        );
        assert_eq!(
            diffs[1].change,
            Change::Text {
                before: None,
                now: Some("new\n".into())
            }
        );
        let only = history.diffs(Some(&dir.join("b.txt"))).unwrap();
        assert_eq!(only.len(), 1);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        assert_eq!(history.diffs(None).unwrap()[0].change, Change::Unchanged);
    }

    #[tokio::test]
    async fn copies_and_logs_are_owner_only() {
        let (dir, sandbox, history) = workspace("rewind-private");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        history.begin_turn("change").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let folder = history.lock().unwrap().dir().unwrap();
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&folder), 0o700);
            assert_eq!(mode(&folder.join(LOG_FILE)), 0o600);
            let copy = std::fs::read_dir(folder.join(COPIES))
                .unwrap()
                .next()
                .unwrap()
                .unwrap();
            assert_eq!(mode(&copy.path()), 0o600);
            assert_eq!(std::fs::read_to_string(copy.path()).unwrap(), "one\n");
        }
    }

    #[test]
    fn stale_histories_are_pruned() {
        let root = temp_dir("rewind-prune");
        std::fs::create_dir_all(root.join("old")).unwrap();
        std::fs::write(root.join("old").join(LOG_FILE), "{}").unwrap();
        let later = SystemTime::now() + STALE_AFTER + Duration::from_secs(60);
        prune_stale_in(&root, SystemTime::now());
        assert!(root.join("old").exists(), "recent ones stay");
        prune_stale_in(&root, later);
        assert!(!root.join("old").exists());
    }

    #[test]
    fn odd_conversation_ids_become_safe_folder_names() {
        assert_eq!(folder_name("1b2c-3d_e"), "1b2c-3d_e");
        let odd = folder_name("../../etc");
        assert_eq!(odd.len(), 64);
        assert!(odd.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_disabled_history_keeps_nothing() {
        let history = EditHistory::disabled();
        assert!(!history.is_enabled());
        history.begin_turn("x").unwrap();
        assert!(history.messages().unwrap().is_empty());
        assert!(history.diffs(None).unwrap().is_empty());
    }
}
