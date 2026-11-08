//! Keys and the input box: sending, history, interrupting, and switching
//! between pages and the transcript.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::assistant::attachments::{self, PreparedTurn};
use crate::assistant::Decision;
use crate::code_ui::agent_pages::AgentView;
use crate::code_ui::items::Item;
use crate::code_ui::transcript::{self, Viewer, ViewerAction};

use super::{new_input, queue, Effect, UiState};

impl UiState {
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.viewer.is_some() {
            self.on_viewer_key(key);
            return Vec::new();
        }
        if self.agent_view.is_some() {
            self.on_agent_key(key);
            return Vec::new();
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if shift
            && matches!(key.code, KeyCode::Down | KeyCode::Up)
            && !self.pages.is_empty()
            && self.picker.is_none()
            && self.prompt.is_none()
        {
            self.step_page(key.code == KeyCode::Down);
            return Vec::new();
        }
        if ctrl && key.code == KeyCode::Char('o') && self.picker.is_none() {
            self.viewer = Some(Viewer::open());
            return Vec::new();
        }
        if self.picker.is_some() {
            return self.on_picker_key(key);
        }
        if self.prompt.is_some() {
            if ctrl && key.code == KeyCode::Char('c') {
                self.answer((Decision::Deny, None));
                return vec![Effect::Cancel];
            }
            if self.is_typed_ahead(key) {
                if let KeyCode::Char(c) = key.code {
                    self.typed_ahead.push(c);
                }
                return Vec::new();
            }
            self.on_prompt_key(key);
            return Vec::new();
        }
        if ctrl && key.code == KeyCode::Char('c') {
            return self.on_ctrl_c();
        }
        self.exit_armed = None;
        // Ctrl+X Ctrl+S: send now, for terminals that don't report Ctrl+Enter.
        let chord = std::mem::take(&mut self.ctrl_x_armed);
        if ctrl && key.code == KeyCode::Char('x') {
            self.ctrl_x_armed = true;
            return Vec::new();
        }
        if chord && ctrl && key.code == KeyCode::Char('s') {
            return self.send_now();
        }
        if let Some(effects) = self.on_menu_key(key) {
            return effects;
        }
        if let Some(effects) = self.on_mention_key(key) {
            return effects;
        }
        if ctrl && key.code == KeyCode::Char('v') {
            self.paste_clipboard_image();
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('d') if ctrl && self.input_text().is_empty() => vec![Effect::Exit],
            KeyCode::Char('u') if ctrl => {
                self.input = new_input();
                Vec::new()
            }
            KeyCode::Enter if ctrl => self.send_now(),
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.input.insert_newline();
                Vec::new()
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Esc => {
                if self.show_shortcuts {
                    self.show_shortcuts = false;
                    Vec::new()
                } else if self.is_busy() {
                    vec![Effect::Cancel]
                } else {
                    Vec::new()
                }
            }
            KeyCode::BackTab => {
                self.cycle_mode();
                Vec::new()
            }
            KeyCode::Char('?') if self.input_text().is_empty() => {
                self.show_shortcuts = !self.show_shortcuts;
                Vec::new()
            }
            KeyCode::Up if self.input.cursor().0 == 0 && self.can_take_back() => {
                self.take_back();
                Vec::new()
            }
            KeyCode::Up if self.input.lines().len() == 1 => {
                self.history_step(true);
                Vec::new()
            }
            KeyCode::Down if self.input.lines().len() == 1 => {
                self.history_step(false);
                Vec::new()
            }
            _ => {
                if key.code == KeyCode::Char('@') {
                    self.ensure_file_index();
                }
                self.input.input(key);
                self.menu_selected = 0;
                self.mention_selected = 0;
                Vec::new()
            }
        }
    }

    /// Keys on a subagent's page: shift+↑↓ switch pages, esc goes back to
    /// main, the rest scroll and search like the transcript.
    fn on_agent_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Down | KeyCode::Up)
        {
            self.step_page(key.code == KeyCode::Down);
            return;
        }
        let Some(view) = self.agent_view.as_mut() else {
            return;
        };
        let items = self
            .pages
            .get(view.index)
            .map(|page| page.items())
            .unwrap_or_default();
        let lines = transcript::plain(&transcript::lines(&items, self.transcript_width));
        if view.viewer.on_key(key, &lines) == ViewerAction::Close {
            self.agent_view = None;
        }
    }

    /// Moves to the next (or previous) page; main sits before the first
    /// subagent and after the last.
    fn step_page(&mut self, forward: bool) {
        let count = self.pages.len();
        let current = self.agent_view.as_ref().map(|v| v.index);
        let next = match (current, forward) {
            (None, true) => Some(0),
            (None, false) => count.checked_sub(1),
            (Some(i), true) => (i + 1 < count).then_some(i + 1),
            (Some(i), false) => i.checked_sub(1),
        };
        self.agent_view = next.map(AgentView::open);
    }

    fn on_viewer_key(&mut self, key: KeyEvent) {
        let width = self.transcript_width;
        let lines = transcript::plain(&transcript::lines(&self.transcript_items(), width));
        let Some(viewer) = self.viewer.as_mut() else {
            return;
        };
        if viewer.on_key(key, &lines) == ViewerAction::Close {
            self.viewer = None;
        }
    }

    fn on_ctrl_c(&mut self) -> Vec<Effect> {
        if !self.input_text().is_empty() {
            self.input = new_input();
            return Vec::new();
        }
        if self.exit_armed() {
            return vec![Effect::Exit];
        }
        self.exit_armed = Some(Instant::now());
        if self.is_busy() {
            vec![Effect::Cancel]
        } else {
            Vec::new()
        }
    }

    pub fn input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    pub(super) fn submit(&mut self) -> Vec<Effect> {
        let text = self.input_text();
        let text = text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        let text = text.to_string();
        self.history.push(text.clone());
        self.history_pos = None;
        self.input = new_input();
        if text.starts_with('/') {
            if (self.is_busy() || self.compacting) && queue::waits_for_turn(&text) {
                self.queue.push_back(text);
                return Vec::new();
            }
            return self.slash(&text);
        }
        if self.is_busy() || self.compacting {
            return self.submit_while_busy(text);
        }
        vec![Effect::Send(text)]
    }

    /// Turns a message about to be sent into a [`PreparedTurn`]: mentions
    /// read and checked, images queued for upload. On a bad mention the
    /// error is shown, the text goes back into the input, and `None` means
    /// nothing is sent.
    pub fn take_turn(&mut self, text: &str) -> Option<PreparedTurn> {
        let Some(sandbox) = &self.sandbox else {
            return Some(PreparedTurn::plain(text));
        };
        match attachments::prepare(sandbox, text, &self.images) {
            Ok(turn) => {
                if self.queue.is_empty() {
                    self.images.clear();
                }
                Some(turn)
            }
            Err(err) => {
                self.record(Item::Error(format!("{err:#}")));
                if self.input_text().is_empty() {
                    self.input.insert_str(text);
                }
                None
            }
        }
    }

    pub(super) fn set_input(&mut self, text: &str) {
        self.input = new_input();
        self.input.insert_str(text);
        self.menu_selected = 0;
    }

    fn history_step(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() - 1;
        let next = match (self.history_pos, back) {
            (None, true) => Some(last),
            (None, false) => None,
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i >= last => None,
            (Some(i), false) => Some(i + 1),
        };
        self.history_pos = next;
        let text = next.map(|i| self.history[i].clone()).unwrap_or_default();
        let mut input = new_input();
        input.insert_str(&text);
        self.input = input;
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_ui::state::test_support::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::assistant::{AssistantEvent, TurnEnd};

    #[test]
    fn enter_sends_and_shift_enter_adds_a_line() {
        let mut s = state();
        type_text(&mut s, "hi");
        s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        type_text(&mut s, "there");
        assert_eq!(s.input_text(), "hi\nthere");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Send("hi\nthere".into())]
        );
        assert_eq!(s.input_text(), "");
    }

    #[test]
    fn history_recalls_previous_prompts() {
        let mut s = state();
        type_text(&mut s, "first");
        s.on_key(key(KeyCode::Enter));
        type_text(&mut s, "second");
        s.on_key(key(KeyCode::Enter));
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "second");
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "first");
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.input_text(), "second");
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.input_text(), "");
    }

    #[test]
    fn typing_while_busy_queues_and_sends_after_the_turn() {
        let mut s = state();
        s.start_turn("one");
        type_text(&mut s, "two");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(s.queue.len(), 1);
        let effects = s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert_eq!(effects, vec![Effect::Send("two".into())]);
    }

    #[test]
    fn esc_interrupts_and_ctrl_c_twice_exits() {
        let mut s = state();
        s.start_turn("x");
        assert_eq!(s.on_key(key(KeyCode::Esc)), vec![Effect::Cancel]);
        s.apply(AssistantEvent::Done(TurnEnd::Cancelled));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        type_text(&mut s, "draft");
        assert!(s.on_key(ctrl_c).is_empty(), "first ctrl+c clears the input");
        assert_eq!(s.input_text(), "");
        assert!(s.on_key(ctrl_c).is_empty());
        assert!(s.exit_armed());
        assert_eq!(s.on_key(ctrl_c), vec![Effect::Exit]);
    }

    #[test]
    fn shift_arrows_cycle_main_and_subagent_pages() {
        use crate::subagents::SubagentEvent;
        let mut s = state();
        s.start_turn("go");
        for i in 0..2 {
            s.apply(subagent(SubagentEvent::Started {
                id: format!("sa_{i}"),
                name: format!("Agent {i}"),
                profile: None,
                agent_id: None,
                task: None,
            }));
        }
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT);
        let page = |s: &UiState| s.agent_view.as_ref().map(|v| v.index);
        s.on_key(down);
        assert_eq!(page(&s), Some(0));
        s.on_key(down);
        assert_eq!(page(&s), Some(1));
        s.on_key(down);
        assert_eq!(page(&s), None, "past the last subagent is main again");
        s.on_key(up);
        assert_eq!(page(&s), Some(1));
        s.on_key(key(KeyCode::Esc));
        assert_eq!(page(&s), None, "esc goes back to main");
        s.apply(crate::assistant::AssistantEvent::Done(
            crate::assistant::TurnEnd::Completed,
        ));
        type_text(&mut s, "/new");
        s.on_key(key(KeyCode::Enter));
        assert!(s.pages.is_empty(), "a new conversation has no pages");
    }
}
