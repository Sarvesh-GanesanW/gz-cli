//! How shell commands show: the user's own `!command` with what it printed,
//! a command's output for `/shells <id>`, and the live lines while one runs.

use std::time::Duration;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::code_ui::items::fit;
use crate::code_ui::usage::duration;
use crate::tui::theme;

const TREE: &str = "  ⎿  ";
const OUTPUT_INDENT: &str = "     ";
const CUT_NOTE: &str = "… earlier output is not shown";
const FOREGROUND_HINT: &str = "esc runs it in the background · ctrl+c stops it";

/// What a command printed and where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutput {
    /// `exit code 0`, `stopped`, `bg_2 · running for 5s · npm test`.
    pub status: String,
    /// The end of its output.
    pub text: String,
    /// Older output is missing from `text`.
    pub is_cut: bool,
    pub is_error: bool,
}

/// `! command` on the bar the user's messages use, so it reads as theirs.
pub fn command_rows(command: &str, width: usize) -> Vec<Line<'static>> {
    crate::tui::chat::wrap(command, width.saturating_sub(2))
        .into_iter()
        .enumerate()
        .map(|(i, piece)| {
            let lead = if i == 0 { "! " } else { "  " };
            let row = format!("{lead}{piece}");
            let pad = width.saturating_sub(row.width());
            Line::from(Span::styled(
                format!("{row}{}", " ".repeat(pad)),
                theme::user_row(),
            ))
        })
        .collect()
}

/// The `⎿` status line, then the output dim and indented under it.
pub fn output_lines(output: &ShellOutput, width: usize) -> Vec<Line<'static>> {
    let status_style = if output.is_error {
        Style::default().fg(theme::ERROR)
    } else {
        theme::muted()
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(TREE, theme::muted()),
        Span::styled(
            fit(&output.status, width.saturating_sub(TREE.width())),
            status_style,
        ),
    ])];
    let room = width.saturating_sub(OUTPUT_INDENT.len());
    let cut = output.is_cut.then_some(CUT_NOTE);
    lines.extend(
        cut.into_iter()
            .chain(output.text.trim_end().lines())
            .map(|line| {
                Line::from(Span::styled(
                    format!("{OUTPUT_INDENT}{}", fit(line, room)),
                    theme::muted(),
                ))
            }),
    );
    lines
}

/// `12s`, `2m 10s`: whole seconds, so a live line doesn't flicker through
/// milliseconds in a command's first second.
pub fn running_for(elapsed: Duration) -> String {
    duration(Duration::from_secs(elapsed.as_secs().max(1)))
}

/// The user's command while the input waits for it.
pub fn running_line(command: &str, elapsed: Duration, tick: u64, width: usize) -> Line<'static> {
    let spark = theme::SPINNER[(tick as usize) % theme::SPINNER.len()];
    let hint = format!("  running {} · {FOREGROUND_HINT}", running_for(elapsed));
    let room = width.saturating_sub(hint.width() + 4);
    Line::from(vec![
        Span::styled(format!("{spark} ! {}", fit(command, room)), theme::brand()),
        Span::styled(hint, theme::muted()),
    ])
}

/// `1 command running · /shells` under the input box; nothing when none run.
pub fn count_line(running: usize) -> Option<Line<'static>> {
    let noun = match running {
        0 => return None,
        1 => "command",
        _ => "commands",
    };
    Some(Line::from(vec![
        Span::styled("◦ ", theme::muted()),
        Span::styled(format!("{running} {noun} running"), Style::default()),
        Span::styled(" · /shells", theme::muted()),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.to_string().trim_end().to_string())
            .collect()
    }

    #[test]
    fn output_shows_its_status_then_says_when_older_lines_are_missing() {
        let output = ShellOutput {
            status: "exit code 1".into(),
            text: "two\nthree\n".into(),
            is_cut: true,
            is_error: true,
        };
        assert_eq!(
            text(&output_lines(&output, 60)),
            vec![
                "  ⎿  exit code 1",
                "     … earlier output is not shown",
                "     two",
                "     three",
            ]
        );
        assert_eq!(text(&command_rows("make test", 40)), vec!["! make test"]);
    }

    #[test]
    fn the_count_line_shows_only_while_commands_run() {
        assert!(count_line(0).is_none());
        let one = count_line(1).map(|line| line.to_string());
        assert_eq!(one.as_deref(), Some("◦ 1 command running · /shells"));
        let two = count_line(2).map(|line| line.to_string());
        assert_eq!(two.as_deref(), Some("◦ 2 commands running · /shells"));
    }
}
