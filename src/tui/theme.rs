//! One palette for the whole TUI. ANSI/indexed colors only, so it renders the
//! same in Terminal.app (no truecolor) and on light or dark backgrounds.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Cyan;
pub const ACCENT_ALT: Color = Color::Magenta;
pub const OK: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const ERROR: Color = Color::Red;
pub const MUTED: Color = Color::DarkGray;

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn brand() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn border(focused: bool) -> Style {
    if focused {
        Style::default().fg(ACCENT)
    } else {
        muted()
    }
}

pub fn title() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn header_cell() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn selected_row() -> Style {
    Style::default()
        .bg(Color::Blue)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

pub fn key_hint() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn json_key() -> Style {
    Style::default().fg(ACCENT)
}

pub fn json_string() -> Style {
    Style::default().fg(OK)
}

pub fn json_number() -> Style {
    Style::default().fg(WARN)
}

pub fn json_literal() -> Style {
    Style::default().fg(ACCENT_ALT)
}
