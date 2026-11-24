//! Keeps keys meant for the input box from answering a prompt that has
//! only just appeared: an Enter pressed to send a message must not approve
//! a write that opened a moment earlier.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::UiState;

/// How long after a permission prompt or question is first drawn its
/// answering and moving keys are held back: long enough to cover a key
/// already on its way, short enough not to slow a deliberate answer.
pub const PROMPT_SETTLE: Duration = Duration::from_millis(500);

/// When the open prompt was first drawn, and how long keys wait after that.
#[derive(Debug)]
pub struct AnswerGuard {
    settle: Duration,
    drawn_at: Option<Instant>,
}

impl AnswerGuard {
    pub fn new(settle: Duration) -> Self {
        Self {
            settle,
            drawn_at: None,
        }
    }

    /// A new prompt is open and not yet on screen.
    pub(super) fn reset(&mut self) {
        self.drawn_at = None;
    }

    /// The prompt was on screen at `now`; only the first draw counts.
    fn drawn(&mut self, now: Instant) {
        self.drawn_at.get_or_insert(now);
    }

    /// True while keys can't yet answer: the prompt isn't drawn, or was
    /// drawn less than the settle time ago.
    fn holds(&self, now: Instant) -> bool {
        if self.settle.is_zero() {
            return false;
        }
        self.drawn_at
            .is_none_or(|at| now.saturating_duration_since(at) < self.settle)
    }
}

impl UiState {
    /// Called after each frame: a prompt or question on screen starts its
    /// settle time.
    pub fn prompt_drawn(&mut self, now: Instant) {
        if self.prompt.is_some() || self.query.is_some() {
            self.answer_guard.drawn(now);
        }
    }

    /// Holds `key` back from the open prompt while it settles: a printable
    /// key goes to the input box, anything else is dropped. Esc and Ctrl+C
    /// are never held (they only refuse or stop). True when held.
    pub(super) fn hold_unsettled_key(&mut self, key: KeyEvent, now: Instant) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let stops = key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c'));
        if stops || !self.answer_guard.holds(now) {
            return false;
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if let (KeyCode::Char(c), true) = (key.code, plain) {
            self.typed_ahead.push(c);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use crate::assistant::questions::{Choice, Query, Question, Reply};
    use crate::assistant::tools::Access;
    use crate::assistant::Decision;
    use crate::code_ui::approver::UiQuery;
    use crate::code_ui::state::test_support::*;
    use crate::code_ui::state::{Picker, PickerRow};

    use super::*;

    fn question_query() -> (UiQuery, tokio::sync::oneshot::Receiver<Reply>) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let query = Query::Questions(vec![Question {
            question: "Which database?".into(),
            header: "Database".into(),
            options: vec![Choice {
                label: "Postgres".into(),
                description: None,
            }],
            multi_select: false,
        }]);
        (UiQuery { query, reply }, answer)
    }

    fn guarded() -> UiState {
        let mut s = state();
        s.answer_guard = AnswerGuard::new(PROMPT_SETTLE);
        s
    }

    #[test]
    fn enter_right_after_a_prompt_appears_does_not_approve_it() {
        let mut s = guarded();
        let (prompt, mut answer) = ask(Access::Write, "write_file notes.md", None);
        s.open_prompt(prompt);
        let shown = Instant::now();
        s.prompt_drawn(shown);
        s.on_key_at(key(KeyCode::Enter), shown + PROMPT_SETTLE / 5);
        assert!(s.prompt.is_some(), "the early Enter is dropped");
        assert!(answer.try_recv().is_err());
        s.on_key_at(key(KeyCode::Enter), shown + PROMPT_SETTLE);
        assert_eq!(answer.try_recv().unwrap(), (Decision::AllowOnce, None));
    }

    #[test]
    fn keys_before_the_first_draw_are_held_however_long_it_was_queued() {
        let mut s = guarded();
        let (prompt, mut answer) = ask(Access::Write, "write_file notes.md", None);
        s.open_prompt(prompt);
        s.on_key_at(key(KeyCode::Char('1')), Instant::now() + PROMPT_SETTLE * 4);
        assert!(answer.try_recv().is_err(), "not drawn yet, so not answered");
    }

    #[test]
    fn early_letters_and_digits_go_to_the_input_and_moves_are_dropped() {
        let mut s = guarded();
        let (prompt, mut answer) = ask(Access::Write, "write_file notes.md", None);
        s.open_prompt(prompt);
        let shown = Instant::now();
        s.prompt_drawn(shown);
        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('2'),
            KeyCode::Down,
            KeyCode::Tab,
            KeyCode::BackTab,
        ] {
            s.on_key_at(key(code), shown);
        }
        assert!(answer.try_recv().is_err());
        assert_eq!(s.prompt.as_ref().unwrap().selected, 0);
        s.on_key_at(key(KeyCode::Char('n')), shown + PROMPT_SETTLE);
        s.on_key_at(key(KeyCode::Enter), shown + PROMPT_SETTLE);
        assert_eq!(answer.try_recv().unwrap().0, Decision::Deny);
        assert_eq!(s.input_text(), "y2");
    }

    #[test]
    fn esc_stops_the_turn_at_once() {
        let mut s = guarded();
        s.start_turn("write the notes");
        let (prompt, mut answer) = ask(Access::Write, "write_file notes.md", None);
        s.open_prompt(prompt);
        s.on_key_at(key(KeyCode::Esc), Instant::now());
        assert_eq!(answer.try_recv().unwrap().0, Decision::Deny);
    }

    #[test]
    fn the_next_prompt_settles_again() {
        let mut s = guarded();
        let (first, mut first_answer) = ask(Access::Write, "write_file a.md", None);
        let (second, mut second_answer) = ask(Access::Write, "write_file b.md", None);
        s.open_prompt(first);
        s.open_prompt(second);
        let shown = Instant::now();
        s.prompt_drawn(shown);
        s.on_key_at(key(KeyCode::Char('1')), shown + PROMPT_SETTLE);
        assert_eq!(first_answer.try_recv().unwrap().0, Decision::AllowOnce);
        s.on_key_at(key(KeyCode::Char('1')), shown + PROMPT_SETTLE * 2);
        assert!(second_answer.try_recv().is_err(), "b.md is not drawn yet");
    }

    fn autocompact_picker() -> Picker {
        Picker {
            title: "Auto-compact".into(),
            rows: vec![
                PickerRow::AutoCompact(None),
                PickerRow::AutoCompact(Some(100_000)),
            ],
            selected: 0,
            model: None,
        }
    }

    #[test]
    fn a_prompt_drawn_over_a_picker_takes_the_keys() {
        let mut s = state();
        s.picker = Some(autocompact_picker());
        let (prompt, mut answer) = ask(Access::Write, "write_file later.md", None);
        s.open_prompt(prompt);
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.prompt.as_ref().unwrap().selected, 1);
        assert_eq!(s.picker.as_ref().unwrap().selected, 0);
        s.on_key(key(KeyCode::Enter));
        assert_eq!(answer.try_recv().unwrap().0, Decision::AllowChat);
        let picker = s.picker.as_ref().expect("the picker is still open");
        assert_eq!(picker.selected, 0, "the picker row was not applied");
    }

    #[test]
    fn a_question_drawn_over_a_picker_takes_the_keys() {
        let mut s = state();
        s.picker = Some(autocompact_picker());
        let (query, mut reply) = question_query();
        s.open_query(query);
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.picker.as_ref().unwrap().selected, 0);
        s.on_key(key(KeyCode::Up));
        s.on_key(key(KeyCode::Enter));
        assert!(s.query.is_none());
        assert!(reply.try_recv().is_ok());
        assert!(s.picker.is_some(), "the picker row was not applied");
    }

    #[test]
    fn text_typed_while_a_question_settles_comes_back_when_the_turn_ends() {
        let mut s = guarded();
        s.start_turn("pick a database");
        let (query, _reply) = question_query();
        s.open_query(query);
        let shown = Instant::now();
        s.prompt_drawn(shown);
        for c in ['a', 'b', 'c'] {
            s.on_key_at(key(KeyCode::Char(c)), shown);
        }
        s.apply(crate::assistant::AssistantEvent::Done(
            crate::assistant::TurnEnd::Cancelled,
        ));
        assert_eq!(s.input_text(), "abc");
        assert!(s.typed_ahead.is_empty());
    }

    #[test]
    fn a_question_holds_enter_too() {
        let mut s = guarded();
        let (query, mut reply) = question_query();
        s.open_query(query);
        let shown = Instant::now();
        s.prompt_drawn(shown);
        s.on_key_at(key(KeyCode::Enter), shown);
        assert!(s.query.is_some(), "the early Enter is dropped");
        assert!(reply.try_recv().is_err());
        s.on_key_at(key(KeyCode::Enter), shown + PROMPT_SETTLE);
        assert!(s.query.is_none());
        assert!(reply.try_recv().is_ok());
    }
}
