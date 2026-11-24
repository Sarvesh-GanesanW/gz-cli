//! `/resume`: the list of earlier `gz code` conversations, newest first,
//! with type-to-search and a switch to every chat; and the earlier messages
//! printed back on resume.
//!
//! The server's list (`GET /chat/conversations`) has no times, message
//! counts or marker for chats `gz code` started (gz sends no
//! `assistantContext.surface`), so the gz code list comes from the local
//! index of conversations started here; the server supplies titles, since
//! it renames chats after the first turn, and the "all chats" view.

use std::path::{Path, PathBuf};

use crate::assistant::history::{Conversation, Entry, Message};

use super::items::Item;
use super::replay;
use super::state::PickerRow;

/// Rows the list shows at most, so it fits on screen; searching narrows it.
const ROWS_SHOWN: usize = 12;
/// Earlier messages printed back on resume; older ones are summarised.
const MESSAGES_SHOWN: usize = 20;
/// Background subagents run as conversations of their own, named so.
const SUBAGENT_TITLE_PREFIX: &str = "Subagent · ";
pub const PICKER_TITLE: &str = "Resume a conversation";
const SEARCH_HINT: &str = "Type to search · tab: all your chats";
const ALL_HINT: &str = "Type to search · tab: gz code conversations only";

/// What `/resume` lists, kept so typing can narrow it without asking the
/// server again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeList {
    /// Conversations started from `gz code`, newest first.
    pub local: Vec<Entry>,
    /// Your chats as the server lists them; `Err` when it couldn't load.
    pub remote: Result<Vec<Conversation>, String>,
    /// The folder gz runs in, named "this folder".
    pub here: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub now_millis: u64,
    pub query: String,
    /// Every chat, not only gz code's.
    pub show_all: bool,
}

impl ResumeList {
    pub fn new(
        local: Vec<Entry>,
        remote: Result<Vec<Conversation>, String>,
        here: Option<PathBuf>,
        now_millis: u64,
    ) -> Self {
        let mut local: Vec<Entry> = local
            .into_iter()
            .filter(|e| !e.title.starts_with(SUBAGENT_TITLE_PREFIX))
            .collect();
        local.sort_by(|a, b| b.updated.cmp(&a.updated));
        Self {
            local,
            remote,
            here,
            home: directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()),
            now_millis,
            query: String::new(),
            show_all: false,
        }
    }

    /// The list without conversation `id`: the one open now is not
    /// something to resume.
    pub fn without(mut self, id: &str) -> Self {
        self.local.retain(|entry| entry.id != id);
        if let Ok(remote) = &mut self.remote {
            remote.retain(|chat| chat.id != id);
        }
        self
    }

    /// Nothing at all to list, even with every chat shown.
    pub fn is_empty(&self) -> bool {
        self.local.is_empty() && self.remote.as_ref().map_or(true, |r| r.is_empty())
    }

    /// The picker rows for the current query and view: a search line, then
    /// the gz code conversations, then (showing all) the other chats.
    pub fn rows(&self) -> Vec<PickerRow> {
        let mut rows = vec![PickerRow::Header(self.search_line())];
        let matching: Vec<PickerRow> = self
            .local
            .iter()
            .filter(|e| self.matches(&self.title_of(e), Some(&e.folder)))
            .map(|e| PickerRow::Conversation {
                id: e.id.clone(),
                title: self.title_of(e),
                detail: self.detail_of(e),
            })
            .collect();
        if self.show_all {
            rows.push(PickerRow::Header("gz code".into()));
        }
        if matching.is_empty() {
            rows.push(PickerRow::Header(self.nothing_here()));
        }
        push_capped(&mut rows, matching);
        if self.show_all {
            rows.push(PickerRow::Header("Other chats".into()));
            match &self.remote {
                Ok(list) => {
                    let others: Vec<PickerRow> = list
                        .iter()
                        .filter(|c| !self.local.iter().any(|e| e.id == c.id))
                        .filter(|c| !c.title.starts_with(SUBAGENT_TITLE_PREFIX))
                        .filter(|c| self.matches(&c.title, None))
                        .map(|c| PickerRow::Conversation {
                            id: c.id.clone(),
                            title: untitled(&c.title),
                            detail: String::new(),
                        })
                        .collect();
                    if others.is_empty() {
                        rows.push(PickerRow::Header("  none match".into()));
                    }
                    push_capped(&mut rows, others);
                }
                Err(err) => rows.push(PickerRow::Header(format!(
                    "  couldn't load your chats: {}",
                    crate::http::readable_error(err)
                ))),
            }
        }
        rows
    }

    fn search_line(&self) -> String {
        if self.query.is_empty() {
            return if self.show_all { ALL_HINT } else { SEARCH_HINT }.to_string();
        }
        format!("Search: {}▏", self.query)
    }

    fn nothing_here(&self) -> String {
        if self.query.is_empty() {
            "  No gz code conversations on this computer yet".into()
        } else {
            format!("  No gz code conversation matches “{}”", self.query)
        }
    }

    /// Case-insensitive: every word of the query in the title or folder.
    fn matches(&self, title: &str, folder: Option<&Path>) -> bool {
        let haystack = format!(
            "{} {}",
            title.to_lowercase(),
            folder
                .map(|f| self.folder_label(f))
                .unwrap_or_default()
                .to_lowercase()
        );
        self.query
            .to_lowercase()
            .split_whitespace()
            .all(|word| haystack.contains(word))
    }

    /// The server's title (it renames chats), else the first prompt's.
    fn title_of(&self, entry: &Entry) -> String {
        self.remote
            .as_ref()
            .ok()
            .and_then(|list| list.iter().find(|c| c.id == entry.id))
            .map(|c| c.title.clone())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| untitled(&entry.title))
    }

    /// `this folder · 2h ago · 14 messages`.
    fn detail_of(&self, entry: &Entry) -> String {
        let mut parts = vec![
            self.folder_label(&entry.folder),
            ago(self.now_millis.saturating_sub(entry.updated)),
        ];
        if entry.messages > 0 {
            let plural = if entry.messages == 1 { "" } else { "s" };
            parts.push(format!("{} message{plural}", entry.messages));
        }
        parts.join(" · ")
    }

    fn folder_label(&self, folder: &Path) -> String {
        if self.here.as_deref() == Some(folder) {
            return "this folder".into();
        }
        match self.home.as_deref().map(|home| folder.strip_prefix(home)) {
            Some(Ok(rel)) if rel.as_os_str().is_empty() => "~".into(),
            Some(Ok(rel)) => format!("~/{}", rel.display()),
            _ => folder.display().to_string(),
        }
    }
}

/// A list holding these gz code conversations (`(id, title)`, newest
/// first), for tests of what the picker leads to.
#[cfg(test)]
pub fn list_of(chats: &[(&str, &str)]) -> ResumeList {
    let count = chats.len() as u64;
    let local = chats
        .iter()
        .zip(0..)
        .map(|((id, title), i)| Entry {
            id: id.to_string(),
            folder: PathBuf::from("/w"),
            title: title.to_string(),
            updated: count - i,
            messages: 0,
        })
        .collect();
    ResumeList::new(local, Ok(Vec::new()), None, count)
}

fn untitled(title: &str) -> String {
    if title.trim().is_empty() {
        "Untitled".into()
    } else {
        title.to_string()
    }
}

/// `5m ago`, `3h ago`, `2d ago`.
pub fn ago(millis: u64) -> String {
    let minutes = millis / 60_000;
    match minutes {
        0 => "just now".into(),
        1..=59 => format!("{minutes}m ago"),
        60..=1439 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}

/// The first [`ROWS_SHOWN`] of `matching`, then how many more there are.
fn push_capped(rows: &mut Vec<PickerRow>, matching: Vec<PickerRow>) {
    let more = matching.len().saturating_sub(ROWS_SHOWN);
    rows.extend(matching.into_iter().take(ROWS_SHOWN));
    if more > 0 {
        rows.push(PickerRow::Header(format!(
            "  … {more} more · type to search"
        )));
    }
}

/// Items that reprint an earlier conversation: a note, then its last
/// messages as you/gz turns.
#[cfg(test)]
pub fn history_items(title: &str, messages: &[Message]) -> Vec<Item> {
    history_entries(title, messages)
        .into_iter()
        .map(|entry| entry.item)
        .collect()
}

/// [`history_items`], each marked when it is a mid-answer note.
pub fn history_entries(title: &str, messages: &[Message]) -> Vec<replay::Replayed> {
    let notice = |text: String| replay::Replayed {
        item: Item::Notice(text),
        is_note: false,
    };
    let hidden = messages.len().saturating_sub(MESSAGES_SHOWN);
    let mut items = vec![notice(format!(
        "Resumed “{title}” · {} earlier message{}",
        messages.len(),
        if messages.len() == 1 { "" } else { "s" }
    ))];
    if hidden > 0 {
        items.push(notice(format!(
            "… {hidden} older message{} not shown",
            if hidden == 1 { "" } else { "s" }
        )));
    }
    items.extend(messages[hidden..].iter().flat_map(replay::message_entries));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::history::Speaker;
    use std::path::PathBuf;

    fn entry(id: &str, folder: &str, title: &str, updated: u64, messages: u64) -> Entry {
        Entry {
            id: id.into(),
            folder: PathBuf::from(folder),
            title: title.into(),
            updated,
            messages,
        }
    }

    fn convo(id: &str, title: &str) -> Conversation {
        Conversation {
            id: id.into(),
            title: title.into(),
        }
    }

    fn titles(rows: &[PickerRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                PickerRow::Header(h) => format!("# {h}"),
                PickerRow::Conversation { title, .. } => title.clone(),
                _ => String::new(),
            })
            .collect()
    }

    fn list(remote: Result<Vec<Conversation>, String>) -> ResumeList {
        let now = 10 * 3_600_000;
        let local = vec![
            entry("b", "/w/other", "local b", 0, 0),
            entry("a", "/w/app", "local a", now - 120_000, 14),
            entry("s", "/w/app", "Subagent · tests", now, 2),
        ];
        let mut list = ResumeList::new(local, remote, Some(PathBuf::from("/w/app")), now);
        list.home = Some(PathBuf::from("/w"));
        list
    }

    #[test]
    fn the_conversation_open_now_is_not_offered() {
        let remote = vec![Conversation {
            id: "a".into(),
            title: "server title".into(),
        }];
        let mut list = list(Ok(remote)).without("a");
        list.show_all = true;
        let titles = titles(&list.rows());
        assert!(
            !titles
                .iter()
                .any(|t| t.contains("local a") || t.contains("server title")),
            "{titles:?}"
        );
        assert!(titles.iter().any(|t| t == "local b"), "{titles:?}");
    }

    #[test]
    fn says_how_many_more_match_past_the_cut() {
        let local: Vec<Entry> = (0..15)
            .map(|i| entry(&format!("l{i}"), "/w/app", &format!("local {i}"), i, 2))
            .collect();
        let remote: Vec<Conversation> = (0..14)
            .map(|i| convo(&format!("r{i}"), &format!("web {i}")))
            .collect();
        let mut list = ResumeList::new(local, Ok(remote), None, 100);
        let rows = titles(&list.rows());
        assert_eq!(rows.len(), 1 + ROWS_SHOWN + 1);
        assert_eq!(rows.last().unwrap(), "#   … 3 more · type to search");
        list.show_all = true;
        let rows = titles(&list.rows());
        assert_eq!(rows.last().unwrap(), "#   … 2 more · type to search");
        list.query = "local 1".into();
        let rows = titles(&list.rows());
        assert!(!rows.iter().any(|r| r.contains("more ·")), "{rows:?}");
    }

    #[test]
    fn lists_only_gz_code_conversations_newest_first_with_details() {
        let list = list(Ok(vec![convo("a", "Renamed A"), convo("c", "Web chat")]));
        let rows = list.rows();
        assert_eq!(
            titles(&rows),
            [
                "# Type to search · tab: all your chats",
                "Renamed A",
                "local b"
            ]
        );
        assert!(matches!(&rows[1], PickerRow::Conversation { detail, .. }
            if detail == "this folder · 2m ago · 14 messages"));
        assert!(matches!(&rows[2], PickerRow::Conversation { detail, .. }
            if detail == "~/other · 10h ago"));
    }

    #[test]
    fn tab_view_adds_the_other_chats() {
        let mut list = list(Ok(vec![convo("a", "Renamed A"), convo("c", "Web chat")]));
        list.show_all = true;
        assert_eq!(
            titles(&list.rows()),
            [
                "# Type to search · tab: gz code conversations only",
                "# gz code",
                "Renamed A",
                "local b",
                "# Other chats",
                "Web chat"
            ]
        );
    }

    #[test]
    fn typing_narrows_by_title_and_folder() {
        let mut list = list(Ok(vec![]));
        list.query = "OTHER".into();
        assert_eq!(titles(&list.rows()), ["# Search: OTHER▏", "local b"]);
        list.query = "nothing".into();
        assert_eq!(
            titles(&list.rows()),
            [
                "# Search: nothing▏",
                "#   No gz code conversation matches “nothing”"
            ]
        );
    }

    #[test]
    fn a_server_error_still_lists_gz_code_and_says_so_in_all_view() {
        let mut list = list(Err("offline".into()));
        assert_eq!(titles(&list.rows())[1..], ["local a", "local b"]);
        list.show_all = true;
        assert!(titles(&list.rows())
            .last()
            .unwrap()
            .contains("couldn't load your chats: offline"));
        assert!(!list.is_empty());
        assert!(ResumeList::new(vec![], Ok(vec![]), None, 0).is_empty());
    }

    #[test]
    fn reprints_only_the_last_messages() {
        let messages: Vec<Message> = (0..25)
            .map(|i| {
                let speaker = if i % 2 == 0 {
                    Speaker::You
                } else {
                    Speaker::Gz
                };
                Message::new(speaker, format!("m{i}"))
            })
            .collect();
        let items = history_items("Jobs", &messages);
        assert!(matches!(&items[0], Item::Notice(n) if n.contains("25 earlier messages")));
        assert!(matches!(&items[1], Item::Notice(n) if n.contains("5 older messages")));
        assert_eq!(items.len(), 2 + MESSAGES_SHOWN);
        assert!(matches!(&items[2], Item::Assistant(t) if t == "m5"));
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(ago(10_000), "just now");
        assert_eq!(ago(5 * 60_000), "5m ago");
        assert_eq!(ago(3 * 3_600_000), "3h ago");
        assert_eq!(ago(49 * 3_600_000), "2d ago");
    }
}
