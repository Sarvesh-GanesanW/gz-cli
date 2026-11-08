//! Run now: pick a runtime and compute like the web's runtime picker, see
//! what will start, confirm (it starts paid pods), then follow the run in
//! the log viewer. The submit body is gz's own `jobs::run_spec`, built from
//! the saved job, so a TUI run matches `gz jobs run` and the web editor.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    Frame,
};
use reqwest::Method;
use serde_json::Value;

use super::form_host::HostedForm;
use super::run_log::{RunLog, RunTarget};
use super::{Ctx, Handled, RenderCtx, RowContext, View};
use crate::cli::RunChoiceArgs;
use crate::cmd::jobs::run_spec;
use crate::tui::app::Tone;
use crate::tui::errors::{self, Subject};
use crate::tui::fields;
use crate::tui::format::text_of;
use crate::tui::nav::Source;
use crate::tui::overlay::form::Outcome;
use crate::tui::screen::{Choices, FieldKind, FormField, FormSpec};
use crate::tui::theme::{self, BadgeTone};

static RUN_FORM: FormSpec = FormSpec {
    title: "Run job",
    submit: "Review",
    fields: &[
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
        FormField::new(
            "workers",
            "Minimum workers",
            FieldKind::Number {
                min: Some(1.0),
                max: None,
            },
        )
        .default("{config.minimumWorkers|$defaultWorkers}")
        .hint("Spark runtimes only; others run one worker"),
    ],
};

/// The web's default when a job has no saved worker count.
const DEFAULT_WORKERS: &str = "2";

#[derive(Debug, Clone)]
enum Step {
    Choosing,
    /// Showing what will start; Enter runs it.
    Reviewing(RunChoiceArgs),
    Submitting(u64),
}

pub struct RunJob {
    row: RowContext,
    form: Option<HostedForm>,
    /// The saved job (its config holds connections and sub-jobs).
    saved: Option<Value>,
    saved_request: Option<u64>,
    step: Step,
    error: Option<String>,
}

impl RunJob {
    /// `row` is a job; its first parent is the project.
    pub fn new(row: &RowContext) -> Self {
        Self {
            row: row.clone(),
            form: None,
            saved: None,
            saved_request: None,
            step: Step::Choosing,
            error: None,
        }
    }

    fn project(&self) -> Option<&Value> {
        self.row.parents.first()
    }

    fn ids(&self) -> Option<(i64, i64)> {
        let id = |v: &Value| fields::lookup_any(v, &["Id", "id"]).and_then(Value::as_i64);
        Some((id(self.project()?)?, id(&self.row.row)?))
    }

    fn job_name(&self) -> String {
        fields::lookup_any(&self.row.row, &["Name", "name"]).map_or_else(String::new, text_of)
    }

    fn config(&self) -> &Value {
        let saved = self
            .saved
            .as_ref()
            .map(|s| &s["config"])
            .filter(|c| c.is_object());
        saved.unwrap_or(&self.row.row["config"])
    }

    /// The web's blocker: a job needs a saved connection to run.
    fn blocker(&self) -> Option<&'static str> {
        let has_connection = self.config()["connectionNames"]
            .as_array()
            .is_some_and(|c| !c.is_empty());
        (!has_connection).then_some(
            "This job has no connection. Pick one in the web job editor, then run it here.",
        )
    }

    fn choice(values: &Value) -> Option<RunChoiceArgs> {
        Some(RunChoiceArgs {
            compute: values["compute"].as_str()?.parse().ok()?,
            rte: values["rte"].as_str()?.to_string(),
            workers: values["workers"].as_u64().map(|w| w as u32),
            gpu: None,
        })
    }

    fn submit(&mut self, choice: RunChoiceArgs, ctx: &mut Ctx) {
        let Some((project_id, job_id)) = self.ids() else {
            self.error = Some("This job has no project or id".into());
            return;
        };
        let project_name = self
            .project()
            .and_then(|p| fields::lookup_any(p, &["Name", "name"]))
            .map_or_else(String::new, text_of);
        let saved = serde_json::json!({ "config": self.config().clone() });
        let body = run_spec(
            &project_name,
            project_id,
            job_id,
            &self.job_name(),
            &saved,
            &choice,
        );
        let id = ctx.fetch_raw("etl", Method::POST, "/etl/job".to_string(), Some(body));
        self.step = Step::Submitting(id);
    }

    fn started(&mut self, answer: Value, ctx: &mut Ctx) {
        let Some((project_id, job_id)) = self.ids() else {
            return;
        };
        let mut target = RunTarget::from_row(&answer);
        target.job_id = Some(job_id);
        target.project_id = Some(project_id);
        target.job_name = Some(self.job_name());
        target.started_ms = Some(ctx.now.epoch_ms);
        if target.execution_id.is_none() {
            self.step = Step::Choosing;
            self.error = Some("The run started but returned no run id to follow".into());
            return;
        }
        ctx.toast(Tone::Ok, format!("Run of {} started", self.job_name()));
        ctx.pop();
        ctx.push(Box::new(RunLog::new(target)));
    }

    fn review_lines(&self, choice: &RunChoiceArgs) -> Vec<Line<'static>> {
        let compute = self
            .form
            .as_ref()
            .and_then(|f| f.choice_label("compute"))
            .unwrap_or_else(|| choice.compute.to_string());
        let sub_jobs = self.config()["selectedJobs"].as_array().map_or(0, Vec::len);
        let mut lines = vec![Line::from(Span::styled(
            format!("Run {} on {compute} with {}?", self.job_name(), choice.rte),
            theme::title(),
        ))];
        if sub_jobs > 0 {
            lines.push(Line::from(format!("It also runs {sub_jobs} sub-job(s).")));
        }
        lines.push(Line::from(Span::styled(
            "This starts paid compute. ⏎/y run · esc change",
            theme::badge(BadgeTone::Warn),
        )));
        lines
    }
}

impl View for RunJob {
    fn title(&self) -> String {
        format!("Run {}", self.job_name())
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        let site = ctx.site.clone();
        let scope = self
            .row
            .scope(None, site.as_deref())
            .var("defaultWorkers", DEFAULT_WORKERS);
        self.form = Some(HostedForm::open(&RUN_FORM, &scope, ctx));
        if let Some((project, job)) = self.ids() {
            let path = format!("/etl/etlJobs/jobs/{project}/{job}");
            self.saved_request = Some(ctx.fetch_raw("etlprojects", Method::GET, path, None));
        }
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        match self.step.clone() {
            Step::Submitting(_) => {}
            Step::Reviewing(choice) => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => self.submit(choice, ctx),
                KeyCode::Esc | KeyCode::Char('n') => self.step = Step::Choosing,
                _ => {}
            },
            Step::Choosing => {
                let Some(form) = self.form.as_mut() else {
                    return Handled::No;
                };
                match form.on_key(key) {
                    Outcome::Editing => {}
                    Outcome::Cancelled => ctx.pop(),
                    Outcome::Submitted(values) => {
                        if let Some(blocker) = self.blocker() {
                            self.error = Some(blocker.to_string());
                        } else if let Some(choice) = Self::choice(&values) {
                            self.error = None;
                            self.step = Step::Reviewing(choice);
                        }
                    }
                }
            }
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, ctx: &mut Ctx) {
        if self.saved_request == Some(id) {
            self.saved_request = None;
            self.saved = result.ok();
            return;
        }
        if matches!(self.step, Step::Submitting(request) if request == id) {
            match result {
                Ok(answer) => self.started(answer, ctx),
                Err(err) => {
                    self.step = Step::Choosing;
                    let reason = errors::explain(
                        &err,
                        Subject {
                            what: "this job",
                            service: "etl",
                        },
                    );
                    self.error = Some(format!("Run failed: {reason}"));
                }
            }
            return;
        }
        if let Some(form) = self.form.as_mut() {
            form.on_fetched(id, result);
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ui: RenderCtx) {
        let Some(form) = &self.form else {
            return;
        };
        let mut extra = Vec::new();
        match &self.step {
            Step::Reviewing(choice) => extra.extend(self.review_lines(choice)),
            Step::Submitting(_) => extra.push(Line::from(Span::styled(
                "Starting the run…",
                theme::muted(),
            ))),
            Step::Choosing => {
                if let Some(blocker) = self.blocker().filter(|_| self.saved_request.is_none()) {
                    extra.push(Line::from(Span::styled(
                        blocker,
                        theme::badge(BadgeTone::Warn),
                    )));
                }
            }
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
        vec![
            ("tab", "next"),
            ("←→", "choose"),
            ("⏎", "review"),
            ("esc", "cancel"),
        ]
    }

    fn is_typing(&self) -> bool {
        true
    }

    fn is_busy(&self) -> bool {
        matches!(self.step, Step::Submitting(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;
    use crate::tui::app::App;
    use crate::tui::fetch::{FetchReq, Request};
    use serde_json::json;

    fn job_row() -> RowContext {
        RowContext {
            row: json!({"Id": 1006, "Name": "test Run job",
                        "config": {"connectionNames": ["GZS3"], "minimumWorkers": 3, "selectedJobs": []}}),
            title: Some("test Run job".into()),
            noun: "job",
            spec: None,
            parents: vec![json!({"Id": 1000, "Name": "ETL"})],
            ids: vec!["1000".into(), "1006".into()],
            resource: None,
            view: None,
        }
    }

    fn request_to(app: &App, path: &str) -> FetchReq {
        app.outbox
            .iter()
            .rev()
            .find_map(|r| match r {
                Request::Fetch(f) if f.path == path => Some(f.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no request to {path}"))
    }

    fn reply(app: &mut App, path: &str, value: Value) {
        let req = request_to(app, path);
        app.fetched(req.id, req.target, Ok(value));
    }

    fn opened() -> App {
        let mut app = app();
        app.push_view(Box::new(RunJob::new(&job_row())));
        reply(
            &mut app,
            "/rte/image",
            json!([{"Name": "gz-rte-duckdb-sklearn:1.0.0"}]),
        );
        reply(
            &mut app,
            "/rte/compute",
            json!([{"Id": 1003, "Name": "xsmall", "Description": "Extra small"}]),
        );
        reply(
            &mut app,
            "/etl/etlJobs/jobs/1000/1006",
            json!({"config": {"connectionNames": ["GZS3"], "minimumWorkers": 3,
                              "selectedJobs": [{"Id": 1007, "Name": "child"}]}}),
        );
        app
    }

    #[test]
    fn reviews_before_starting_paid_compute_then_sends_the_run_spec() {
        let mut app = opened();
        press(&mut app, &[KeyCode::Enter]);
        let screen = draw(&mut app, 100, 30);
        assert!(
            screen.contains("Run test Run job on Extra small"),
            "{screen}"
        );
        assert!(screen.contains("starts paid compute"), "{screen}");
        assert!(!app
            .outbox
            .iter()
            .any(|r| matches!(r, Request::Fetch(f) if f.path == "/etl/job")));
        press(&mut app, &[KeyCode::Enter]);
        let run = request_to(&app, "/etl/job");
        assert_eq!(run.method, Method::POST);
        assert_eq!(
            run.body.unwrap(),
            json!({"ConnectionNames": ["GZS3"], "projectName": "ETL", "projectId": 1000,
                   "jobName": "testRunjob", "jobId": 1006, "computeId": 1003,
                   "rteName": "gz-rte-duckdb-sklearn:1.0.0", "minimumWorkers": 3, "gpuSize": 0,
                   "subJobIds": [1007], "subJobNames": ["child"]})
        );
    }

    #[test]
    fn a_started_run_opens_its_log() {
        let mut app = opened();
        press(&mut app, &[KeyCode::Enter, KeyCode::Enter]);
        reply(
            &mut app,
            "/etl/job",
            json!({"jobIdentifier": "abc123", "sessionId": "s1"}),
        );
        assert_eq!(app.views.len(), 1);
        assert_eq!(app.views.titles(), vec!["Run abc123 · test Run job"]);
        let logs = request_to(&app, "/etl/logs");
        assert_eq!(
            logs.body.unwrap(),
            json!({"jobIdentifier": "abc123", "sessionId": "s1"})
        );
    }

    #[test]
    fn jobs_without_a_connection_cannot_run() {
        let mut app = app();
        let mut row = job_row();
        row.row["config"]["connectionNames"] = json!([]);
        app.push_view(Box::new(RunJob::new(&row)));
        reply(
            &mut app,
            "/etl/etlJobs/jobs/1000/1006",
            json!({"config": {}}),
        );
        press(&mut app, &[KeyCode::Enter]);
        assert!(draw(&mut app, 100, 30).contains("This job has no connection"));
    }

    #[test]
    fn server_refusals_are_shown_in_the_form() {
        let mut app = opened();
        press(&mut app, &[KeyCode::Enter, KeyCode::Enter]);
        let req = request_to(&app, "/etl/job");
        app.fetched(
            req.id,
            req.target,
            Err("POST u → HTTP 400 Bad Request: Invalid RTE.".into()),
        );
        assert!(draw(&mut app, 100, 30).contains("Run failed: Invalid RTE."));
    }
}
