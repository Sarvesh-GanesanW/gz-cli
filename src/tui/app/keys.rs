//! Keys, routed by what has focus.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::chat::ChatFocus;

use super::{App, Focus};

impl App {
    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            if self.chat.is_streaming() {
                self.cancel_run();
            } else {
                self.quit = true;
            }
            return;
        }
        if self.palette.is_some() {
            return self.on_palette_key(key);
        }
        if self.overlay.is_some() {
            return self.on_overlay_key(key);
        }
        if self.modal.is_some() {
            return self.on_modal_key(key);
        }
        if self.help {
            return self.on_help_key(key);
        }
        if self.naming.is_some() {
            return self.on_naming_key(key);
        }
        if ctrl && key.code == KeyCode::Char('k') {
            return self.open_palette();
        }
        if ctrl && key.code == KeyCode::Char('e') {
            return self.open_recent();
        }
        if ctrl && key.code == KeyCode::Char('g') {
            return self.jump_to_notice();
        }
        if !self.views.is_empty() {
            return self.on_view_key(key);
        }
        if self.editing_filter {
            return self.on_filter_key(key);
        }
        if self.is_typing() {
            return self.on_composer_key(key);
        }
        match key.code {
            KeyCode::Char(':') => self.open_palette(),
            KeyCode::Char('/') => self.open_search(""),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char(super::home::HOME_KEY) => self.open_home(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Content,
                    Focus::Content => Focus::Sidebar,
                }
            }
            KeyCode::Char('r') => self.reload(),
            _ => match self.focus {
                Focus::Sidebar => self.on_sidebar_key(key),
                Focus::Content if self.is_chat() => self.on_chat_list_key(key),
                Focus::Content => self.on_grid_key(key),
            },
        }
    }

    fn on_sidebar_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => self.sidebar_enter(),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => self.sidebar_left(),
            KeyCode::Char(' ') => self.toggle_module(),
            KeyCode::Char('n') => self.new_chat(),
            _ => {}
        }
    }

    fn on_grid_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('f') {
                self.editing_filter = true;
            }
            return;
        }
        let page = self.grid_viewport;
        let Some(view) = self.view_mut() else { return };
        let grid = view.grid_mut();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => grid.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => grid.move_sel(1),
            KeyCode::PageUp => grid.page(page, false),
            KeyCode::PageDown => grid.page(page, true),
            KeyCode::Home | KeyCode::Char('g') => grid.home(),
            KeyCode::End | KeyCode::Char('G') => grid.end(),
            KeyCode::Char(']') => grid.scroll_cols(1),
            KeyCode::Char('[') => grid.scroll_cols(-1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open_row(),
            KeyCode::Char('i') => self.open_details(),
            KeyCode::Char('o') => grid.sort_next_column(),
            KeyCode::Char('O') => grid.reverse_sort(),
            KeyCode::Char('a') => match self.row_context() {
                Some(row) => self.open_action_menu(row),
                None => self.set_status(super::Tone::Info, "Nothing selected"),
            },
            KeyCode::Esc if !grid.filter.is_empty() => grid.set_filter(""),
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.back(),
            KeyCode::Char(c) => {
                self.row_shortcut(c);
            }
            _ => {}
        }
        if matches!(
            key.code,
            KeyCode::Down
                | KeyCode::Char('j')
                | KeyCode::PageDown
                | KeyCode::End
                | KeyCode::Char('G')
        ) {
            self.load_more_if_at_end();
        }
    }

    fn on_chat_list_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.chat.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.chat.move_sel(1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(conversation) = self.chat.selected_conversation().cloned() {
                    self.open_conversation(conversation);
                }
            }
            KeyCode::Char('n') => self.new_chat(),
            KeyCode::Char('i') => self.chat.focus = ChatFocus::Composer,
            KeyCode::PageUp => self.chat.scroll += self.grid_viewport / 2,
            KeyCode::PageDown => {
                self.chat.scroll = self.chat.scroll.saturating_sub(self.grid_viewport / 2)
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                self.focus = Focus::Sidebar
            }
            _ => {}
        }
    }

    fn on_composer_key(&mut self, key: KeyEvent) {
        let newline = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Enter if newline => {
                self.chat.composer.insert_newline();
            }
            KeyCode::Enter => self.send_prompt(),
            KeyCode::Esc if self.chat.is_streaming() => self.cancel_run(),
            KeyCode::Esc => self.chat.focus = ChatFocus::List,
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => self.new_chat(),
            KeyCode::PageUp => self.chat.scroll += self.grid_viewport / 2,
            KeyCode::PageDown => {
                self.chat.scroll = self.chat.scroll.saturating_sub(self.grid_viewport / 2)
            }
            _ => {
                self.chat.composer.input(key);
            }
        }
    }

    fn on_palette_key(&mut self, key: KeyEvent) {
        let Some(palette) = self.palette.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Up => palette.move_sel(-1),
            KeyCode::Down | KeyCode::Tab => palette.move_sel(1),
            KeyCode::Backspace => palette.pop(),
            KeyCode::Enter => {
                let chosen = palette.chosen();
                self.palette = None;
                if let Some(action) = chosen {
                    self.run_action(action);
                }
            }
            KeyCode::Char(c) => palette.push(c),
            _ => {}
        }
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        let page = modal.viewport.max(1) as isize;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => self.modal = None,
            KeyCode::Up | KeyCode::Char('k') => modal.scroll_by(-1),
            KeyCode::Down | KeyCode::Char('j') => modal.scroll_by(1),
            KeyCode::PageUp => modal.scroll_by(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => modal.scroll_by(page),
            KeyCode::Home | KeyCode::Char('g') => modal.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => modal.scroll_by(isize::MAX / 2),
            _ => {}
        }
    }

    /// Filtering is live: every keystroke re-filters the grid.
    fn on_filter_key(&mut self, key: KeyEvent) {
        let Some(view) = self.view_mut() else {
            self.editing_filter = false;
            return;
        };
        let mut text = view.grid().filter.clone();
        match key.code {
            KeyCode::Esc => {
                text.clear();
                self.editing_filter = false;
            }
            KeyCode::Enter => self.editing_filter = false,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) => text.push(c),
            _ => return,
        }
        if let Some(view) = self.view_mut() {
            view.grid_mut().set_filter(&text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::tui::palette::Action;
    use serde_json::json;

    #[test]
    fn enter_opens_into_content_and_tab_switches_focus() {
        let mut app = app();
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(app.focus, Focus::Content);
        press(&mut app, &[KeyCode::Tab]);
        assert_eq!(app.focus, Focus::Sidebar);
    }

    #[test]
    fn palette_fuzzy_jumps_and_runs_actions() {
        let mut app = app();
        app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(app.palette.is_some());
        for c in "dashb".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.palette.is_none());
        assert_eq!(app.active, resource("Designer", "Dashboards"));
        assert_eq!(app.focus, Focus::Content);
        press(&mut app, &[KeyCode::Char(':')]);
        for c in "quit".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.quit);
    }

    #[test]
    fn letters_type_into_the_composer_not_shortcuts() {
        let mut app = app();
        app.run_action(Action::NewChat);
        press(&mut app, &[KeyCode::Char('q'), KeyCode::Char('?')]);
        assert!(!app.quit && !app.help);
        assert_eq!(app.chat.composer_text(), "q?");
    }

    #[test]
    fn ctrl_c_quits_when_idle() {
        let mut app = app();
        app.help = true;
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn o_cycles_sort_columns_and_capital_o_reverses() {
        let mut app = app();
        app.open_and_focus(resource("Chat", "Budget"));
        answer(
            &mut app,
            json!([{"n": "beta", "size": "2MB"}, {"n": "alpha", "size": "900KB"}, {"n": "gamma", "size": "1GB"}]),
        );
        let column = |app: &App, c: usize| -> Vec<String> {
            let grid = app.view().unwrap().grid();
            grid.rows.iter().map(|r| r[c].clone()).collect()
        };
        press(&mut app, &[KeyCode::Char('o')]);
        assert_eq!(column(&app, 0), ["alpha", "beta", "gamma"]);
        press(&mut app, &[KeyCode::Char('o')]);
        assert_eq!(
            column(&app, 1),
            ["900KB", "2MB", "1GB"],
            "sizes sort by bytes, not text"
        );
        assert!(draw(&mut app, 100, 20).contains("3 rows · sorted by size ▲"));
        press(&mut app, &[KeyCode::Char('O')]);
        assert_eq!(column(&app, 1), ["1GB", "2MB", "900KB"]);
        press(&mut app, &[KeyCode::Char('o')]);
        assert_eq!(column(&app, 0), ["alpha", "beta", "gamma"], "wraps around");
        press(&mut app, &[KeyCode::Char('r')]);
        answer(
            &mut app,
            json!([{"n": "zeta", "size": 1}, {"n": "beta", "size": 2}]),
        );
        assert_eq!(
            column(&app, 0),
            ["beta", "zeta"],
            "the sort survives a reload"
        );
    }

    #[test]
    fn capital_o_sorts_descending_when_nothing_is_sorted() {
        let mut app = app();
        app.open_and_focus(resource("Chat", "Budget"));
        answer(&mut app, json!([{"n": 9}, {"n": 10}, {"n": 2}]));
        press(&mut app, &[KeyCode::Char('O')]);
        let grid = app.view().unwrap().grid();
        let col: Vec<&str> = grid.rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(col, ["10", "9", "2"]);
    }

    #[test]
    fn filter_applies_while_typing() {
        let mut app = app();
        app.open_and_focus(resource("Chat", "Budget"));
        answer(&mut app, json!([{"n": "alpha"}, {"n": "beta"}]));
        app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        press(&mut app, &[KeyCode::Char('b')]);
        assert_eq!(app.view().unwrap().grid().visible_len(), 1);
        assert!(draw(&mut app, 100, 20).contains("filter b▏   1 match(es)"));
        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(app.view().unwrap().grid().visible_len(), 2);
    }
}
