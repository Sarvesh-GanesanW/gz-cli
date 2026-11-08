//! A conversation as a readable transcript: each turn rendered as
//! Markdown, a dim footer under every answer (model, tokens, cost, timing),
//! tool blocks folded, and the conversation's totals in the header.
//!
//! Contracts: `GET chat /chat/conversations/{id}` → `{conversationName,
//! groundzeroMode, reasoningEffort, useRag, messages[]}` where each message
//! has `speaker` (`Human`/`AI`), `message` (text or content blocks),
//! `timestamp` and, on answers, `responseMetrics` with `costUsd` as a
//! decimal string (gz-chat-eks `api/routes/conversations.py:813-866`).
//! Mode labels come from `GET chat /chat/capabilities` →
//! `groundzeroModes[].{value,label}` (`api/routes/chat/capabilities.py`).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Rect},
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
use crate::tui::format::{self, Format, TimeUnit};
use crate::tui::help::HelpSection;
use crate::tui::markdown;
use crate::tui::theme;
use crate::tui::ui::widgets::render_scrollbar;

const CHAT: &str = "chat";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Speaker {
    You,
    Gz,
}

/// One turn as shown.
#[derive(Debug, Clone, PartialEq)]
struct Turn {
    speaker: Speaker,
    text: String,
    /// Tool blocks in the message (`tool_use` / `tool_result`), folded.
    tools: Vec<String>,
    metrics: Value,
    mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Load {
    Loading(u64),
    Ready,
    Failed(String),
}

pub struct Transcript {
    id: String,
    title: String,
    load: Load,
    capabilities: Option<u64>,
    /// Groundzero mode value → label ("grok47" → "Grok 4.7").
    mode_labels: Vec<(String, String)>,
    conversation: Value,
    turns: Vec<Turn>,
    show_tools: bool,
    scroll: usize,
    viewport: usize,
    line_count: usize,
}

impl Transcript {
    /// The transcript of the conversation in `row` (a `/chat/conversations` row).
    pub fn new(row: &Value) -> Self {
        let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_string);
        Self {
            id: text("conversationId").unwrap_or_default(),
            title: text("conversationName")
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| "New chat".to_string()),
            load: Load::Ready,
            capabilities: None,
            mode_labels: Vec::new(),
            conversation: Value::Null,
            turns: Vec::new(),
            show_tools: false,
            scroll: 0,
            viewport: 1,
            line_count: 0,
        }
    }

    /// The command that carries this conversation on in `gz code`.
    pub fn continue_command(&self) -> String {
        format!("gz code -c {}", self.id)
    }

    fn load(&mut self, ctx: &mut Ctx) {
        let path = format!("/chat/conversations/{}", seg(&self.id));
        self.load = Load::Loading(ctx.fetch_raw(CHAT, Method::GET, path, None));
    }

    fn loaded(&mut self, result: Result<Value, String>) {
        match result {
            Ok(conversation) => {
                if let Some(name) = conversation["conversationName"]
                    .as_str()
                    .filter(|n| !n.trim().is_empty())
                {
                    self.title = name.to_string();
                }
                self.turns = turns_of(&conversation);
                self.conversation = conversation;
                self.load = Load::Ready;
            }
            Err(err) => {
                let subject = Subject {
                    what: "this conversation",
                    service: CHAT,
                };
                self.load = Load::Failed(errors::explain(&err, subject));
            }
        }
    }

    fn mode_label(&self, mode: &str) -> String {
        self.mode_labels
            .iter()
            .find(|(value, _)| value == mode)
            .map_or_else(|| mode.to_string(), |(_, label)| label.clone())
    }

    fn header(&self) -> Vec<Line<'static>> {
        let c = &self.conversation;
        let mut facts = Vec::new();
        if let Some(mode) = c["groundzeroMode"].as_str() {
            facts.push(self.mode_label(mode));
        } else if let Some(connection) = c["modelConnectionName"].as_str() {
            facts.push(connection.to_string());
        }
        if let Some(effort) = c["reasoningEffort"].as_str() {
            facts.push(format!("{effort} effort"));
        }
        if c["useRag"].as_bool() == Some(true) {
            facts.push("RAG on".to_string());
        }
        let (cost, tokens) = totals(&self.turns);
        facts.push(format!("{} turns", self.turns.len()));
        if tokens > 0.0 {
            facts.push(format!("{} tokens", format::compact(tokens)));
        }
        if cost > 0.0 {
            facts.push(show(&json!(cost), Format::Usd));
        }
        vec![
            Line::from(Span::styled(facts.join(" · "), theme::muted())),
            Line::from(""),
        ]
    }

    fn lines(&self, width: usize) -> Vec<Line<'static>> {
        match &self.load {
            Load::Loading(_) => {
                return vec![Line::from(Span::styled(
                    "Loading the conversation…",
                    theme::muted(),
                ))]
            }
            Load::Failed(err) => {
                return vec![Line::from(Span::styled(
                    err.clone(),
                    theme::badge(theme::BadgeTone::Bad),
                ))]
            }
            Load::Ready => {}
        }
        let mut lines = self.header();
        if self.turns.is_empty() {
            lines.push(Line::from(Span::styled(
                "This conversation has no messages yet.",
                theme::muted(),
            )));
        }
        for turn in &self.turns {
            let (label, style) = match turn.speaker {
                Speaker::You => ("you", theme::key_hint()),
                Speaker::Gz => ("gz", theme::badge(theme::BadgeTone::Info)),
            };
            lines.push(Line::from(Span::styled(label, style)));
            lines.extend(markdown::render(&turn.text, width));
            if !turn.tools.is_empty() {
                let arrow = if self.show_tools { "▾" } else { "▸" };
                lines.push(Line::from(Span::styled(
                    format!("{arrow} {} tool step(s) · t", turn.tools.len()),
                    theme::muted(),
                )));
                if self.show_tools {
                    for tool in &turn.tools {
                        lines.push(Line::from(Span::styled(
                            format!("  ⚙ {tool}"),
                            theme::muted(),
                        )));
                    }
                }
            }
            if turn.speaker == Speaker::Gz {
                let footer = self.footer(turn);
                if !footer.is_empty() {
                    lines.push(Line::from(Span::styled(footer, theme::muted())));
                }
            }
            lines.push(Line::from(""));
        }
        lines
    }

    /// "Grok 4.7 · 11.7k tokens · $0.02 · first token 4.0 s · 4.5 s".
    fn footer(&self, turn: &Turn) -> String {
        let m = &turn.metrics;
        let mut parts = Vec::new();
        if let Some(mode) = &turn.mode {
            parts.push(self.mode_label(mode));
        }
        if m["totalTokens"].as_f64().is_some_and(|t| t > 0.0) {
            parts.push(format!(
                "{} tokens",
                show(&m["totalTokens"], Format::Compact)
            ));
        }
        if !m["costUsd"].is_null() {
            parts.push(show(&m["costUsd"], Format::Usd));
        }
        if !m["timeToFirstTokenMs"].is_null() {
            parts.push(format!(
                "first token {}",
                show(&m["timeToFirstTokenMs"], Format::Duration(TimeUnit::Millis))
            ));
        }
        if !m["latencyMs"].is_null() {
            parts.push(show(&m["latencyMs"], Format::Duration(TimeUnit::Millis)));
        }
        parts.join(" · ")
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.line_count.saturating_sub(self.viewport) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max.max(0)) as usize;
    }
}

fn show(value: &Value, format: Format) -> String {
    format::show(Some(value), format, 0).text
}

fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0.0)
}

/// The conversation's total cost (USD) and tokens, summed over answers.
fn totals(turns: &[Turn]) -> (f64, f64) {
    turns.iter().fold((0.0, 0.0), |(cost, tokens), turn| {
        (
            cost + number(&turn.metrics["costUsd"]),
            tokens + number(&turn.metrics["totalTokens"]),
        )
    })
}

fn turns_of(conversation: &Value) -> Vec<Turn> {
    let Some(messages) = conversation["messages"].as_array() else {
        return Vec::new();
    };
    messages
        .iter()
        .filter_map(|m| {
            let role = m["speaker"]
                .as_str()
                .or(m["role"].as_str())
                .unwrap_or("AI")
                .to_ascii_lowercase();
            let speaker = if role == "human" || role == "user" {
                Speaker::You
            } else {
                Speaker::Gz
            };
            let content = if m["message"].is_null() {
                &m["content"]
            } else {
                &m["message"]
            };
            let (text, tools) = split_content(content);
            if text.trim().is_empty() && tools.is_empty() {
                return None;
            }
            Some(Turn {
                speaker,
                text,
                tools,
                metrics: m["responseMetrics"].clone(),
                mode: m["groundzeroMode"].as_str().map(str::to_string),
            })
        })
        .collect()
}

/// Text blocks joined, and one summary line per tool block.
fn split_content(content: &Value) -> (String, Vec<String>) {
    match content {
        Value::String(text) => (text.clone(), Vec::new()),
        Value::Array(blocks) => {
            let mut text = Vec::new();
            let mut tools = Vec::new();
            for block in blocks {
                match block["type"].as_str() {
                    Some("tool_use") => {
                        tools.push(block["name"].as_str().unwrap_or("tool").to_string())
                    }
                    Some("tool_result") => {
                        let failed = block["is_error"].as_bool() == Some(true);
                        tools.push(if failed { "result (failed)" } else { "result" }.into());
                    }
                    _ => {
                        if let Some(t) = block["text"].as_str().or(block.as_str()) {
                            text.push(t.to_string());
                        }
                    }
                }
            }
            (text.join("\n"), tools)
        }
        _ => (String::new(), Vec::new()),
    }
}

impl View for Transcript {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        self.load(ctx);
        self.capabilities =
            Some(ctx.fetch_raw(CHAT, Method::GET, "/chat/capabilities".to_string(), None));
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let page = self.viewport.max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll_by(isize::MAX / 2),
            KeyCode::Char('t') => self.show_tools = !self.show_tools,
            KeyCode::Char('c') => {
                let command = self.continue_command();
                ctx.copy(
                    command.clone(),
                    format!("`{command}` · paste it in a terminal"),
                );
            }
            KeyCode::Char('y') => {
                if self.id.is_empty() {
                    ctx.toast(Tone::Error, "This conversation has no id");
                } else {
                    ctx.copy(self.id.clone(), "conversation id");
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, _ctx: &mut Ctx) {
        if self.load == Load::Loading(id) {
            return self.loaded(result);
        }
        if self.capabilities == Some(id) {
            self.capabilities = None;
            // Without labels the raw mode value still shows; nothing to report.
            if let Ok(capabilities) = result {
                self.mode_labels = mode_labels(&capabilities);
            }
        }
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        self.load(ctx);
    }

    fn on_scroll(&mut self, delta: isize) {
        self.scroll_by(delta);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ui: RenderCtx) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border(true))
            .title(Span::styled(format!(" {} ", self.title), theme::title()))
            .title(
                Line::from(Span::styled(" c continue in gz code ", theme::muted()))
                    .alignment(Alignment::Right),
            )
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = self.lines(inner.width as usize);
        self.viewport = inner.height as usize;
        self.line_count = lines.len();
        self.scroll_by(0);
        let shown: Vec<Line> = lines
            .into_iter()
            .skip(self.scroll)
            .take(self.viewport)
            .collect();
        frame.render_widget(Paragraph::new(shown), inner);
        if self.line_count > self.viewport {
            render_scrollbar(
                frame,
                area,
                self.line_count.saturating_sub(self.viewport),
                self.scroll,
            );
        }
    }

    fn help(&self) -> Vec<HelpSection> {
        vec![HelpSection::new(
            "Transcript",
            &[
                ("↑ ↓  PgUp PgDn", "scroll"),
                ("t", "show or hide tool steps"),
                ("c", "copy `gz code -c <id>` to continue it"),
                ("y", "copy the conversation id"),
                ("r", "reload"),
                ("esc", "back"),
            ],
        )]
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑↓", "scroll"),
            ("t", "tools"),
            ("c", "continue in gz code"),
            ("esc", "back"),
        ]
    }

    fn is_busy(&self) -> bool {
        matches!(self.load, Load::Loading(_))
    }
}

fn mode_labels(capabilities: &Value) -> Vec<(String, String)> {
    capabilities["groundzeroModes"]
        .as_array()
        .map(|modes| {
            modes
                .iter()
                .filter_map(|m| {
                    Some((
                        m["value"].as_str()?.to_string(),
                        m["label"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
