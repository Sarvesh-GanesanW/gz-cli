//! Agent playground: type a prompt, watch the agent's answer stream in,
//! then see what it cost and which tools it called.
//!
//! The same two steps as `gz agents invoke` and the agent builder's test
//! pane: start the run in the background (`POST /v1/agents/{id}/preview`
//! for the draft, `/invoke` for the approved version, `{message,
//! background: true}` → `{runId, sessionId}`), then follow
//! `GET /v1/agents/{id}/runs/{runId}/events`. The stream carries only
//! answer text (`llm_chunk`) and a terminal event whose `result` holds
//! usage and cost (agent builder `api.py:1770-1785`,
//! `services/invocations.py:2462-2503`); tool calls are read afterwards
//! from the run's observability record.
//!
//! Draft tests keep no memory (a `sessionId` is refused with 422), so each
//! draft prompt is a fresh run; the approved version continues its session.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
    Frame,
};
use reqwest::Method;
use serde_json::{json, Value};

use super::{Ctx, Handled, RenderCtx, View};
use crate::cmd::seg;
use crate::tui::app::Tone;
use crate::tui::errors::{self, Subject};
use crate::tui::fetch::StreamItem;
use crate::tui::format::{self, Format, TimeUnit};
use crate::tui::help::HelpSection;
use crate::tui::markdown;
use crate::tui::theme;
use crate::tui::ui::widgets::{key_hints, render_scrollbar};

const AGENTS: &str = "agents";
const SUBJECT: Subject = Subject {
    what: "this agent",
    service: AGENTS,
};
/// Shown when a draft test is refused for lack of permission: the agent
/// builder needs Invoke and Modify (creator or admin) for drafts.
const DRAFT_ACCESS_HINT: &str =
    "Draft tests need Invoke and Modify permission (the agent's creator or an admin).";

/// Which version of the agent a prompt runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The current, unapproved definition (`/preview`).
    Draft,
    /// The approved version (`/invoke`), keeping a session between prompts.
    Approved,
}

impl Mode {
    fn endpoint(self) -> &'static str {
        match self {
            Mode::Draft => "preview",
            Mode::Approved => "invoke",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Mode::Draft => "draft",
            Mode::Approved => "approved",
        }
    }
}

/// One tool the run called, from its observability record.
#[derive(Debug, Clone, PartialEq)]
struct ToolUse {
    name: String,
    calls: u64,
    errors: u64,
    total_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
enum TurnState {
    Starting,
    Streaming,
    Done,
    Paused(Vec<Interrupt>),
    Stopped,
    Failed(String),
}

/// A tool call waiting for a human decision.
#[derive(Debug, Clone, PartialEq)]
struct Interrupt {
    id: String,
    tool: String,
    reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Turn {
    prompt: String,
    answer: String,
    state: TurnState,
    /// "4.5 s · 3 steps · 12.4k tokens · $0.0123", once finished.
    footer: Option<String>,
    tools: Option<Vec<ToolUse>>,
    run_id: Option<String>,
}

impl Turn {
    fn new(prompt: String) -> Self {
        Self {
            prompt,
            answer: String::new(),
            state: TurnState::Starting,
            footer: None,
            tools: None,
            run_id: None,
        }
    }
}

/// What the view is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waiting {
    Idle,
    Start(u64),
    Stream(u64),
    Resume(u64),
}

pub struct Playground {
    agent_id: String,
    name: String,
    version: Option<String>,
    can_invoke: bool,
    mode: Mode,
    session_id: Option<String>,
    input: String,
    turns: Vec<Turn>,
    waiting: Waiting,
    /// Observability fetch → index of the turn it belongs to.
    tool_fetches: Vec<(u64, usize)>,
    show_tools: bool,
    scroll: usize,
    viewport: usize,
    line_count: usize,
    /// Follow the newest text unless the user scrolled up.
    follow: bool,
}

impl Playground {
    /// A playground for the agent in `row` (an `/v1/agents` row).
    pub fn new(row: &Value) -> Self {
        let text = |key: &str| row.get(key).and_then(|v| v.as_str()).map(str::to_string);
        Self {
            agent_id: text("id").unwrap_or_default(),
            name: text("name").unwrap_or_else(|| "agent".to_string()),
            version: row.get("version").map(format::text_of),
            can_invoke: row.get("isInvokable").and_then(Value::as_bool) == Some(true),
            mode: Mode::Draft,
            session_id: None,
            input: String::new(),
            turns: Vec::new(),
            waiting: Waiting::Idle,
            tool_fetches: Vec::new(),
            show_tools: false,
            scroll: 0,
            viewport: 1,
            line_count: 0,
            follow: true,
        }
    }

    fn agent_path(&self, rest: &str) -> String {
        format!("/v1/agents/{}/{rest}", seg(&self.agent_id))
    }

    fn current(&mut self) -> Option<&mut Turn> {
        self.turns.last_mut()
    }

    fn is_running(&self) -> bool {
        self.waiting != Waiting::Idle
    }

    fn paused(&self) -> Option<&[Interrupt]> {
        match &self.turns.last()?.state {
            TurnState::Paused(interrupts) if !self.is_running() => Some(interrupts),
            _ => None,
        }
    }

    fn send(&mut self, ctx: &mut Ctx) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() || self.is_running() {
            return;
        }
        if self.agent_id.is_empty() {
            return ctx.toast(Tone::Error, "This row has no agent id");
        }
        self.input.clear();
        let mut body = json!({"message": prompt, "background": true});
        if let (Mode::Approved, Some(session)) = (self.mode, &self.session_id) {
            body["sessionId"] = json!(session);
        }
        let path = self.agent_path(self.mode.endpoint());
        let id = ctx.fetch_raw(AGENTS, Method::POST, path, Some(body));
        self.turns.push(Turn::new(prompt));
        self.waiting = Waiting::Start(id);
        self.follow = true;
    }

    fn started(&mut self, result: Result<Value, String>, ctx: &mut Ctx) {
        let reply = match result {
            Ok(reply) => reply,
            Err(err) => return self.fail(self.explain(&err)),
        };
        let Some(run_id) = reply["runId"].as_str().map(str::to_string) else {
            return self.fail("The agent didn't return a run id.".to_string());
        };
        if self.mode == Mode::Approved {
            self.session_id = reply["sessionId"].as_str().map(str::to_string);
        }
        let events = self.agent_path(&format!("runs/{}/events", seg(&run_id)));
        let stream = ctx.stream(AGENTS, events);
        if let Some(turn) = self.current() {
            turn.run_id = Some(run_id);
            turn.state = TurnState::Streaming;
        }
        self.waiting = Waiting::Stream(stream);
    }

    fn explain(&self, err: &str) -> String {
        let message = errors::explain(err, SUBJECT);
        let denied = message.starts_with("You don't have access");
        if denied && self.mode == Mode::Draft {
            format!("{message} {DRAFT_ACCESS_HINT}")
        } else {
            message
        }
    }

    fn fail(&mut self, message: String) {
        if let Some(turn) = self.current() {
            turn.state = TurnState::Failed(message);
        }
        self.waiting = Waiting::Idle;
    }

    /// Folds one stream event into the current turn.
    fn on_event(&mut self, data: &Value, kind: &str, ctx: &mut Ctx) {
        match kind {
            "llm_chunk" => {
                if let (Some(text), Some(turn)) = (data["text"].as_str(), self.current()) {
                    turn.answer.push_str(text);
                }
            }
            "completed" | "failed" | "cancelled" | "interrupted" => {
                self.finished(kind, data, ctx);
            }
            _ => {}
        }
    }

    /// A terminal event, or the answer to a resume: show the outcome, cost
    /// and pending approvals, then look up the tools the run called.
    fn finished(&mut self, status: &str, data: &Value, ctx: &mut Ctx) {
        if let Waiting::Stream(id) = self.waiting {
            ctx.stop_stream(id);
        }
        self.waiting = Waiting::Idle;
        let result = data.get("result").unwrap_or(data);
        let version = self.version.clone();
        let mode = self.mode;
        let Some(turn) = self.turns.last_mut() else {
            return;
        };
        if turn.answer.trim().is_empty() {
            if let Some(response) = result["response"].as_str() {
                turn.answer = response.to_string();
            }
        }
        turn.footer = Some(footer(result, mode, version.as_deref()));
        let interrupts = interrupts_of(result);
        turn.state = match status {
            "failed" => TurnState::Failed(
                ["message", "error"]
                    .iter()
                    .find_map(|k| data[k].as_str())
                    .unwrap_or("The run failed.")
                    .to_string(),
            ),
            "cancelled" => TurnState::Stopped,
            _ if !interrupts.is_empty() => TurnState::Paused(interrupts),
            _ => TurnState::Done,
        };
        if let Some(run_id) = turn.run_id.clone() {
            let path = self.agent_path(&format!("runs/{}/observability", seg(&run_id)));
            let id = ctx.fetch_raw(AGENTS, Method::GET, path, None);
            self.tool_fetches.push((id, self.turns.len() - 1));
        }
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        let run_id = self.turns.last().and_then(|t| t.run_id.clone());
        if let Waiting::Stream(id) = self.waiting {
            ctx.stop_stream(id);
        }
        if let Some(run_id) = run_id {
            let path = self.agent_path(&format!("runs/{}/cancel", seg(&run_id)));
            ctx.fetch_raw(AGENTS, Method::POST, path, None);
        }
        if let Some(turn) = self.current() {
            turn.state = TurnState::Stopped;
        }
        self.waiting = Waiting::Idle;
        ctx.toast(Tone::Info, "Agent run stopped.");
    }

    /// Answers every pending tool call the same way, once.
    fn decide(&mut self, approve: bool, ctx: &mut Ctx) {
        let Some(interrupts) = self.paused().map(<[Interrupt]>::to_vec) else {
            return;
        };
        let Some(run_id) = self.turns.last().and_then(|t| t.run_id.clone()) else {
            return;
        };
        let decision = if approve { "approved" } else { "denied" };
        let responses: Vec<Value> = interrupts
            .iter()
            .map(|i| json!({"interruptId": i.id, "decision": decision, "remember": "once"}))
            .collect();
        let path = self.agent_path(&format!("runs/{}/resume", seg(&run_id)));
        let id = ctx.fetch_raw(
            AGENTS,
            Method::POST,
            path,
            Some(json!({"interruptResponses": responses})),
        );
        if let Some(turn) = self.current() {
            turn.state = TurnState::Streaming;
        }
        self.waiting = Waiting::Resume(id);
    }

    fn resumed(&mut self, result: Result<Value, String>, ctx: &mut Ctx) {
        match result {
            Ok(reply) => {
                let status = reply["status"].as_str().unwrap_or("completed").to_string();
                if let (Some(run_id), Some(turn)) =
                    (reply["runId"].as_str().map(str::to_string), self.current())
                {
                    turn.run_id = Some(run_id);
                }
                self.finished(&status, &reply, ctx);
            }
            Err(err) => self.fail(self.explain(&err)),
        }
    }

    fn tools_loaded(&mut self, turn: usize, result: Result<Value, String>) {
        // No observability record (404) means no tools were recorded.
        let tools = result
            .ok()
            .map(|record| tools_of(&record))
            .unwrap_or_default();
        if let Some(turn) = self.turns.get_mut(turn) {
            turn.tools = Some(tools);
        }
    }

    fn toggle_mode(&mut self, ctx: &mut Ctx) {
        if self.is_running() {
            return;
        }
        if !self.can_invoke {
            return ctx.toast(
                Tone::Info,
                "No approved version yet · only the draft can be tested",
            );
        }
        self.mode = match self.mode {
            Mode::Draft => Mode::Approved,
            Mode::Approved => Mode::Draft,
        };
        self.session_id = None;
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.line_count.saturating_sub(self.viewport) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max.max(0)) as usize;
        self.follow = self.scroll as isize >= max;
    }

    fn transcript(&self, width: usize, tick: u64) -> Vec<Line<'static>> {
        if self.turns.is_empty() {
            let mut lines = vec![Line::from(Span::styled(
                format!(
                    "Ask {} something. Each prompt runs the agent once.",
                    self.name
                ),
                theme::muted(),
            ))];
            if self.mode == Mode::Draft {
                lines.push(Line::from(Span::styled(
                    "Draft runs never carry conversation memory; each prompt is a fresh test.",
                    theme::muted(),
                )));
            }
            return lines;
        }
        let mut lines = Vec::new();
        for turn in &self.turns {
            for line in format::wrap(&turn.prompt, width.saturating_sub(4).max(1)) {
                lines.push(Line::from(vec![
                    Span::styled("you ", theme::key_hint()),
                    Span::styled(line, theme::title()),
                ]));
            }
            lines.extend(markdown::render(&turn.answer, width));
            lines.extend(self.turn_status(turn, tick));
            lines.extend(self.tool_lines(turn));
            lines.push(Line::from(""));
        }
        lines
    }

    fn turn_status(&self, turn: &Turn, tick: u64) -> Vec<Line<'static>> {
        let spinner = theme::SPINNER[(tick as usize) % theme::SPINNER.len()];
        let line = match &turn.state {
            TurnState::Starting => Line::from(Span::styled(
                format!("{spinner} starting the run…"),
                theme::muted(),
            )),
            TurnState::Streaming => Line::from(Span::styled(
                format!("{spinner} running · esc stops"),
                theme::muted(),
            )),
            TurnState::Done => Line::from(Span::styled(
                turn.footer.clone().unwrap_or_default(),
                theme::muted(),
            )),
            TurnState::Stopped => Line::from(Span::styled(
                "Agent run stopped.",
                theme::badge(theme::BadgeTone::Warn),
            )),
            TurnState::Failed(why) => Line::from(Span::styled(
                why.clone(),
                theme::badge(theme::BadgeTone::Bad),
            )),
            TurnState::Paused(interrupts) => {
                let mut lines = vec![Line::from(Span::styled(
                    match interrupts.len() {
                        1 => "Paused: a tool call needs your decision.".to_string(),
                        n => format!("Paused: {n} tool calls need your decision."),
                    },
                    theme::badge(theme::BadgeTone::Warn),
                ))];
                for interrupt in interrupts {
                    let reason = interrupt
                        .reason
                        .as_deref()
                        .map(|r| format!(" · {r}"))
                        .unwrap_or_default();
                    lines.push(Line::from(Span::raw(format!(
                        "  {}{reason}",
                        interrupt.tool
                    ))));
                }
                lines.push(Line::from(key_hints_line()));
                return lines;
            }
        };
        vec![line]
    }

    fn tool_lines(&self, turn: &Turn) -> Vec<Line<'static>> {
        let Some(tools) = &turn.tools else {
            return Vec::new();
        };
        if tools.is_empty() {
            return Vec::new();
        }
        let calls: u64 = tools.iter().map(|t| t.calls).sum();
        let arrow = if self.show_tools { "▾" } else { "▸" };
        let mut lines = vec![Line::from(Span::styled(
            format!("{arrow} {calls} tool call(s) · ctrl+t"),
            theme::muted(),
        ))];
        if self.show_tools {
            for tool in tools {
                let failed = if tool.errors > 0 {
                    format!(", {} failed", tool.errors)
                } else {
                    String::new()
                };
                let took = format::show(
                    Some(&json!(tool.total_ms)),
                    Format::Duration(TimeUnit::Millis),
                    0,
                )
                .text;
                lines.push(Line::from(Span::styled(
                    format!("  ⚙ {} ×{}{failed} · {took}", tool.name, tool.calls),
                    theme::muted(),
                )));
            }
        }
        lines
    }
}

fn key_hints_line() -> Vec<Span<'static>> {
    vec![
        Span::styled("  y", theme::key_hint()),
        Span::styled(" approve once  ", theme::muted()),
        Span::styled("n", theme::key_hint()),
        Span::styled(" deny", theme::muted()),
    ]
}

/// "v3 draft · 4.5 s · 3 steps · 12.4k tokens · $0.0123" from a run result.
fn footer(result: &Value, mode: Mode, version: Option<&str>) -> String {
    let show = |value: &Value, format| {
        let shown = format::show(Some(value), format, 0).text;
        (shown != format::MISSING).then_some(shown)
    };
    let mut parts = Vec::new();
    parts.push(match (mode, version) {
        (Mode::Draft, Some(v)) => format!("v{v} draft"),
        (mode, _) => mode.label().to_string(),
    });
    if let Some(took) = show(&result["durationMs"], Format::Duration(TimeUnit::Millis)) {
        parts.push(took);
    }
    if let Some(steps) = result["cycleCount"].as_u64() {
        parts.push(format!("{steps} step{}", if steps == 1 { "" } else { "s" }));
    }
    if let Some(tokens) = show(&result["usage"]["totalTokens"], Format::Compact) {
        parts.push(format!("{tokens} tokens"));
    }
    if let Some(cost) = show(&result["estimatedModelCost"]["totalUsd"], Format::Usd) {
        parts.push(cost);
    }
    parts.join(" · ")
}

/// Pending tool approvals in a run result (`approvals.py:46-55`).
fn interrupts_of(result: &Value) -> Vec<Interrupt> {
    result["interrupts"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| {
                    Some(Interrupt {
                        id: i["id"].as_str()?.to_string(),
                        tool: i["toolName"].as_str().unwrap_or("a tool").to_string(),
                        reason: i["reason"].as_str().map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The tools an observability record lists (camelCased `_safe_tool` rows).
fn tools_of(record: &Value) -> Vec<ToolUse> {
    let count = |tool: &Value, key: &str| tool[key].as_u64().unwrap_or(0);
    record["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| ToolUse {
                    name: format::text_of(&tool["name"]),
                    calls: count(tool, "callCount"),
                    errors: count(tool, "errorCount"),
                    total_ms: count(tool, "totalDurationMs"),
                })
                .collect()
        })
        .unwrap_or_default()
}

impl View for Playground {
    fn title(&self) -> String {
        format!("Playground · {}", self.name)
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc if self.is_running() => self.stop(ctx),
            KeyCode::Esc => return Handled::No,
            KeyCode::Char('t') if ctrl => self.show_tools = !self.show_tools,
            KeyCode::Char('y') if self.paused().is_some() => self.decide(true, ctx),
            KeyCode::Char('n') if self.paused().is_some() => self.decide(false, ctx),
            KeyCode::Tab => self.toggle_mode(ctx),
            KeyCode::Enter => self.send(ctx),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) if !ctrl => self.input.push(c),
            KeyCode::PageUp => self.scroll_by(-(self.viewport.max(1) as isize)),
            KeyCode::PageDown => self.scroll_by(self.viewport.max(1) as isize),
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, ctx: &mut Ctx) {
        match self.waiting {
            Waiting::Start(start) if start == id => return self.started(result, ctx),
            Waiting::Resume(resume) if resume == id => return self.resumed(result, ctx),
            _ => {}
        }
        if let Some(index) = self.tool_fetches.iter().position(|(f, _)| *f == id) {
            let (_, turn) = self.tool_fetches.remove(index);
            self.tools_loaded(turn, result);
        }
    }

    fn on_stream(&mut self, id: u64, item: StreamItem, ctx: &mut Ctx) {
        if self.waiting != Waiting::Stream(id) {
            return;
        }
        match item {
            StreamItem::Event(event) => {
                let data = event.json().unwrap_or(Value::Null);
                let kind = data["type"]
                    .as_str()
                    .or(event.event.as_deref())
                    .unwrap_or_default()
                    .to_string();
                self.on_event(&data, &kind, ctx);
            }
            // The stream closed without a terminal event.
            StreamItem::Ended => self.finished("completed", &Value::Null, ctx),
            StreamItem::Failed(err) => self.fail(self.explain(&err)),
        }
    }

    fn on_scroll(&mut self, delta: isize) {
        self.scroll_by(delta);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ui: RenderCtx) {
        let mode = match (self.mode, &self.version) {
            (Mode::Draft, Some(v)) => format!("draft v{v}"),
            (mode, _) => mode.label().to_string(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border(true))
            .title(Span::styled(
                format!(" Playground · {} · {mode} ", self.name),
                theme::title(),
            ))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [body, composer, hints] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(inner);

        let lines = self.transcript(body.width as usize, ui.tick);
        self.viewport = body.height as usize;
        self.line_count = lines.len();
        if self.follow {
            self.scroll = self.line_count.saturating_sub(self.viewport);
        }
        self.scroll_by(0);
        let shown: Vec<Line> = lines
            .into_iter()
            .skip(self.scroll)
            .take(self.viewport)
            .collect();
        frame.render_widget(Paragraph::new(shown), body);
        if self.line_count > self.viewport {
            render_scrollbar(
                frame,
                body,
                self.line_count.saturating_sub(self.viewport),
                self.scroll,
            );
        }

        let prompt_style = if self.is_running() {
            theme::muted()
        } else {
            Style::default()
        };
        let input = Paragraph::new(Line::from(vec![
            Span::styled("› ", theme::key_hint()),
            Span::styled(self.input.clone(), prompt_style),
            Span::styled("▏", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme::border(!self.is_running())),
        );
        frame.render_widget(input, composer);
        frame.render_widget(Paragraph::new(key_hints(&self.hints())), hints);
    }

    fn help(&self) -> Vec<HelpSection> {
        vec![HelpSection::new(
            "Playground",
            &[
                ("⏎", "run the prompt"),
                ("tab", "draft ↔ approved version"),
                ("esc", "stop the run / back"),
                ("y  n", "approve / deny paused tool calls"),
                ("ctrl+t", "show or hide tool calls"),
                ("PgUp PgDn", "scroll"),
            ],
        )]
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.is_running() {
            return vec![("esc", "stop")];
        }
        vec![
            ("⏎", "run"),
            ("tab", "draft/approved"),
            ("ctrl+t", "tools"),
            ("esc", "back"),
        ]
    }

    fn is_busy(&self) -> bool {
        self.is_running()
    }

    fn is_typing(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests;
