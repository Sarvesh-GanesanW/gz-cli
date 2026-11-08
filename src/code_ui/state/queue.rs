//! Messages typed while gz works, like Claude Code's queue: a plain note goes
//! to the running turn (live steering) and the model reads it before its
//! next step, right after the tool calls in flight; a message with `@`
//! mentions or images, or too long for a note, waits for the turn to end.
//! Messages show gray under the spinner until the model reads them; ↑ takes
//! back the ones the server doesn't have yet, and Ctrl+Enter (or Ctrl+X
//! Ctrl+S) stops the turn and sends everything now. Without live steering
//! (`features.liveSteering`) every message waits for the turn.
//!
//! Commands that change the conversation (`/new`, `/clear`, `/resume`,
//! `/compact`) also wait and run in order once the turn ends; the rest
//! (`/usage`, `/model`, `/exit`, …) run at once.

use crate::assistant::steering::{SentNote, Steering, MAX_NOTE_CHARS};
use crate::code_ui::items::Item;
use crate::subagents::SubagentReport;

use super::background::waiting_text;
use super::{new_input, Effect, UiState};

/// Where a message typed during a turn stands, for the live region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waiting {
    /// The run has it; the model reads it at its next step.
    Sent(String),
    /// Goes to the run when it next takes notes (after the current tools).
    Queued(String),
    /// Goes as the next message, after the turn.
    AfterTurn(String),
}

impl UiState {
    /// Enter while busy: a note for the running turn, or the next message.
    pub(super) fn submit_while_busy(&mut self, text: String) -> Vec<Effect> {
        let can_steer = !self.compacting
            && self.images.is_empty()
            && !text.contains('@')
            && text.chars().count() <= MAX_NOTE_CHARS;
        match self.live_steering().filter(|_| can_steer) {
            Some(steering) => {
                steering.queue(&text);
                vec![Effect::Steer]
            }
            None => {
                self.queue.push_back(text);
                Vec::new()
            }
        }
    }

    /// What happens once a turn (or `/compact`) ends: queued commands run
    /// in order, then the user's next queued message goes out, else every
    /// waiting report as one turn.
    pub(super) fn next_message(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        while let Some(next) = self.queue.pop_front() {
            if !is_command(&next) {
                effects.push(Effect::Send(next));
                return effects;
            }
            effects.extend(self.slash(&next));
            let picks = matches!(
                next.split_whitespace().next(),
                Some("/resume" | "/continue")
            );
            if self.compacting || picks {
                // The rest waits for the compaction, or for the user to
                // pick which conversation to continue.
                return effects;
            }
        }
        if self.reports.is_empty() {
            return effects;
        }
        let reports: Vec<String> = self.reports.drain(..).collect();
        effects.push(Effect::SendReports(reports.join("\n\n")));
        effects
    }

    /// The running turn's notes, when the chat server takes them.
    pub(super) fn live_steering(&self) -> Option<&Steering> {
        let supported = self.catalog.as_ref().is_some_and(|c| c.live_steering);
        self.steering.as_ref().filter(|_| supported)
    }

    /// The run took these notes.
    pub fn notes_sent(&mut self, notes: Vec<SentNote>) {
        self.sent_notes.extend(notes);
    }

    /// The model read a note: it joins the conversation where it was read.
    pub(super) fn note_read(&mut self, steer_id: &str) {
        let Some(at) = self.sent_notes.iter().position(|n| n.steer_id == steer_id) else {
            return;
        };
        let note = self.sent_notes.remove(at);
        self.close_text();
        let item = match SubagentReport::name_in(&note.text) {
            Some(name) => Item::Notice(format!("⎿  gz read the report from {name}")),
            None => Item::User(note.text),
        };
        self.record(item);
    }

    /// At the end of a turn, notes the model never read go first in the
    /// queue, in the order they were typed.
    /// Subagent reports among them wait with the other reports.
    pub(super) fn requeue_unread_notes(&mut self) {
        let mut unread: Vec<String> = self.sent_notes.drain(..).map(|n| n.text).collect();
        if let Some(steering) = &self.steering {
            unread.extend(steering.take_all());
        }
        let (reports, typed): (Vec<String>, Vec<String>) = unread
            .into_iter()
            .partition(|text| SubagentReport::name_in(text).is_some());
        for text in typed.into_iter().rev() {
            self.queue.push_front(text);
        }
        for report in reports.into_iter().rev() {
            self.reports.push_front(report);
        }
    }

    /// Everything typed during the turn and not yet read, oldest first.
    pub fn waiting(&self) -> Vec<Waiting> {
        let mut rows: Vec<Waiting> = self
            .sent_notes
            .iter()
            .map(|n| Waiting::Sent(waiting_text(&n.text)))
            .collect();
        if let Some(steering) = &self.steering {
            let pending = steering.pending();
            rows.extend(pending.iter().map(|t| Waiting::Queued(waiting_text(t))));
        }
        rows.extend(self.queue.iter().cloned().map(Waiting::AfterTurn));
        rows
    }

    /// Whether ↑ would take messages back rather than recall history.
    pub(super) fn can_take_back(&self) -> bool {
        !self.queue.is_empty()
            || self.steering.as_ref().is_some_and(|s| {
                s.pending()
                    .iter()
                    .any(|t| SubagentReport::name_in(t).is_none())
            })
    }

    /// ↑ from the first line: messages the server doesn't have yet come back
    /// into the input, one per line, ahead of what was typed.
    pub(super) fn take_back(&mut self) {
        let mut lines: Vec<String> = self.steering.as_ref().map(take_typed).unwrap_or_default();
        lines.extend(self.queue.drain(..));
        let draft = self.input_text();
        if !draft.trim().is_empty() {
            lines.push(draft);
        }
        self.set_input(&lines.join("\n"));
    }

    /// Ctrl+Enter: stop the turn and send every waiting message and the
    /// draft as one message.
    pub(super) fn send_now(&mut self) -> Vec<Effect> {
        if !self.is_busy() {
            return self.submit();
        }
        // Unread reports go with the next report turn, and commands run on
        // their own once the turn ends, ahead of the message.
        self.requeue_unread_notes();
        let (commands, mut parts): (Vec<String>, Vec<String>) =
            self.queue.drain(..).partition(|text| is_command(text));
        self.queue.extend(commands);
        let draft = self.input_text();
        if !draft.trim().is_empty() {
            parts.push(draft.trim().to_string());
        }
        self.input = new_input();
        if !parts.is_empty() {
            self.queue.push_back(parts.join("\n\n"));
        }
        vec![Effect::Cancel]
    }
}

/// Whether a queued entry is a slash command rather than a message.
pub(super) fn is_command(text: &str) -> bool {
    text.starts_with('/')
}

/// Slash commands that change the conversation, so they wait for the
/// running turn (and for `/compact`) instead of running under it.
pub(super) fn waits_for_turn(command: &str) -> bool {
    matches!(
        command.split_whitespace().next(),
        // Moving folders mid-turn would change where the running turn's
        // next tool call lands.
        Some("/new" | "/clear" | "/resume" | "/continue" | "/compact" | "/cd" | "/add-dir")
    )
}

/// Takes back the notes the user typed; subagent reports stay queued.
fn take_typed(steering: &Steering) -> Vec<String> {
    let (reports, typed): (Vec<String>, Vec<String>) = steering
        .take_all()
        .into_iter()
        .partition(|text| SubagentReport::name_in(text).is_some());
    for report in reports {
        steering.queue(&report);
    }
    typed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::steering::Steering;
    use crate::assistant::{AssistantEvent, TurnEnd};
    use crate::code_ui::state::test_support::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::{Arc, Mutex};

    fn steering_state() -> UiState {
        let mut s = with_catalog();
        let resolved = crate::config::resolve(
            &crate::config::Config::default(),
            Some("queue-test"),
            Some("http://127.0.0.1:9".into()),
            Some("stub-token".to_string()),
        );
        let http =
            crate::http::ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5)
                .unwrap();
        s.steering = Some(Steering::new(http, Arc::new(Mutex::new(None))));
        s.start_turn("fix the tests");
        s
    }

    fn sent(id: &str, text: &str) -> SentNote {
        SentNote {
            steer_id: id.into(),
            text: text.into(),
        }
    }

    #[test]
    fn a_note_typed_while_working_goes_to_the_run_and_shows_until_read() {
        let mut s = steering_state();
        type_text(&mut s, "also update the docs");
        assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Effect::Steer]);
        assert_eq!(
            s.waiting(),
            vec![Waiting::Queued("also update the docs".into())]
        );

        let notes = s.steering.as_ref().unwrap().take_all();
        s.notes_sent(vec![sent("steer_1", &notes[0])]);
        assert_eq!(
            s.waiting(),
            vec![Waiting::Sent("also update the docs".into())]
        );

        s.flush.clear();
        s.apply(AssistantEvent::SteerApplied("steer_1".into()));
        assert!(s.waiting().is_empty());
        assert!(matches!(s.flush.last(), Some(Item::User(t)) if t == "also update the docs"));
    }

    #[test]
    fn mentions_and_long_messages_wait_for_the_turn() {
        let mut s = steering_state();
        type_text(&mut s, "look at @src/main.rs");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        let long = "x".repeat(MAX_NOTE_CHARS + 1);
        s.on_paste(&long);
        s.on_key(key(KeyCode::Enter));
        assert_eq!(s.queue.len(), 2);
        assert!(matches!(&s.waiting()[0], Waiting::AfterTurn(t) if t.starts_with("look at")));
    }

    #[test]
    fn unread_notes_go_out_first_when_the_turn_ends() {
        let mut s = steering_state();
        s.notes_sent(vec![sent("steer_1", "first")]);
        s.steering.as_ref().unwrap().queue("second");
        s.queue.push_back("third".into());
        let effects = s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert_eq!(effects, vec![Effect::Send("first".into())]);
        assert_eq!(s.queue, vec!["second".to_string(), "third".to_string()]);
    }

    #[test]
    fn up_takes_back_what_the_server_does_not_have_yet() {
        let mut s = steering_state();
        s.notes_sent(vec![sent("steer_1", "already sent")]);
        s.steering.as_ref().unwrap().queue("held note");
        s.queue.push_back("after turn".into());
        type_text(&mut s, "draft");
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "held note\nafter turn\ndraft");
        assert_eq!(s.waiting(), vec![Waiting::Sent("already sent".into())]);
    }

    #[test]
    fn up_recalls_history_when_nothing_waits() {
        let mut s = state();
        type_text(&mut s, "hello");
        s.on_key(key(KeyCode::Enter));
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "hello");
    }

    #[test]
    fn ctrl_enter_and_ctrl_x_ctrl_s_stop_the_turn_and_send_everything_now() {
        for keys in [
            vec![KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)],
            vec![
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            ],
        ] {
            let mut s = steering_state();
            s.notes_sent(vec![sent("steer_1", "use python")]);
            s.queue.push_back("and add tests".into());
            type_text(&mut s, "now");
            let mut effects = Vec::new();
            for k in keys {
                effects = s.on_key(k);
            }
            assert_eq!(effects, vec![Effect::Cancel]);
            assert_eq!(
                s.queue,
                vec!["use python\n\nand add tests\n\nnow".to_string()]
            );
            assert!(s.input_text().is_empty());
        }
    }

    #[test]
    fn conversation_commands_wait_for_the_turn_and_run_in_order() {
        let mut s = steering_state();
        type_text(&mut s, "/new");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty(), "held");
        s.queue.push_back("hello there".into());
        assert!(matches!(&s.waiting()[0], Waiting::AfterTurn(t) if t == "/new"));
        let effects = s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert_eq!(
            effects,
            vec![Effect::NewConversation, Effect::Send("hello there".into())]
        );
    }

    #[test]
    fn other_commands_run_at_once_while_busy() {
        let mut s = steering_state();
        type_text(&mut s, "/usage");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::FetchAccountUsage]
        );
        assert!(s.queue.is_empty());
    }

    #[test]
    fn a_queued_compact_holds_the_messages_behind_it() {
        let mut s = steering_state();
        s.queue.extend(["/compact".to_string(), "next".to_string()]);
        let effects = s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert_eq!(effects, vec![Effect::Compact(None)]);
        let report = crate::assistant::compaction::Report {
            compacted: false,
            summarized_messages: 0,
            before_tokens: 0,
            after_tokens: 0,
        };
        assert_eq!(
            s.show_compaction(Ok(report)),
            vec![Effect::Send("next".into())]
        );
    }

    #[test]
    fn without_live_steering_messages_wait_for_the_turn() {
        let mut s = steering_state();
        s.catalog.as_mut().unwrap().live_steering = false;
        type_text(&mut s, "use python");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(s.waiting(), vec![Waiting::AfterTurn("use python".into())]);
    }

    #[test]
    fn send_now_keeps_queued_commands_ahead_of_the_message() {
        let mut s = steering_state();
        s.queue
            .extend(["/clear".to_string(), "and add tests".to_string()]);
        type_text(&mut s, "now");
        assert_eq!(s.send_now(), vec![Effect::Cancel]);
        assert_eq!(
            s.queue,
            vec!["/clear".to_string(), "and add tests\n\nnow".to_string()]
        );
    }

    #[test]
    fn ctrl_x_then_another_key_types_normally() {
        let mut s = state();
        s.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        s.on_key(key(KeyCode::Char('a')));
        assert_eq!(s.input_text(), "a");
    }
}
