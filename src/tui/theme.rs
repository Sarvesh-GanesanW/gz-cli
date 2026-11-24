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

/// How close spend is to a limit, as a color: fine, past 80%, spent.
pub fn spend_level(level: crate::spend::Level) -> Color {
    match level {
        crate::spend::Level::Ok => OK,
        crate::spend::Level::Warn => WARN,
        crate::spend::Level::Over => ERROR,
    }
}

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

/// The changed words of an added line, on a stronger bar than the line.
pub fn diff_added_word() -> Style {
    diff_added().bg(ADDED_WORD_BG).add_modifier(Modifier::BOLD)
}

/// The changed words of a removed line, on a stronger bar than the line.
pub fn diff_removed_word() -> Style {
    diff_removed()
        .bg(REMOVED_WORD_BG)
        .add_modifier(Modifier::BOLD)
}

/// Whether `bg` is one of the added-line bars (green).
pub fn is_added_bar(bg: Option<Color>) -> bool {
    bg.is_some() && (bg == diff_added().bg || bg == diff_added_word().bg)
}

/// Whether `bg` is one of the removed-line bars (red).
pub fn is_removed_bar(bg: Option<Color>) -> bool {
    bg.is_some() && (bg == diff_removed().bg || bg == diff_removed_word().bg)
}

/// A syntax colour drawn on a diff bar: the bright ANSI variant, since
/// the normal one is too dark on a coloured background.
pub fn on_bar(colour: Color) -> Color {
    match colour {
        Color::Red => Color::LightRed,
        Color::Green => Color::LightGreen,
        Color::Yellow => Color::LightYellow,
        Color::Blue => Color::LightBlue,
        Color::Magenta => Color::LightMagenta,
        Color::Cyan => Color::LightCyan,
        // Dark grey (comments) disappears on a coloured bar.
        Color::DarkGray => Color::Gray,
        other => other,
    }
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

/// A color theme. Every screen is drawn with the palette above, then each
/// cell's colors are mapped for the chosen theme ([`recolor`]), so no call
/// site needs to know which theme is on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeName {
    /// The palette as written, for dark backgrounds.
    #[default]
    Dark,
    /// Darker text colors and pale row backgrounds for light backgrounds.
    Light,
    /// Dark, with blue and orange where red and green carry meaning.
    DarkDaltonized,
    /// Light, with blue and orange where red and green carry meaning.
    LightDaltonized,
    /// The 16 standard colors only, which the terminal's own scheme sets.
    Ansi,
}

impl ThemeName {
    pub const ALL: [ThemeName; 5] = [
        ThemeName::Dark,
        ThemeName::Light,
        ThemeName::DarkDaltonized,
        ThemeName::LightDaltonized,
        ThemeName::Ansi,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ThemeName::Dark => "dark",
            ThemeName::Light => "light",
            ThemeName::DarkDaltonized => "dark-daltonized",
            ThemeName::LightDaltonized => "light-daltonized",
            ThemeName::Ansi => "ansi",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            ThemeName::Dark => "for dark backgrounds (the default)",
            ThemeName::Light => "for light backgrounds",
            ThemeName::DarkDaltonized => "dark, red/green shown as orange/blue",
            ThemeName::LightDaltonized => "light, red/green shown as orange/blue",
            ThemeName::Ansi => "only the terminal's 16 colors",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|theme| theme.name().eq_ignore_ascii_case(text.trim()))
    }
}

static ACTIVE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Draws everything from now on in `theme`.
pub fn set_active(theme: ThemeName) {
    let index = ThemeName::ALL.iter().position(|t| *t == theme).unwrap_or(0);
    ACTIVE.store(index as u8, std::sync::atomic::Ordering::Relaxed);
}

pub fn active() -> ThemeName {
    let index = ACTIVE.load(std::sync::atomic::Ordering::Relaxed) as usize;
    ThemeName::ALL.get(index).copied().unwrap_or_default()
}

/// Row backgrounds of the palette (user echo, added and removed lines).
const USER_ROW_BG: Color = Color::Indexed(237);
const ADDED_BG: Color = Color::Indexed(22);
const REMOVED_BG: Color = Color::Indexed(52);
/// The stronger bars under the changed words of an edited line.
const ADDED_WORD_BG: Color = Color::Indexed(28);
const REMOVED_WORD_BG: Color = Color::Indexed(88);

/// `color` as a foreground in `theme`.
pub fn foreground(theme: ThemeName, color: Color) -> Color {
    use ThemeName::*;
    match (theme, color) {
        (Dark | Ansi, _) => color,
        (Light | LightDaltonized, Color::White) => Color::Black,
        (Light | LightDaltonized, Color::Cyan) => Color::Indexed(25),
        (Light | LightDaltonized, Color::Magenta) => Color::Indexed(90),
        (Light | LightDaltonized, Color::Yellow) => Color::Indexed(130),
        (Light | LightDaltonized, Color::DarkGray | Color::Gray) => Color::Indexed(242),
        // The bright variants syntax colours take on diff bars.
        (Light | LightDaltonized, Color::LightCyan | Color::LightBlue) => Color::Indexed(25),
        (Light | LightDaltonized, Color::LightMagenta) => Color::Indexed(90),
        (Light | LightDaltonized, Color::LightYellow) => Color::Indexed(130),
        (Light, Color::LightGreen) => Color::Indexed(28),
        (Light, Color::LightRed) => Color::Indexed(124),
        (LightDaltonized, Color::LightGreen) => Color::Indexed(25),
        (LightDaltonized, Color::LightRed) => Color::Indexed(166),
        (Light, Color::Green) => Color::Indexed(28),
        (Light, Color::Red) => Color::Indexed(124),
        (LightDaltonized, Color::Green) => Color::Indexed(25),
        (LightDaltonized, Color::Red) => Color::Indexed(166),
        (DarkDaltonized, Color::Green) => Color::Indexed(39),
        (DarkDaltonized, Color::Red) => Color::Indexed(208),
        _ => color,
    }
}

/// `color` as a background in `theme`.
pub fn background(theme: ThemeName, color: Color) -> Color {
    use ThemeName::*;
    match (theme, color) {
        (Dark, _) => color,
        (Ansi, USER_ROW_BG) => Color::DarkGray,
        (Ansi, ADDED_BG) => Color::Green,
        (Ansi, REMOVED_BG) => Color::Red,
        // Sixteen colours have no stronger bar; the words stay bold.
        (Ansi, ADDED_WORD_BG) => Color::Green,
        (Ansi, REMOVED_WORD_BG) => Color::Red,
        (Light, ADDED_WORD_BG) => Color::Indexed(157),
        (Light, REMOVED_WORD_BG) => Color::Indexed(217),
        (LightDaltonized, ADDED_WORD_BG) => Color::Indexed(117),
        (LightDaltonized, REMOVED_WORD_BG) => Color::Indexed(216),
        (DarkDaltonized, ADDED_WORD_BG) => Color::Indexed(31),
        (DarkDaltonized, REMOVED_WORD_BG) => Color::Indexed(130),
        (Light | LightDaltonized, USER_ROW_BG) => Color::Indexed(254),
        (Light, ADDED_BG) => Color::Indexed(194),
        (Light, REMOVED_BG) => Color::Indexed(224),
        (LightDaltonized, ADDED_BG) => Color::Indexed(153),
        (LightDaltonized, REMOVED_BG) => Color::Indexed(223),
        (DarkDaltonized, ADDED_BG) => Color::Indexed(24),
        (DarkDaltonized, REMOVED_BG) => Color::Indexed(94),
        _ => color,
    }
}

/// Maps every cell of `buffer` to the active theme; the default theme
/// leaves it as drawn.
pub fn recolor(buffer: &mut ratatui::buffer::Buffer) {
    recolor_as(active(), buffer);
}

/// Maps every cell of `buffer` to `theme`.
fn recolor_as(theme: ThemeName, buffer: &mut ratatui::buffer::Buffer) {
    if theme == ThemeName::Dark {
        return;
    }
    for cell in buffer.content.iter_mut() {
        cell.fg = foreground(theme, cell.fg);
        cell.bg = background(theme, cell.bg);
        // Black on the ANSI theme's bright row colors reads on any scheme.
        if theme == ThemeName::Ansi && matches!(cell.bg, Color::Green | Color::Red) {
            cell.fg = Color::Black;
        }
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    #[test]
    fn changed_word_bars_follow_every_theme() {
        for theme in ThemeName::ALL {
            let line = background(theme, ADDED_BG);
            let word = background(theme, ADDED_WORD_BG);
            let removed_line = background(theme, REMOVED_BG);
            let removed_word = background(theme, REMOVED_WORD_BG);
            if theme == ThemeName::Ansi {
                assert_eq!((word, removed_word), (line, removed_line));
            } else {
                assert_ne!(word, line, "{theme:?}");
                assert_ne!(removed_word, removed_line, "{theme:?}");
            }
            if matches!(theme, ThemeName::Light | ThemeName::LightDaltonized) {
                // Dark text on the pale bars, bright syntax colours included.
                for bright in [
                    Color::LightGreen,
                    Color::LightRed,
                    Color::LightCyan,
                    Color::Gray,
                ] {
                    assert!(matches!(foreground(theme, bright), Color::Indexed(_)));
                }
            }
        }
    }

    #[test]
    fn recoloring_a_buffer_maps_every_cell_and_dark_leaves_it_alone() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let drawn = || {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
            buffer[(0, 0)].set_fg(Color::White);
            buffer[(1, 0)].set_fg(Color::Red).set_bg(ADDED_BG);
            buffer
        };
        let mut dark = drawn();
        recolor_as(ThemeName::Dark, &mut dark);
        assert_eq!(dark, drawn());

        let mut light = drawn();
        recolor_as(ThemeName::Light, &mut light);
        assert_eq!(light[(0, 0)].fg, Color::Black);
        assert_eq!(light[(1, 0)].fg, Color::Indexed(124));
        assert_eq!(light[(1, 0)].bg, Color::Indexed(194));

        let mut ansi = drawn();
        recolor_as(ThemeName::Ansi, &mut ansi);
        assert_eq!(ansi[(1, 0)].bg, Color::Green);
        assert_eq!(ansi[(1, 0)].fg, Color::Black, "readable on the bright row");
    }

    #[test]
    fn names_round_trip_and_unknown_ones_are_refused() {
        for theme in ThemeName::ALL {
            assert_eq!(ThemeName::parse(theme.name()), Some(theme));
        }
        assert_eq!(ThemeName::parse(" Light "), Some(ThemeName::Light));
        assert_eq!(ThemeName::parse("solarized"), None);
    }

    #[test]
    fn daltonized_themes_never_use_red_or_green() {
        for theme in [ThemeName::DarkDaltonized, ThemeName::LightDaltonized] {
            for color in [OK, ERROR] {
                let mapped = foreground(theme, color);
                assert!(!matches!(mapped, Color::Red | Color::Green), "{theme:?}");
            }
            for row in [diff_added(), diff_removed()] {
                let bg = background(theme, row.bg.unwrap());
                assert!(![ADDED_BG, REMOVED_BG].contains(&bg), "{theme:?}");
            }
        }
    }

    #[test]
    fn the_ansi_theme_uses_only_the_sixteen_colors() {
        for style in [user_row(), diff_added(), diff_removed()] {
            let bg = background(ThemeName::Ansi, style.bg.unwrap());
            assert!(!matches!(bg, Color::Indexed(_) | Color::Rgb(..)), "{bg:?}");
        }
    }

    #[test]
    fn light_themes_darken_text_meant_for_dark_backgrounds() {
        assert_eq!(foreground(ThemeName::Light, Color::White), Color::Black);
        assert_ne!(foreground(ThemeName::Light, WARN), WARN);
        assert_eq!(foreground(ThemeName::Dark, WARN), WARN);
    }
}
