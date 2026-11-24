//! Prompt history in the input: ↑/↓ recall (kept per folder across
//! sessions) and Ctrl+R reverse search.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::code_ui::items::Item;
use crate::code_ui::prompt_history::PromptHistory;

use super::UiState;

/// An open Ctrl+R search: the typed text, the entry it found, and the
/// input as it was, for Esc.
#[derive(Debug, Clone, PartialEq)]
pub struct HistorySearch {
    pub query: String,
    /// Index into the history of the entry shown; `None`: no match.
    pub found: Option<usize>,
    original: String,
}

impl UiState {
    /// Loads the prompt history kept for `folder` (the session's start
    /// folder). A history that can't be read is reported and the session
    /// keeps prompts in memory.
    pub fn open_prompt_history(&mut self, folder: &std::path::Path) {
        match PromptHistory::for_folder(folder) {
            Ok(history) => self.history = history,
            Err(err) => self.record(Item::Error(format!(
                "cannot read your prompt history: {err:#}; this session's prompts are kept in memory only"
            ))),
        }
    }

    /// Records a sent prompt in the history; a failed save is shown once
    /// per session but never stops the prompt.
    pub(super) fn remember_prompt(&mut self, text: &str) {
        if let Err(err) = self.history.push(text) {
            if std::mem::replace(&mut self.history_save_failed, true) {
                return;
            }
            self.record(Item::Error(format!(
                "cannot save your prompt history: {err:#}; check that gz's config folder is writable"
            )));
        }
    }

    /// ↑/↓ while a recalled prompt is shown unedited: they keep browsing
    /// the history even when that prompt (a `/command`, an `@mention`)
    /// opened a menu that would otherwise take the arrows. Returns false
    /// for other keys or when the cursor isn't on the edge row.
    pub(super) fn on_browsing_key(&mut self, key: KeyEvent) -> bool {
        if !key.modifiers.is_empty() || !self.history.is_browsing(&self.input_text()) {
            return false;
        }
        let row = self.input.cursor().0;
        let back = match key.code {
            KeyCode::Up if row == 0 => true,
            KeyCode::Down if row + 1 >= self.input.lines().len() => false,
            _ => return false,
        };
        self.history_step(back);
        true
    }

    /// ↑ (`back`) or ↓ through earlier prompts.
    pub(super) fn history_step(&mut self, back: bool) {
        let current = self.input_text();
        if let Some(text) = self.history.step(back, &current) {
            self.set_input(&text);
        }
    }

    /// Ctrl+R: starts a reverse search over earlier prompts.
    pub(super) fn open_history_search(&mut self) {
        let original = self.input_text();
        self.history_search = Some(HistorySearch {
            query: String::new(),
            found: self.history.find("", None),
            original,
        });
        self.show_search_match();
    }

    /// Keys while Ctrl+R search is open: typing narrows it, Ctrl+R finds
    /// the next older match, Enter keeps the match, Esc puts the input back.
    pub(super) fn on_history_search_key(&mut self, key: KeyEvent) {
        let Some(search) = self.history_search.as_mut() else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('r') if ctrl => {
                if let Some(older) = search
                    .found
                    .and_then(|at| self.history.find_older_distinct(&search.query, at))
                {
                    search.found = Some(older);
                }
            }
            KeyCode::Esc => return self.cancel_history_search(),
            KeyCode::Char('c' | 'g') if ctrl => return self.cancel_history_search(),
            KeyCode::Enter
            | KeyCode::Tab
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down => {
                self.history_search = None;
                self.history.reset_browsing();
                return;
            }
            KeyCode::Backspace => {
                search.query.pop();
                search.found = self.history.find(&search.query, None);
            }
            KeyCode::Char(c) if !ctrl => {
                search.query.push(c);
                search.found = self.history.find(&search.query, None);
            }
            _ => return,
        }
        self.show_search_match();
    }

    fn cancel_history_search(&mut self) {
        if let Some(search) = self.history_search.take() {
            self.set_input(&search.original);
        }
    }

    /// The input shows the entry the search found (the original text while
    /// nothing matches), with the cursor at its end.
    fn show_search_match(&mut self) {
        let Some(search) = &self.history_search else {
            return;
        };
        let text = match search.found {
            Some(at) => self.history.entries()[at].clone(),
            None => search.original.clone(),
        };
        self.set_input(&text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_ui::state::test_support::*;
    use crate::code_ui::state::Effect;

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn with_history(entries: &[&str]) -> UiState {
        let mut s = state();
        s.history = PromptHistory::in_memory(entries);
        s
    }

    #[test]
    fn ctrl_r_finds_older_matches_and_enter_keeps_one() {
        let mut s = with_history(&["fix the tests", "deploy", "run the tests again"]);
        s.on_key(ctrl('r'));
        type_text(&mut s, "test");
        assert_eq!(s.input_text(), "run the tests again");
        s.on_key(ctrl('r'));
        assert_eq!(s.input_text(), "fix the tests");
        s.on_key(ctrl('r'));
        assert_eq!(s.input_text(), "fix the tests", "no older match: stays");
        assert!(
            s.on_key(key(KeyCode::Enter)).is_empty(),
            "enter keeps, never sends"
        );
        assert!(s.history_search.is_none());
        assert_eq!(s.input_text(), "fix the tests");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Send("fix the tests".into())]
        );
    }

    #[test]
    fn ctrl_r_skips_repeats_of_the_match_and_arrows_keep_it() {
        let mut s = with_history(&["make test", "deploy", "make test", "make build"]);
        s.on_key(ctrl('r'));
        type_text(&mut s, "make");
        assert_eq!(s.input_text(), "make build");
        s.on_key(ctrl('r'));
        assert_eq!(s.input_text(), "make test");
        s.on_key(ctrl('r'));
        assert_eq!(s.input_text(), "make test", "the older copy is skipped");
        assert_eq!(s.history_search.as_ref().unwrap().found, Some(2));
        s.on_key(key(KeyCode::Up));
        assert!(s.history_search.is_none(), "↑ keeps the match");
        assert_eq!(s.input_text(), "make test");
    }

    #[test]
    fn a_history_file_that_cannot_be_written_is_reported_once() {
        let dir = crate::assistant::sandbox::tests::temp_dir("history-unwritable");
        let file = dir.join("history.jsonl");
        let mut s = state();
        s.history = PromptHistory::open(&file).unwrap();
        // A folder where the file should be: every append fails.
        std::fs::create_dir_all(&file).unwrap();
        s.remember_prompt("one");
        s.remember_prompt("two");
        let errors = s
            .flush
            .iter()
            .filter(|item| matches!(item, Item::Error(e) if e.contains("prompt history")))
            .count();
        assert_eq!(errors, 1, "{:?}", s.flush);
    }

    #[test]
    fn esc_cancels_the_search_and_restores_the_draft() {
        let mut s = with_history(&["alpha", "beta"]);
        type_text(&mut s, "my draft");
        s.on_key(ctrl('r'));
        assert_eq!(s.input_text(), "beta", "empty search shows the newest");
        type_text(&mut s, "zzz");
        let search = s.history_search.as_ref().unwrap();
        assert_eq!((search.query.as_str(), search.found), ("zzz", None));
        assert_eq!(s.input_text(), "my draft", "no match shows the draft");
        s.on_key(key(KeyCode::Backspace));
        s.on_key(key(KeyCode::Backspace));
        s.on_key(key(KeyCode::Backspace));
        type_text(&mut s, "alp");
        assert_eq!(s.input_text(), "alpha");
        s.on_key(key(KeyCode::Esc));
        assert!(s.history_search.is_none());
        assert_eq!(s.input_text(), "my draft");
    }

    #[test]
    fn pasting_keeps_the_match_and_ends_the_search() {
        let mut s = with_history(&["deploy to dev"]);
        s.on_key(ctrl('r'));
        type_text(&mut s, "dep");
        s.on_paste(" now");
        assert!(s.history_search.is_none());
        assert_eq!(s.input_text(), "deploy to dev now");
    }

    #[test]
    fn arrows_move_through_multi_line_prompts_before_browsing() {
        let mut s = with_history(&["older", "one\ntwo"]);
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "one\ntwo");
        assert_eq!(s.input.cursor().0, 1, "cursor on the last row");
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "one\ntwo", "first ↑ moves to the first row");
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "older");
    }

    #[test]
    fn recalled_commands_and_mentions_do_not_trap_the_arrows() {
        let mut s = with_history(&["older", "/help", "newer"]);
        s.on_key(key(KeyCode::Up));
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "/help");
        assert!(!s.command_menu().is_empty(), "the slash menu is open");
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "older");
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.input_text(), "newer", "↓ also passes the menu");

        let mut s = with_history(&["older", "check @src/main.rs"]);
        s.files = Some(crate::code_ui::mentions::FileIndex::from_entries(&[
            "src/main.rs",
        ]));
        s.on_key(key(KeyCode::Up));
        assert!(!s.mention_menu().is_empty(), "the file popup is open");
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "older");
    }

    #[test]
    fn once_a_recalled_command_is_edited_the_menu_takes_the_arrows() {
        let mut s = with_history(&["older", "/he"]);
        s.on_key(key(KeyCode::Up));
        type_text(&mut s, "l");
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "/hel", "↑ moves in the menu now");
    }

    #[test]
    fn up_and_down_keep_the_draft_and_sent_prompts_are_recalled() {
        let mut s = state();
        type_text(&mut s, "first");
        s.on_key(key(KeyCode::Enter));
        type_text(&mut s, "half typed");
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "first");
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.input_text(), "half typed");
    }
}
