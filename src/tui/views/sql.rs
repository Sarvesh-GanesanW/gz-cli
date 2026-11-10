//! Running a query on a lakehouse warehouse from the TUI, through the same
//! client `gz sql` uses (`crate::sql::session::Lakehouse`). Starting a
//! session launches a Spark pod (a minute or more, and it uses compute), so
//! it is an explicit step: pick a compute, press Enter, watch it start. A
//! running session the profile remembers for the warehouse is reused, and a
//! new one is remembered, so `gz sql` and the TUI share it.

use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
    Frame,
};
use serde_json::{json, Value};

use super::{Ctx, Handled, RenderCtx, View};
use crate::http::ApiClient;
use crate::sql::results::{self, Page, Reply};
use crate::sql::session::{pick_warehouse, Compute, Lakehouse, RunningQuery};
use crate::tui::app::Tone;
use crate::tui::help::HelpSection;
use crate::tui::theme;
use crate::tui::ui::widgets::render_scrollbar;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// Looking up the warehouse, a session to reuse and the compute sizes.
    Preparing(u64),
    /// No session yet: pick a compute and press Enter to start one.
    Choose,
    Starting(u64),
    /// A session is ready and nothing is running.
    Ready,
    Running(u64),
    Failed(String),
}

/// What the last statement produced.
#[derive(Debug, Clone, PartialEq)]
enum Output {
    Rows(Page, f64),
    Message(String),
}

pub struct SqlView {
    warehouse: String,
    query: String,
    editing: bool,
    step: Step,
    computes: Vec<Compute>,
    compute: usize,
    warehouse_id: Option<i64>,
    session: Option<String>,
    output: Option<Output>,
    /// When the current step started (epoch ms), for the elapsed time.
    since_ms: i64,
    running: RunningQuery,
    scroll: usize,
    viewport: usize,
    line_count: usize,
}

fn failure(err: anyhow::Error) -> String {
    format!("{err:#}")
}

impl SqlView {
    pub fn new(warehouse: impl Into<String>, query: impl Into<String>) -> Self {
        Self {
            warehouse: warehouse.into(),
            query: query.into(),
            editing: false,
            step: Step::Ready,
            computes: Vec::new(),
            compute: 0,
            warehouse_id: None,
            session: None,
            output: None,
            since_ms: 0,
            running: Arc::new(Mutex::new(None)),
            scroll: 0,
            viewport: 1,
            line_count: 0,
        }
    }

    /// Finds the warehouse id, a running session to reuse, and the computes.
    fn prepare(&mut self, ctx: &mut Ctx) {
        let warehouse = self.warehouse.clone();
        let id = ctx.run_task("prepare a lakehouse session", move |client: ApiClient| {
            let warehouse = warehouse.clone();
            async move { prepare(client, &warehouse).await.map_err(failure) }
        });
        self.step = Step::Preparing(id);
    }

    fn start_session(&mut self, ctx: &mut Ctx) {
        let (Some(warehouse_id), Some(compute)) =
            (self.warehouse_id, self.computes.get(self.compute).cloned())
        else {
            return ctx.toast(Tone::Error, "Pick a compute first");
        };
        let warehouse = self.warehouse.clone();
        let id = ctx.run_task("start a lakehouse session", move |client: ApiClient| {
            let (warehouse, compute) = (warehouse.clone(), compute.clone());
            async move {
                start(client, warehouse_id, &warehouse, &compute)
                    .await
                    .map_err(failure)
            }
        });
        self.since_ms = ctx.now.epoch_ms;
        self.step = Step::Starting(id);
    }

    fn run(&mut self, ctx: &mut Ctx) {
        let Some(session) = self.session.clone() else {
            return self.prepare(ctx);
        };
        let statement = self.query.trim().to_string();
        if statement.is_empty() {
            return ctx.toast(Tone::Info, "Type a query first (e to edit)");
        }
        let warehouse = self.warehouse.clone();
        let running = self.running.clone();
        let id = ctx.run_task("run a lakehouse query", move |client: ApiClient| {
            let (session, warehouse, statement, running) = (
                session.clone(),
                warehouse.clone(),
                statement.clone(),
                running.clone(),
            );
            async move {
                let outcome = Lakehouse::new(client)
                    .run(&session, &warehouse, &statement, &running)
                    .await
                    .map_err(failure)?;
                Ok(outcome_json(&outcome.reply, outcome.elapsed.as_secs_f64()))
            }
        });
        self.editing = false;
        self.since_ms = ctx.now.epoch_ms;
        self.scroll = 0;
        self.step = Step::Running(id);
    }

    /// Asks the session to cancel the query that is running.
    fn cancel(&mut self, ctx: &mut Ctx) {
        let query = self.running.lock().ok().and_then(|slot| slot.clone());
        let (Some(session), Some(query)) = (self.session.clone(), query) else {
            return ctx.toast(Tone::Info, "The query hasn't reached the session yet");
        };
        ctx.run_task("cancel a lakehouse query", move |client: ApiClient| {
            let (session, query) = (session.clone(), query.clone());
            async move {
                Lakehouse::new(client)
                    .cancel(&session, &query)
                    .await
                    .map_err(failure)?;
                Ok(json!({"message": "Cancel requested"}))
            }
        });
        ctx.toast(Tone::Info, "Cancelling…");
    }

    fn prepared(&mut self, answer: &Value) {
        self.warehouse_id = answer["warehouseId"].as_i64();
        self.computes = answer["computes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| {
                Some(Compute {
                    id: c["id"].as_i64()?,
                    name: c["name"].as_str()?.to_string(),
                    description: c["description"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect();
        self.session = answer["session"].as_str().map(str::to_string);
        self.step = if self.session.is_some() {
            Step::Ready
        } else {
            Step::Choose
        };
    }

    fn lines(&self, ui: RenderCtx, width: usize) -> Vec<Line<'static>> {
        let mut lines = vec![self.query_line(), Line::from("")];
        let elapsed = (ui.now_ms - self.since_ms).max(0) / 1000;
        let spinner = theme::SPINNER[(ui.tick as usize) % theme::SPINNER.len()];
        match &self.step {
            Step::Preparing(_) => lines.push(muted(format!(
                "{spinner} Looking for a running session on {}…",
                self.warehouse
            ))),
            Step::Choose => lines.extend(self.compute_lines()),
            Step::Starting(_) => {
                lines.push(Line::from(Span::styled(
                    format!("{spinner} Starting a lakehouse session… {elapsed}s"),
                    theme::badge(theme::BadgeTone::Info),
                )));
                lines.push(muted(
                    "Spark pods usually take a minute or two. The session is kept for gz sql and later previews.",
                ));
            }
            Step::Running(_) => lines.push(Line::from(Span::styled(
                format!("{spinner} Running… {elapsed}s · x to cancel"),
                theme::badge(theme::BadgeTone::Info),
            ))),
            Step::Failed(err) => {
                lines.push(Line::from(Span::styled(
                    err.clone(),
                    theme::badge(theme::BadgeTone::Bad),
                )));
                lines.push(muted(
                    "⏎ try again · n start a new session · e edit the query",
                ));
            }
            Step::Ready => {
                let session = self.session.as_deref().unwrap_or("?");
                lines.push(muted(format!(
                    "Session {session} is running · ⏎ run · e edit"
                )));
            }
        }
        match &self.output {
            Some(Output::Rows(page, seconds)) => {
                lines.push(Line::from(""));
                lines.push(results::summary(
                    page,
                    Some(std::time::Duration::from_secs_f64(*seconds)),
                ));
                lines.extend(results::render(page, width, false));
            }
            Some(Output::Message(text)) => {
                lines.push(Line::from(""));
                lines.push(Line::from(text.clone()));
            }
            None => {}
        }
        lines
    }

    fn query_line(&self) -> Line<'static> {
        let cursor = if self.editing { "▏" } else { "" };
        let label = if self.editing {
            "SQL (editing) "
        } else {
            "SQL "
        };
        Line::from(vec![
            Span::styled(label, theme::brand()),
            Span::raw(format!("{}{cursor}", self.query)),
        ])
    }

    fn compute_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(Span::styled(
                format!("No running session on {}.", self.warehouse),
                theme::title(),
            )),
            muted("Starting one launches a Spark pod: it takes a minute or more and uses compute."),
            muted("Pick a compute and press ⏎ to start it."),
            Line::from(""),
        ];
        if self.computes.is_empty() {
            lines.push(Line::from(Span::styled(
                "No compute sizes are available to you.",
                theme::badge(theme::BadgeTone::Warn),
            )));
        }
        for (index, compute) in self.computes.iter().enumerate() {
            let marker = if index == self.compute { "▶ " } else { "  " };
            let line = Line::from(vec![
                Span::styled(format!("{marker}{}", compute.name), theme::title()),
                Span::styled(format!("  {}", compute.description), theme::muted()),
            ]);
            lines.push(if index == self.compute {
                line.patch_style(theme::selected_row())
            } else {
                line
            });
        }
        lines
    }

    fn on_edit_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        match key.code {
            KeyCode::Esc => self.editing = false,
            KeyCode::Enter => self.run(ctx),
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Char(c) => self.query.push(c),
            _ => {}
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.line_count.saturating_sub(self.viewport) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max.max(0)) as usize;
    }
}

fn muted(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), theme::muted()))
}

/// The answer of the prepare task: warehouse id, reusable session, computes.
async fn prepare(client: ApiClient, warehouse: &str) -> anyhow::Result<Value> {
    let lake = Lakehouse::new(client.clone());
    let warehouses = lake.warehouses().await?;
    let found = pick_warehouse(&warehouses, warehouse).ok_or_else(|| {
        anyhow::anyhow!("You can't see warehouse {warehouse} (or it was deleted).")
    })?;
    let remembered = crate::sql::remembered_session(&client.resolved().profile, &found.name);
    let mut session = None;
    if let Some(id) = remembered {
        if lake.session_state(&id).await.ok().as_deref() == Some("Running") {
            session = Some(id);
        }
    }
    let computes = if session.is_some() {
        Vec::new()
    } else {
        lake.computes().await?
    };
    Ok(json!({
        "warehouseId": found.id,
        "session": session,
        "computes": computes
            .iter()
            .map(|c| json!({"id": c.id, "name": c.name, "description": c.description}))
            .collect::<Vec<_>>(),
    }))
}

/// Starts a session and remembers it for `gz sql`, even if the view was
/// closed meanwhile, so it is never left running unknown.
async fn start(
    client: ApiClient,
    warehouse_id: i64,
    warehouse: &str,
    compute: &Compute,
) -> anyhow::Result<Value> {
    let profile = client.resolved().profile_name.clone();
    let session = Lakehouse::new(client)
        .start(compute.id, warehouse_id)
        .await?;
    // The session works even if the profile can't be saved; gz sql will
    // just not find it again.
    let _ = crate::sql::remember_in_profile(&profile, |p| {
        p.sql_warehouse = Some(warehouse.to_string());
        p.sql_compute = Some(compute.name.clone());
        p.sql_session = Some(session.clone());
        p.sql_session_warehouse = Some(warehouse.to_string());
    });
    Ok(json!({"session": session}))
}

fn outcome_json(reply: &Reply, seconds: f64) -> Value {
    match reply {
        Reply::Rows(page) => json!({
            "columns": page.columns,
            "rows": page.rows,
            "pageNumber": page.page_number,
            "hasMore": page.has_more,
            "totalRecords": page.total_records,
            "seconds": seconds,
        }),
        Reply::Done(message) => json!({"message": message, "seconds": seconds}),
        Reply::Stored | Reply::Running => json!({"message": "Finished.", "seconds": seconds}),
    }
}

fn output_of(answer: &Value) -> Output {
    let seconds = answer["seconds"].as_f64().unwrap_or_default();
    match answer["columns"].as_array() {
        Some(columns) => Output::Rows(
            Page {
                columns: columns
                    .iter()
                    .filter_map(|c| c.as_str().map(str::to_string))
                    .collect(),
                column_types: Vec::new(),
                rows: answer["rows"].as_array().cloned().unwrap_or_default(),
                page_number: answer["pageNumber"].as_u64().unwrap_or(1) as u32,
                has_more: answer["hasMore"].as_bool().unwrap_or(false),
                total_records: answer["totalRecords"].as_u64(),
            },
            seconds,
        ),
        None => Output::Message(
            answer["message"]
                .as_str()
                .unwrap_or("Statement executed.")
                .to_string(),
        ),
    }
}

impl View for SqlView {
    fn title(&self) -> String {
        format!("SQL on {}", self.warehouse)
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        self.prepare(ctx);
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if self.editing {
            self.on_edit_key(key, ctx);
            return Handled::Yes;
        }
        let busy = self.is_busy();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if self.step == Step::Choose => {
                self.compute = self.compute.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') if self.step == Step::Choose => {
                self.compute = (self.compute + 1).min(self.computes.len().saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-(self.viewport as isize)),
            KeyCode::PageDown => self.scroll_by(self.viewport as isize),
            KeyCode::Enter if self.step == Step::Choose => self.start_session(ctx),
            KeyCode::Enter if !busy => self.run(ctx),
            KeyCode::Char('e') if !busy => self.editing = true,
            KeyCode::Char('n') if !busy && self.warehouse_id.is_some() => {
                self.session = None;
                self.step = Step::Choose;
                if self.computes.is_empty() {
                    self.prepare(ctx);
                }
            }
            KeyCode::Char('x') if matches!(self.step, Step::Running(_)) => self.cancel(ctx),
            KeyCode::Char('y') => ctx.copy(self.query.clone(), "query"),
            // A refresh would restart a session; keep it explicit.
            KeyCode::Char('r') => {}
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, ctx: &mut Ctx) {
        let answer = match (&self.step, result) {
            (Step::Preparing(i) | Step::Starting(i) | Step::Running(i), Ok(answer)) if *i == id => {
                answer
            }
            (Step::Preparing(i) | Step::Starting(i) | Step::Running(i), Err(err)) if *i == id => {
                self.step = Step::Failed(err);
                return;
            }
            // A cancel's answer, or one for a step left behind.
            (_, Err(err)) => return ctx.toast(Tone::Error, err),
            _ => return,
        };
        match self.step {
            Step::Preparing(_) => {
                self.prepared(&answer);
                if self.session.is_some() {
                    self.run(ctx);
                }
            }
            Step::Starting(_) => {
                self.session = answer["session"].as_str().map(str::to_string);
                self.step = Step::Ready;
                self.run(ctx);
            }
            Step::Running(_) => {
                self.output = Some(output_of(&answer));
                self.step = Step::Ready;
            }
            _ => {}
        }
    }

    fn on_scroll(&mut self, delta: isize) {
        self.scroll_by(delta);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ui: RenderCtx) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border(true))
            .title(Span::styled(format!(" {} ", self.title()), theme::title()))
            .title(
                Line::from(Span::styled(" ⏎ run · e edit ", theme::muted()))
                    .alignment(Alignment::Right),
            )
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = self.lines(ui, inner.width as usize);
        self.viewport = inner.height as usize;
        self.line_count = lines.len();
        self.scroll_by(0);
        let shown: Vec<Line> = lines
            .into_iter()
            .skip(self.scroll)
            .take(self.viewport)
            .collect();
        frame.render_widget(Paragraph::new(shown).wrap(Wrap { trim: false }), inner);
        if self.line_count > self.viewport {
            render_scrollbar(frame, area, self.line_count - self.viewport, self.scroll);
        }
    }

    fn help(&self) -> Vec<HelpSection> {
        vec![HelpSection::new(
            "SQL",
            &[
                ("⏎", "start the session (first) / run the query"),
                ("e", "edit the query (⏎ runs, esc stops editing)"),
                ("x", "cancel the running query"),
                ("n", "start a new session"),
                ("y", "copy the query"),
                ("↑ ↓  PgUp PgDn", "pick a compute / scroll the results"),
                ("esc", "close (the session keeps running for gz sql)"),
            ],
        )]
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match self.step {
            Step::Choose => vec![("↑↓", "compute"), ("⏎", "start session"), ("esc", "back")],
            Step::Running(_) => vec![("x", "cancel"), ("esc", "back")],
            _ if self.editing => vec![("⏎", "run"), ("esc", "stop editing")],
            _ => vec![("⏎", "run"), ("e", "edit"), ("y", "copy"), ("esc", "back")],
        }
    }

    fn is_busy(&self) -> bool {
        matches!(
            self.step,
            Step::Preparing(_) | Step::Starting(_) | Step::Running(_)
        )
    }

    fn is_typing(&self) -> bool {
        self.editing
    }
}

#[cfg(test)]
mod tests;
