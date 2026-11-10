//! One ETL run, live: its status, elapsed time, the last log line, which
//! sub-job a multi-job run is on, an estimated cost, and the log itself
//! with tail mode. Logs are polled (`POST etl /etl/logs`); there is no
//! stream. Only the run being viewed is ever asked for: `/etl/logs` can
//! mark a stale run Failed, so it must never be polled across a list.

pub mod follow;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
    Frame,
};
use reqwest::Method;
use serde_json::{json, Value};

use self::follow::{is_terminal, Follower, Level, Stopped};
use super::{Ctx, Effect, Handled, RenderCtx, RowContext, View};
use crate::tui::app::Tone;
use crate::tui::errors::{self, Subject};
use crate::tui::fields;
use crate::tui::format::{self, Format, TimeUnit};
use crate::tui::help::HelpSection;
use crate::tui::nav::{Source, Verb};
use crate::tui::screen::{ActionKind, ActionSpec, After, RequestSpec};
use crate::tui::theme::{self, BadgeTone};
use crate::tui::ui::states;
use crate::tui::ui::widgets::{fit, render_scrollbar};

const SERVICE: &str = "etl";
/// After a run ends its real cost is collected; look this often…
const COST_POLL_MS: i64 = 30_000;
/// …for this long.
const COST_WAIT_MS: i64 = 5 * 60_000;
/// The log stream's status for a failed run.
const FAILED: &str = "Failed";
const STOP_HELP: (&str, &str) = ("x", "stop the run (its whole session)");
const STOP_HINT: (&str, &str) = ("x", "stop");
/// Lines above the log: status, last line, and room for a warning.
const HEADER_LINES: usize = 4;

/// Stops the whole session: every active execution in it, multi-job
/// children included (`etl app.py` `_authorize_abort_scope`).
pub static STOP: ActionSpec = ActionSpec::new(
    'x',
    "Stop run",
    ActionKind::Request(RequestSpec {
        source: Source::new(
            SERVICE,
            Verb::Post,
            "/etl/job",
            Some(r#"{"action":"Abort","sessionId":"{sessionId}","jobIdentifier":"{executionId}"}"#),
        ),
        success: "Job aborted successfully",
        after: After::Refresh,
    }),
)
.destructive(
    "Stop this run? Its whole session stops now, including every sub-job of a multi-job run.",
);

/// Which run to show.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunTarget {
    pub execution_id: Option<String>,
    pub session_id: Option<String>,
    pub job_id: Option<i64>,
    pub job_name: Option<String>,
    pub project_id: Option<i64>,
    pub started_ms: Option<i64>,
}

impl RunTarget {
    /// From a run row (consumption metrics, schedule runs, a submit reply).
    pub fn from_row(row: &Value) -> Self {
        let text = |path: &[&str]| fields::lookup_any(row, path).map(format::text_of);
        let int = |path: &[&str]| {
            fields::lookup_any(row, path)
                .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
        };
        Self {
            execution_id: text(&["executionId", "jobIdentifier", "identity.executionId"]),
            session_id: text(&["sessionId", "identity.sessionId"]),
            job_id: int(&["jobId", "identity.jobId"]),
            job_name: text(&["jobName", "identity.jobName"]),
            project_id: int(&["projectId", "identity.projectId"]),
            started_ms: fields::lookup_any(row, &["startedAt", "startTime", "identity.startedAt"])
                .and_then(format::epoch_ms),
        }
    }

    fn short_id(&self) -> String {
        self.execution_id
            .as_deref()
            .map_or_else(|| "…".to_string(), |id| id.chars().take(8).collect())
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// Looking up the job's latest run (opened from a job, not a run).
    FindingLatest(Option<u64>),
    NoRuns,
    Following,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Usual {
    Unknown,
    Loading(u64),
    /// The job's average cost per hour and run length, when it has them.
    Known {
        usd_per_hour: Option<f64>,
        seconds: Option<f64>,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum Cost {
    Waiting,
    Polling {
        request: Option<u64>,
        next_ms: i64,
        until_ms: i64,
    },
    Final(f64),
}

pub struct RunLog {
    target: RunTarget,
    phase: Phase,
    follower: Follower,
    logs_request: Option<u64>,
    usual: Usual,
    cost: Cost,
    /// Keep the newest line in view.
    tail: bool,
    scroll: usize,
    viewport: usize,
    notice_sent: bool,
}

impl RunLog {
    pub fn new(target: RunTarget) -> Self {
        let phase = if target.execution_id.is_some() {
            Phase::Following
        } else {
            Phase::FindingLatest(None)
        };
        Self {
            target,
            phase,
            follower: Follower::default(),
            logs_request: None,
            usual: Usual::Unknown,
            cost: Cost::Waiting,
            tail: true,
            scroll: 0,
            viewport: 1,
            notice_sent: false,
        }
    }

    /// The latest run of a job, found when the view opens.
    pub fn latest_of(project_id: Option<i64>, job_id: i64, job_name: Option<String>) -> Self {
        Self::new(RunTarget {
            job_id: Some(job_id),
            job_name,
            project_id,
            ..RunTarget::default()
        })
    }

    fn explain(&self, err: &str) -> String {
        errors::explain(
            err,
            Subject {
                what: "this run",
                service: SERVICE,
            },
        )
    }

    fn find_latest(&mut self, ctx: &mut Ctx) {
        let Some(job) = self.target.job_id else {
            self.phase = Phase::NoRuns;
            return;
        };
        let path = format!("/etl/consumption-metrics?jobId={job}&workloadType=job&limit=1");
        self.phase = Phase::FindingLatest(Some(ctx.fetch_raw(SERVICE, Method::GET, path, None)));
    }

    fn start_following(&mut self, ctx: &mut Ctx) {
        self.phase = Phase::Following;
        self.follower = Follower::new(ctx.now.epoch_ms);
        self.poll_logs(ctx);
        self.fetch_usual(ctx);
    }

    fn poll_logs(&mut self, ctx: &mut Ctx) {
        let Some(execution) = self.target.execution_id.clone() else {
            return;
        };
        let body = self
            .follower
            .body(&execution, self.target.session_id.as_deref());
        self.follower.sent();
        self.logs_request =
            Some(ctx.fetch_raw(SERVICE, Method::POST, "/etl/logs".to_string(), Some(body)));
    }

    /// The job's average $/h and run length, for the estimate and the
    /// "may be stuck" check. A plain read, safe to ask once.
    fn fetch_usual(&mut self, ctx: &mut Ctx) {
        let (Some(project), Some(job)) = (self.target.project_id, self.target.job_id) else {
            self.usual = Usual::Known {
                usd_per_hour: None,
                seconds: None,
            };
            return;
        };
        if matches!(self.usual, Usual::Known { .. } | Usual::Loading(_)) {
            return;
        }
        let path =
            format!("/etl/consumption-metrics?groupBy=job&projectId={project}&jobId={job}&limit=1");
        self.usual = Usual::Loading(ctx.fetch_raw(SERVICE, Method::GET, path, None));
    }

    fn latest_found(&mut self, result: Result<Value, String>, ctx: &mut Ctx) {
        let run = result
            .ok()
            .and_then(|answer| answer["items"].as_array().and_then(|i| i.first().cloned()));
        let Some(run) = run else {
            self.phase = Phase::NoRuns;
            return;
        };
        let found = RunTarget::from_row(&run);
        self.target = RunTarget {
            project_id: found.project_id.or(self.target.project_id),
            job_name: found.job_name.or(self.target.job_name.take()),
            ..found
        };
        self.start_following(ctx);
    }

    fn logs_answered(&mut self, result: Result<Value, String>, ctx: &mut Ctx) {
        self.logs_request = None;
        let now = ctx.now.epoch_ms;
        match result {
            Ok(page) => {
                if let Some(session) = page["jobStatus"][0]["sessionId"].as_str() {
                    self.target
                        .session_id
                        .get_or_insert_with(|| session.to_string());
                }
                self.follower.page(&page, now);
            }
            Err(err) => {
                let explained = self.explain(&err);
                self.follower.failed(&err, explained, now);
            }
        }
        if let Some(stopped) = self.follower.stopped.clone() {
            self.on_stopped(&stopped, ctx);
        }
    }

    fn on_stopped(&mut self, stopped: &Stopped, ctx: &mut Ctx) {
        if self.notice_sent {
            return;
        }
        self.notice_sent = true;
        let tone = match stopped {
            Stopped::Finished(status) if status == "Success" => Tone::Ok,
            Stopped::Finished(_) | Stopped::NotFound | Stopped::Refused(_) => Tone::Error,
            Stopped::GaveUp => Tone::Info,
        };
        ctx.toast(tone, stopped.notice());
        if matches!(stopped, Stopped::Finished(_)) {
            let now = ctx.now.epoch_ms;
            self.cost = Cost::Polling {
                request: None,
                next_ms: now,
                until_ms: now + COST_WAIT_MS,
            };
        }
    }

    fn usual_answered(&mut self, result: Result<Value, String>) {
        let item = result
            .ok()
            .and_then(|a| a["items"].as_array().and_then(|i| i.first().cloned()));
        let read = |path: &str| {
            item.as_ref()
                .and_then(|i| fields::lookup(i, path)?.as_f64())
        };
        self.usual = Usual::Known {
            usd_per_hour: read("statistics.costUsd.averageRatePerHour"),
            seconds: read("statistics.durationSeconds.average"),
        };
    }

    fn cost_answered(&mut self, result: Result<Value, String>, now: i64) {
        let Cost::Polling { until_ms, .. } = self.cost else {
            return;
        };
        let total = result
            .ok()
            .and_then(|run| fields::lookup(&run, "usage.cost.totalUsd")?.as_f64());
        self.cost = match total {
            Some(total) => Cost::Final(total),
            None if now + COST_POLL_MS <= until_ms => Cost::Polling {
                request: None,
                next_ms: now + COST_POLL_MS,
                until_ms,
            },
            None => Cost::Waiting,
        };
    }

    fn poll_cost(&mut self, ctx: &mut Ctx) {
        let Cost::Polling {
            request: None,
            next_ms,
            until_ms,
        } = self.cost
        else {
            return;
        };
        let Some(execution) = self.target.execution_id.clone() else {
            return;
        };
        if ctx.now.epoch_ms < next_ms {
            return;
        }
        let path = format!("/etl/consumption-metrics/{}", crate::cmd::seg(&execution));
        let id = ctx.fetch_raw(SERVICE, Method::GET, path, None);
        self.cost = Cost::Polling {
            request: Some(id),
            next_ms,
            until_ms,
        };
    }

    fn elapsed_ms(&self, now: i64) -> Option<i64> {
        let started = self.follower.started_ms.or(self.target.started_ms)?;
        let ended = self
            .follower
            .stopped_ms
            .filter(|_| self.follower.status.as_deref().is_some_and(is_terminal))
            .unwrap_or(now);
        Some((ended - started).max(0))
    }

    fn status(&self) -> Option<&str> {
        self.follower.status.as_deref()
    }

    fn has_ended(&self) -> bool {
        self.status().is_some_and(is_terminal)
    }

    /// Stop is offered for a run being followed, until its status says it
    /// ended.
    fn can_stop(&self) -> bool {
        self.phase == Phase::Following && !self.has_ended()
    }

    fn stop_row(&self, ctx: &Ctx) -> RowContext {
        RowContext {
            row: json!({
                "executionId": self.target.execution_id,
                "sessionId": self.target.session_id,
                "jobId": self.target.job_id,
            }),
            title: None,
            noun: "run",
            spec: None,
            parents: Vec::new(),
            ids: Vec::new(),
            resource: None,
            view: ctx.view_id(),
        }
    }

    /// `F`: hand a failed run to `gz jobs fix`. Only once the log stream
    /// reported it Failed, so the fix never reads a live run's logs.
    fn fix(&self, ctx: &mut Ctx) {
        let (Some(job), Some(run)) = (self.target.job_id, self.target.execution_id.clone()) else {
            return ctx.toast(Tone::Error, "This run has no job or run id");
        };
        if self.status() != Some(FAILED) {
            return ctx.toast(Tone::Info, "Only a failed run can be fixed with gz code");
        }
        ctx.effect(Effect::HandOff(super::HandOff::FixRun {
            project: self.target.project_id,
            job,
            run: Some(run),
        }));
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.follower.lines.len().saturating_sub(self.viewport);
        let from = if self.tail { max } else { self.scroll };
        self.scroll = (from as isize + delta).clamp(0, max as isize) as usize;
        self.tail = self.scroll >= max;
    }
}

impl View for RunLog {
    fn title(&self) -> String {
        match &self.target.job_name {
            Some(job) => format!("Run {} · {job}", self.target.short_id()),
            None => format!("Run {}", self.target.short_id()),
        }
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        match self.phase {
            Phase::FindingLatest(_) => self.find_latest(ctx),
            Phase::NoRuns | Phase::Following => self.start_following(ctx),
        }
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let page = self.viewport.max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(page),
            KeyCode::Home | KeyCode::Char('g') => {
                self.tail = false;
                self.scroll = 0;
            }
            KeyCode::End | KeyCode::Char('G') => self.tail = true,
            KeyCode::Char('f') => {
                self.tail = !self.tail;
                if !self.tail {
                    self.scroll = self.follower.lines.len().saturating_sub(self.viewport);
                }
            }
            KeyCode::Char('x') if self.phase == Phase::Following => {
                if self.has_ended() {
                    ctx.toast(
                        Tone::Info,
                        "This run already ended; there is nothing to stop",
                    );
                } else if self.target.session_id.is_none() {
                    ctx.toast(
                        Tone::Error,
                        "This run has no session id yet; try again in a moment",
                    );
                } else {
                    ctx.effect(Effect::RunAction(&STOP, self.stop_row(ctx)));
                }
            }
            KeyCode::Char('R') if !self.follower.is_following() => self.refresh(ctx),
            KeyCode::Char('F') => self.fix(ctx),
            KeyCode::Char('y') => match &self.target.execution_id {
                Some(id) => ctx.copy(id.clone(), format!("run id {id}")),
                None => ctx.toast(Tone::Error, "No run yet"),
            },
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, ctx: &mut Ctx) {
        if self.phase == Phase::FindingLatest(Some(id)) {
            return self.latest_found(result, ctx);
        }
        if self.logs_request == Some(id) {
            return self.logs_answered(result, ctx);
        }
        if self.usual == Usual::Loading(id) {
            return self.usual_answered(result);
        }
        if matches!(self.cost, Cost::Polling { request: Some(r), .. } if r == id) {
            self.cost_answered(result, ctx.now.epoch_ms);
        }
    }

    fn on_tick(&mut self, ctx: &mut Ctx) {
        if self.phase != Phase::Following {
            return;
        }
        if self.logs_request.is_none() && self.follower.due(ctx.now.epoch_ms) {
            self.poll_logs(ctx);
        }
        self.poll_cost(ctx);
    }

    /// `r`, `R`, and after Stop: pick the log up again from the cursor.
    fn refresh(&mut self, ctx: &mut Ctx) {
        match self.phase {
            Phase::Following => {
                self.notice_sent = false;
                self.follower.resume(ctx.now.epoch_ms);
                if self.logs_request.is_none() {
                    self.poll_logs(ctx);
                }
            }
            Phase::NoRuns | Phase::FindingLatest(_) => self.find_latest(ctx),
        }
    }

    fn on_scroll(&mut self, delta: isize) {
        self.scroll_by(delta);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ui: RenderCtx) {
        let tail = if self.tail {
            " tail on · f "
        } else {
            " tail off · f "
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border(true))
            .title(Span::styled(format!(" {} ", self.title()), theme::title()))
            .title(Line::from(Span::styled(tail, theme::muted())).alignment(Alignment::Right))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let width = inner.width as usize;
        let mut lines = self.header_lines(ui, width);
        let header_len = lines.len().max(HEADER_LINES);
        lines.resize(header_len, Line::from(""));
        self.viewport = (inner.height as usize).saturating_sub(header_len).max(1);
        let total = self.follower.lines.len();
        let max = total.saturating_sub(self.viewport);
        let top = if self.tail { max } else { self.scroll.min(max) };
        lines.extend(
            self.follower.lines[top..]
                .iter()
                .take(self.viewport)
                .map(|line| log_line(line, width)),
        );
        frame.render_widget(Paragraph::new(lines), inner);
        if total > self.viewport {
            render_scrollbar(frame, area, max, top);
        }
    }

    fn help(&self) -> Vec<HelpSection> {
        let mut entries = vec![
            ("↑ ↓  PgUp PgDn", "scroll (stops tailing)"),
            ("f  End", "tail the newest lines"),
            STOP_HELP,
            ("F", "fix a failed run with gz code"),
            ("R", "resume after streaming stopped"),
            ("y", "copy the run id"),
            ("r", "reload"),
            ("esc", "back"),
        ];
        if !self.can_stop() {
            entries.retain(|entry| *entry != STOP_HELP);
        }
        vec![HelpSection::new("Run log", &entries)]
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let mut hints = vec![("f", "tail"), STOP_HINT, ("y", "copy id"), ("esc", "back")];
        if !self.can_stop() {
            hints.retain(|hint| *hint != STOP_HINT);
        }
        hints
    }

    fn is_busy(&self) -> bool {
        matches!(self.phase, Phase::FindingLatest(Some(_)))
            || (self.logs_request.is_some() && self.follower.lines.is_empty())
    }
}

impl RunLog {
    fn header_lines(&self, ui: RenderCtx, width: usize) -> Vec<Line<'static>> {
        match self.phase {
            Phase::FindingLatest(_) => return vec![states::loading("the latest run", ui.tick)],
            Phase::NoRuns => {
                return states::empty(
                    "No runs yet. Run the job from its row to see logs here.",
                    &[("R", "run (on the job)"), ("esc", "back")],
                )
            }
            Phase::Following => {}
        }
        let now = ui.now_ms;
        let mut status = vec![status_badge(self.status())];
        if let Some(elapsed) = self.elapsed_ms(now) {
            status.push(Span::styled(
                format!("  {}", duration_text(elapsed)),
                theme::title(),
            ));
        }
        if let Some(sub) = &self.follower.sub_job {
            let state = if sub.finished { " done" } else { "" };
            status.push(Span::styled(
                format!(
                    "  · sub-job {}/{} {}{state}",
                    sub.index, sub.total, sub.name
                ),
                theme::brand(),
            ));
        }
        if let Some(cost) = self.cost_text(now) {
            status.push(Span::styled(format!("  · {cost}"), theme::muted()));
        }
        let mut lines = vec![Line::from(status)];
        let last = self.follower.last_line().map_or_else(
            || "Waiting for the first log line…".to_string(),
            |l| l.text.clone(),
        );
        lines.push(Line::from(vec![
            Span::styled("Last  ", theme::muted()),
            Span::raw(fit(&last, width.saturating_sub(6))),
        ]));
        if let Some(warning) = self.warning(now) {
            lines.push(Line::from(Span::styled(
                fit(&warning, width),
                theme::badge(BadgeTone::Warn),
            )));
        }
        lines
    }

    fn warning(&self, now: i64) -> Option<String> {
        if let Some(stopped) = &self.follower.stopped {
            return Some(stopped.notice());
        }
        if let Some(retrying) = &self.follower.retrying {
            return Some(format!("{retrying} · retrying"));
        }
        if let Some(reason) = &self.follower.stale_reason {
            return Some(reason.clone());
        }
        let active = self.status().is_none_or(|s| !is_terminal(s));
        let usual_ms = match self.usual {
            Usual::Known { seconds, .. } => seconds.map(|s| (s * 1_000.0) as i64),
            _ => None,
        };
        if let Some(elapsed) = self.elapsed_ms(now).filter(|_| active) {
            if follow::may_be_stuck(elapsed, usual_ms, false) {
                let usually = usual_ms
                    .map_or_else(String::new, |u| format!(", usually {}", duration_text(u)));
                return Some(format!(
                    "⚠ May be stuck: running {}{usually}. Check the last line, or stop it with x.",
                    duration_text(elapsed)
                ));
            }
        }
        self.follower
            .is_quiet(now)
            .then(|| "No new logs for 15 minutes. The job may still be running.".to_string())
    }

    /// "≈ $0.01 so far (est.)" while running; the collected cost once known.
    fn cost_text(&self, now: i64) -> Option<String> {
        if let Cost::Final(total) = self.cost {
            return Some(format!("cost {}", usd(total)));
        }
        let Usual::Known {
            usd_per_hour: Some(rate),
            ..
        } = self.usual
        else {
            return None;
        };
        let elapsed = self.elapsed_ms(now)?;
        let estimate = usd(rate * elapsed as f64 / 3_600_000.0);
        Some(if self.follower.is_following() {
            format!("≈ {estimate} so far (est. at {}/h)", usd(rate))
        } else {
            format!("≈ {estimate} (est.; real cost appears once collected)")
        })
    }
}

fn status_badge(status: Option<&str>) -> Span<'static> {
    let (label, tone) = match status {
        None => ("Starting".to_string(), BadgeTone::Muted),
        Some(s) => {
            let tone = match s {
                "Success" => BadgeTone::Good,
                "Failed" => BadgeTone::Bad,
                "Aborted" => BadgeTone::Warn,
                _ => BadgeTone::Info,
            };
            (s.to_string(), tone)
        }
    };
    Span::styled(format!("● {label}"), theme::badge(tone))
}

fn log_line(line: &follow::LogLine, width: usize) -> Line<'static> {
    let style = match line.level {
        Level::Error => theme::badge(BadgeTone::Bad),
        Level::Warning => theme::badge(BadgeTone::Warn),
        Level::Debug => theme::muted(),
        Level::Info | Level::Other => ratatui::style::Style::default(),
    };
    let clock = line.at_ms.map_or_else(String::new, |ms| {
        let text = format::date_time(ms);
        format!("{} ", &text[11..16])
    });
    Line::from(vec![
        Span::styled(clock.clone(), theme::muted()),
        Span::styled(fit(&line.text, width.saturating_sub(clock.len())), style),
    ])
}

fn duration_text(ms: i64) -> String {
    format::show(Some(&json!(ms)), Format::Duration(TimeUnit::Millis), 0).text
}

fn usd(amount: f64) -> String {
    format::show(Some(&json!(amount)), Format::Usd, 0).text
}

#[cfg(test)]
mod tests;
