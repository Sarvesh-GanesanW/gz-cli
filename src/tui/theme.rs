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

/// Reverse video swaps the terminal's own foreground and background, so the
/// highlight stays readable under any color theme (a fixed blue bar hid the
/// text on themes where "white" is close to that blue).
pub fn selected_row() -> Style {
    Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
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

/// Full-width rows (prompt echo, diff lines) set an explicit foreground on
/// their background, so text stays readable on light and dark themes.
pub fn user_row() -> Style {
    Style::default().bg(Color::Indexed(237)).fg(Color::White)
}

pub fn diff_added() -> Style {
    Style::default().bg(Color::Indexed(22)).fg(Color::White)
}

pub fn diff_removed() -> Style {
    Style::default().bg(Color::Indexed(52)).fg(Color::White)
}

/// Tone of a status badge or a formatted value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "spec vocabulary for the screens being converted next"
)]
pub enum BadgeTone {
    Good,
    Warn,
    Bad,
    Info,
    Muted,
}

pub fn badge(tone: BadgeTone) -> Style {
    match tone {
        BadgeTone::Good => Style::default().fg(OK),
        BadgeTone::Warn => Style::default().fg(WARN),
        BadgeTone::Bad => Style::default().fg(ERROR),
        BadgeTone::Info => Style::default().fg(ACCENT),
        BadgeTone::Muted => muted(),
    }
}
