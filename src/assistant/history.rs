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

/// One conversation started from `gz code`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub folder: PathBuf,
    pub title: String,
    /// Last turn sent, in milliseconds since the Unix epoch.
    pub updated: u64,
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
            }
            None => entries.push(Entry {
                id: id.to_string(),
                folder: folder.to_path_buf(),
                title: title.to_string(),
                updated: now,
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

fn write_private(path: &Path, text: &str) -> Result<()> {
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
            let text = message_text(content).trim().to_string();
            (!text.is_empty()).then_some(Message { speaker, text })
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
}
