//! How any screen, tile or view looks while it loads, when it has nothing
//! to show, and when it failed, so the three states read the same
//! everywhere.

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::widgets::key_hints;
use crate::tui::theme;

/// The spinner frame for `tick`.
pub fn spinner(tick: u64) -> &'static str {
    theme::SPINNER[(tick as usize) % theme::SPINNER.len()]
}

/// "⠋ Loading warehouses…".
pub fn loading(what: &str, tick: u64) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{} ", spinner(tick)),
            Style::default().fg(theme::ACCENT),
        ),
        Span::styled(format!("Loading {what}…"), theme::muted()),
    ])
}

/// A centered "nothing here" block: the screen's own words, then what to
/// do about it.
pub fn empty(message: &str, hints: &[(&str, &str)]) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("○", theme::muted())),
        Line::from(Span::styled(message.to_string(), theme::title())),
    ];
    if !hints.is_empty() {
        lines.push(Line::from(""));
        lines.push(key_hints(hints));
    }
    lines
}

/// A centered failure block: what failed, the plain-language reason from
/// `errors::explain`, then what to do about it.
pub fn failed(headline: &str, reason: &str, hints: &[(&str, &str)]) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            format!("✗ {headline}"),
            Style::default()
                .fg(theme::ERROR)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(reason.to_string()),
    ];
    if !hints.is_empty() {
        lines.push(Line::from(""));
        lines.push(key_hints(hints));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> String {
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
    fn empty_and_failed_states_carry_their_next_step() {
        let empty = text(&empty("No runs yet", &[("r", "reload")]));
        assert!(empty.contains("No runs yet") && empty.contains("r reload"));
        assert_eq!(
            super::empty("Nothing", &[]).len(),
            2,
            "no hint row without hints"
        );
        let failed = text(&failed(
            "Couldn't load runs",
            "The etl service had an error.",
            &[],
        ));
        assert!(failed.starts_with("✗ Couldn't load runs"));
        assert!(failed.ends_with("The etl service had an error."));
    }

    #[test]
    fn loading_names_what_it_waits_for() {
        let line = loading("warehouses", 3);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, format!("{} Loading warehouses…", theme::SPINNER[3]));
    }
}
