//! Writing longer messages: newlines in the input (`\` then Enter, Ctrl+J,
//! Shift/Alt+Enter) and the message edited in the user's editor (Ctrl+G);
//! and Esc Esc on an empty input, which opens `/rewind`.

use std::time::{Duration, Instant};

use crate::code_ui::items::Item;

use super::{Effect, UiState};

/// Two Esc presses this close together on an empty input open `/rewind`.
pub(super) const DOUBLE_ESC: Duration = Duration::from_millis(800);

impl UiState {
    /// True when the character just before the cursor is a `\`: Enter then
    /// turns it into a newline instead of sending.
    pub(super) fn backslash_before_cursor(&self) -> bool {
        let (row, col) = self.input.cursor();
        col > 0
            && self
                .input
                .lines()
                .get(row)
                .and_then(|line| line.chars().nth(col - 1))
                == Some('\\')
    }

    /// Esc with no turn running: on an empty input, a second Esc within
    /// [`DOUBLE_ESC`] opens the `/rewind` picker.
    pub(super) fn on_idle_esc(&mut self) -> Vec<Effect> {
        if !self.input_text().is_empty() {
            self.esc_armed = None;
            return Vec::new();
        }
        if self.esc_rewind_armed() {
            self.esc_armed = None;
            return self.rewind_command();
        }
        self.esc_armed = Some(Instant::now());
        Vec::new()
    }

    /// True for a moment after one Esc on an empty input: the footer says
    /// a second one opens `/rewind`.
    pub fn esc_rewind_armed(&self) -> bool {
        self.esc_armed.is_some_and(|at| at.elapsed() < DOUBLE_ESC)
    }

    /// The editor closed: its text replaces the input (unsent, cursor at
    /// the end); on an error the input is left as it was.
    pub fn edited_input(&mut self, edited: Result<String, String>) {
        match edited {
            Ok(text) => {
                self.set_input(&text);
                self.mention_selected = 0;
            }
            Err(why) => self.record(Item::Error(format!("{why} (your message is unchanged)"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::code_ui::items::Item;
    use crate::code_ui::state::test_support::*;
    use crate::code_ui::state::Effect;

    #[test]
    fn backslash_enter_adds_a_line_instead_of_sending() {
        let mut s = state();
        type_text(&mut s, "first \\");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        type_text(&mut s, "second");
        assert_eq!(s.input_text(), "first \nsecond", "the backslash is used up");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Send("first \nsecond".into())]
        );
    }

    #[test]
    fn a_backslash_elsewhere_still_sends() {
        let mut s = state();
        type_text(&mut s, "path C:\\temp now");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Send("path C:\\temp now".into())]
        );
    }

    #[test]
    fn ctrl_j_adds_a_line() {
        let mut s = state();
        type_text(&mut s, "a");
        s.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        type_text(&mut s, "b");
        assert_eq!(s.input_text(), "a\nb");
    }

    #[test]
    fn ctrl_g_hands_the_message_to_the_editor_and_takes_its_text() {
        let mut s = state();
        type_text(&mut s, "draft");
        let effects = s.on_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(effects, vec![Effect::EditInput("draft".into())]);
        s.edited_input(Ok("draft\nwith more".into()));
        assert_eq!(s.input_text(), "draft\nwith more");
        assert_eq!(s.input.cursor(), (1, 9), "cursor at the end");
        s.edited_input(Err("the editor 'vi' exited with 1".into()));
        assert_eq!(s.input_text(), "draft\nwith more");
        assert!(
            matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("unchanged")),
            "{:?}",
            s.flush.last()
        );
    }

    #[test]
    fn esc_twice_on_an_empty_input_opens_rewind() {
        let mut s = state();
        assert!(s.on_key(key(KeyCode::Esc)).is_empty());
        assert!(s.esc_rewind_armed());
        let footer = crate::code_ui::input_render::footer(&s, 40).unwrap();
        assert!(format!("{footer:?}").contains("esc again"), "{footer:?}");
        s.on_key(key(KeyCode::Esc));
        assert!(!s.esc_rewind_armed());
        // No edit history in a bare state: /rewind says why it can't.
        assert!(
            matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("rewind") || e.contains("history")),
            "{:?}",
            s.flush.last()
        );
    }

    #[test]
    fn esc_with_text_or_slowly_does_not_rewind() {
        let mut s = state();
        type_text(&mut s, "draft");
        s.on_key(key(KeyCode::Esc));
        s.on_key(key(KeyCode::Esc));
        assert!(s.flush.is_empty(), "{:?}", s.flush);
        assert_eq!(s.input_text(), "draft", "single esc behaviour unchanged");
        let mut s = state();
        s.on_key(key(KeyCode::Esc));
        s.esc_armed = s.esc_armed.map(|at| at - super::DOUBLE_ESC);
        s.on_key(key(KeyCode::Esc));
        assert!(s.flush.is_empty(), "too slow: {:?}", s.flush);
        assert!(s.esc_rewind_armed(), "the second press arms again");
    }

    #[test]
    fn other_keys_between_the_two_esc_presses_disarm_it() {
        let mut s = state();
        s.on_key(key(KeyCode::Esc));
        type_text(&mut s, "x");
        s.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(!s.esc_rewind_armed());
        s.on_key(key(KeyCode::Esc));
        assert!(s.flush.is_empty(), "not consecutive: {:?}", s.flush);
    }

    #[test]
    fn esc_during_a_turn_still_interrupts() {
        let mut s = state();
        s.start_turn("x");
        assert_eq!(s.on_key(key(KeyCode::Esc)), vec![Effect::Cancel]);
        assert_eq!(s.on_key(key(KeyCode::Esc)), vec![Effect::Cancel]);
        assert!(s.picker.is_none());
    }
}
