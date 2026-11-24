//! The model's reasoning summary while it streams and after: one dim,
//! folded line with the time spent (`✻ Thinking… 4s`, then `✻ Thought for
//! 4s`), and the full text in the Ctrl+O transcript. Reasoning is shown
//! only: it is never sent back to the model, copied or exported.

use std::time::{Duration, Instant};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::code_ui::items::{Item, EXPAND_HINT};
use crate::tui::{markdown, theme};

const MARK: &str = "✻ ";
const INDENT: &str = "  ";

/// Reasoning arriving now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveThinking {
    text: String,
    started: Instant,
}

impl LiveThinking {
    pub fn new(started: Instant) -> Self {
        Self {
            text: String::new(),
            started,
        }
    }

    pub fn push(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// How it shows at `now`: still thinking.
    pub fn item(&self, now: Instant) -> Item {
        Item::Thinking {
            text: self.text.clone(),
            secs: secs(now.saturating_duration_since(self.started)),
            live: true,
        }
    }

    /// The finished item, with the time it took up to `now`.
    pub fn finish(self, now: Instant) -> Item {
        Item::Thinking {
            text: self.text,
            secs: secs(now.saturating_duration_since(self.started)),
            live: false,
        }
    }
}

fn secs(elapsed: Duration) -> u64 {
    elapsed.as_secs()
}

fn heading(secs: u64, live: bool) -> String {
    let spent = seconds(secs);
    if live {
        format!("{MARK}Thinking… {spent}")
    } else {
        format!("{MARK}Thought for {spent}")
    }
}

/// Whole seconds spent thinking, as `4s`; under one second reads `<1s`,
/// never `0s`.
pub fn seconds(secs: u64) -> String {
    match secs {
        0 => "<1s".to_string(),
        s => format!("{s}s"),
    }
}

/// The folded line: `✻ Thinking… 4s` while it streams, `✻ Thought for 4s`
/// after, with the expand hint when there is text to read.
pub fn folded(text: &str, secs: u64, live: bool) -> Vec<Line<'static>> {
    let what = heading(secs, live);
    let mut spans = vec![Span::styled(what, dim())];
    if !text.trim().is_empty() {
        spans.push(Span::styled(EXPAND_HINT, theme::muted()));
    }
    vec![Line::from(spans)]
}

/// The heading line and the whole reasoning, dim and italic, for the
/// transcript.
pub fn expanded(text: &str, secs: u64, live: bool, width: usize) -> Vec<Line<'static>> {
    let what = heading(secs, live);
    let mut lines = vec![Line::from(Span::styled(what, dim()))];
    let body = markdown::render(text, width.saturating_sub(INDENT.len()));
    lines.extend(body.into_iter().map(|line| {
        let mut spans = vec![Span::raw(INDENT)];
        spans.extend(line.spans.into_iter().map(|span| {
            let style = span.style.patch(dim());
            Span::styled(span.content, style)
        }));
        Line::from(spans)
    }));
    lines
}

fn dim() -> Style {
    theme::muted().add_modifier(Modifier::ITALIC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn live_thinking_shows_elapsed_time_folded() {
        let start = Instant::now();
        let mut live = LiveThinking::new(start);
        live.push("Checking the ");
        live.push("schema first.");
        let item = live.item(start + Duration::from_secs(3));
        assert_eq!(
            text(&item.render(60)),
            ["✻ Thinking… 3s (ctrl+o to expand)", ""]
        );
        let done = live.finish(start + Duration::from_secs(7));
        assert_eq!(
            text(&done.render(60)),
            ["✻ Thought for 7s (ctrl+o to expand)", ""]
        );
        let lines = done.render(60);
        assert_eq!(lines[0].spans[0].style, dim());
    }

    #[test]
    fn the_transcript_shows_the_whole_reasoning_dimmed() {
        let item = Item::Thinking {
            text: "First **look** at the table.\n\nThen count rows.".into(),
            secs: 2,
            live: false,
        };
        let lines = item.render_expanded(40);
        let out = text(&lines);
        assert_eq!(out[0], "✻ Thought for 2s");
        assert_eq!(out[1], "  First look at the table.");
        assert_eq!(out[3], "  Then count rows.");
        let look = lines[1].spans.iter().find(|s| s.content == "look").unwrap();
        assert!(look.style.add_modifier.contains(Modifier::BOLD));
        assert!(look.style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(look.style.fg, Some(theme::MUTED));
        for width in [40, 80] {
            let long = Item::Thinking {
                text: "word ".repeat(80),
                secs: 1,
                live: false,
            };
            assert!(text(&long.render_expanded(width))
                .iter()
                .all(|l| l.width() <= width));
        }
    }

    #[test]
    fn empty_reasoning_has_no_expand_hint() {
        assert_eq!(text(&folded("", 0, true)), ["✻ Thinking… <1s"]);
    }

    #[test]
    fn a_short_thought_reads_under_a_second_not_zero() {
        assert_eq!(
            text(&folded("quick check", 0, false)),
            ["✻ Thought for <1s (ctrl+o to expand)"]
        );
        assert_eq!(
            text(&expanded("quick check", 0, true, 80))[0],
            "✻ Thinking… <1s"
        );
    }
}
