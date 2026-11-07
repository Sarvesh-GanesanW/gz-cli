//! Drawing subagents the Claude Code way: `● Task(name · profile)` with `⎿`
//! lines under it. [`live_group`] is the in-progress block (one row per
//! running subagent), [`finished_lines`] the item printed once it ends, and
//! [`compact_line`] the one-liner for plain streams (`gz chat ask`).

use std::time::Instant;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::code_ui::items::{fit, EXPAND_HINT};
use crate::code_ui::usage::{compact, duration};
use crate::subagents::{Board, Status, Subagent, ToolState};
use crate::tui::{chat, markdown, theme};

const TREE: &str = "  ⎿  ";
const NESTED: &str = "     ";
/// Summary lines shown before folding behind the transcript.
const SUMMARY_ROWS: usize = 3;

/// `Task` for chat subagents, `Agent` for builder agents called as tools,
/// `Step` for workflow nodes.
pub fn verb(agent: &Subagent) -> &'static str {
    match (&agent.node_id, &agent.agent_id) {
        (Some(_), _) => "Step",
        (None, Some(_)) => "Agent",
        (None, None) => "Task",
    }
}

/// What goes in the parentheses: `name · profile`.
pub fn target(agent: &Subagent) -> String {
    match &agent.profile {
        Some(profile) => format!("{} · {profile}", agent.name),
        None => agent.name.clone(),
    }
}

/// `Task(Check logs · data_engineer)`, plain text.
pub fn title(agent: &Subagent) -> String {
    format!("{}({})", verb(agent), target(agent))
}

fn status_style(status: Status) -> Style {
    match status {
        Status::Running => theme::muted(),
        Status::Completed => Style::default().fg(theme::OK),
        Status::Failed => Style::default().fg(theme::ERROR),
        Status::Cancelled | Status::Skipped => Style::default().fg(theme::WARN),
    }
}

fn head(agent: &Subagent, bullet: Span<'static>, width: usize) -> Line<'static> {
    let verb = verb(agent);
    let room = width.saturating_sub(verb.width() + 4);
    Line::from(vec![
        bullet,
        Span::styled(verb, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!("({})", fit(&target(agent), room))),
    ])
}

fn tree(text: &str, width: usize, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(TREE, theme::muted()),
        Span::styled(fit(text, width.saturating_sub(TREE.width())), style),
    ])
}

/// `12s · 3 tools · ↑ 1.2k ↓ 300 · $0.0021`.
pub fn stats(agent: &Subagent, now: Instant) -> String {
    let mut parts = vec![duration(agent.elapsed(now))];
    if !agent.tools.is_empty() {
        let n = agent.tools.len();
        parts.push(format!("{n} tool{}", if n == 1 { "" } else { "s" }));
    }
    let usage = agent.usage;
    if usage.input_tokens + usage.output_tokens > 0 {
        parts.push(format!(
            "↑ {} ↓ {}",
            compact(usage.input_tokens),
            compact(usage.output_tokens)
        ));
    }
    if let Some(cost) = usage.cost_usd {
        parts.push(format!("${cost:.4}"));
    }
    parts.join(" · ")
}

/// The in-progress block: one spinner row per running subagent with its
/// current tool underneath, at most `max_rows` of them plus `+N more`.
pub fn live_group(
    board: &Board,
    now: Instant,
    tick: u64,
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let running: Vec<&Subagent> = board.running().collect();
    let mut lines = Vec::new();
    for (i, agent) in running.iter().take(max_rows).enumerate() {
        // Offset each spinner so parallel rows don't tick in lockstep.
        let frame = (tick as usize + i * 3) % theme::SPINNER.len();
        let spinner = Span::styled(format!("{} ", theme::SPINNER[frame]), theme::brand());
        let mut row = head(agent, spinner, width);
        row.spans.push(Span::styled(
            format!("  {}", duration(agent.elapsed(now))),
            theme::muted(),
        ));
        lines.push(row);
        let doing = agent
            .current_tool()
            .map(str::to_string)
            .unwrap_or_else(|| "thinking…".to_string());
        lines.push(tree(&doing, width, theme::muted()));
    }
    if running.len() > max_rows {
        lines.push(Line::from(Span::styled(
            format!("  +{} more running", running.len() - max_rows),
            theme::muted(),
        )));
    }
    lines
}

/// Whether the compact view hides anything the transcript would show.
pub fn hides_detail(agent: &Subagent) -> bool {
    !agent.text.trim().is_empty()
        || !agent.tools.is_empty()
        || agent.summary.trim().lines().count() > SUMMARY_ROWS
}

/// The printed item for a finished subagent: header, a `⎿` status line with
/// its stats, then the summary (folded to a few lines unless `expanded`,
/// where every tool call and the progress text follow too).
pub fn finished_lines(agent: &Subagent, width: usize, expanded: bool) -> Vec<Line<'static>> {
    let style = status_style(agent.status);
    let mut lines = vec![head(agent, Span::styled("● ", style), width)];
    let word = match agent.status {
        Status::Completed => "Done".to_string(),
        other => capitalize(other.word()),
    };
    let mut status = tree(
        // Finished subagents carry their duration; a running one (shown in
        // the transcript) counts up to now.
        &format!("{word} ({})", stats(agent, Instant::now())),
        width,
        style,
    );
    if !expanded && hides_detail(agent) {
        status.spans.push(Span::styled(EXPAND_HINT, theme::muted()));
    }
    lines.push(status);
    let room = width.saturating_sub(NESTED.width()).max(10);
    let summary = agent.summary.trim();
    if expanded {
        for call in &agent.tools {
            let mark = match call.state {
                ToolState::Started => "…",
                ToolState::Finished => "✓",
                ToolState::Failed => "✗",
            };
            lines.push(nested(
                &format!("{mark} {}", call.name),
                room,
                theme::muted(),
            ));
        }
        if !agent.text.trim().is_empty() {
            for line in markdown::render(agent.text.trim(), room) {
                lines.push(indent(line));
            }
        }
        if !summary.is_empty() {
            lines.push(nested("Summary", room, theme::title()));
            for line in markdown::render(summary, room) {
                lines.push(indent(line));
            }
        }
    } else {
        let wrapped = chat::wrap(summary, room);
        for row in wrapped.iter().take(SUMMARY_ROWS) {
            lines.push(nested(row, room, theme::muted()));
        }
    }
    lines
}

fn nested(text: &str, room: usize, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::raw(NESTED),
        Span::styled(fit(text, room), style),
    ])
}

fn indent(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw(NESTED)];
    spans.extend(line.spans);
    Line::from(spans)
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// One plain line for streams that print as they go:
/// `⚙ Task(Check logs · data_engineer) ✓ done (12s)`.
pub fn compact_line(agent: &Subagent, now: Instant) -> String {
    let mark = match agent.status {
        Status::Running => "…",
        Status::Completed => "✓",
        Status::Failed => "✗",
        Status::Cancelled | Status::Skipped => "–",
    };
    if agent.status.is_done() {
        format!(
            "⚙ {} {mark} {} ({})",
            title(agent),
            agent.status.word(),
            duration(agent.elapsed(now))
        )
    } else {
        format!("⚙ {} {mark} started", title(agent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagents::{SubagentEvent, SubagentUsage};
    use std::time::Duration;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn board(now: Instant, count: usize) -> Board {
        let mut board = Board::default();
        for i in 0..count {
            board.apply(
                &SubagentEvent::Started {
                    id: format!("sa_{i}"),
                    name: format!("Investigate table {i}"),
                    profile: Some("analyst".into()),
                    agent_id: None,
                },
                now,
            );
        }
        board
    }

    #[test]
    fn live_group_caps_rows_and_counts_the_rest() {
        let now = Instant::now();
        let mut board = board(now, 6);
        board.apply(
            &SubagentEvent::Tool {
                id: "sa_0".into(),
                tool: "run_sql".into(),
                state: ToolState::Started,
            },
            now,
        );
        for width in [80, 120] {
            let lines = text(&live_group(
                &board,
                now + Duration::from_secs(7),
                0,
                width,
                4,
            ));
            assert_eq!(lines.len(), 4 * 2 + 1, "{lines:#?}");
            assert!(lines[0].contains("Task(Investigate table 0 · analyst)"));
            assert!(lines[0].ends_with("7s"));
            assert!(lines[1].contains("run_sql"));
            assert!(lines[3].contains("thinking…"));
            assert_eq!(lines[8].trim(), "+2 more running");
            assert!(lines.iter().all(|l| l.width() <= width));
        }
    }

    #[test]
    fn finished_item_shows_status_stats_and_folded_summary() {
        let now = Instant::now();
        let mut board = board(now, 1);
        board.apply(
            &SubagentEvent::Tool {
                id: "sa_0".into(),
                tool: "run_sql".into(),
                state: ToolState::Finished,
            },
            now,
        );
        let agent = board
            .apply(
                &SubagentEvent::Finished {
                    id: "sa_0".into(),
                    status: Status::Completed,
                    summary: "one\ntwo\nthree\nfour".into(),
                    usage: SubagentUsage {
                        input_tokens: 1200,
                        output_tokens: 300,
                        cost_usd: Some(0.0021),
                    },
                    duration: Some(Duration::from_secs(12)),
                },
                now,
            )
            .unwrap();
        let compact_view = text(&finished_lines(&agent, 80, false));
        assert_eq!(compact_view[0], "● Task(Investigate table 0 · analyst)");
        assert!(compact_view[1].contains("Done (12s · 1 tool · ↑ 1.2k ↓ 300 · $0.0021)"));
        assert!(compact_view[1].contains("ctrl+o"));
        assert_eq!(compact_view.len(), 2 + 3, "summary folds to three lines");
        let full = text(&finished_lines(&agent, 80, true));
        assert!(full.iter().any(|l| l.contains("✓ run_sql")));
        assert!(full.iter().any(|l| l.contains("four")));
    }

    #[test]
    fn failures_render_red_and_compact_lines_read_well() {
        let now = Instant::now();
        let mut board = board(now, 1);
        let agent = board
            .apply(
                &SubagentEvent::Finished {
                    id: "sa_0".into(),
                    status: Status::Failed,
                    summary: "budget exhausted".into(),
                    usage: SubagentUsage::default(),
                    duration: Some(Duration::from_secs(4)),
                },
                now,
            )
            .unwrap();
        let lines = finished_lines(&agent, 80, false);
        assert_eq!(lines[0].spans[0].style.fg, Some(theme::ERROR));
        assert!(text(&lines)[1].contains("Failed (4s)"));
        assert_eq!(
            compact_line(&agent, now),
            "⚙ Task(Investigate table 0 · analyst) ✗ failed (4s)"
        );
        let fresh = board.apply(
            &SubagentEvent::Started {
                id: "n".into(),
                name: "Extractor".into(),
                profile: None,
                agent_id: Some("a1".into()),
            },
            now,
        );
        assert!(fresh.is_none());
        assert_eq!(title(board.get("n").unwrap()), "Agent(Extractor)");
    }
}
