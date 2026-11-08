//! Usage & cost: AI (chat) usage and agent usage side by side, per day and
//! per model, with bar sparklines and totals over 7, 30 or 90 days; above
//! them the caller's rolling budget (the totals answer's `budget`) and gz
//! code's own spend on this machine from its local ledger.
//!
//! The two are never added together: `/chat/metrics` already includes the
//! agent builder's model spend (its `agent_builder` feature row), so a sum
//! would count agent runs twice.
//!
//! Contracts (TUI research notes §3, §8, §11):
//! - `GET chat /chat/metrics?from&to` (timezone required) → `{costUsd.total,
//!   usage{requestCount,totalTokens}, modelMetrics[]}`; no day buckets, so
//!   days come from one call per day (at most [`CHAT_PARALLEL`] at a time,
//!   and only up to [`CHAT_DAILY_MAX_DAYS`] days)
//! - `GET agents /v1/agents/metrics/timeseries?from&to&bucket=day` →
//!   `{items: [{bucketStart, runCount, successRate, estimatedModelCostUsd}]}`
//! - `GET agents /v1/agents/metrics/models?from&to` → `{items: [{agentName,
//!   modelMetrics: [{modelId, runCount, totalTokens, estimatedModelCostUsd}]}]}`

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
    Frame,
};
use reqwest::Method;
use serde_json::Value;

use super::{Ctx, Handled, RenderCtx, View};
use crate::spend::budget::BudgetStatus;
use crate::spend::ledger::Summary;
use crate::tui::errors::{self, Subject};
use crate::tui::format::{self, Format};
use crate::tui::help::HelpSection;
use crate::tui::theme;
use crate::tui::ui::widgets::{self, fit, key_hints};

const MS_PER_DAY: i64 = 86_400_000;
/// Chat metrics have no day buckets; at most this many day calls run at once.
const CHAT_PARALLEL: usize = 4;
/// Longer ranges would need too many chat calls; they show totals only.
const CHAT_DAILY_MAX_DAYS: u32 = 30;
const RANGES: [u32; 3] = [7, 30, 90];
const BAR_WIDTH: usize = 18;
const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const TOP_MODELS: usize = 8;
/// Models named on the gz code line.
const TOP_LOCAL_MODELS: usize = 3;

/// One day's figures for one side.
#[derive(Debug, Clone, PartialEq)]
struct Day {
    /// "2026-11-08".
    date: String,
    cost: Option<f64>,
    /// Requests (chat) or runs (agents).
    count: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
struct ModelRow {
    model: String,
    count: f64,
    tokens: f64,
    cost: f64,
}

/// What one side (chat or agents) has loaded.
#[derive(Debug, Clone, Default, PartialEq)]
struct Side {
    total_cost: Option<f64>,
    total_count: Option<f64>,
    tokens: Option<f64>,
    days: Vec<Day>,
    models: Vec<ModelRow>,
    errors: Vec<String>,
}

/// Which request an answer belongs to.
#[derive(Debug, Clone, PartialEq)]
enum Pending {
    ChatTotals,
    ChatDay(usize),
    AgentDays,
    AgentModels,
}

pub struct Usage {
    days: u32,
    /// The caller's rolling budget, from the totals answer.
    budget: Option<BudgetStatus>,
    /// gz code's own spend on this machine (local ledger).
    local: Option<Summary>,
    chat: Side,
    agents: Side,
    pending: Vec<(u64, Pending)>,
    /// Chat days not requested yet (index into `chat.days`).
    chat_queue: VecDeque<usize>,
}

impl Default for Usage {
    fn default() -> Self {
        Self::new()
    }
}

impl Usage {
    pub fn new() -> Self {
        Self {
            days: 7,
            budget: None,
            local: None,
            chat: Side::default(),
            agents: Side::default(),
            pending: Vec::new(),
            chat_queue: VecDeque::new(),
        }
    }

    /// Midnight UTC `days - 1` days ago and next midnight, so today counts.
    fn range(&self, now_ms: i64) -> (i64, i64) {
        let tomorrow = (now_ms.div_euclid(MS_PER_DAY) + 1) * MS_PER_DAY;
        (tomorrow - i64::from(self.days) * MS_PER_DAY, tomorrow)
    }

    fn load(&mut self, ctx: &mut Ctx) {
        let (from, to) = self.range(ctx.now.epoch_ms);
        let period = format!(
            "from={}&to={}",
            format::iso8601_utc(from),
            format::iso8601_utc(to)
        );
        self.chat = Side::default();
        self.agents = Side::default();
        self.local = ctx.local_spend();
        self.pending.clear();
        self.chat_queue.clear();
        self.agents.days = day_slots(from, self.days);
        if self.days <= CHAT_DAILY_MAX_DAYS {
            self.chat.days = day_slots(from, self.days);
            self.chat_queue = (0..self.chat.days.len()).collect();
        }
        let fetch = |ctx: &mut Ctx, service: &str, path: String| {
            ctx.fetch_raw(service, Method::GET, path, None)
        };
        let id = fetch(ctx, "chat", format!("/chat/metrics?{period}"));
        self.pending.push((id, Pending::ChatTotals));
        let id = fetch(
            ctx,
            "agents",
            format!("/v1/agents/metrics/timeseries?{period}&bucket=day"),
        );
        self.pending.push((id, Pending::AgentDays));
        let id = fetch(ctx, "agents", format!("/v1/agents/metrics/models?{period}"));
        self.pending.push((id, Pending::AgentModels));
        self.next_chat_days(ctx);
    }

    /// Keeps up to [`CHAT_PARALLEL`] per-day chat calls in flight.
    fn next_chat_days(&mut self, ctx: &mut Ctx) {
        let in_flight = self
            .pending
            .iter()
            .filter(|(_, p)| matches!(p, Pending::ChatDay(_)))
            .count();
        for _ in in_flight..CHAT_PARALLEL {
            let Some(index) = self.chat_queue.pop_front() else {
                return;
            };
            let Some(start) = format::epoch_ms(&Value::String(self.chat.days[index].date.clone()))
            else {
                continue;
            };
            let path = format!(
                "/chat/metrics?from={}&to={}",
                format::iso8601_utc(start),
                format::iso8601_utc(start + MS_PER_DAY)
            );
            let id = ctx.fetch_raw("chat", Method::GET, path, None);
            self.pending.push((id, Pending::ChatDay(index)));
        }
    }

    fn apply(&mut self, pending: Pending, result: Result<Value, String>) {
        let service = match pending {
            Pending::ChatTotals | Pending::ChatDay(_) => "chat",
            Pending::AgentDays | Pending::AgentModels => "agents",
        };
        let answer = match result {
            Ok(answer) => answer,
            Err(err) => {
                let what = format!("{service} usage");
                let message = errors::explain(
                    &err,
                    Subject {
                        what: &what,
                        service,
                    },
                );
                let side = if service == "chat" {
                    &mut self.chat
                } else {
                    &mut self.agents
                };
                if !side.errors.contains(&message) {
                    side.errors.push(message);
                }
                return;
            }
        };
        match pending {
            Pending::ChatTotals => {
                self.chat.total_cost = money(&answer["costUsd"]["total"]);
                self.chat.total_count = money(&answer["usage"]["requestCount"]);
                self.chat.tokens = money(&answer["usage"]["totalTokens"]);
                self.chat.models = chat_models(&answer);
                self.budget = BudgetStatus::from_json(&answer["budget"]);
            }
            Pending::ChatDay(index) => {
                if let Some(day) = self.chat.days.get_mut(index) {
                    day.cost = Some(money(&answer["costUsd"]["total"]).unwrap_or(0.0));
                    day.count = Some(money(&answer["usage"]["requestCount"]).unwrap_or(0.0));
                }
            }
            Pending::AgentDays => self.agent_days(&answer),
            Pending::AgentModels => self.agents.models = agent_models(&answer),
        }
    }

    fn agent_days(&mut self, answer: &Value) {
        let buckets = answer["items"].as_array().cloned().unwrap_or_default();
        for bucket in &buckets {
            let date = bucket["bucketStart"].as_str().unwrap_or_default();
            if let Some(day) = self
                .agents
                .days
                .iter_mut()
                .find(|d| date.starts_with(&d.date))
            {
                day.cost = Some(money(&bucket["estimatedModelCostUsd"]).unwrap_or(0.0));
                day.count = Some(money(&bucket["runCount"]).unwrap_or(0.0));
            }
        }
        let sum = |pick: fn(&Day) -> Option<f64>| self.agents.days.iter().filter_map(pick).sum();
        self.agents.total_cost = Some(sum(|d| d.cost));
        self.agents.total_count = Some(sum(|d| d.count));
    }

    fn side_lines(
        &self,
        side: &Side,
        title: &str,
        count_label: &str,
        width: usize,
    ) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(Span::styled(title.to_string(), theme::brand()))];
        let total = |v: Option<f64>, f: Format| v.map_or_else(|| "…".to_string(), |v| shown(v, f));
        let mut headline = format!(
            "{} · {} {count_label}",
            total(side.total_cost, Format::Usd),
            total(side.total_count, Format::Compact)
        );
        if let Some(tokens) = side.tokens {
            headline.push_str(&format!(" · {} tokens", format::compact(tokens)));
        }
        lines.push(Line::from(Span::styled(headline, theme::title())));
        for err in &side.errors {
            lines.push(Line::from(Span::styled(
                fit(err, width),
                theme::badge(theme::BadgeTone::Bad),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Per day", theme::header_cell())));
        if side.days.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("Daily figures cover up to {CHAT_DAILY_MAX_DAYS} days"),
                theme::muted(),
            )));
        } else {
            let costs: Vec<Option<f64>> = side.days.iter().map(|d| d.cost).collect();
            lines.push(Line::from(Span::styled(
                sparkline(&costs),
                theme::badge(theme::BadgeTone::Info),
            )));
            let max = costs.iter().flatten().cloned().fold(0.0, f64::max);
            // The most recent days, newest first, as bars.
            for day in side.days.iter().rev().take(7) {
                let cost = day
                    .cost
                    .map_or_else(|| "…".to_string(), |c| shown(c, Format::Usd));
                let count = day
                    .count
                    .map_or(String::new(), |c| format!(" · {}", format::compact(c)));
                lines.push(Line::from(vec![
                    Span::styled(format!("{} ", &day.date[5..]), theme::muted()),
                    Span::styled(
                        bar(day.cost.unwrap_or(0.0), max),
                        theme::badge(theme::BadgeTone::Info),
                    ),
                    Span::raw(format!(" {cost}{count}")),
                ]));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Per model", theme::header_cell())));
        if side.models.is_empty() {
            lines.push(Line::from(Span::styled(
                "No model usage in this period",
                theme::muted(),
            )));
        }
        let max = side.models.iter().map(|m| m.cost).fold(0.0, f64::max);
        for model in side.models.iter().take(TOP_MODELS) {
            lines.push(Line::from(vec![
                Span::raw(format!("{:<16} ", fit(&model.model, 16))),
                Span::styled(bar(model.cost, max), theme::badge(theme::BadgeTone::Good)),
                Span::raw(format!(" {}", shown(model.cost, Format::Usd))),
                Span::styled(
                    format!(" · {} {count_label}", format::compact(model.count)),
                    theme::muted(),
                ),
            ]));
        }
        lines
    }
}

impl Usage {
    /// The budget meter and gz code's own spend, above both sides.
    fn spend_lines(&self) -> Vec<Line<'static>> {
        let label = |text: &str| Span::styled(format!("{text:<10}"), theme::header_cell());
        let mut budget = vec![label("Budget")];
        match &self.budget {
            Some(status) => {
                budget.extend(widgets::budget_meter(status, BAR_WIDTH));
                budget.push(Span::styled(
                    format!(" · {}", status.set_by()),
                    theme::muted(),
                ));
            }
            None if self.is_busy() => budget.push(Span::styled("…", theme::muted())),
            None => budget.push(Span::styled("unavailable", theme::muted())),
        }
        let mut local = vec![label("gz code")];
        match &self.local {
            Some(summary) => {
                local.push(Span::raw(widgets::local_spend_text(
                    summary,
                    TOP_LOCAL_MODELS,
                )));
                local.push(Span::styled("  on this machine", theme::muted()));
            }
            None => local.push(Span::styled(
                "no spend recorded on this machine",
                theme::muted(),
            )),
        }
        vec![Line::from(budget), Line::from(local)]
    }
}

fn shown(value: f64, format: Format) -> String {
    format::show(Some(&serde_json::json!(value)), format, 0).text
}

/// Money and counts arrive as numbers or decimal strings.
fn money(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

fn day_slots(from: i64, days: u32) -> Vec<Day> {
    (0..i64::from(days))
        .map(|i| Day {
            date: format::date_time(from + i * MS_PER_DAY)[..10].to_string(),
            cost: None,
            count: None,
        })
        .collect()
}

fn bar(value: f64, max: f64) -> String {
    let filled = if max > 0.0 {
        ((value / max) * BAR_WIDTH as f64).round() as usize
    } else {
        0
    };
    let filled = filled.min(BAR_WIDTH).max(usize::from(value > 0.0));
    format!("{}{}", "█".repeat(filled), "·".repeat(BAR_WIDTH - filled))
}

/// One block per day, scaled to the busiest day; unknown days are blank.
fn sparkline(values: &[Option<f64>]) -> String {
    let max = values.iter().flatten().cloned().fold(0.0, f64::max);
    values
        .iter()
        .map(|v| match v {
            None => ' ',
            Some(v) if max <= 0.0 || *v <= 0.0 => SPARK[0],
            Some(v) => {
                SPARK
                    [(((v / max) * (SPARK.len() - 1) as f64).round() as usize).min(SPARK.len() - 1)]
            }
        })
        .collect()
}

fn by_cost(mut rows: Vec<ModelRow>) -> Vec<ModelRow> {
    rows.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    rows
}

fn chat_models(answer: &Value) -> Vec<ModelRow> {
    let rows = answer["modelMetrics"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    by_cost(
        rows.iter()
            .map(|m| ModelRow {
                model: format::model_name(m["modelId"].as_str().unwrap_or("unknown")),
                count: money(&m["requestCount"]).unwrap_or(0.0),
                tokens: money(&m["totalTokens"]).unwrap_or(0.0),
                cost: money(&m["totalCostUsd"]).unwrap_or(0.0),
            })
            .collect(),
    )
}

/// Agent × model rows summed per model.
fn agent_models(answer: &Value) -> Vec<ModelRow> {
    let mut rows: Vec<ModelRow> = Vec::new();
    let agents = answer["items"].as_array().cloned().unwrap_or_default();
    for metric in agents
        .iter()
        .flat_map(|a| a["modelMetrics"].as_array().cloned().unwrap_or_default())
    {
        let model = format::model_name(metric["modelId"].as_str().unwrap_or("unknown"));
        let index = match rows.iter().position(|r| r.model == model) {
            Some(index) => index,
            None => {
                rows.push(ModelRow {
                    model,
                    count: 0.0,
                    tokens: 0.0,
                    cost: 0.0,
                });
                rows.len() - 1
            }
        };
        let row = &mut rows[index];
        row.count += money(&metric["runCount"]).unwrap_or(0.0);
        row.tokens += money(&metric["totalTokens"]).unwrap_or(0.0);
        row.cost += money(&metric["estimatedModelCostUsd"]).unwrap_or(0.0);
    }
    by_cost(rows)
}

impl View for Usage {
    fn title(&self) -> String {
        format!("Usage & cost · {} days", self.days)
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        self.load(ctx);
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let days = match key.code {
            KeyCode::Char('7') => 7,
            KeyCode::Char('3') => 30,
            KeyCode::Char('9') => 90,
            KeyCode::Tab => {
                let at = RANGES.iter().position(|d| *d == self.days).unwrap_or(0);
                RANGES[(at + 1) % RANGES.len()]
            }
            _ => return Handled::No,
        };
        if days != self.days {
            self.days = days;
            self.load(ctx);
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, ctx: &mut Ctx) {
        let Some(index) = self.pending.iter().position(|(p, _)| *p == id) else {
            return;
        };
        let (_, pending) = self.pending.remove(index);
        self.apply(pending, result);
        self.next_chat_days(ctx);
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        self.load(ctx);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ui: RenderCtx) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border(true))
            .title(Span::styled(format!(" {} ", self.title()), theme::title()))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [spend, note, body, hints] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        frame.render_widget(Paragraph::new(self.spend_lines()), spend);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Shown side by side, never added: AI usage already includes agent model spend.",
                theme::muted(),
            ))),
            note,
        );
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(body);
        let width = left.width.saturating_sub(2) as usize;
        let chat = self.side_lines(&self.chat, "AI usage (chat)", "requests", width);
        let agents = self.side_lines(&self.agents, "Agent usage", "runs", width);
        frame.render_widget(Paragraph::new(chat), left);
        frame.render_widget(Paragraph::new(agents), right);
        frame.render_widget(Paragraph::new(key_hints(&self.hints())), hints);
    }

    fn help(&self) -> Vec<HelpSection> {
        vec![HelpSection::new(
            "Usage & cost",
            &[
                ("7  3  9", "last 7, 30 or 90 days"),
                ("tab", "next range"),
                ("r", "reload"),
                ("esc", "back"),
            ],
        )]
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("7/3/9", "7, 30, 90 days"),
            ("r", "reload"),
            ("esc", "back"),
        ]
    }

    fn is_busy(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests;
