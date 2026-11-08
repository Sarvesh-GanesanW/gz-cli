//! Creating a schedule for a job, or changing an existing schedule's
//! timing, with a live plain-English preview of the cron and its next runs.
//!
//! Edit sends `PUT schedules /schedule/{id}/{name}` with the **whole**
//! record: `fn_u_schedule` overwrites every column (anything left out is
//! nulled) and re-enables the schedule, so the form reads the record fresh,
//! merges the edited fields and warns that saving resumes a paused
//! schedule. Create is `POST schedules /schedule` with the web's body,
//! `config` included so the web's details modal shows TUI-made schedules.

use crossterm::event::KeyEvent;
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    Frame,
};
use regex::Regex;
use reqwest::Method;
use serde_json::{json, Value};

use super::cron;
use super::form_host::HostedForm;
use super::{Ctx, Handled, RenderCtx, RowContext, View};
use crate::cmd::seg;
use crate::tui::app::Tone;
use crate::tui::errors::{self, Subject};
use crate::tui::fields;
use crate::tui::format::{self, text_of};
use crate::tui::nav::Source;
use crate::tui::overlay::form::Outcome;
use crate::tui::screen::{Choices, FieldKind, FormField, FormSpec};
use crate::tui::template::Scope;
use crate::tui::theme::{self, BadgeTone};

const SERVICE: &str = "schedules";
const TIME_PATTERN: &str = r"\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}";
const TIME_MESSAGE: &str = "Write it as YYYY-MM-DD HH:MM (UTC)";
/// Backend rule plus EventBridge's 64-character limit on schedule names.
const SCHEDULE_NAME_PATTERN: &str = r"[.\-_A-Za-z0-9]+";
const SCHEDULE_NAME_MAX: usize = 64;
/// Only these runtimes pass the schedule schema (`schedule.schema.mjs`).
const SCHEDULABLE_RTE: &str = r"^gz-rte-[a-zA-Z-]+:\d+\.\d+\.\d+$";
/// A start in the past is moved this far ahead, so the scheduler accepts it.
const START_LEAD_MS: i64 = 2 * 60_000;
const BATCH: &str = "BATCH";
const STREAMING: &str = "STREAMING";
const PREVIEW_RUNS: usize = 3;

static CREATE_FORM: FormSpec = FormSpec {
    title: "Schedule this job",
    submit: "Create schedule",
    fields: &[
        FormField::new("name", "Schedule name", FieldKind::Text)
            .required()
            .pattern(
                SCHEDULE_NAME_PATTERN,
                "Letters, numbers, . - and _ only (no spaces)",
            )
            .max_len(SCHEDULE_NAME_MAX),
        FormField::new(
            "type",
            "Type",
            FieldKind::Select(Choices::Static(&[
                (BATCH, "Repeats on the cron (batch)"),
                (STREAMING, "Starts once at the start time (streaming)"),
            ])),
        ),
        FormField::new("cron", "Runs (AWS cron, UTC)", FieldKind::Text)
            .default("cron(0 9 ? * MON-FRI *)")
            .hint("minute hour day month weekday year; weekday 1 = Sunday; one day field is ?"),
        FormField::new("start", "Start (UTC)", FieldKind::Text)
            .required()
            .default("{$start}")
            .pattern(TIME_PATTERN, TIME_MESSAGE),
        FormField::new("end", "End (UTC, empty = never)", FieldKind::Text)
            .pattern(TIME_PATTERN, TIME_MESSAGE),
        FormField::new(
            "rte",
            "Runtime",
            FieldKind::Select(Choices::Endpoint {
                source: Source::get("rtes", "/rte/image"),
                value: &["Name"],
                label: &["Name"],
            }),
        )
        .required(),
        FormField::new(
            "compute",
            "Compute",
            FieldKind::Select(Choices::Endpoint {
                source: Source::get("rtes", "/rte/compute"),
                value: &["Id"],
                label: &["Description", "Name"],
            }),
        )
        .required(),
        FormField::new("description", "Description", FieldKind::Text),
    ],
};

static EDIT_FORM: FormSpec = FormSpec {
    title: "Change schedule",
    submit: "Save",
    fields: &[
        FormField::new("cron", "Runs (AWS cron, UTC)", FieldKind::Text)
            .default("{cron}")
            .hint("minute hour day month weekday year; weekday 1 = Sunday; one day field is ?"),
        FormField::new("start", "Start (UTC)", FieldKind::Text)
            .required()
            .default("{$start}")
            .pattern(TIME_PATTERN, TIME_MESSAGE),
        FormField::new("end", "End (UTC, empty = never)", FieldKind::Text)
            .default("{$end}")
            .pattern(TIME_PATTERN, TIME_MESSAGE),
        FormField::new("description", "Description", FieldKind::Text).default("{description}"),
    ],
};

enum Mode {
    /// For the job in `row` (its first parent is the project).
    Create { row: RowContext },
    /// The schedule `id` called `name`; `listed` is its list row.
    Edit {
        id: String,
        name: String,
        listed: Value,
        record: Option<Value>,
    },
}

pub struct ScheduleForm {
    mode: Mode,
    form: Option<HostedForm>,
    loading: Option<u64>,
    saving: Option<u64>,
    error: Option<String>,
}

impl ScheduleForm {
    pub fn create(row: &RowContext) -> Self {
        Self::with(Mode::Create { row: row.clone() })
    }

    /// From a schedule row (`scheduleId`/`Id` and `scheduleName`/`name`).
    pub fn edit(row: &Value) -> Self {
        let text = |path: &[&str]| {
            fields::lookup_any(row, path)
                .map(text_of)
                .unwrap_or_default()
        };
        Self::with(Mode::Edit {
            id: text(&["scheduleId", "Id"]),
            name: text(&["scheduleName", "name"]),
            listed: row.clone(),
            record: None,
        })
    }

    fn with(mode: Mode) -> Self {
        Self {
            mode,
            form: None,
            loading: None,
            saving: None,
            error: None,
        }
    }

    fn values(&self) -> Value {
        self.form
            .as_ref()
            .map_or(Value::Null, |f| f.form.values_json())
    }

    fn schedule_type(&self) -> String {
        match &self.mode {
            Mode::Create { .. } => self.values()["type"].as_str().unwrap_or(BATCH).to_string(),
            Mode::Edit { record, .. } => record
                .as_ref()
                .and_then(|r| r["scheduleType"].as_str())
                .unwrap_or(BATCH)
                .to_string(),
        }
    }

    /// The cron to send: typed for batch, a one-shot at the start for streaming.
    fn cron_to_send(&self) -> Result<String, String> {
        let values = self.values();
        if self.schedule_type() == STREAMING {
            let start = time_ms(values["start"].as_str().unwrap_or_default())
                .ok_or_else(|| TIME_MESSAGE.to_string())?;
            return Ok(one_shot_cron(start));
        }
        let typed = values["cron"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        match cron::parse(&typed).map_err(|e| e.to_string())? {
            cron::Expression::Cron(_) => Ok(if typed.starts_with("cron(") {
                typed
            } else {
                format!("cron({typed})")
            }),
            // The schema wants ≥17 characters, which most rate()s aren't; the web writes cron only.
            _ => Err("Write a cron(...) expression; rate() and at() can't be saved here".into()),
        }
    }

    fn preview(&self, now_ms: i64) -> Vec<Line<'static>> {
        let values = self.values();
        let mut lines = Vec::new();
        match self.cron_to_send() {
            Err(err) => lines.push(Line::from(Span::styled(
                format!("✗ {err}"),
                theme::badge(BadgeTone::Bad),
            ))),
            Ok(text) => {
                let expression = cron::parse(&text).ok();
                let described = expression.as_ref().map_or_else(String::new, cron::describe);
                lines.push(Line::from(vec![
                    Span::styled("Runs  ", theme::muted()),
                    Span::styled(described, theme::title()),
                ]));
                let start = time_ms(values["start"].as_str().unwrap_or_default());
                let end = time_ms(values["end"].as_str().unwrap_or_default());
                let from = start.map_or(now_ms, |s| s.max(now_ms) - 1);
                let next = expression
                    .map(|e| cron::next_runs(&e, from, end, PREVIEW_RUNS))
                    .unwrap_or_default();
                let next_text = if next.is_empty() {
                    "no upcoming runs".to_string()
                } else {
                    next.iter()
                        .map(|t| format::date_time(*t))
                        .collect::<Vec<_>>()
                        .join(" · ")
                };
                lines.push(Line::from(vec![
                    Span::styled("Next  ", theme::muted()),
                    Span::raw(next_text),
                ]));
            }
        }
        if let Mode::Edit { record, .. } = &self.mode {
            let paused = record
                .as_ref()
                .is_some_and(|r| r["enabled"] == json!(false));
            let warning = if paused {
                "⚠ This schedule is paused. Saving resumes it."
            } else {
                "Saving replaces the schedule's timing and keeps it running."
            };
            lines.push(Line::from(Span::styled(
                warning,
                theme::badge(BadgeTone::Warn),
            )));
        }
        lines
    }

    fn check(&self) -> Result<(), String> {
        self.cron_to_send()?;
        let values = self.values();
        if let (Some(start), Some(end)) = (
            time_ms(values["start"].as_str().unwrap_or_default()),
            time_ms(values["end"].as_str().unwrap_or_default()),
        ) {
            if end <= start {
                return Err("The end must be after the start".into());
            }
        }
        if let Some(rte) = values["rte"].as_str() {
            let schedulable = Regex::new(SCHEDULABLE_RTE).is_ok_and(|re| re.is_match(rte));
            if !schedulable {
                return Err(format!("{rte} can't be scheduled; pick a gz-rte runtime"));
            }
        }
        Ok(())
    }

    fn submit(&mut self, ctx: &mut Ctx) {
        if let Err(err) = self.check() {
            self.error = Some(err);
            return;
        }
        let Ok(cron_text) = self.cron_to_send() else {
            return;
        };
        let values = self.values();
        let request = match &self.mode {
            Mode::Create { row } => create_body(row, &values, &cron_text)
                .map(|body| (Method::POST, "/schedule".to_string(), body)),
            Mode::Edit {
                id,
                name,
                listed,
                record,
            } => record
                .as_ref()
                .map(|record| edit_body(record, listed, &values, &cron_text))
                .map(|body| {
                    (
                        Method::PUT,
                        format!("/schedule/{}/{}", seg(id), seg(name)),
                        body,
                    )
                }),
        };
        match request {
            Some((method, path, body)) => {
                self.error = None;
                self.saving = Some(ctx.fetch_raw(SERVICE, method, path, Some(body)));
            }
            None => self.error = Some("This job has no project".into()),
        }
    }

    fn saved(&mut self, ctx: &mut Ctx) {
        let message = match &self.mode {
            Mode::Create { .. } => "Job scheduled successfully".to_string(),
            Mode::Edit { name, .. } => {
                format!("{name} is successfully rescheduled · r reloads the list")
            }
        };
        ctx.toast(Tone::Ok, message);
        ctx.pop();
    }

    fn open_form(&mut self, ctx: &mut Ctx) {
        let now = ctx.now.epoch_ms;
        let site = ctx.site.clone();
        let form = match &self.mode {
            Mode::Create { row } => {
                let scope = row
                    .scope(None, site.as_deref())
                    .var("start", time_text(now + START_LEAD_MS));
                HostedForm::open(&CREATE_FORM, &scope, ctx)
            }
            Mode::Edit {
                record: Some(record),
                ..
            } => {
                let saved_start =
                    fields::lookup_any(record, &["startAt"]).and_then(format::epoch_ms);
                let start = saved_start
                    .filter(|s| *s > now)
                    .unwrap_or(now + START_LEAD_MS);
                let end = fields::lookup_any(record, &["stopAt"])
                    .and_then(format::epoch_ms)
                    .map_or_else(String::new, time_text);
                let scope = Scope::default()
                    .nearest(record)
                    .var("start", time_text(start))
                    .var("end", end);
                HostedForm::open(&EDIT_FORM, &scope, ctx)
            }
            Mode::Edit { record: None, .. } => return,
        };
        self.form = Some(form);
    }
}

impl View for ScheduleForm {
    fn title(&self) -> String {
        match &self.mode {
            Mode::Create { row } => format!("Schedule {}", row.title.clone().unwrap_or_default()),
            Mode::Edit { name, .. } => format!("Change {name}"),
        }
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        match &self.mode {
            Mode::Create { .. } => self.open_form(ctx),
            Mode::Edit { id, .. } => {
                let path = format!("/schedule/{}", seg(id));
                self.loading = Some(ctx.fetch_raw(SERVICE, Method::GET, path, None));
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if self.saving.is_some() {
            return Handled::Yes;
        }
        let Some(form) = self.form.as_mut() else {
            return Handled::No;
        };
        match form.on_key(key) {
            Outcome::Editing => self.error = None,
            Outcome::Cancelled => ctx.pop(),
            Outcome::Submitted(_) => self.submit(ctx),
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, ctx: &mut Ctx) {
        let explain = |err: &str| {
            errors::explain(
                err,
                Subject {
                    what: "this schedule",
                    service: SERVICE,
                },
            )
        };
        if self.loading == Some(id) {
            self.loading = None;
            match result {
                Ok(record) => {
                    if let Mode::Edit { record: slot, .. } = &mut self.mode {
                        *slot = Some(record);
                    }
                    self.open_form(ctx);
                }
                Err(err) => self.error = Some(explain(&err)),
            }
            return;
        }
        if self.saving == Some(id) {
            self.saving = None;
            match result {
                Ok(_) => self.saved(ctx),
                Err(err) => self.error = Some(format!("Save failed: {}", explain(&err))),
            }
            return;
        }
        if let Some(form) = self.form.as_mut() {
            form.on_fetched(id, result);
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ui: RenderCtx) {
        let Some(form) = &self.form else {
            let text = self
                .error
                .clone()
                .unwrap_or_else(|| "Loading the schedule…".into());
            frame.render_widget(ratatui::widgets::Paragraph::new(text), area);
            return;
        };
        let mut extra = self.preview(ui.now_ms);
        if self.saving.is_some() {
            extra.push(Line::from(Span::styled("Saving…", theme::muted())));
        }
        if let Some(err) = &self.error {
            extra.push(Line::from(Span::styled(
                err.clone(),
                theme::badge(BadgeTone::Bad),
            )));
        }
        form.render(frame, area, extra);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("tab", "next"), ("⏎", "save"), ("esc", "cancel")]
    }

    fn is_typing(&self) -> bool {
        self.form.is_some()
    }

    fn is_busy(&self) -> bool {
        self.loading.is_some() || self.saving.is_some()
    }
}

/// "YYYY-MM-DD HH:MM" (UTC) for a form field.
fn time_text(ms: i64) -> String {
    format::date_time(ms).trim_end_matches(" UTC").to_string()
}

fn time_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    (!text.is_empty()).then(|| format::epoch_ms(&Value::String(text.to_string())))?
}

fn iso(ms: i64) -> String {
    format!(
        "{}:00.000Z",
        format::date_time(ms)
            .trim_end_matches(" UTC")
            .replace(' ', "T")
    )
}

/// The web's streaming schedule: one run at the start minute, `cron(M H D Mo ? YYYY)`.
fn one_shot_cron(start_ms: i64) -> String {
    let text = format::date_time(start_ms);
    let (date, clock) = (&text[..10], &text[11..16]);
    let num = |s: &str| s.trim_start_matches('0').parse::<u32>().unwrap_or(0);
    format!(
        "cron({} {} {} {} ? {})",
        num(&clock[3..5]),
        num(&clock[..2]),
        num(&date[8..10]),
        num(&date[5..7]),
        &date[..4]
    )
}

fn create_body(row: &RowContext, values: &Value, cron_text: &str) -> Option<Value> {
    let project = row.parents.first()?;
    let job = &row.row;
    let id = |v: &Value| fields::lookup_any(v, &["Id", "id"]).and_then(Value::as_i64);
    let name_of =
        |v: &Value| fields::lookup_any(v, &["Name", "name"]).map_or_else(String::new, text_of);
    let (project_id, job_id) = (id(project)?, id(job)?);
    let compute: i64 = values["compute"].as_str()?.parse().ok()?;
    let rte = values["rte"].as_str()?;
    let kind = values["type"].as_str().unwrap_or(BATCH);
    let start = iso(time_ms(values["start"].as_str()?)?);
    let end = values["end"]
        .as_str()
        .and_then(time_ms)
        .filter(|_| kind == BATCH)
        .map(iso);
    let name = values["name"].as_str()?;
    let description = values["description"].as_str().unwrap_or_default();
    let mut body = json!({
        "name": name,
        "cron": cron_text,
        "description": description,
        "startTime": start,
        "scheduleType": kind,
        "jobDetails": {
            "projectName": name_of(project), "projectId": project_id,
            "jobName": name_of(job), "jobId": job_id,
            "computeId": compute, "rteName": rte,
        },
        "config": {
            "name": name, "startTime": start, "endTime": end,
            "scheduleType": if kind == STREAMING { "Streaming" } else { "Batch" },
            "selectedProject": {"Id": project_id, "Name": name_of(project)},
            "selectedJob": {"Id": job_id, "Name": name_of(job)},
            "selectedCompute": {"computeId": compute},
            "selectedRte": rte,
            "schedulerDropdownEnv": "",
            "scheduledJobDescription": description,
            "endsOn": if end.is_some() { "On" } else { "Never" },
            "whenToRepeat": "Custom",
            "repeatInterval": {},
        },
    });
    if let Some(end) = end {
        body["stopTime"] = json!(end);
    }
    Some(body)
}

/// The saved record with the edited fields laid over it. `GET /schedule/{id}`
/// has no project or job names, so they come from the list row or `config`.
fn edit_body(record: &Value, listed: &Value, values: &Value, cron_text: &str) -> Value {
    let text = |paths: &[&str]| {
        fields::lookup_any(listed, paths)
            .or_else(|| fields::lookup_any(record, paths))
            .map_or_else(String::new, text_of)
    };
    let start = values["start"].as_str().and_then(time_ms).map(iso);
    let end = values["end"].as_str().and_then(time_ms).map(iso);
    let mut config = record["config"].clone();
    if !config.is_object() {
        config = json!({});
    }
    config["startTime"] = json!(start);
    config["endTime"] = json!(end);
    config["endsOn"] = json!(if end.is_some() { "On" } else { "Never" });
    config["scheduledJobDescription"] = values["description"].clone();
    let mut body = json!({
        "cron": cron_text,
        "description": values["description"],
        "scheduleType": record["scheduleType"],
        "startTime": start,
        "jobDetails": {
            "projectId": record["projectId"], "jobId": record["jobId"],
            "computeId": record["computeId"], "rteName": record["rteName"],
            "projectName": text(&["projectName", "config.selectedProject.Name"]),
            "jobName": text(&["jobName", "config.selectedJob.Name"]),
        },
        "config": config,
    });
    if let Some(end) = end {
        body["stopTime"] = json!(end);
    }
    body
}

#[cfg(test)]
mod tests;
