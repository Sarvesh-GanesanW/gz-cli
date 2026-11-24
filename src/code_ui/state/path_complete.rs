//! Tab in the input completes the path being typed; with several matches a
//! small menu shows them and repeated Tab steps through it.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};

use crate::code_ui::path_completion::{self, Lookup};

use super::{Effect, UiState};

/// The matches of an open completion and what the input held before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMenu {
    pub choices: Vec<String>,
    /// The match written into the input; `None` while only the shared
    /// start is.
    pub selected: Option<usize>,
    quoted: bool,
    /// The line before the word, which completion never changes.
    head: String,
    /// The word as the user typed it, for Esc.
    typed: String,
    /// The word as completion last wrote it: the menu stays open only while
    /// the input still ends with it.
    written: String,
}

impl UiState {
    /// The completion menu, while the input still ends with what it wrote.
    pub fn path_menu(&self) -> Option<&PathMenu> {
        let menu = self.path_menu.as_ref()?;
        (self.line_before_cursor() == format!("{}{}", menu.head, menu.written)).then_some(menu)
    }

    /// Tab: completes the word before the cursor, or steps to the next
    /// match when a menu is open.
    pub(super) fn complete_path(&mut self) -> Vec<Effect> {
        if self.path_menu().is_some() {
            self.step_path_menu(true);
            return Vec::new();
        }
        self.path_menu = None;
        let line = self.line_before_cursor();
        let Some(word) = path_completion::word_at_end(&line) else {
            return Vec::new();
        };
        let choices = self.path_candidates(&word.text);
        let head = line[..word.start].to_string();
        if let [only] = choices.as_slice() {
            let text = path_completion::written(only, word.quoted);
            self.replace_line_before_cursor(&format!("{head}{text}"));
            return Vec::new();
        }
        if choices.is_empty() {
            return Vec::new();
        }
        let shared = path_completion::shared_start(&choices);
        let (selected, written) = if shared.len() > word.text.len() {
            let quote = if word.quoted || shared.contains(' ') {
                "\""
            } else {
                ""
            };
            (None, format!("{quote}{shared}"))
        } else {
            (Some(0), path_completion::written(&choices[0], word.quoted))
        };
        self.replace_line_before_cursor(&format!("{head}{written}"));
        self.path_menu = Some(PathMenu {
            choices,
            selected,
            quoted: word.quoted,
            typed: line[word.start..].to_string(),
            head,
            written,
        });
        Vec::new()
    }

    /// Keys while the completion menu is open; `None` means "not handled
    /// here" (the menu closes and the key does what it always does).
    pub(super) fn on_path_menu_key(&mut self, key: KeyEvent) -> Option<Vec<Effect>> {
        if self.path_menu().is_none() {
            self.path_menu = None;
            return None;
        }
        match key.code {
            KeyCode::Tab if key.modifiers.is_empty() => self.step_path_menu(true),
            KeyCode::Down => self.step_path_menu(true),
            KeyCode::Up => self.step_path_menu(false),
            KeyCode::Enter => self.path_menu = None,
            KeyCode::Esc => {
                if let Some(menu) = self.path_menu.take() {
                    self.replace_line_before_cursor(&format!("{}{}", menu.head, menu.typed));
                }
            }
            _ => {
                self.path_menu = None;
                return None;
            }
        }
        Some(Vec::new())
    }

    fn step_path_menu(&mut self, forward: bool) {
        let Some(menu) = self.path_menu.as_mut() else {
            return;
        };
        let count = menu.choices.len();
        let next = match (menu.selected, forward) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(i), true) => (i + 1) % count,
            (Some(i), false) => i.checked_sub(1).unwrap_or(count - 1),
        };
        menu.selected = Some(next);
        menu.written = path_completion::written(&menu.choices[next], menu.quoted);
        let line = format!("{}{}", menu.head, menu.written);
        self.replace_line_before_cursor(&line);
    }

    /// Matches for `word` in the current folder; secrets are never listed.
    fn path_candidates(&self, word: &str) -> Vec<String> {
        let base = self
            .sandbox
            .as_ref()
            .map(|s| s.cwd())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
        let hidden = |path: &Path| self.sandbox.as_ref().is_some_and(|s| s.is_secret(path));
        path_completion::candidates(
            word,
            &Lookup {
                base: &base,
                home: home.as_deref(),
                hidden: &hidden,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::assistant::sandbox::Sandbox;
    use crate::code_ui::state::test_support::*;
    use crate::code_ui::state::UiState;
    use crossterm::event::KeyCode;

    fn workspace(name: &str) -> UiState {
        let dir = crate::assistant::sandbox::tests::temp_dir(&format!("tab-{name}"));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("my docs")).unwrap();
        for file in [
            "src/main.rs",
            "src/math.rs",
            "src/lib.rs",
            "notes.md",
            ".env",
        ] {
            std::fs::write(dir.join(file), "x").unwrap();
        }
        let mut s = state();
        s.sandbox = Some(Sandbox::new(&dir).unwrap());
        s
    }

    #[test]
    fn one_match_completes_at_once() {
        let mut s = workspace("one");
        type_text(&mut s, "read no");
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "read notes.md ");
        type_text(&mut s, "and sr");
        s.on_key(key(KeyCode::Tab));
        assert_eq!(
            s.input_text(),
            "read notes.md and src/",
            "folders stay open"
        );
        assert!(s.path_menu().is_none());
    }

    #[test]
    fn several_matches_open_a_menu_that_tab_cycles() {
        let mut s = workspace("cycle");
        type_text(&mut s, "open src/m");
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "open src/ma", "the shared start first");
        assert_eq!(
            s.path_menu().unwrap().choices,
            vec!["src/main.rs", "src/math.rs"]
        );
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "open src/main.rs ");
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "open src/math.rs ");
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "open src/main.rs ", "wraps around");
        s.on_key(key(KeyCode::Enter));
        assert!(s.path_menu().is_none(), "enter keeps the choice");
        assert_eq!(s.input_text(), "open src/main.rs ", "and doesn't send");
    }

    #[test]
    fn esc_puts_back_what_was_typed_and_typing_closes_the_menu() {
        let mut s = workspace("esc");
        type_text(&mut s, "src/");
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "src/lib.rs ");
        s.on_key(key(KeyCode::Esc));
        assert_eq!(s.input_text(), "src/");
        s.on_key(key(KeyCode::Tab));
        type_text(&mut s, "x");
        assert!(s.path_menu().is_none());
        assert_eq!(s.input_text(), "src/lib.rs x");
    }

    #[test]
    fn spaces_are_quoted_and_secrets_never_offered() {
        let mut s = workspace("quote");
        type_text(&mut s, "my");
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "\"my docs/");
        let mut s = workspace("secret");
        type_text(&mut s, ".e");
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), ".e");
    }
}
