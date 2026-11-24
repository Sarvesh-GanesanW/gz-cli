//! What the input box shows about how it is being used: the prompt marker
//! (`›`, or `!` for a shell command) and the rows under the box while
//! Ctrl+R search or the Tab path menu is open.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::code_ui::items;
use crate::code_ui::mentions::MAX_SHOWN;
use crate::code_ui::state::{PathMenu, UiState};
use crate::tui::theme;

const SEARCH_HINT: &str = "ctrl+r older · enter keep · esc cancel";
const PATH_HINT: &str = "tab next · enter keep · esc undo";

/// Shown under the input while it holds a `!command`.
pub const SHELL_HINT: &str = "! shell command · not sent to gz";

/// True while the input holds a `!command` the user runs themselves:
/// the same test sending uses, so leading blanks still count.
pub fn is_shell(state: &UiState) -> bool {
    state.input_text().trim_start().starts_with('!')
}

fn shell_style() -> Style {
    Style::default()
        .fg(theme::WARN)
        .add_modifier(Modifier::BOLD)
}

/// The prompt marker on the input's first row: a shell `$` in the shell
/// colour for a `!command`, else `›`.
pub fn marker(state: &UiState) -> Span<'static> {
    if is_shell(state) {
        Span::styled("$ ", shell_style())
    } else {
        Span::styled("› ", theme::brand())
    }
}

/// The shell hint for the footer, in the shell colour.
pub fn shell_hint() -> Span<'static> {
    Span::styled(SHELL_HINT, Style::default().fg(theme::WARN))
}

/// Rows under the input box that replace the usual footer, or `None` to
/// keep it.
pub fn footer(state: &UiState, width: usize) -> Option<Vec<Line<'static>>> {
    if let Some(menu) = state.path_menu() {
        return Some(path_menu_lines(menu, width));
    }
    if state.esc_rewind_armed() {
        return Some(vec![Line::from(Span::styled(
            "  esc again to rewind files",
            theme::muted(),
        ))]);
    }
    if state.mention_indexing() {
        return Some(vec![Line::from(Span::styled(
            "    indexing files…",
            theme::muted(),
        ))]);
    }
    let search = state.history_search.as_ref()?;
    let label = "  search history: ";
    let missing = if search.found.is_none() {
        "  no match"
    } else {
        ""
    };
    let room = width.saturating_sub(label.chars().count() + missing.chars().count() + 2);
    Some(vec![
        Line::from(vec![
            Span::styled(label, theme::muted()),
            Span::styled(
                items::fit(&search.query, room),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("▏", theme::brand()),
            Span::styled(missing, Style::default().fg(theme::WARN)),
        ]),
        Line::from(Span::styled(format!("  {SEARCH_HINT}"), theme::muted())),
    ])
}

/// `text` cut to `room` columns from the front, so the end of a path
/// (the part the choices differ in) stays visible: `…ile_alpha.txt`.
fn fit_tail(text: &str, room: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if text.width() <= room {
        return text.to_string();
    }
    let mut used = 1; // the leading ellipsis
    let mut tail: Vec<char> = Vec::new();
    for ch in text.chars().rev() {
        used += ch.width().unwrap_or(0);
        if used > room {
            break;
        }
        tail.push(ch);
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

/// The path completions: a window of rows around the chosen one, the
/// chosen one marked, then how many more and the keys.
fn path_menu_lines(menu: &PathMenu, width: usize) -> Vec<Line<'static>> {
    let count = menu.choices.len();
    let selected = menu.selected.unwrap_or(0);
    let first = selected
        .saturating_sub(MAX_SHOWN - 1)
        .min(count.saturating_sub(MAX_SHOWN));
    let room = width.saturating_sub(5);
    let mut lines: Vec<Line<'static>> = menu
        .choices
        .iter()
        .enumerate()
        .skip(first)
        .take(MAX_SHOWN)
        .map(|(i, choice)| {
            let chosen = menu.selected == Some(i);
            let style = if chosen {
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(
                    if chosen { "  ❯ " } else { "    " },
                    Style::default().fg(theme::ACCENT),
                ),
                Span::styled(fit_tail(choice, room), style),
            ])
        })
        .collect();
    let more = count.saturating_sub(MAX_SHOWN);
    let hint = if more > 0 {
        format!("    {count} matches · {PATH_HINT}")
    } else {
        format!("    {PATH_HINT}")
    };
    lines.push(Line::from(Span::styled(
        items::fit(&hint, width.saturating_sub(1)),
        theme::muted(),
    )));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_ui::state::test_support::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn plain(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_search_line_shows_the_query_and_whether_it_matched() {
        let mut s = state();
        assert!(footer(&s, 80).is_none());
        s.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        type_text(&mut s, "deploy");
        let text = plain(&footer(&s, 80).unwrap());
        assert!(text.contains("search history: deploy"), "{text}");
        assert!(text.contains("no match"), "{text}");
        type_text(&mut s, &"x".repeat(60));
        let first = &footer(&s, 40).unwrap()[0];
        assert!(first.width() <= 40, "{first:?}");
        assert!(footer(&s, 40).unwrap()[1].width() <= 40);
    }

    #[test]
    fn the_path_menu_keeps_the_chosen_row_in_view() {
        let dir = crate::assistant::sandbox::tests::temp_dir("menu-window");
        for i in 0..12 {
            std::fs::write(dir.join(format!("file{i:02}.txt")), "x").unwrap();
        }
        let mut s = state();
        s.sandbox = Some(crate::assistant::sandbox::Sandbox::new(&dir).unwrap());
        type_text(&mut s, "fi");
        s.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(s.input_text(), "file");
        for _ in 0..11 {
            s.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
        let lines = footer(&s, 40).unwrap();
        let text = plain(&lines);
        assert_eq!(lines.len(), MAX_SHOWN + 1, "{text}");
        assert!(text.contains("❯ file10.txt"), "{text}");
        assert!(text.contains("12 matches"), "{text}");
        assert!(lines.iter().all(|l| l.width() <= 40), "{text}");
    }

    #[test]
    fn a_bang_after_blanks_is_still_shell_mode() {
        let mut s = state();
        type_text(&mut s, " !echo spaced");
        assert!(is_shell(&s));
        assert_eq!(marker(&s).content, "$ ");
        let mut s = state();
        s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        type_text(&mut s, "!ls");
        assert!(is_shell(&s), "a blank first line is trimmed when sent too");
    }

    #[test]
    fn a_bang_switches_the_marker_to_shell_mode() {
        let mut s = state();
        assert_eq!(marker(&s).content, "› ");
        assert!(!is_shell(&s));
        type_text(&mut s, "!git status");
        assert_eq!(marker(&s).content, "$ ");
        assert_eq!(marker(&s).style.fg, Some(theme::WARN));
        assert!(is_shell(&s));
        s.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(!is_shell(&s));
    }

    fn screen(s: &mut UiState, width: u16) -> String {
        use ratatui::{backend::TestBackend, Terminal};
        let height = crate::code_ui::render::live_height(s, width, 40);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| crate::code_ui::render::draw_live(f, s))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shell_mode_shows_on_screen_at_every_width() {
        for width in [40u16, 80, 120] {
            let mut s = state();
            type_text(&mut s, "!ls");
            let shown = screen(&mut s, width);
            assert!(shown.contains("$ !ls"), "{width}: {shown}");
            assert!(shown.contains(SHELL_HINT), "{width}: {shown}");
            s.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
            let shown = screen(&mut s, width);
            assert!(!shown.contains("shell command"), "{width}: {shown}");
        }
    }

    #[test]
    fn the_path_menu_and_search_line_fit_the_live_width() {
        let dir = crate::assistant::sandbox::tests::temp_dir("menu-live-width");
        let folder = dir.join("very_long_folder_name_for_testing");
        std::fs::create_dir_all(&folder).unwrap();
        for name in ["alpha", "bravo"] {
            std::fs::write(
                folder.join(format!("configuration_backup_file_{name}.txt")),
                "x",
            )
            .unwrap();
        }
        for width in [40u16, 120] {
            let mut s = state();
            s.sandbox = Some(crate::assistant::sandbox::Sandbox::new(&dir).unwrap());
            type_text(&mut s, "very_long_folder_name_for_testing/c");
            s.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            let shown = screen(&mut s, width);
            let rows: Vec<&str> = shown
                .lines()
                .filter(|row| row.contains("configuration") || row.contains("…"))
                .filter(|row| !row.contains('›'))
                .collect();
            assert_eq!(rows.len(), 2, "{width}: {shown}");
            assert_ne!(
                rows[0].trim(),
                rows[1].trim(),
                "{width}: rows differ\n{shown}"
            );
            if width == 120 {
                assert!(
                    shown.contains("configuration_backup_file_bravo.txt"),
                    "{shown}"
                );
            }

            let mut s = state();
            s.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
            type_text(&mut s, &"x".repeat(47));
            let shown = screen(&mut s, width);
            assert!(shown.contains("no match"), "{width}: {shown}");
        }
    }
}
