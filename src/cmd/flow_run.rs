//! Flow runs (`/v1/flows/{id}/runs/...` in the agent builder): decoding the
//! engine's `orchestration.*` events, folding them into per-node state, and
//! showing that state live.
//!
//! [`FlowEvent::parse`] decodes one SSE event; [`Board`] is pure state, so the
//! display logic is unit-testable; [`follow_segment`] streams one run segment
//! (a run pauses for approvals and continues as a new segment).

use anyhow::Result;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use serde_json::Value;
use std::io::IsTerminal;
use std::time::Duration;

use crate::cmd::stream;
use crate::cmd::Runtime;
use crate::output::OutputFormat;
use crate::sse::{Event, Flow};

const AGENTS: &str = "agents";

/// One flow-run event, decoded. Payload fields as the engine emits them
/// (gz-agent-builder orchestration/engine.py, supervisor.py, swarm.py).
#[derive(Debug, Clone, PartialEq)]
pub enum FlowEvent {
    NodeStarted {
        node_id: String,
        execution: Option<String>,
        member: Option<String>,
        tool: Option<String>,
        attempt: u64,
    },
    NodeTool {
        node_id: String,
        tool: String,
        running: bool,
    },
    NodeCompleted {
        node_id: String,
        execution: Option<String>,
        duration_ms: Option<u64>,
        cost_usd: Option<f64>,
        preview: String,
    },
    NodeFailed {
        node_id: String,
        execution: Option<String>,
        message: String,
        will_retry: bool,
    },
    NodeSkipped {
        node_id: String,
        reason: Option<String>,
    },
    /// A wait step sleeping, or a sensor polling, until `until`.
    NodeWaiting {
        node_id: String,
        until: Option<String>,
    },
    /// A team supervisor asked one of its members.
    MemberCall {
        member: String,
        task: String,
    },
    /// A handoff-mode team member passed the task on.
    Handoff {
        from: String,
        to: String,
        message: String,
    },
    /// The run paused for these approvals.
    Interrupted(Vec<Value>),
    Totals {
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: Option<f64>,
    },
    BudgetExceeded(String),
    /// The run (segment) ended: completed, failed, cancelled, or interrupted.
    Finished {
        status: String,
        result: Option<Value>,
        error: Option<String>,
    },
    Reconnect,
    Done,
    Other,
}

const RUN_STATUSES: &[&str] = &["completed", "failed", "cancelled", "interrupted"];

fn money(value: &Value) -> Option<f64> {
    match value {
        Value::String(text) => text.parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

impl FlowEvent {
    pub fn parse(event: &Event) -> Self {
        if event.is_done() {
            return Self::Done;
        }
        let data = event.json().unwrap_or(Value::Null);
        let kind = data["type"]
            .as_str()
            .or(event.event.as_deref())
            .unwrap_or("message");
        let text = |key: &str| data[key].as_str().map(str::to_string);
        let node = || text("nodeId").unwrap_or_default();
        match kind {
            "orchestration.node.started" => Self::NodeStarted {
                node_id: node(),
                execution: text("executionId"),
                member: text("memberKey"),
                tool: text("toolName"),
                attempt: data["attempt"].as_u64().unwrap_or(1),
            },
            "orchestration.node.tool" => Self::NodeTool {
                node_id: node(),
                tool: text("toolName").unwrap_or_else(|| "tool".to_string()),
                running: data["status"].as_str().is_none_or(|s| s == "started"),
            },
            "orchestration.node.completed" => Self::NodeCompleted {
                node_id: node(),
                execution: text("executionId"),
                duration_ms: data["durationMs"].as_u64(),
                cost_usd: money(&data["totalCostUsd"]),
                preview: text("outputPreview").unwrap_or_default(),
            },
            "orchestration.node.failed" => Self::NodeFailed {
                node_id: node(),
                execution: text("executionId"),
                message: data["error"]["message"]
                    .as_str()
                    .or(data["error"]["code"].as_str())
                    .unwrap_or("failed")
                    .to_string(),
                will_retry: data["willRetry"].as_bool().unwrap_or(false),
            },
            "orchestration.node.skipped" => Self::NodeSkipped {
                node_id: node(),
                reason: text("reason"),
            },
            "orchestration.node.waiting" | "orchestration.node.parked" => Self::NodeWaiting {
                node_id: node(),
                until: text("wakeAt").or_else(|| text("nextPollAt")),
            },
            "orchestration.member_call" => Self::MemberCall {
                member: text("memberKey").unwrap_or_default(),
                task: text("task").unwrap_or_default(),
            },
            "orchestration.handoff" => Self::Handoff {
                from: text("fromMember").unwrap_or_default(),
                to: text("toMember").unwrap_or_default(),
                message: text("message").unwrap_or_default(),
            },
            "orchestration.interrupted" => {
                Self::Interrupted(data["interrupts"].as_array().cloned().unwrap_or_default())
            }
            "orchestration.totals" => Self::Totals {
                input_tokens: data["inputTokens"].as_u64().unwrap_or(0),
                output_tokens: data["outputTokens"].as_u64().unwrap_or(0),
                cost_usd: money(&data["totalCostUsd"]),
            },
            "orchestration.budget.exceeded" => Self::BudgetExceeded(stream::message(&data)),
            "reconnect" => Self::Reconnect,
            status if RUN_STATUSES.contains(&status) => Self::Finished {
                status: status.to_string(),
                result: data.get("result").cloned().filter(|r| !r.is_null()),
                error: text("message").or_else(|| text("error")),
            },
            _ => Self::Other,
        }
    }
}

/// How a node ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    Completed,
    Failed,
    Skipped,
}

/// Where a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Pending,
    Running,
    Waiting,
    Paused,
    Done(Finish),
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeState {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub phase: Phase,
    /// The tool running right now, or what a waiting step waits for.
    pub detail: Option<String>,
    /// The team member working now (agent.team nodes).
    pub member: Option<String>,
    pub duration_ms: Option<u64>,
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
    /// Executions of team members, which report under the team's step id;
    /// their completion is not the step's.
    member_executions: Vec<String>,
}

impl NodeState {
    fn new(id: &str, title: &str, kind: &str) -> Self {
        Self {
            id: id.to_string(),
            title: title.to_string(),
            kind: kind.to_string(),
            phase: Phase::Pending,
            detail: None,
            member: None,
            duration_ms: None,
            cost_usd: None,
            error: None,
            member_executions: Vec::new(),
        }
    }

    fn is_member_execution(&self, execution: &Option<String>) -> bool {
        execution
            .as_ref()
            .is_some_and(|id| self.member_executions.contains(id))
    }
}

/// Run state folded from flow events. Pure: no I/O.
#[derive(Debug, Default)]
pub struct Board {
    pub nodes: Vec<NodeState>,
    pub totals: Option<(u64, u64, Option<f64>)>,
}

impl Board {
    /// A board seeded with a flow definition's steps, all pending.
    pub fn for_flow(flow: &Value) -> Self {
        let nodes = flow["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|node| {
                let id = node["id"].as_str()?;
                let title = node["title"].as_str().unwrap_or(id);
                Some(NodeState::new(
                    id,
                    title,
                    node["type"].as_str().unwrap_or_default(),
                ))
            })
            .collect();
        Self {
            nodes,
            totals: None,
        }
    }

    fn node_mut(&mut self, node_id: &str) -> &mut NodeState {
        if let Some(index) = self.nodes.iter().position(|n| n.id == node_id) {
            return &mut self.nodes[index];
        }
        // Engine-made steps (e.g. `$synthesize`) aren't in the definition.
        self.nodes
            .push(NodeState::new(node_id, node_id.trim_start_matches('$'), ""));
        self.nodes.last_mut().expect("just pushed")
    }

    /// The team node whose members are working: the running agent.team.
    fn running_team(&mut self) -> Option<&mut NodeState> {
        self.nodes
            .iter_mut()
            .find(|n| n.kind == "agent.team" && n.phase == Phase::Running)
    }

    /// Applies one event; returns the id of the node it changed, if any.
    pub fn apply(&mut self, event: &FlowEvent) -> Option<String> {
        match event {
            FlowEvent::NodeStarted {
                node_id,
                execution,
                member,
                tool,
                ..
            } => {
                let node = self.node_mut(node_id);
                node.phase = Phase::Running;
                node.error = None;
                if let Some(member) = member {
                    node.member = Some(member.clone());
                    node.member_executions.extend(execution.clone());
                }
                node.detail = tool.clone();
                Some(node_id.clone())
            }
            FlowEvent::NodeTool {
                node_id,
                tool,
                running,
            } => {
                let node = self.node_mut(node_id);
                node.detail = running.then(|| tool.clone());
                Some(node_id.clone())
            }
            FlowEvent::NodeCompleted {
                node_id,
                execution,
                duration_ms,
                cost_usd,
                ..
            } => {
                let node = self.node_mut(node_id);
                // A team member's execution finishing isn't the team finishing.
                if node.is_member_execution(execution) {
                    node.detail = None;
                    return Some(node_id.clone());
                }
                node.phase = Phase::Done(Finish::Completed);
                node.duration_ms = *duration_ms;
                node.cost_usd = *cost_usd;
                node.detail = None;
                node.member = None;
                Some(node_id.clone())
            }
            FlowEvent::NodeFailed {
                node_id,
                execution,
                message,
                will_retry,
            } => {
                let node = self.node_mut(node_id);
                node.error = Some(message.clone());
                node.detail = None;
                if !will_retry && !node.is_member_execution(execution) {
                    node.phase = Phase::Done(Finish::Failed);
                }
                Some(node_id.clone())
            }
            FlowEvent::NodeSkipped { node_id, .. } => {
                let node = self.node_mut(node_id);
                node.phase = Phase::Done(Finish::Skipped);
                Some(node_id.clone())
            }
            FlowEvent::NodeWaiting { node_id, until } => {
                let node = self.node_mut(node_id);
                node.phase = Phase::Waiting;
                node.detail = until.as_ref().map(|t| format!("until {t}"));
                Some(node_id.clone())
            }
            FlowEvent::MemberCall { member, .. } => {
                let team = self.running_team()?;
                team.member = Some(member.clone());
                team.detail = None;
                Some(team.id.clone())
            }
            FlowEvent::Handoff { to, .. } => {
                let team = self.running_team()?;
                team.member = Some(to.clone());
                team.detail = None;
                Some(team.id.clone())
            }
            FlowEvent::Interrupted(interrupts) => {
                for interrupt in interrupts {
                    if let Some(node_id) = interrupt["nodeId"].as_str() {
                        self.node_mut(node_id).phase = Phase::Paused;
                    }
                }
                None
            }
            FlowEvent::Totals {
                input_tokens,
                output_tokens,
                cost_usd,
            } => {
                self.totals = Some((*input_tokens, *output_tokens, *cost_usd));
                None
            }
            _ => None,
        }
    }

    pub fn node(&self, node_id: &str) -> Option<&NodeState> {
        self.nodes.iter().find(|n| n.id == node_id)
    }
}

/// `○ Extract`, `⠋ Review team · billing · get_invoice`, `✓ Extract  12.3s · $0.0021`.
pub fn node_line(node: &NodeState, spinner: &str) -> String {
    match node.phase {
        Phase::Pending => format!("○ {}", node.title),
        Phase::Paused => format!("⏸ {} · waiting for approval", node.title),
        Phase::Running | Phase::Waiting => {
            let mut parts = vec![node.title.clone()];
            parts.extend(node.member.clone());
            parts.extend(node.detail.clone());
            let symbol = if node.phase == Phase::Waiting {
                "◷"
            } else {
                spinner
            };
            format!("{symbol} {}", parts.join(" · "))
        }
        Phase::Done(finish) => {
            let symbol = match finish {
                Finish::Completed => "✓",
                Finish::Failed => "✗",
                Finish::Skipped => "–",
            };
            let mut line = format!("{symbol} {}", node.title);
            let stats = node_stats(node);
            if !stats.is_empty() {
                line.push_str("  ");
                line.push_str(&stats);
            }
            if let (Finish::Failed, Some(error)) = (finish, &node.error) {
                line.push_str(&format!(" · {error}"));
            }
            line
        }
    }
}

fn node_stats(node: &NodeState) -> String {
    let mut parts = Vec::new();
    if let Some(ms) = node.duration_ms {
        parts.push(format_duration(ms));
    }
    if let Some(cost) = node.cost_usd.filter(|c| *c > 0.0) {
        parts.push(format!("${cost:.4}"));
    }
    parts.join(" · ")
}

pub fn format_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{}m {}s", ms / 60_000, ms % 60_000 / 1_000)
    }
}

pub fn compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let cut: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// A printed line per event for logs and non-interactive output; team
/// member calls and handoffs also print above the live board.
pub fn log_line(board: &Board, event: &FlowEvent) -> Option<String> {
    let title = |id: &str| {
        board
            .node(id)
            .map(|n| n.title.clone())
            .unwrap_or_else(|| id.to_string())
    };
    match event {
        FlowEvent::NodeStarted {
            node_id,
            member,
            attempt,
            ..
        } => {
            let retry = if *attempt > 1 {
                format!(" (attempt {attempt})")
            } else {
                String::new()
            };
            Some(match member {
                Some(member) => format!("▶ {} · {member}{retry}", title(node_id)),
                None => format!("▶ {}{retry}", title(node_id)),
            })
        }
        FlowEvent::NodeTool {
            node_id,
            tool,
            running: true,
        } => Some(format!("  ⎿  {} · {tool}", title(node_id))),
        FlowEvent::NodeCompleted { node_id, .. } | FlowEvent::NodeSkipped { node_id, .. } => {
            board.node(node_id).map(|n| node_line(n, ""))
        }
        FlowEvent::NodeFailed {
            node_id,
            message,
            will_retry: true,
            ..
        } => Some(format!("↻ {} failed, retrying: {message}", title(node_id))),
        FlowEvent::NodeFailed { node_id, .. } => board.node(node_id).map(|n| node_line(n, "")),
        FlowEvent::NodeWaiting { node_id, .. } => board.node(node_id).map(|n| node_line(n, "")),
        FlowEvent::MemberCall { member, task } => {
            Some(format!("  ⎿  asks {member}: {}", first_line(task, 80)))
        }
        FlowEvent::Handoff { from, to, message } => {
            let reason = first_line(message, 80);
            Some(if reason.is_empty() {
                format!("⇢ {from} → {to}")
            } else {
                format!("⇢ {from} → {to} ({reason})")
            })
        }
        FlowEvent::BudgetExceeded(why) => Some(format!("⚠ budget exceeded: {why}")),
        _ => None,
    }
}

/// How one run segment ended.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SegmentEnd {
    pub status: Option<String>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub interrupts: Vec<Value>,
}

/// Streams one segment (`/v1/flows/{flow}/runs/{run}/events`) into the board.
/// Prints NDJSON in `-o json`/`yaml`; otherwise a live per-node display on a
/// terminal, or one line per event.
pub async fn follow_segment(
    rt: &Runtime,
    flow_id: &str,
    run_id: &str,
    board: &mut Board,
) -> Result<SegmentEnd> {
    let path = format!(
        "/v1/flows/{}/runs/{}/events",
        crate::cmd::seg(flow_id),
        crate::cmd::seg(run_id)
    );
    let interactive = rt.format == OutputFormat::Table && std::io::stderr().is_terminal();
    let mut live = interactive.then(|| LiveBoard::new(board));
    let mut end = SegmentEnd::default();
    let mut last_id = None;
    stream::read_run(&rt.http, AGENTS, &path, &mut last_id, |event, _| {
        if rt.format != OutputFormat::Table {
            print_ndjson(event)?;
        }
        let parsed = FlowEvent::parse(event);
        let changed = board.apply(&parsed);
        if rt.format == OutputFormat::Table {
            show(&mut live, board, &parsed, changed.as_deref());
        }
        Ok(match parsed {
            FlowEvent::Interrupted(interrupts) => {
                end.interrupts = interrupts;
                Flow::Continue
            }
            FlowEvent::Finished {
                status,
                result,
                error,
            } => {
                end.status = Some(status);
                end.result = result;
                end.error = error;
                Flow::Continue
            }
            FlowEvent::Done => Flow::Stop,
            _ => Flow::Continue,
        })
    })
    .await?;
    if let Some(live) = live {
        live.finish(board);
    }
    Ok(end)
}

fn show(live: &mut Option<LiveBoard>, board: &Board, event: &FlowEvent, changed: Option<&str>) {
    match live {
        Some(live) => {
            if matches!(
                event,
                FlowEvent::MemberCall { .. }
                    | FlowEvent::Handoff { .. }
                    | FlowEvent::BudgetExceeded(_)
            ) {
                if let Some(line) = log_line(board, event) {
                    live.println(&line);
                }
            }
            if let Some(node_id) = changed {
                live.update(board, node_id);
            }
        }
        None => {
            if let Some(line) = log_line(board, event) {
                eprintln!("{}", line.dimmed());
            }
        }
    }
}

fn print_ndjson(event: &Event) -> Result<()> {
    let mut line = event.json().unwrap_or(Value::Null);
    if line.is_object() && line.get("type").is_none() {
        if let Some(kind) = &event.event {
            line["type"] = Value::String(kind.clone());
        }
    }
    if !line.is_null() {
        println!("{}", serde_json::to_string(&line)?);
    }
    Ok(())
}

/// The interactive display: one redrawn line per node.
struct LiveBoard {
    progress: MultiProgress,
    bars: Vec<(String, ProgressBar)>,
}

impl LiveBoard {
    fn new(board: &Board) -> Self {
        let mut live = Self {
            progress: MultiProgress::new(),
            bars: Vec::new(),
        };
        for node in &board.nodes {
            live.add(node);
        }
        live
    }

    fn add(&mut self, node: &NodeState) -> &ProgressBar {
        let bar = self.progress.add(ProgressBar::new_spinner());
        style_for(&bar, node);
        self.bars.push((node.id.clone(), bar));
        &self.bars.last().expect("just pushed").1
    }

    fn update(&mut self, board: &Board, node_id: &str) {
        let Some(node) = board.node(node_id) else {
            return;
        };
        match self.bars.iter().find(|(id, _)| id == node_id) {
            Some((_, bar)) => style_for(bar, node),
            None => {
                self.add(node);
            }
        }
    }

    fn println(&self, line: &str) {
        self.progress.println(line.dimmed().to_string()).ok();
    }

    fn finish(self, board: &Board) {
        for (id, bar) in &self.bars {
            if let Some(node) = board.node(id) {
                bar.set_style(still_style());
                bar.finish_with_message(node_line(node, "•"));
            }
        }
    }
}

/// Running nodes spin; everything else is a still line.
fn style_for(bar: &ProgressBar, node: &NodeState) {
    if node.phase == Phase::Running {
        bar.set_style(running_style());
        bar.set_message(node_line(node, "").trim_start().to_string());
        bar.enable_steady_tick(Duration::from_millis(80));
    } else {
        bar.disable_steady_tick();
        bar.set_style(still_style());
        bar.set_message(node_line(node, ""));
        bar.tick();
    }
}

fn still_style() -> ProgressStyle {
    ProgressStyle::with_template("  {msg}").unwrap_or_else(|_| ProgressStyle::default_spinner())
}

fn running_style() -> ProgressStyle {
    ProgressStyle::with_template("  {spinner:.cyan} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(kind: &str, data: Value) -> Event {
        Event {
            id: Some("1".into()),
            event: Some(kind.into()),
            data: data.to_string(),
        }
    }

    fn feed(board: &mut Board, kind: &str, mut data: Value) -> FlowEvent {
        data["type"] = json!(kind);
        let parsed = FlowEvent::parse(&event(kind, data));
        board.apply(&parsed);
        parsed
    }

    fn flow() -> Board {
        Board::for_flow(&json!({"nodes": [
            {"id": "extract", "type": "agent.invoke", "title": "Extract invoices"},
            {"id": "review", "type": "agent.team", "title": "Review team"},
            {"id": "approve", "type": "people.approval", "title": "Finance sign-off"},
            {"id": "post", "type": "platform.action"},
        ]}))
    }

    #[test]
    fn graph_run_moves_nodes_through_their_phases() {
        let mut board = flow();
        assert_eq!(node_line(&board.nodes[3], "⠋"), "○ post");
        feed(
            &mut board,
            "orchestration.node.started",
            json!({"nodeId": "extract", "attempt": 1}),
        );
        feed(
            &mut board,
            "orchestration.node.tool",
            json!({"nodeId": "extract", "toolName": "run_sql", "status": "started"}),
        );
        assert_eq!(
            node_line(&board.nodes[0], "⠋"),
            "⠋ Extract invoices · run_sql"
        );
        feed(
            &mut board,
            "orchestration.node.completed",
            json!({"nodeId": "extract", "durationMs": 12300, "totalCostUsd": "0.002100"}),
        );
        assert_eq!(
            node_line(&board.nodes[0], ""),
            "✓ Extract invoices  12.3s · $0.0021"
        );
        feed(
            &mut board,
            "orchestration.node.skipped",
            json!({"nodeId": "post", "reason": "edge not taken"}),
        );
        assert_eq!(node_line(&board.nodes[3], ""), "– post");
    }

    #[test]
    fn failures_retry_then_fail_with_the_message() {
        let mut board = flow();
        feed(
            &mut board,
            "orchestration.node.started",
            json!({"nodeId": "extract"}),
        );
        let retry = feed(
            &mut board,
            "orchestration.node.failed",
            json!({"nodeId": "extract", "error": {"code": "Timeout", "message": "took too long"}, "willRetry": true}),
        );
        assert_eq!(board.nodes[0].phase, Phase::Running);
        assert_eq!(
            log_line(&board, &retry).as_deref(),
            Some("↻ Extract invoices failed, retrying: took too long")
        );
        feed(
            &mut board,
            "orchestration.node.failed",
            json!({"nodeId": "extract", "error": {"code": "Timeout"}, "willRetry": false}),
        );
        assert_eq!(
            node_line(&board.nodes[0], ""),
            "✗ Extract invoices · Timeout"
        );
    }

    #[test]
    fn team_members_and_handoffs_show_on_the_team_node() {
        let mut board = flow();
        feed(
            &mut board,
            "orchestration.node.started",
            json!({"nodeId": "review", "executionId": "t1"}),
        );
        let call = feed(
            &mut board,
            "orchestration.member_call",
            json!({"memberKey": "billing", "task": "Check invoice 42\nwith detail"}),
        );
        assert_eq!(node_line(&board.nodes[1], "⠋"), "⠋ Review team · billing");
        assert_eq!(
            log_line(&board, &call).as_deref(),
            Some("  ⎿  asks billing: Check invoice 42")
        );
        let handoff = feed(
            &mut board,
            "orchestration.handoff",
            json!({"fromMember": "billing", "toMember": "refunds", "message": "needs a refund"}),
        );
        assert_eq!(
            log_line(&board, &handoff).as_deref(),
            Some("⇢ billing → refunds (needs a refund)")
        );
        assert_eq!(board.nodes[1].member.as_deref(), Some("refunds"));
        // A member's own execution finishing or failing leaves the team running.
        feed(
            &mut board,
            "orchestration.node.started",
            json!({"nodeId": "review", "executionId": "m1", "memberKey": "refunds"}),
        );
        feed(
            &mut board,
            "orchestration.node.completed",
            json!({"nodeId": "review", "executionId": "m1", "durationMs": 10}),
        );
        assert_eq!(board.nodes[1].phase, Phase::Running);
        feed(
            &mut board,
            "orchestration.node.failed",
            json!({"nodeId": "review", "executionId": "m1", "error": {"message": "x"}}),
        );
        assert_eq!(board.nodes[1].phase, Phase::Running);
        feed(
            &mut board,
            "orchestration.node.completed",
            json!({"nodeId": "review", "executionId": "t1", "durationMs": 4000}),
        );
        assert_eq!(board.nodes[1].phase, Phase::Done(Finish::Completed));
    }

    #[test]
    fn interrupts_pause_their_nodes_and_terminal_events_finish() {
        let mut board = flow();
        feed(
            &mut board,
            "orchestration.node.started",
            json!({"nodeId": "approve"}),
        );
        let paused = feed(
            &mut board,
            "orchestration.interrupted",
            json!({"interrupts": [
                {"id": "orc:1:approval", "name": "orchestration_approval", "nodeId": "approve", "title": "Finance sign-off"}
            ]}),
        );
        assert!(matches!(paused, FlowEvent::Interrupted(ref list) if list.len() == 1));
        assert_eq!(
            node_line(&board.nodes[2], ""),
            "⏸ Finance sign-off · waiting for approval"
        );
        let done = FlowEvent::parse(&event(
            "completed",
            json!({"type": "completed", "runId": "r", "result": {"text": "ok"}}),
        ));
        assert_eq!(
            done,
            FlowEvent::Finished {
                status: "completed".into(),
                result: Some(json!({"text": "ok"})),
                error: None
            }
        );
        let failed = FlowEvent::parse(&event(
            "failed",
            json!({"type": "failed", "error": "Budget", "message": "over budget"}),
        ));
        assert_eq!(
            failed,
            FlowEvent::Finished {
                status: "failed".into(),
                result: None,
                error: Some("over budget".into())
            }
        );
    }

    #[test]
    fn engine_steps_outside_the_definition_are_added() {
        let mut board = flow();
        feed(
            &mut board,
            "orchestration.node.started",
            json!({"nodeId": "$synthesize"}),
        );
        assert_eq!(board.nodes.last().unwrap().title, "synthesize");
    }

    #[test]
    fn waiting_steps_and_totals() {
        let mut board = flow();
        feed(
            &mut board,
            "orchestration.node.waiting",
            json!({"nodeId": "post", "wakeAt": "2026-11-07T12:00:00Z"}),
        );
        assert_eq!(
            node_line(&board.nodes[3], "⠋"),
            "◷ post · until 2026-11-07T12:00:00Z"
        );
        feed(
            &mut board,
            "orchestration.totals",
            json!({"inputTokens": 1200, "outputTokens": 30, "totalCostUsd": "0.01"}),
        );
        assert_eq!(board.totals, Some((1200, 30, Some(0.01))));
    }

    #[test]
    fn unknown_events_and_done() {
        assert_eq!(
            FlowEvent::parse(&event(
                "orchestration.edge",
                json!({"type": "orchestration.edge"})
            )),
            FlowEvent::Other
        );
        let done = Event {
            id: None,
            event: Some("done".into()),
            data: "[DONE]".into(),
        };
        assert_eq!(FlowEvent::parse(&done), FlowEvent::Done);
    }

    #[test]
    fn formats_durations_and_counts() {
        assert_eq!(format_duration(450), "450ms");
        assert_eq!(format_duration(61_000), "1m 1s");
        assert_eq!(compact(1_250_000), "1.2M");
        assert_eq!(compact(999), "999");
    }
}
