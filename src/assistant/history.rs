//! Earlier `gz code` conversations. The chat itself lives on the server;
//! this module keeps a small local index of which conversation was started
//! in which folder (`code_sessions.json`, next to the gz config, owner-only)
//! and loads conversations and their messages back for `/resume` and
//! `gz code --continue`. The index holds ids, folders, titles and times,
//! never message text.

use anyhow::{Context, Result};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cmd::seg;
use crate::http::ApiClient;

const CHAT: &str = "chat";
/// Older entries are dropped past this, so the file stays small.
const KEEP: usize = 1000;
/// A turn adds your message and gz's answer.
const MESSAGES_PER_TURN: u64 = 2;

/// One conversation started from `gz code`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub folder: PathBuf,
    pub title: String,
    /// Last turn sent, in milliseconds since the Unix epoch.
    pub updated: u64,
    /// Messages in the conversation (yours and gz's), as last counted.
    #[serde(default)]
    pub messages: u64,
}

#[derive(Debug, Clone)]
pub struct Index {
    path: PathBuf,
}

impl Index {
    pub fn open() -> Result<Self> {
        let config = crate::config::config_path()?;
        let dir = config.parent().context("config path has no folder")?;
        Ok(Self::at(dir.join("code_sessions.json")))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Notes a turn in conversation `id`, started in `folder`. The title is
    /// kept from the first turn.
    pub fn record(&self, folder: &Path, id: &str, title: &str) -> Result<()> {
        let mut entries = self.load();
        let now = now_millis();
        match entries.iter_mut().find(|e| e.id == id) {
            Some(entry) => {
                entry.updated = now;
                entry.folder = folder.to_path_buf();
                entry.messages += MESSAGES_PER_TURN;
            }
            None => entries.push(Entry {
                id: id.to_string(),
                folder: folder.to_path_buf(),
                title: title.to_string(),
                updated: now,
                messages: MESSAGES_PER_TURN,
            }),
        }
        entries.sort_by(|a, b| b.updated.cmp(&a.updated));
        entries.truncate(KEEP);
        self.save(&entries)
    }

    /// Conversations started in `folder`, most recent first.
    pub fn recent(&self, folder: &Path) -> Vec<Entry> {
        let mut entries: Vec<Entry> = self
            .load()
            .into_iter()
            .filter(|e| e.folder == folder)
            .collect();
        entries.sort_by(|a, b| b.updated.cmp(&a.updated));
        entries
    }

    /// Every conversation started from gz code, most recent first.
    pub fn all(&self) -> Vec<Entry> {
        let mut entries = self.load();
        entries.sort_by(|a, b| b.updated.cmp(&a.updated));
        entries
    }

    /// Notes a conversation under `folder` without a turn: one `/fork`
    /// made, holding `messages` copied messages.
    pub fn add(&self, folder: &Path, id: &str, title: &str, messages: u64) -> Result<()> {
        let mut entries = self.load();
        entries.retain(|e| e.id != id);
        entries.push(Entry {
            id: id.to_string(),
            folder: folder.to_path_buf(),
            title: title.to_string(),
            updated: now_millis(),
            messages,
        });
        entries.sort_by(|a, b| b.updated.cmp(&a.updated));
        entries.truncate(KEEP);
        self.save(&entries)
    }

    /// Updates what the index knows of a conversation after loading it:
    /// its exact message count and, when given, its title. Conversations
    /// not in the index are left out of it.
    pub fn refresh(&self, id: &str, messages: u64, title: Option<&str>) -> Result<()> {
        let mut entries = self.load();
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            return Ok(());
        };
        entry.messages = messages;
        if let Some(title) = title {
            entry.title = title.to_string();
        }
        self.save(&entries)
    }

    /// Renames a conversation in the index, if it is there.
    pub fn rename(&self, id: &str, title: &str) -> Result<()> {
        let mut entries = self.load();
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            return Ok(());
        };
        entry.title = title.to_string();
        self.save(&entries)
    }

    /// The last conversation started in `folder`.
    pub fn last(&self, folder: &Path) -> Option<Entry> {
        self.recent(folder).into_iter().next()
    }

    /// A missing or unreadable index is an empty one.
    fn load(&self) -> Vec<Entry> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self, entries: &[Entry]) -> Result<()> {
        let dir = self.path.parent().context("index path has no folder")?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let text = serde_json::to_string_pretty(entries)?;
        let temp = self.path.with_extension("json.tmp");
        write_private(&temp, &text)?;
        std::fs::rename(&temp, &self.path)
            .with_context(|| format!("cannot write {}", self.path.display()))
    }
}

/// Writes `text` to `path`, readable by the owner only.
pub(crate) fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A conversation as the server lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: String,
    pub title: String,
}

/// Your chat conversations, as the server orders them (newest first).
pub async fn conversations(http: &ApiClient) -> Result<Vec<Conversation>> {
    let query = [("assistantMode".to_string(), "chat".to_string())];
    let value = http
        .request_json(CHAT, Method::GET, "/chat/conversations", &query, None)
        .await?;
    Ok(parse_conversations(&value))
}

fn parse_conversations(value: &Value) -> Vec<Conversation> {
    let list = value
        .as_array()
        .or_else(|| value["conversations"].as_array())
        .or_else(|| value["items"].as_array());
    list.into_iter()
        .flatten()
        .filter_map(|c| {
            let id = c["conversationId"].as_str()?.to_string();
            let title = c["conversationName"].as_str().unwrap_or("").to_string();
            Some(Conversation { id, title })
        })
        .collect()
}

/// Longest conversation name the server takes (gz-chat
/// `BoundedConversationName`, `api/routes/conversations.py`).
pub const MAX_NAME_CHARS: usize = 512;

/// Renames conversation `id` (`POST /chat/conversations`).
pub async fn rename(http: &ApiClient, id: &str, name: &str) -> Result<String> {
    let body = serde_json::json!({"conversationId": id, "conversationName": name});
    let reply = http
        .request_json(CHAT, Method::POST, "/chat/conversations", &[], Some(body))
        .await?;
    Ok(reply["conversationName"]
        .as_str()
        .unwrap_or(name)
        .to_string())
}

/// What to copy into a fork: everything, or the history up to one message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForkOptions {
    /// The message (by its saved `timestamp`) the copy ends at.
    pub through_timestamp: Option<String>,
    /// Leave that message itself out, so the fork ends just before it.
    pub exclude_selected: bool,
    /// The fork's name; the server names it "<title> (branch)" without one.
    pub name: Option<String>,
}

/// A new conversation made by [`fork`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forked {
    pub id: String,
    pub name: String,
}

/// Copies conversation `id` into a new one (`POST
/// /chat/conversations/{id}/fork`); the original is left as it was.
pub async fn fork(http: &ApiClient, id: &str, options: &ForkOptions) -> Result<Forked> {
    let path = format!("/chat/conversations/{}/fork", seg(id));
    let reply = http
        .request_json(CHAT, Method::POST, &path, &[], Some(fork_body(options)))
        .await?;
    let id = reply["conversationId"]
        .as_str()
        .context("the server's fork reply has no conversationId")?;
    Ok(Forked {
        id: id.to_string(),
        name: reply["conversationName"].as_str().unwrap_or("").to_string(),
    })
}

fn fork_body(options: &ForkOptions) -> Value {
    let mut body = serde_json::json!({});
    if let Some(timestamp) = &options.through_timestamp {
        body["throughTimestamp"] = serde_json::json!(timestamp);
        body["excludeSelected"] = serde_json::json!(options.exclude_selected);
    }
    if let Some(name) = &options.name {
        body["conversationName"] = serde_json::json!(name);
    }
    body
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    You,
    Gz,
}

/// One message of an earlier conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub speaker: Speaker,
    pub text: String,
    /// When the server saved it (ISO 8601): what a fork names to copy
    /// history through this message.
    pub timestamp: Option<String>,
    /// Notes the user sent while this answer was being written, with how
    /// far into the answer each one arrived.
    pub steering: Vec<SteerNote>,
    /// The tools the answer called, in order.
    pub tools: Vec<String>,
}

impl Message {
    /// A message with no stored detail beyond its text.
    #[cfg(test)]
    pub fn new(speaker: Speaker, text: impl Into<String>) -> Self {
        Self {
            speaker,
            text: text.into(),
            timestamp: None,
            steering: Vec::new(),
            tools: Vec::new(),
        }
    }
}

/// A note the user sent while an answer was being written
/// (`steer_applied` in the answer's saved `toolEvents`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteerNote {
    pub text: String,
    /// Bytes of [`Message::text`] written before the note was read.
    pub offset: Option<usize>,
}

/// An earlier conversation: its server title and messages, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub title: Option<String>,
    pub messages: Vec<Message>,
}

pub async fn transcript(http: &ApiClient, id: &str) -> Result<Transcript> {
    let path = format!("/chat/conversations/{}", seg(id));
    let value = http
        .request_json(CHAT, Method::GET, &path, &[], None)
        .await?;
    Ok(Transcript {
        title: value["conversationName"]
            .as_str()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string),
        messages: parse_messages(&value),
    })
}

fn parse_messages(value: &Value) -> Vec<Message> {
    value["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let role = m["speaker"]
                .as_str()
                .or(m["role"].as_str())
                .unwrap_or("assistant")
                .to_ascii_lowercase();
            let speaker = if role == "human" || role == "user" {
                Speaker::You
            } else {
                Speaker::Gz
            };
            let content = if m["message"].is_null() {
                &m["content"]
            } else {
                &m["message"]
            };
            let raw = message_text(content);
            let text = raw.trim_end().trim_start_matches('\n');
            let start = raw.len() - raw.trim_start_matches('\n').len();
            let events = m["toolEvents"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let message = Message {
                speaker,
                timestamp: m["timestamp"].as_str().map(str::to_string),
                steering: steer_notes(events, &raw, start),
                tools: tool_names(events),
                text: text.to_string(),
            };
            (!message.text.trim().is_empty() || !message.steering.is_empty()).then_some(message)
        })
        .collect()
}

/// The `steer_applied` events of a saved answer. The server counts
/// `responseOffset` in characters of the whole answer (Python `len`), so
/// each becomes a byte index into the text kept here, which starts `start`
/// bytes into `raw`.
fn steer_notes(events: &[Value], raw: &str, start: usize) -> Vec<SteerNote> {
    events
        .iter()
        .filter(|e| e["type"] == "steer_applied")
        .filter_map(|e| {
            let chars = e["responseOffset"].as_u64().map(|o| o as usize);
            Some(SteerNote {
                text: e["message"].as_str()?.to_string(),
                offset: chars.map(|n| byte_index(raw, n).saturating_sub(start)),
            })
        })
        .collect()
}

/// The byte where the `chars`-th character of `text` starts, or its end.
fn byte_index(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(at, _)| at)
}

/// The tool names of a saved answer's `tool_use` events.
fn tool_names(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["type"] == "tool_use")
        .filter_map(|e| {
            ["tool_name", "toolName", "name"]
                .iter()
                .find_map(|key| e[*key].as_str())
                .map(str::to_string)
        })
        .collect()
}

fn message_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str().or(b.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_index(name: &str) -> Index {
        let dir = std::env::temp_dir().join(format!("gz-history-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        Index::at(dir.join("code_sessions.json"))
    }

    #[test]
    fn remembers_conversations_per_folder_newest_first() {
        let index = temp_index("folders");
        let (a, b) = (Path::new("/work/a"), Path::new("/work/b"));
        index.record(a, "c1", "first").unwrap();
        index.record(b, "c2", "other folder").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        index.record(a, "c3", "second").unwrap();
        let ids: Vec<_> = index.recent(a).into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["c3", "c1"]);
        assert_eq!(index.last(b).unwrap().title, "other folder");
        assert!(index.last(Path::new("/work/none")).is_none());
    }

    #[test]
    fn a_later_turn_keeps_the_title_and_moves_it_to_the_top() {
        let index = temp_index("touch");
        let folder = Path::new("/work/a");
        index.record(folder, "c1", "first").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        index.record(folder, "c2", "second").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        index.record(folder, "c1", "ignored").unwrap();
        let last = index.last(folder).unwrap();
        assert_eq!((last.id.as_str(), last.title.as_str()), ("c1", "first"));
    }

    #[test]
    fn an_unreadable_index_counts_as_empty() {
        let index = temp_index("corrupt");
        std::fs::create_dir_all(index.path.parent().unwrap()).unwrap();
        std::fs::write(&index.path, "not json").unwrap();
        assert!(index.recent(Path::new("/work")).is_empty());
        index.record(Path::new("/work"), "c1", "t").unwrap();
        assert_eq!(index.recent(Path::new("/work")).len(), 1);
    }

    #[test]
    fn reads_conversation_lists_and_messages() {
        let list = json!([{"conversationId": "c1", "conversationName": "Jobs"}, {"x": 1}]);
        assert_eq!(
            parse_conversations(&list),
            [Conversation {
                id: "c1".into(),
                title: "Jobs".into()
            }]
        );
        let wrapped = json!({"conversations": [{"conversationId": "c2"}]});
        assert_eq!(parse_conversations(&wrapped)[0].id, "c2");

        let convo = json!({"messages": [
            {"speaker": "Human", "message": "hi"},
            {"speaker": "AI", "message": "hello"},
            {"role": "assistant", "content": [{"text": "block"}]},
            {"speaker": "AI", "message": "  "},
        ]});
        let messages = parse_messages(&convo);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].speaker, Speaker::You);
        assert_eq!(messages[1].text, "hello");
        assert_eq!(messages[2].text, "block");
    }

    #[test]
    fn keeps_timestamps_steering_tools_and_leading_indentation() {
        let convo = json!({"messages": [
            {"speaker": "Human", "message": "    indented()\n", "timestamp": "2026-11-24T10:00:00+00:00"},
            {"speaker": "AI", "message": "done", "toolEvents": [
                {"type": "tool_use", "tool_name": "read_file"},
                {"type": "tool_result", "tool_use_id": "t1"},
                {"type": "tool_use", "toolName": "run_command"},
                {"type": "steer_applied", "message": "also main()", "responseOffset": 2},
            ]},
        ]});
        let messages = parse_messages(&convo);
        assert_eq!(messages[0].text, "    indented()");
        assert_eq!(
            messages[0].timestamp.as_deref(),
            Some("2026-11-24T10:00:00+00:00")
        );
        assert_eq!(messages[1].tools, ["read_file", "run_command"]);
        assert_eq!(
            messages[1].steering,
            [SteerNote {
                text: "also main()".into(),
                offset: Some(2)
            }]
        );
    }

    #[test]
    fn steering_offsets_count_characters_from_the_saved_start() {
        let answer = "\n\nRésumé — naïve café façade → done. Second part here.";
        let split = "\n\nRésumé — naïve café façade → done. ".chars().count();
        let convo = json!({"messages": [
            {"speaker": "AI", "message": answer, "toolEvents": [
                {"type": "steer_applied", "message": "also main()", "responseOffset": split},
                {"type": "steer_applied", "message": "late", "responseOffset": 9999},
            ]},
        ]});
        let message = &parse_messages(&convo)[0];
        let at = message.steering[0].offset.unwrap();
        assert_eq!(&message.text[at..], "Second part here.");
        assert_eq!(message.steering[1].offset, Some(message.text.len()));
    }

    #[test]
    fn fork_bodies_match_the_server_contract() {
        assert_eq!(fork_body(&ForkOptions::default()), json!({}));
        let through = ForkOptions {
            through_timestamp: Some("2026-11-24T10:00:00+00:00".into()),
            exclude_selected: true,
            name: Some("Try B".into()),
        };
        assert_eq!(
            fork_body(&through),
            json!({"throughTimestamp": "2026-11-24T10:00:00+00:00", "excludeSelected": true, "conversationName": "Try B"})
        );
    }
}
