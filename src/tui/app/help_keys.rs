//! Keys for the `?` overlay: `/` searches, arrows and pages scroll, Esc
//! clears a search and then closes.

use crossterm::event::{KeyCode, KeyEvent};

use crate::tui::help::HelpState;

use super::App;

/// Lines a PgUp/PgDn moves; the overlay is about this tall on a laptop.
const HELP_PAGE: isize = 10;

impl App {
    pub(super) fn on_help_key(&mut self, key: KeyEvent) {
        if self.help_state.searching {
            return self.on_help_search_key(key);
        }
        match key.code {
            KeyCode::Esc if !self.help_state.query.is_empty() => {
                self.help_state = HelpState::default();
            }
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => self.close_help(),
            KeyCode::Char('/') => self.help_state.searching = true,
            KeyCode::Up | KeyCode::Char('k') => self.scroll_help(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_help(1),
            KeyCode::PageUp => self.scroll_help(-HELP_PAGE),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_help(HELP_PAGE),
            KeyCode::Home | KeyCode::Char('g') => self.help_state.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.help_state.scroll = usize::MAX / 2,
            _ => {}
        }
    }

    fn on_help_search_key(&mut self, key: KeyEvent) {
        let state = &mut self.help_state;
        match key.code {
            KeyCode::Esc => *state = HelpState::default(),
            KeyCode::Enter => state.searching = false,
            KeyCode::Backspace => {
                state.query.pop();
                state.scroll = 0;
            }
            KeyCode::Char(c) => {
                state.query.push(c);
                state.scroll = 0;
            }
            _ => {}
        }
    }

    /// The renderer clamps the offset to what is there.
    fn scroll_help(&mut self, delta: isize) {
        let scroll = self.help_state.scroll as isize + delta;
        self.help_state.scroll = scroll.max(0) as usize;
    }

    fn close_help(&mut self) {
        self.help = false;
        self.help_state = HelpState::default();
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use crate::tui::app::test_support::*;

    #[test]
    fn help_searches_then_clears_then_closes() {
        let mut app = app();
        press(&mut app, &[KeyCode::Char('?'), KeyCode::Char('/')]);
        for c in "quit".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert!(app.help, "q is search text while searching");
        assert_eq!(app.help_state.query, "quit");
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("ctrl+c"), "{screen}");
        assert!(!screen.contains("toggle a module"), "{screen}");
        press(&mut app, &[KeyCode::Enter, KeyCode::Esc]);
        assert!(
            app.help && app.help_state.query.is_empty(),
            "esc clears first"
        );
        press(&mut app, &[KeyCode::Esc]);
        assert!(!app.help);
    }

    #[test]
    fn help_groups_screen_actions_views_and_global_keys() {
        let mut app = app();
        app.open_and_focus(resource("Connections", "Connections"));
        answer(
            &mut app,
            serde_json::json!({"data": [{"Name": "GZS3", "Id": 1000, "type": "AWSS3"}]}),
        );
        let titles: Vec<String> = app.help_sections().into_iter().map(|s| s.title).collect();
        assert_eq!(titles, ["This screen", "Actions", "Views", "Global"]);
    }

    #[test]
    fn long_help_scrolls_and_reopens_at_the_top() {
        let mut app = app();
        app.help = true;
        let top = draw(&mut app, 60, 14);
        press(
            &mut app,
            &[KeyCode::PageDown, KeyCode::PageDown, KeyCode::PageDown],
        );
        let scrolled = draw(&mut app, 60, 14);
        assert_ne!(top, scrolled);
        assert!(
            scrolled.contains("quit"),
            "the end is reachable\n{scrolled}"
        );
        press(&mut app, &[KeyCode::Char('?'), KeyCode::Char('?')]);
        assert_eq!(app.help_state.scroll, 0);
    }
}
