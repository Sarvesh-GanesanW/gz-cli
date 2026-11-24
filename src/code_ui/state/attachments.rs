//! What the input attaches to a message: `@` file mentions and pasted or
//! dragged-in images.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};

use crate::assistant::attachments;
#[cfg(test)]
use crate::code_ui::clipboard::ImageClipboard;
use crate::code_ui::items::Item;
use crate::code_ui::mentions::{self, FileIndex};

use super::{new_input, Effect, UiState};

impl UiState {
    /// The input's last line up to the cursor: where an `@word` is typed.
    pub(super) fn line_before_cursor(&self) -> String {
        let (row, col) = self.input.cursor();
        let line = self.input.lines().get(row).cloned().unwrap_or_default();
        line.chars().take(col).collect()
    }

    /// Workspace paths matching the `@word` being typed (empty when no
    /// mention is being typed, or the slash menu is open).
    pub fn mention_menu(&self) -> Vec<String> {
        if !self.command_menu().is_empty() {
            return Vec::new();
        }
        let (Some(files), Some(query)) = (
            &self.files,
            mentions::active_query(&self.line_before_cursor()),
        ) else {
            return Vec::new();
        };
        files.matches(&query)
    }

    /// Keys while the `@` popup is showing; `None` means "not handled here".
    pub(super) fn on_mention_key(&mut self, key: KeyEvent) -> Option<Vec<Effect>> {
        let menu = self.mention_menu();
        if menu.is_empty() {
            return None;
        }
        let selected = self.mention_selected.min(menu.len() - 1);
        match key.code {
            KeyCode::Up => {
                self.mention_selected = selected.checked_sub(1).unwrap_or(menu.len() - 1)
            }
            KeyCode::Down => self.mention_selected = (selected + 1) % menu.len(),
            KeyCode::Tab | KeyCode::Enter if key.modifiers.is_empty() => {
                self.insert_mention(&menu[selected]);
            }
            KeyCode::Esc => {
                let line = self.line_before_cursor();
                let start = mentions::mention_start(&line).unwrap_or(line.len());
                self.replace_line_before_cursor(&line[..start]);
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    fn insert_mention(&mut self, path: &str) {
        let line = self.line_before_cursor();
        self.replace_line_before_cursor(&mentions::complete(&line, path));
        self.mention_selected = 0;
    }

    /// Replaces the current line's text before the cursor, keeping the
    /// other lines and the text after the cursor.
    pub(super) fn replace_line_before_cursor(&mut self, replacement: &str) {
        let (row, col) = self.input.cursor();
        let mut lines: Vec<String> = self.input.lines().to_vec();
        let after: String = lines
            .get(row)
            .map(|l| l.chars().skip(col).collect())
            .unwrap_or_default();
        if let Some(line) = lines.get_mut(row) {
            *line = format!("{replacement}{after}");
        }
        self.input = new_input();
        self.input.insert_str(lines.join("\n"));
        let up = lines.len().saturating_sub(row + 1);
        for _ in 0..up {
            self.input.move_cursor(tui_textarea::CursorMove::Up);
        }
        self.input.move_cursor(tui_textarea::CursorMove::Head);
        for _ in 0..replacement.chars().count() {
            self.input.move_cursor(tui_textarea::CursorMove::Forward);
        }
    }

    /// Scans the current folder for the `@` popup the first time it's
    /// needed, and again after the session moved to another folder.
    pub(super) fn ensure_file_index(&mut self) {
        let Some(sandbox) = &self.sandbox else {
            return;
        };
        match &self.files {
            Some(files) if files.base() == sandbox.cwd() => files.refresh(sandbox),
            _ => self.files = Some(FileIndex::scan(sandbox).with_skills(&self.skill_names)),
        }
    }

    /// True while an `@` mention is typed and the folder is still being
    /// indexed with nothing matching yet: the popup says so.
    pub fn mention_indexing(&self) -> bool {
        self.mention_menu().is_empty()
            && self.command_menu().is_empty()
            && mentions::active_query(&self.line_before_cursor()).is_some()
            && self.files.as_ref().is_some_and(FileIndex::is_filling)
    }

    #[cfg(test)]
    pub fn wait_for_file_index(&self) {
        if let Some(files) = &self.files {
            files.wait();
        }
    }

    #[cfg(test)]
    pub fn set_clipboard(&mut self, clipboard: Box<dyn ImageClipboard>) {
        self.clipboard = clipboard;
    }

    /// Adds an image and drops its `[Image #N]` chip at the cursor.
    pub fn attach_image(&mut self, path: PathBuf) {
        self.images.push(path);
        let chip = attachments::image_chip(self.images.len());
        self.input.insert_str(format!("{chip} "));
    }

    pub(super) fn paste_clipboard_image(&mut self) {
        match self.clipboard.paste_image() {
            Ok(Some(path)) => self.attach_image(path),
            Ok(None) => self
                .flush
                .push(Item::Notice("No image on the clipboard".into())),
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }

    /// Bracketed paste: a dragged-in image file becomes a chip; anything
    /// else is typed into the input.
    pub fn on_paste(&mut self, text: &str) {
        // Pasting ends a Ctrl+R search, keeping its match, like Enter.
        self.history_search = None;
        match mentions::pasted_image(text) {
            Some(path) => self.attach_image(path),
            None => {
                // Terminals end pasted lines with CR; the editor splits on LF.
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                self.input.insert_str(text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_ui::state::test_support::*;

    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::assistant::sandbox::Sandbox;

    use crate::code_ui::items::Item;

    fn workspace_state(name: &str) -> (PathBuf, UiState) {
        let dir = std::env::temp_dir().join(format!("gz-ui-{name}-{}", crate::cmd::new_uuid()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/math.rs"), "pub fn add() {}\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        let mut s = state();
        s.sandbox = Some(Sandbox::new(&dir).unwrap());
        (dir, s)
    }

    #[test]
    fn pasted_carriage_returns_become_line_breaks() {
        let mut s = state();
        s.on_paste("a\r\nb\rc");
        assert_eq!(s.input.lines(), ["a", "b", "c"]);
    }

    #[test]
    fn the_folder_is_indexed_only_once_an_at_is_typed() {
        let (_dir, mut s) = workspace_state("lazy-index");
        type_text(&mut s, "plain text, no mention");
        assert!(s.files.is_none(), "no scan before the first @");
        type_text(&mut s, " @ma");
        assert!(s.files.is_some(), "the first @ starts the scan");
        s.wait_for_file_index();
        assert_eq!(s.mention_menu(), vec!["src/main.rs", "src/math.rs"]);
    }

    #[test]
    fn the_file_popup_follows_the_current_folder() {
        let (dir, mut s) = workspace_state("popup-cwd");
        type_text(&mut s, "@ma");
        s.wait_for_file_index();
        assert_eq!(s.mention_menu(), vec!["src/main.rs", "src/math.rs"]);
        s.input = crate::code_ui::state::new_input();
        s.sandbox
            .as_ref()
            .unwrap()
            .set_cwd(&dir.canonicalize().unwrap().join("src"))
            .unwrap();
        type_text(&mut s, "@ma");
        s.wait_for_file_index();
        assert_eq!(s.mention_menu(), vec!["main.rs", "math.rs"]);
    }

    #[test]
    fn at_opens_a_file_popup_that_inserts_mentions() {
        let (_dir, mut s) = workspace_state("popup");
        type_text(&mut s, "explain @src/ma");
        s.wait_for_file_index();
        assert_eq!(s.mention_menu(), vec!["src/main.rs", "src/math.rs"]);
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "explain @src/math.rs ");
        assert!(s.mention_menu().is_empty(), "closed after inserting a file");
        type_text(&mut s, "@");
        assert!(
            !s.mention_menu().iter().any(|p| p.contains(".env")),
            "secrets never listed"
        );
        type_text(&mut s, "sr");
        s.on_key(key(KeyCode::Enter));
        assert_eq!(
            s.input_text(),
            "explain @src/math.rs @src/",
            "folders stay open"
        );
        assert!(!s.mention_menu().is_empty());
        s.on_key(key(KeyCode::Esc));
        assert_eq!(s.input_text(), "explain @src/math.rs ");
    }

    #[test]
    fn the_popup_says_when_the_folder_is_still_being_indexed() {
        let mut s = state();
        s.files = Some(FileIndex::still_filling());
        type_text(&mut s, "see @zz");
        assert!(s.mention_indexing());
        let lines = crate::code_ui::input_render::footer(&s, 40).unwrap();
        assert!(format!("{lines:?}").contains("indexing files"));
        s.files = Some(FileIndex::from_entries(&["a.rs"]));
        assert!(
            !s.mention_indexing(),
            "a finished index with no match shows nothing"
        );
    }

    #[test]
    fn a_bad_mention_is_shown_and_nothing_is_sent() {
        let (_dir, mut s) = workspace_state("bad");
        let turn = s.take_turn("read @missing.rs");
        assert!(turn.is_none());
        assert_eq!(s.input_text(), "read @missing.rs", "text comes back to fix");
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("doesn't exist")));
        let ok = s.take_turn("read @src/main.rs").unwrap();
        assert_eq!(ok.notes, vec!["Read src/main.rs (1 lines)"]);
        s.start_prepared_turn(&ok);
        assert!(
            matches!(s.flush.last(), Some(Item::UserWithContext { notes, .. }) if notes.len() == 1)
        );
    }

    #[test]
    fn pasted_image_paths_and_clipboard_images_become_chips() {
        let (dir, mut s) = workspace_state("chips");
        let image = dir.join("shot.png");
        std::fs::write(&image, [0x89, b'P']).unwrap();
        s.on_paste(&format!("'{}'", image.display()));
        assert_eq!(s.input_text(), "[Image #1] ");
        s.on_paste("plain text");
        assert_eq!(s.input_text(), "[Image #1] plain text");
        s.set_clipboard(Box::new(crate::code_ui::clipboard::fake::FakeClipboard(
            Some(image.clone()),
        )));
        s.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(s.input_text().ends_with("[Image #2] "));
        let turn = s.take_turn(&s.input_text()).unwrap();
        assert_eq!(turn.uploads.len(), 2);
        assert!(s.images.is_empty(), "chips are used up by the turn");
        s.set_clipboard(Box::new(crate::code_ui::clipboard::fake::FakeClipboard(
            None,
        )));
        s.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("No image")));
    }
}
