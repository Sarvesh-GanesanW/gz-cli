//! Session management in the UI: the `/resume` list's search and "all
//! chats" view, `/rename`, `/fork`, and the replies the session task sends
//! back for them (see [`crate::code_ui::sessions`]).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::assistant::history::MAX_NAME_CHARS;
use crate::code_ui::items::Item;
use crate::code_ui::resume::{ResumeList, PICKER_TITLE};
use crate::code_ui::sessions::{Forked, Reply, Request};

use super::{Effect, Picker, PickerRow, UiState};

const RENAME_USAGE: &str = "Use /rename <new name>";
/// Commands that leave or change the conversation, which must wait while
/// a fork is on its way: the session would land in one conversation and
/// the screen in another.
const WAIT_FOR_FORK: [&str; 8] = [
    "/new",
    "/clear",
    "/resume",
    "/continue",
    "/rewind",
    "/fork",
    "/rename",
    "/compact",
];
const WAIT_FOR_TURN: &str = "Wait for the answer to finish (or esc) before ";

/// What session management keeps between keys and replies.
#[derive(Debug, Default)]
pub struct SessionsUi {
    /// The `/resume` list while its picker is open.
    resume: Option<ResumeList>,
    /// A rename or fork the session task is working on.
    pending: Option<Request>,
    /// `--fork-session --resume`: the conversation picked next is forked.
    fork_next_resume: bool,
    /// Files-rewound notes from before the pending conversation rewind:
    /// the copy still holds the turns they undid, so they stay.
    rewound_kept: usize,
    /// Where in the transcript the notes sent mid-answer are: the chat
    /// keeps them inside the answer, not as messages of their own.
    notes: Vec<usize>,
}

impl SessionsUi {
    /// A fork is on its way: messages wait for it, so they go to the fork.
    pub fn is_waiting(&self) -> bool {
        matches!(
            self.pending,
            Some(Request::Fork { .. } | Request::RewindConversation { .. })
        )
    }
}

impl UiState {
    /// Opens the `/resume` picker on `list`, or says why it can't.
    pub fn show_conversations(&mut self, list: Result<ResumeList, String>) {
        let list = match list {
            Ok(list) => list,
            Err(err) => {
                return self.record(Item::Error(format!(
                    "cannot load your conversations: {}",
                    crate::http::readable_error(&err)
                )))
            }
        };
        if list.is_empty() {
            if let Err(err) = &list.remote {
                return self.record(Item::Error(format!(
                    "cannot load your conversations: {}",
                    crate::http::readable_error(err)
                )));
            }
            return self.record(Item::Notice("No earlier conversations yet".into()));
        }
        self.sessions.resume = Some(list);
        self.refresh_resume_picker();
    }

    /// Rebuilds the open `/resume` picker from the list's query and view,
    /// keeping the highlighted conversation when it still shows.
    fn refresh_resume_picker(&mut self) {
        let Some(list) = &self.sessions.resume else {
            return;
        };
        let rows = list.rows();
        let was = self
            .picker
            .as_ref()
            .and_then(|p| match p.rows.get(p.selected) {
                Some(PickerRow::Conversation { id, .. }) if p.title == PICKER_TITLE => {
                    Some(id.clone())
                }
                _ => None,
            });
        let is_conversation = |r: &PickerRow| matches!(r, PickerRow::Conversation { .. });
        let selected = rows
            .iter()
            .position(
                |r| matches!(r, PickerRow::Conversation { id, .. } if Some(id) == was.as_ref()),
            )
            .or_else(|| rows.iter().position(is_conversation))
            .unwrap_or(0);
        self.picker = Some(Picker {
            title: PICKER_TITLE.into(),
            rows,
            selected,
            model: None,
        });
    }

    fn is_resume_picker(&self) -> bool {
        self.sessions.resume.is_some()
            && self
                .picker
                .as_ref()
                .is_some_and(|p| p.title == PICKER_TITLE)
    }

    /// Keys the `/resume` picker takes before the generic picker: typing
    /// searches, tab switches views. `None` leaves the key to the picker.
    pub(super) fn on_resume_key(&mut self, key: KeyEvent) -> Option<Vec<Effect>> {
        if !self.is_resume_picker() {
            self.sessions.resume = None;
            return None;
        }
        let list = self.sessions.resume.as_mut()?;
        match key.code {
            KeyCode::Esc if !list.query.is_empty() => list.query.clear(),
            KeyCode::Esc => {
                self.sessions.resume = None;
                return None;
            }
            KeyCode::Tab | KeyCode::BackTab => list.show_all = !list.show_all,
            KeyCode::Backspace => {
                list.query.pop();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                list.query.push(c)
            }
            KeyCode::Enter => {
                let chosen = self
                    .picker
                    .as_ref()
                    .and_then(|p| p.rows.get(p.selected))
                    .is_some_and(|r| matches!(r, PickerRow::Conversation { .. }));
                if chosen {
                    self.sessions.resume = None;
                    return None;
                }
                return Some(Vec::new());
            }
            _ => return None,
        }
        self.refresh_resume_picker();
        Some(Vec::new())
    }

    /// Continues the conversation picked in `/resume` (in a fork of it
    /// after `--fork-session`).
    pub(super) fn resume_chosen(&mut self, id: String, title: String) -> Vec<Effect> {
        let mut effects = self.leave_conversation();
        self.record(Item::Notice(format!("Loading “{title}”…")));
        effects.push(Effect::Resume { id, title });
        if std::mem::take(&mut self.sessions.fork_next_resume) {
            effects.extend(self.start_request(Request::Fork { name: None }));
        }
        effects
    }

    /// `--fork-session` with `--resume`: fork the conversation picked next.
    pub fn fork_next_resume(&mut self) {
        self.sessions.fork_next_resume = true;
    }

    /// Refuses `command` while a fork is on its way when it would leave or
    /// change the conversation (true: refused, and said why).
    pub(super) fn refused_while_forking(&mut self, command: &str) -> bool {
        let name = command.split_whitespace().next().unwrap_or_default();
        if !self.sessions.is_waiting() || !WAIT_FOR_FORK.contains(&name) {
            return false;
        }
        self.record(Item::Error(format!(
            "gz is still copying this conversation; run {name} again once it says where you are"
        )));
        true
    }

    /// `/rename <name>`.
    pub(super) fn rename_command(&mut self, command: &str) -> Vec<Effect> {
        // The session task is busy with the turn until it ends.
        if self.is_busy() || self.compacting {
            self.record(Item::Error(format!("{WAIT_FOR_TURN}renaming")));
            return Vec::new();
        }
        let name = command
            .trim()
            .strip_prefix("/rename")
            .unwrap_or_default()
            .trim();
        if name.is_empty() {
            self.record(Item::Error(RENAME_USAGE.into()));
            return Vec::new();
        }
        if name.chars().count() > MAX_NAME_CHARS {
            self.record(Item::Error(format!(
                "That name is {} characters; the chat takes up to {MAX_NAME_CHARS}",
                name.chars().count()
            )));
            return Vec::new();
        }
        self.start_request(Request::Rename(name.to_string()))
    }

    /// `/fork [name]`: copy the whole conversation and carry on in the copy.
    pub(super) fn fork_command(&mut self, command: &str) -> Vec<Effect> {
        if self.is_busy() || self.compacting {
            self.record(Item::Error(format!("{WAIT_FOR_TURN}forking")));
            return Vec::new();
        }
        let name = command
            .trim()
            .strip_prefix("/fork")
            .unwrap_or_default()
            .trim();
        if name.chars().count() > MAX_NAME_CHARS {
            self.record(Item::Error(format!(
                "That name is {} characters; the chat takes up to {MAX_NAME_CHARS}",
                name.chars().count()
            )));
            return Vec::new();
        }
        let name = (!name.is_empty()).then(|| name.to_string());
        self.start_request(Request::Fork { name })
    }

    /// Records a note the user sent while the answer was being written.
    pub(super) fn record_note(&mut self, item: Item) {
        self.sessions.notes.push(self.transcript.len());
        self.record(item);
    }

    /// Records one item of a replayed conversation.
    pub(super) fn record_replayed(&mut self, entry: crate::code_ui::replay::Replayed) {
        if entry.is_note {
            self.record_note(entry.item);
        } else {
            self.record(entry.item);
        }
    }

    /// Whether the transcript item at `index` is a mid-answer note.
    pub(super) fn is_note(&self, index: usize) -> bool {
        self.sessions.notes.contains(&index)
    }

    /// Keeps the first `count` files-rewound notes if the pending
    /// conversation rewind goes through; later ones are dropped then.
    pub(super) fn keep_rewound_on_fork(&mut self, count: usize) {
        self.sessions.rewound_kept = count;
    }

    /// Hands `request` to the session task, one at a time.
    pub(super) fn start_request(&mut self, request: Request) -> Vec<Effect> {
        if self.sessions.pending.is_some() {
            self.record(Item::Error(
                "gz is still renaming or forking this conversation; try again in a moment".into(),
            ));
            return Vec::new();
        }
        self.record(Item::Notice(request.progress()));
        self.sessions.pending = Some(request.clone());
        vec![Effect::Session(request)]
    }

    /// The session task's answer to a rename or fork.
    pub fn session_reply(&mut self, reply: Reply) -> Vec<Effect> {
        let pending = self.sessions.pending.take();
        match reply {
            Reply::Renamed(Ok(name)) => {
                self.conversation.set_title(&name);
                self.record(Item::Notice(format!("✓ Renamed to “{name}”")));
                Vec::new()
            }
            Reply::Renamed(Err(err)) => {
                self.record(Item::Error(format!(
                    "Could not rename the conversation: {}",
                    crate::http::readable_error(&err)
                )));
                Vec::new()
            }
            Reply::Forked(Ok(forked)) => self.forked(forked, pending),
            Reply::Forked(Err(err)) => {
                let is_rewind = matches!(pending, Some(Request::RewindConversation { .. }));
                let what = if is_rewind { "rewind" } else { "fork" };
                let files = if is_rewind && !self.rewound.is_empty() {
                    " · gz hears about the files you rewound with your next message"
                } else {
                    ""
                };
                self.record(Item::Error(format!(
                    "Could not {what} the conversation: {} · you are still in the same \
                     conversation, unchanged{files}",
                    crate::http::readable_error(&err)
                )));
                self.next_message()
            }
        }
    }

    fn forked(&mut self, forked: Forked, pending: Option<Request>) -> Vec<Effect> {
        self.conversation.set_title(&forked.name);
        if let Some(err) = &forked.history_error {
            self.record(Item::Error(err.clone()));
        }
        let Some(Request::RewindConversation { prompt, .. }) = pending else {
            self.record(Item::Notice(format!(
                "✓ Forked into “{}” ({}); you are now in the fork. The original stays as it \
                 was; /resume opens it.",
                forked.name,
                messages_word(forked.messages)
            )));
            return self.next_message();
        };
        // Files rewound with it need no note: the copy never saw the changes.
        self.rewound.truncate(self.sessions.rewound_kept);
        let mut effects = self.leave_conversation();
        self.record(Item::Notice(format!(
            "✓ Conversation rewound: you are now in “{}”, a copy that ends before that \
             message ({} kept). The original stays as it was; /resume opens it. Your message \
             is back in the input to edit and send.",
            forked.name,
            messages_word(forked.messages)
        )));
        effects.push(Effect::Resume {
            id: forked.id,
            title: forked.name,
        });
        self.set_input(&prompt);
        effects.extend(self.next_message());
        effects
    }
}

fn messages_word(n: usize) -> String {
    if n == 1 {
        "1 message".into()
    } else {
        format!("{n} messages")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::history::{Conversation, Entry};
    use crate::code_ui::state::test_support::*;
    use std::path::PathBuf;

    fn list() -> ResumeList {
        let entry = |id: &str, title: &str, updated: u64| Entry {
            id: id.into(),
            folder: PathBuf::from("/w"),
            title: title.into(),
            updated,
            messages: 2,
        };
        ResumeList::new(
            vec![entry("a", "fix the parser", 2), entry("b", "add tests", 1)],
            Ok(vec![Conversation {
                id: "w".into(),
                title: "Web essay".into(),
            }]),
            Some(PathBuf::from("/w")),
            10,
        )
    }

    fn shown(s: &UiState) -> Vec<String> {
        s.picker
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .filter_map(|r| match r {
                PickerRow::Conversation { title, .. } => Some(title.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn typing_searches_tab_shows_all_and_enter_resumes() {
        let mut s = state();
        s.show_conversations(Ok(list()));
        assert_eq!(shown(&s), ["fix the parser", "add tests"]);
        for c in "tests".chars() {
            s.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(shown(&s), ["add tests"]);
        s.on_key(key(KeyCode::Backspace));
        s.on_key(key(KeyCode::Esc));
        assert_eq!(
            shown(&s),
            ["fix the parser", "add tests"],
            "esc clears the search"
        );
        s.on_key(key(KeyCode::Tab));
        assert_eq!(shown(&s), ["fix the parser", "add tests", "Web essay"]);
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Down));
        let effects = s.on_key(key(KeyCode::Enter));
        assert!(effects.contains(&Effect::Resume {
            id: "w".into(),
            title: "Web essay".into()
        }));
        assert!(s.picker.is_none());
    }

    #[test]
    fn fork_session_with_resume_forks_the_picked_conversation() {
        let mut s = state();
        s.fork_next_resume();
        s.show_conversations(Ok(list()));
        let effects = s.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![
                Effect::Resume {
                    id: "a".into(),
                    title: "fix the parser".into()
                },
                Effect::Session(Request::Fork { name: None })
            ]
        );
        s.show_conversations(Ok(list()));
        let again = s.on_key(key(KeyCode::Enter));
        assert_eq!(again.len(), 1, "only the next pick is forked: {again:?}");
    }

    #[test]
    fn the_list_fits_narrow_terminals() {
        use unicode_width::UnicodeWidthStr;
        let mut s = state();
        let mut list = list();
        list.local[0].title = "a very long conversation title that keeps going".into();
        list.local[0].folder = PathBuf::from("/somewhere/deep/in/a/long/folder/path");
        s.show_conversations(Ok(list));
        for width in [40, 60, 120] {
            let picker = s.picker.as_ref().unwrap();
            let lines = crate::code_ui::render::picker_lines(picker, &s, width);
            let text = crate::code_ui::transcript::plain(&lines);
            for line in &text {
                assert!(line.width() <= width, "{width}: {line:?}");
            }
            // The title names the conversation, so it beats the metadata.
            assert!(
                text.iter().any(|l| l.contains("a very long conversation")),
                "{width}: {text:#?}"
            );
        }
    }

    #[test]
    fn j_and_k_are_search_text_in_the_resume_list() {
        let mut s = state();
        s.show_conversations(Ok(list()));
        s.on_key(key(KeyCode::Char('k')));
        assert!(s.picker.is_some());
        assert_eq!(shown(&s), Vec::<String>::new());
        s.on_key(key(KeyCode::Esc));
        s.on_key(key(KeyCode::Esc));
        assert!(s.picker.is_none());
    }

    #[test]
    fn an_empty_list_says_so_and_a_failed_one_says_why() {
        let mut s = state();
        s.show_conversations(Ok(ResumeList::new(vec![], Ok(vec![]), None, 0)));
        assert!(matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("No earlier")));
        s.show_conversations(Ok(ResumeList::new(vec![], Err("offline".into()), None, 0)));
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("offline")));
    }

    #[test]
    fn rename_needs_a_name_and_reports_the_result() {
        let mut s = state();
        assert!(s.slash("/rename").is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e == RENAME_USAGE));
        assert_eq!(
            s.slash("/rename  Parser work "),
            vec![Effect::Session(Request::Rename("Parser work".into()))]
        );
        assert!(s.slash("/rename again").is_empty(), "one request at a time");
        s.session_reply(Reply::Renamed(Ok("Parser work".into())));
        assert!(
            matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("Renamed to “Parser work”"))
        );
        let long = format!("/rename {}", "x".repeat(MAX_NAME_CHARS + 1));
        assert!(s.slash(&long).is_empty());
    }

    #[test]
    fn fork_waits_for_the_reply_then_queued_messages_go_out() {
        let mut s = state();
        assert_eq!(
            s.slash("/fork Try B"),
            vec![Effect::Session(Request::Fork {
                name: Some("Try B".into())
            })]
        );
        assert!(s.sessions.is_waiting());
        type_text(&mut s, "carry on");
        assert!(
            s.on_key(key(KeyCode::Enter)).is_empty(),
            "held until the fork exists"
        );
        let effects = s.session_reply(Reply::Forked(Ok(Forked {
            id: "f1".into(),
            name: "Try B".into(),
            messages: 4,
            history_error: None,
        })));
        assert_eq!(effects, vec![Effect::Send("carry on".into())]);
        assert!(!s.sessions.is_waiting());
        assert!(s
            .flush
            .iter()
            .any(|i| matches!(i, Item::Notice(n) if n.contains("you are now in the fork"))));
    }

    #[test]
    fn a_failed_fork_keeps_the_original() {
        let mut s = state();
        s.slash("/fork");
        s.session_reply(Reply::Forked(
            Err("HTTP 404: Conversation not found".into()),
        ));
        assert!(
            matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("Could not fork") && e.contains("unchanged"))
        );
        assert!(!s.sessions.is_waiting());
    }

    #[test]
    fn commands_that_leave_the_conversation_wait_for_a_fork() {
        let mut s = state();
        s.slash("/fork");
        for command in [
            "/new",
            "/clear",
            "/resume",
            "/rewind",
            "/fork x",
            "/rename y",
        ] {
            assert!(s.slash(command).is_empty(), "{command} waits");
            assert!(
                matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("still copying")),
                "{command}: {:?}",
                s.flush.last()
            );
        }
        assert!(s.rewind_command().is_empty(), "esc esc waits too");
        s.session_reply(Reply::Forked(Ok(Forked {
            id: "f1".into(),
            name: "Copy".into(),
            messages: 2,
            history_error: None,
        })));
        assert_eq!(s.slash("/new").last(), Some(&Effect::NewConversation));
    }

    #[test]
    fn rename_waits_for_the_answer() {
        let mut s = state();
        s.start_turn("work");
        assert!(s.slash("/rename Parser").is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("before renaming")));
    }
}
