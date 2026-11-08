//! "Are you sure?" with the exact request spelled out. Destructive actions
//! need the row's name typed (or `D` pressed when it has none).

use crossterm::event::{KeyCode, KeyEvent};

/// The key destructive confirmations without a name ask for.
pub const DESTRUCTIVE_KEY: char = 'D';

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Enter or `y` confirms.
    Press,
    /// The text must be typed exactly, then Enter.
    TypeName(String),
    /// Only this key confirms.
    Key(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Waiting,
    Confirmed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Confirm {
    pub title: String,
    /// What will happen, in words ("Delete connection GZS3?").
    pub message: String,
    /// The request itself ("DELETE connections /connection/1000").
    pub request: String,
    pub destructive: bool,
    pub mode: Mode,
    pub typed: String,
    /// Enter was pressed with the wrong name.
    pub mismatch: bool,
}

impl Confirm {
    pub fn new(
        title: impl Into<String>,
        message: impl Into<String>,
        request: impl Into<String>,
        destructive: bool,
        name: Option<String>,
    ) -> Self {
        let mode = match (destructive, name) {
            (false, _) => Mode::Press,
            (true, Some(name)) if !name.trim().is_empty() => Mode::TypeName(name),
            (true, _) => Mode::Key(DESTRUCTIVE_KEY),
        };
        Self {
            title: title.into(),
            message: message.into(),
            request: request.into(),
            destructive,
            mode,
            typed: String::new(),
            mismatch: false,
        }
    }

    /// The instruction line under the message.
    pub fn prompt(&self) -> String {
        match &self.mode {
            Mode::Press => "⏎/y confirm · esc cancel".to_string(),
            Mode::TypeName(name) => format!("Type {name} and press ⏎ to confirm · esc cancel"),
            Mode::Key(key) => format!("Press {key} to confirm · esc cancel"),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        if key.code == KeyCode::Esc {
            return Outcome::Cancelled;
        }
        match &self.mode {
            Mode::Press => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => Outcome::Confirmed,
                KeyCode::Char('n') => Outcome::Cancelled,
                _ => Outcome::Waiting,
            },
            Mode::Key(expected) => match key.code {
                KeyCode::Char(c) if c == *expected => Outcome::Confirmed,
                KeyCode::Char('n') => Outcome::Cancelled,
                _ => Outcome::Waiting,
            },
            Mode::TypeName(name) => {
                match key.code {
                    KeyCode::Enter if self.typed == *name => return Outcome::Confirmed,
                    KeyCode::Enter => self.mismatch = true,
                    KeyCode::Backspace => {
                        self.typed.pop();
                    }
                    KeyCode::Char(c) => {
                        self.typed.push(c);
                        self.mismatch = false;
                    }
                    _ => {}
                }
                Outcome::Waiting
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn plain_confirmations_take_enter_or_y() {
        let mut confirm = Confirm::new("Test", "Test GZS3?", "POST x", false, None);
        assert_eq!(confirm.on_key(key(KeyCode::Char('x'))), Outcome::Waiting);
        assert_eq!(confirm.on_key(key(KeyCode::Enter)), Outcome::Confirmed);
        let mut confirm = Confirm::new("Test", "Test?", "POST x", false, None);
        assert_eq!(confirm.on_key(key(KeyCode::Char('n'))), Outcome::Cancelled);
    }

    #[test]
    fn destructive_ones_need_the_exact_name() {
        let mut confirm = Confirm::new(
            "Delete",
            "Delete GZS3?",
            "DELETE x",
            true,
            Some("GZS3".into()),
        );
        assert_eq!(confirm.on_key(key(KeyCode::Enter)), Outcome::Waiting);
        assert!(confirm.mismatch);
        for c in "GZS".chars() {
            confirm.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(confirm.on_key(key(KeyCode::Enter)), Outcome::Waiting);
        confirm.on_key(key(KeyCode::Char('3')));
        assert!(!confirm.mismatch);
        assert_eq!(confirm.on_key(key(KeyCode::Enter)), Outcome::Confirmed);
    }

    #[test]
    fn y_does_not_confirm_a_destructive_action() {
        let mut confirm = Confirm::new("Delete", "Delete it?", "DELETE x", true, None);
        assert_eq!(confirm.mode, Mode::Key(DESTRUCTIVE_KEY));
        assert_eq!(confirm.on_key(key(KeyCode::Char('y'))), Outcome::Waiting);
        assert_eq!(confirm.on_key(key(KeyCode::Enter)), Outcome::Waiting);
        assert_eq!(confirm.on_key(key(KeyCode::Char('D'))), Outcome::Confirmed);
    }

    #[test]
    fn escape_always_cancels() {
        let mut confirm = Confirm::new("Delete", "x", "y", true, Some("n".into()));
        assert_eq!(confirm.on_key(key(KeyCode::Esc)), Outcome::Cancelled);
    }
}
