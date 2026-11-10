//! Drawing subagents: `● Task(name · profile)` with `⎿`
//! lines under it. [`live_group`] is the in-progress block (one row per
//! running subagent), [`finished_lines`] the item printed once it ends, and
//! [`compact_line`] the one-liner for plain streams (`gz chat ask`).

use std::time::Instant;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::code_ui::items::fit;
use crate::code_ui::usage::{compact, duration};
use crate::subagents::{family_order, Board, RunsOn, Status, Subagent, ToolState};
use crate::tui::{markdown, theme};

const TREE: &str = "  ⎿  ";
const NESTED: &str = "     ";
/// One level of a subagent started by another subagent.
const CHILD_INDENT: &str = "  ";
/// Where the full report is.
const PAGE_HINT: &str = " · shift+↓ to view";

/// `Task` for chat subagents, `Agent` for builder agents called as tools,
/// `Step` for workflow nodes.
pub fn verb(agent: &Subagent) -> &'static str {
    match (&agent.node_id, &agent.agent_id) {
        (Some(_), _) => "Step",
        (None, Some(_)) => "Agent",
        (None, None) => "Task",
    }
}

/// What goes in the parentheses: `name · profile · model · context`.
pub fn target(agent: &Subagent) -> String {
    describe(&agent.name, agent.profile.as_deref(), &agent.runs_on)
}

/// `Check logs · analyst · Claude Haiku 5.5 · 200K`, leaving out what
/// isn't known.
pub fn describe(name: &str, profile: Option<&str>, runs_on: &RunsOn) -> String {
    std::iter::once(name.to_string())
        .chain(profile.map(str::to_string))
        .chain(runs_on.describe())
        .collect::<Vec<_>>()
        .join(" · ")
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
        Status::Cancelled | Status::Skipped | Status::Waiting => Style::default().fg(theme::WARN),
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

/// Shifts a line of a subagent another subagent started under its parent.
fn under_parent(mut line: Line<'static>, depth: usize) -> Line<'static> {
    if depth > 0 {
        line.spans.insert(0, Span::raw(CHILD_INDENT.repeat(depth)));
    }
    line
}

fn room_at(depth: usize, width: usize) -> usize {
    width.saturating_sub(depth * CHILD_INDENT.width())
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
/// current tool underneath, children indented under the subagent that
/// started them, at most `max_rows` of them plus `+N more`.
pub fn live_group(
    board: &Board,
    now: Instant,
    tick: u64,
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let running: Vec<&Subagent> = board.running().collect();
    let rows = family_order(&running).into_iter().map(|i| running[i]);
    let mut lines = Vec::new();
    for (i, agent) in rows.take(max_rows).enumerate() {
        let width = room_at(agent.depth, width);
        // Offset each spinner so parallel rows don't tick in lockstep.
        let frame = (tick as usize + i * 3) % theme::SPINNER.len();
        let spinner = Span::styled(format!("{} ", theme::SPINNER[frame]), theme::brand());
        let mut row = head(agent, spinner, width);
        row.spans.push(Span::styled(
            format!("  {}", duration(agent.elapsed(now))),
            theme::muted(),
        ));
        lines.push(under_parent(row, agent.depth));
        let doing = agent
            .current_tool()
            .map(str::to_string)
            .unwrap_or_else(|| "thinking…".to_string());
        lines.push(under_parent(
            tree(&doing, width, theme::muted()),
            agent.depth,
        ));
    }
    if running.len() > max_rows {
        lines.push(Line::from(Span::styled(
            format!("  +{} more running", running.len() - max_rows),
            theme::muted(),
        )));
    }
    lines
}

/// Background subagents waiting for a free slot (`/subagents parallel`),
/// one muted row each under the running ones; `targets` come from
/// [`describe`].
pub fn queued_group(targets: &[String], width: usize) -> Vec<Line<'static>> {
    targets
        .iter()
        .map(|target| {
            let room = width.saturating_sub("◦ Task()  queued".width());
            Line::from(vec![
                Span::styled("◦ ", theme::muted()),
                Span::styled("Task", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("({})", fit(target, room))),
                Span::styled("  queued", theme::muted()),
            ])
        })
        .collect()
}

/// The printed item for a finished subagent: header and a `⎿` status line
/// with its stats. The report lives on the subagent's own page; `expanded`
/// (the Ctrl+O transcript) adds every tool call, the progress text and the
/// report. One a subagent started is indented by its depth.
pub fn finished_lines(agent: &Subagent, width: usize, expanded: bool) -> Vec<Line<'static>> {
    let width = room_at(agent.depth, width);
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
    if !expanded {
        status.spans.push(Span::styled(PAGE_HINT, theme::muted()));
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
    }
    lines
        .into_iter()
        .map(|line| under_parent(line, agent.depth))
        .collect()
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
/// `⇢ triage → billing (refund question)`: one swarm handoff.
pub fn handoff_line(from: &str, to: &str, reason: Option<&str>) -> String {
    match reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(reason) => format!("⇢ {from} → {to} ({reason})"),
        None => format!("⇢ {from} → {to}"),
    }
}

pub fn compact_line(agent: &Subagent, now: Instant) -> String {
    let mark = match agent.status {
        Status::Running => "…",
        Status::Completed => "✓",
        Status::Failed => "✗",
        Status::Cancelled | Status::Skipped => "–",
        Status::Waiting => "⏸",
    };
    let pad = CHILD_INDENT.repeat(agent.depth);
    if agent.status.is_done() {
        format!(
            "{pad}⚙ {} {mark} {} ({})",
            title(agent),
            agent.status.word(),
            duration(agent.elapsed(now))
        )
    } else {
        format!("{pad}⚙ {} {mark} started", title(agent))
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

    #[test]
    fn rows_name_the_model_and_context_the_server_reports() {
        let now = Instant::now();
        let mut board = Board::default();
        let started = SubagentEvent::parse(
            "subagent_started",
            &serde_json::json!({"subagentId": "sa_1", "name": "Check logs", "profile": "analyst",
                "model": "haiku55", "modelLabel": "Claude Haiku 5.5", "reasoningEffort": "low", "contextTokens": 200000}),
        )
        .unwrap();
        board.apply(&started, now);
        let agent = board.get("sa_1").unwrap();
        let expected = "Task(Check logs · analyst · Claude Haiku 5.5 · 200K)";
        assert_eq!(compact_line(agent, now), format!("⚙ {expected} … started"));
        assert!(text(&live_group(&board, now, 0, 100, 4))[0].contains(expected));
        assert!(text(&finished_lines(agent, 100, false))[0].contains(expected));
        // The chat's own model (no mode) still shows its label; with only a
        // mode, the mode.
        let main_model = SubagentEvent::parse(
            "subagent_started",
            &serde_json::json!({"subagentId": "sa_2", "name": "Read", "model": null, "modelLabel": "Claude Opus 5", "contextTokens": null}),
        )
        .unwrap();
        board.apply(&main_model, now);
        assert_eq!(
            title(board.get("sa_2").unwrap()),
            "Task(Read · Claude Opus 5)"
        );
        let mode_only = RunsOn {
            model: Some("haiku55".into()),
            context_tokens: Some(1_500_000),
            ..RunsOn::default()
        };
        assert_eq!(describe("Scan", None, &mode_only), "Scan · haiku55 · 1.5M");
        assert_eq!(describe("Scan", None, &RunsOn::default()), "Scan");
    }

    #[test]
    fn queued_rows_say_they_wait() {
        let lines = text(&queued_group(&["Check c · analyst".to_string()], 40));
        assert_eq!(lines, vec!["◦ Task(Check c · analyst)  queued"]);
        let narrow = text(&queued_group(
            &["A very long subagent task name · analyst".to_string()],
            30,
        ));
        assert!(narrow[0].width() <= 30, "{narrow:?}");
    }

    #[test]
    fn handoff_lines_show_the_reason_when_given() {
        assert_eq!(
            handoff_line("triage", "billing", Some("refund question")),
            "⇢ triage → billing (refund question)"
        );
        assert_eq!(handoff_line("a", "b", Some("  ")), "⇢ a → b");
        assert_eq!(handoff_line("a", "b", None), "⇢ a → b");
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
                    task: None,
                    runs_on: Default::default(),
                    parent: None,
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
                tool_use_id: None,
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

    fn start_under(board: &mut Board, id: &str, name: &str, parent: &str, now: Instant) {
        let started = SubagentEvent::parse(
            "subagent_started",
            &serde_json::json!({"subagentId": id, "name": name, "parentSubagentId": parent}),
        )
        .unwrap();
        board.apply(&started, now);
    }

    #[test]
    fn a_child_is_drawn_under_its_parent() {
        let now = Instant::now();
        let mut board = board(now, 2);
        start_under(&mut board, "child", "Check nulls", "sa_0", now);
        let live = text(&live_group(&board, now, 0, 44, 4));
        assert!(live[0].contains("Task(Investigate table 0"), "{live:#?}");
        assert!(live[2].starts_with("  ") && live[2].contains("Task(Check nulls)"));
        assert_eq!(live[3], "    ⎿  thinking…");
        assert!(live[4].contains("Task(Investigate table 1"), "{live:#?}");
        let done = board
            .apply(
                &SubagentEvent::Finished {
                    id: "child".into(),
                    status: Status::Completed,
                    summary: "No nulls in the key columns of any table.".into(),
                    usage: SubagentUsage::default(),
                    duration: Some(Duration::from_secs(5)),
                },
                now,
            )
            .unwrap();
        let printed = text(&finished_lines(&done, 44, false));
        assert_eq!(printed[0], "  ● Task(Check nulls)");
        assert_eq!(printed[1], "    ⎿  Done (5s) · shift+↓ to view");
        let full = text(&finished_lines(&done, 44, true));
        assert!(full.iter().all(|l| l.starts_with("  ")), "{full:#?}");
        assert!(full.iter().all(|l| l.width() <= 44), "{full:#?}");
        assert_eq!(
            compact_line(&done, now),
            "  ⚙ Task(Check nulls) ✓ done (5s)"
        );
    }

    #[test]
    fn a_subagent_with_an_unknown_parent_is_drawn_at_the_top_level() {
        let now = Instant::now();
        let mut board = board(now, 1);
        start_under(&mut board, "stray", "Check nulls", "sa_missing", now);
        let live = text(&live_group(&board, now, 0, 80, 4));
        assert_eq!(live.len(), 4, "{live:#?}");
        assert!(!live[2].starts_with(' ') && live[2].contains("Task(Check nulls)"));
        let stray = board.get("stray").unwrap();
        assert_eq!(compact_line(stray, now), "⚙ Task(Check nulls) … started");
        assert_eq!(
            text(&finished_lines(stray, 80, false))[0],
            "● Task(Check nulls)"
        );
    }

    #[test]
    fn finished_item_is_compact_and_points_to_its_page() {
        let now = Instant::now();
        let mut board = board(now, 1);
        board.apply(
            &SubagentEvent::Tool {
                id: "sa_0".into(),
                tool: "run_sql".into(),
                state: ToolState::Finished,
                tool_use_id: None,
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
        assert!(compact_view[1].contains("shift+↓ to view"));
        assert_eq!(
            compact_view.len(),
            2,
            "the report lives on the subagent's page"
        );
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
                task: None,
                runs_on: Default::default(),
                parent: None,
            },
            now,
        );
        assert!(fresh.is_none());
        assert_eq!(title(board.get("n").unwrap()), "Agent(Extractor)");
    }
}
