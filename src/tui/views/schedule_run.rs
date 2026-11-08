//! Run a schedule now, then find the run it started and open its log.
//! `PUT /schedule/run/{id}/{name}` fires the job asynchronously and answers
//! without a run id, so the view notes the schedule's latest runs first and
//! polls `GET /schedule/{id}/runs` until a new one appears.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};
use reqwest::Method;
use serde_json::{json, Value};

use super::run_log::{RunLog, RunTarget};
use super::{Ctx, Handled, RenderCtx, View};
use crate::cmd::seg;
use crate::tui::app::Tone;
use crate::tui::errors::{self, Subject};
use crate::tui::fields;
use crate::tui::format::text_of;
use crate::tui::theme::{self, BadgeTone};
use crate::tui::ui::overlays;

const SERVICE: &str = "schedules";
const POLL_MS: i64 = 3_000;
const GIVE_UP_MS: i64 = 2 * 60_000;
/// The run route validates `cron` (≥ 17 characters) but never uses it.
const IGNORED_CRON: &str = "cron(0 0 * * ? *)";

#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// Reading the runs that already exist.
    Baseline(u64),
    Confirming,
    Starting(u64),
    Waiting {
        request: Option<u64>,
        next_ms: i64,
        until_ms: i64,
    },
    TimedOut,
    Failed(String),
}

pub struct ScheduleRun {
    row: Value,
    id: String,
    name: String,
    known: Vec<String>,
    step: Step,
}

impl ScheduleRun {
    /// From a schedule row: an executions row carries the job details the run needs.
    pub fn new(row: &Value) -> Self {
        let text = |path: &[&str]| {
            fields::lookup_any(row, path)
                .map(text_of)
                .unwrap_or_default()
        };
        Self {
            id: text(&["scheduleId", "Id"]),
            name: text(&["scheduleName", "name"]),
            row: row.clone(),
            known: Vec::new(),
            step: Step::Confirming,
        }
    }

    fn runs_path(&self) -> String {
        format!("/schedule/{}/runs", seg(&self.id))
    }

    fn body(&self) -> Value {
        let row = &self.row;
        json!({
            "cron": IGNORED_CRON,
            "jobDetails": {
                "projectId": row["projectId"], "jobId": row["jobId"],
                "computeId": row["computeId"], "rteName": row["rteName"],
                "projectName": row["projectName"], "jobName": row["jobName"],
            },
            "description": row["description"].as_str().unwrap_or_default(),
            "scheduleType": "BATCH",
        })
    }

    fn execution_ids(runs: &Value) -> Vec<String> {
        runs.as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["executionId"].as_str().map(str::to_string))
            .collect()
    }

    fn explain(err: &str) -> String {
        errors::explain(
            err,
            Subject {
                what: "this schedule",
                service: SERVICE,
            },
        )
    }
}

impl View for ScheduleRun {
    fn title(&self) -> String {
        format!("Run {} now", self.name)
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        self.step = Step::Baseline(ctx.fetch_raw(SERVICE, Method::GET, self.runs_path(), None));
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if self.step != Step::Confirming {
            return Handled::No;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => {
                let path = format!("/schedule/run/{}/{}", seg(&self.id), seg(&self.name));
                self.step =
                    Step::Starting(ctx.fetch_raw(SERVICE, Method::PUT, path, Some(self.body())));
            }
            KeyCode::Char('n') => ctx.pop(),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, ctx: &mut Ctx) {
        let now = ctx.now.epoch_ms;
        match self.step.clone() {
            Step::Baseline(request) if request == id => {
                // Without a baseline every old run would look new.
                match result {
                    Ok(runs) => {
                        self.known = Self::execution_ids(&runs);
                        self.step = Step::Confirming;
                    }
                    Err(err) => self.step = Step::Failed(Self::explain(&err)),
                }
            }
            Step::Starting(request) if request == id => match result {
                Ok(_) => {
                    ctx.toast(Tone::Ok, format!("{} started; finding its run…", self.name));
                    self.step = Step::Waiting {
                        request: None,
                        next_ms: now + POLL_MS,
                        until_ms: now + GIVE_UP_MS,
                    };
                }
                Err(err) => {
                    self.step = Step::Failed(format!("Run failed: {}", Self::explain(&err)))
                }
            },
            Step::Waiting {
                request: Some(request),
                next_ms,
                until_ms,
            } if request == id => {
                let fresh = result.ok().and_then(|runs| {
                    runs.as_array()?
                        .iter()
                        .find(|r| {
                            r["executionId"]
                                .as_str()
                                .is_some_and(|e| !self.known.iter().any(|k| k == e))
                        })
                        .cloned()
                });
                match fresh {
                    Some(run) => {
                        ctx.pop();
                        ctx.push(Box::new(RunLog::new(RunTarget::from_row(&run))));
                    }
                    None if now >= until_ms => self.step = Step::TimedOut,
                    None => {
                        self.step = Step::Waiting {
                            request: None,
                            next_ms: next_ms.max(now) + POLL_MS,
                            until_ms,
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn on_tick(&mut self, ctx: &mut Ctx) {
        if let Step::Waiting {
            request: None,
            next_ms,
            until_ms,
        } = self.step
        {
            if ctx.now.epoch_ms >= next_ms {
                let request = ctx.fetch_raw(SERVICE, Method::GET, self.runs_path(), None);
                self.step = Step::Waiting {
                    request: Some(request),
                    next_ms,
                    until_ms,
                };
            }
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ui: RenderCtx) {
        let (text, tone) = match &self.step {
            Step::Baseline(_) => (
                "Reading this schedule's runs…".to_string(),
                BadgeTone::Muted,
            ),
            Step::Confirming => (
                format!(
                    "Run {} now? It starts {} on {} (paid compute). ⏎/y run · esc cancel",
                    self.name,
                    text_of(&self.row["jobName"]),
                    text_of(&self.row["rteName"])
                ),
                BadgeTone::Warn,
            ),
            Step::Starting(_) => ("Starting…".to_string(), BadgeTone::Muted),
            Step::Waiting { .. } => (
                "Started. Waiting for the run to appear (this can take a few seconds)…".to_string(),
                BadgeTone::Info,
            ),
            Step::TimedOut => (
                "The run hasn't appeared after 2 minutes. It may have failed before it \
                 registered; check the schedule's recent runs."
                    .to_string(),
                BadgeTone::Warn,
            ),
            Step::Failed(err) => (err.clone(), BadgeTone::Bad),
        };
        let inner = overlays::dialog(frame, area, &self.title(), 4, false);
        let line = Line::from(Span::styled(text, theme::badge(tone)));
        frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), inner);
    }

    fn is_busy(&self) -> bool {
        matches!(
            self.step,
            Step::Baseline(_) | Step::Starting(_) | Step::Waiting { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::tui::app::test_support::*;
    use crate::tui::fetch::Request;

    fn row() -> Value {
        json!({"scheduleId": 1102, "scheduleName": "nightly etl", "projectId": 1000, "jobId": 1000,
               "computeId": 1000, "rteName": "gz-rte-duckdb-sklearn:1.0.0", "projectName": "ETL",
               "jobName": "testRunjob", "description": ""})
    }

    fn reply(app: &mut crate::tui::app::App, path: &str, value: Value) {
        let req = app
            .outbox
            .iter()
            .rev()
            .find_map(|r| match r {
                Request::Fetch(f) if f.path == path => Some(f.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no request to {path}"));
        app.fetched(req.id, req.target, Ok(value));
    }

    #[test]
    fn confirms_runs_and_opens_the_new_runs_log() {
        let mut app = app();
        let start = app.now;
        app.push_view(Box::new(ScheduleRun::new(&row())));
        reply(
            &mut app,
            "/schedule/1102/runs",
            json!([{"executionId": "old"}]),
        );
        assert!(draw(&mut app, 100, 20).contains("paid compute"));
        press(&mut app, &[KeyCode::Enter]);
        let run = last_fetch(&app);
        assert_eq!(
            (run.method.as_str(), run.path.as_str()),
            ("PUT", "/schedule/run/1102/nightly%20etl")
        );
        assert_eq!(
            run.body.as_ref().unwrap()["jobDetails"]["rteName"],
            "gz-rte-duckdb-sklearn:1.0.0"
        );
        assert!(run.body.as_ref().unwrap()["cron"].as_str().unwrap().len() >= 17);
        answer(&mut app, json!({"status": "success"}));
        app.on_tick(start.advanced(Duration::from_secs(3)));
        reply(
            &mut app,
            "/schedule/1102/runs",
            json!([{"executionId": "old"}]),
        );
        app.on_tick(start.advanced(Duration::from_secs(6)));
        reply(
            &mut app,
            "/schedule/1102/runs",
            json!([{"executionId": "fresh123abc", "sessionId": "s", "jobName": "testRunjob"}, {"executionId": "old"}]),
        );
        assert_eq!(app.views.titles(), vec!["Run fresh123 · testRunjob"]);
    }
}
