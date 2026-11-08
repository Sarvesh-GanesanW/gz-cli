//! `/resume`: the list of earlier conversations (this folder's first, then
//! the rest of your chats) and the earlier messages printed back on resume.

use crate::assistant::history::{Conversation, Entry, Message, Speaker};

use super::items::Item;
use super::state::PickerRow;

/// Rows per section of the picker, so it fits on screen.
const ROWS_PER_SECTION: usize = 10;
/// Earlier messages printed back on resume; older ones are summarised.
const MESSAGES_SHOWN: usize = 20;

/// Picker rows for `/resume`. `local` are this folder's conversations,
/// newest first; `remote` is the server's list (`Err` if it couldn't load).
/// Server titles win, since the server renames chats after the first turn.
pub fn rows(
    local: &[Entry],
    remote: Result<Vec<Conversation>, String>,
    now_millis: u64,
) -> Result<Vec<PickerRow>, String> {
    let title_of = |id: &str, fallback: &str| -> String {
        remote
            .as_ref()
            .ok()
            .and_then(|list| list.iter().find(|c| c.id == id))
            .map(|c| c.title.clone())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| fallback.to_string())
    };
    let mut rows = Vec::new();
    if !local.is_empty() {
        rows.push(PickerRow::Header("This folder".into()));
        rows.extend(
            local
                .iter()
                .take(ROWS_PER_SECTION)
                .map(|e| PickerRow::Conversation {
                    id: e.id.clone(),
                    title: title_of(&e.id, &e.title),
                    detail: ago(now_millis.saturating_sub(e.updated)),
                }),
        );
    }
    match &remote {
        Ok(list) => {
            let others: Vec<PickerRow> = list
                .iter()
                .filter(|c| !local.iter().any(|e| e.id == c.id))
                .take(ROWS_PER_SECTION)
                .map(|c| PickerRow::Conversation {
                    id: c.id.clone(),
                    title: if c.title.trim().is_empty() {
                        "Untitled".into()
                    } else {
                        c.title.clone()
                    },
                    detail: String::new(),
                })
                .collect();
            if !others.is_empty() {
                rows.push(PickerRow::Header("Other conversations".into()));
                rows.extend(others);
            }
        }
        Err(err) if rows.is_empty() => return Err(err.clone()),
        Err(_) => {}
    }
    Ok(rows)
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

/// Items that reprint an earlier conversation: a note, then its last
/// messages as you/gz turns.
pub fn history_items(title: &str, messages: &[Message]) -> Vec<Item> {
    let hidden = messages.len().saturating_sub(MESSAGES_SHOWN);
    let mut items = vec![Item::Notice(format!(
        "Resumed “{title}” · {} earlier message{}",
        messages.len(),
        if messages.len() == 1 { "" } else { "s" }
    ))];
    if hidden > 0 {
        items.push(Item::Notice(format!(
            "… {hidden} older message{} not shown",
            if hidden == 1 { "" } else { "s" }
        )));
    }
    items.extend(messages[hidden..].iter().map(|m| match m.speaker {
        Speaker::You => Item::User(m.text.clone()),
        Speaker::Gz => Item::Assistant(m.text.clone()),
    }));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(id: &str, title: &str, updated: u64) -> Entry {
        Entry {
            id: id.into(),
            folder: PathBuf::from("/w"),
            title: title.into(),
            updated,
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

    #[test]
    fn this_folder_first_with_server_titles_then_the_rest() {
        let now = 10 * 3_600_000;
        let local = [
            entry("a", "local a", now - 120_000),
            entry("b", "local b", 0),
        ];
        let remote = Ok(vec![convo("a", "Renamed A"), convo("c", "Other C")]);
        let rows = rows(&local, remote, now).unwrap();
        assert_eq!(
            titles(&rows),
            [
                "# This folder",
                "Renamed A",
                "local b",
                "# Other conversations",
                "Other C"
            ]
        );
        assert!(matches!(&rows[1], PickerRow::Conversation { detail, .. } if detail == "2m ago"));
    }

    #[test]
    fn a_server_error_still_lists_this_folder() {
        let local = [entry("a", "local a", 0)];
        let rows = rows(&local, Err("offline".into()), 0).unwrap();
        assert_eq!(titles(&rows), ["# This folder", "local a"]);
        assert_eq!(rows_err(), Err("offline".to_string()));
    }

    fn rows_err() -> Result<usize, String> {
        rows(&[], Err("offline".into()), 0).map(|r| r.len())
    }

    #[test]
    fn reprints_only_the_last_messages() {
        let messages: Vec<Message> = (0..25)
            .map(|i| Message {
                speaker: if i % 2 == 0 {
                    Speaker::You
                } else {
                    Speaker::Gz
                },
                text: format!("m{i}"),
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
