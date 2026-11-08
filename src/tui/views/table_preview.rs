//! A lakehouse table at a glance: row count, size, files and freshness,
//! its columns with types, ten sample rows and its snapshot history, as the
//! web's Table Properties / Schema / Sample Data tabs show them.
//!
//! Contracts (TUI research, "Table preview"):
//! - `GET lakehouse /lakehouse/schema/{wh}/{db}/{table}` → `[{columnId, columnName, dataType}]`
//!   (iceberg-provider schemas.py:462-491; web etlUtlis.js:644-660)
//! - `GET lakehouse /lakehouse/tableproperties/{wh}/{db}/{table}` → `{response: {"Row Count", …}}`;
//!   an unknown table answers **200** with `response.error` (schemas.py:247-280)
//! - `GET lakehouse /lakehouse/samplerecords/{wh}/{db}/{table}` → `response` is a **JSON string**
//!   of row objects; 403 for anyone not in the Admin group, 400 for a governed table
//!   (schemas.py:494-532, sample_data.py:25-44; web etlUtlis.js:800-821)
//!
//! A lineage asset opens this view before it knows its warehouse: the
//! warehouse is in its GZLAKEHOUSE connection's config (`warehouseName`),
//! read from `GET connections /connection/{id}` (key names only).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
    Frame,
};
use reqwest::Method;
use serde_json::Value;

use super::lineage_graph::LineageGraph;
use super::sql::SqlView;
use super::{text_table, Ctx, Handled, RenderCtx, View};
use crate::cmd::seg;
use crate::http::ApiClient;
use crate::output;
use crate::tui::app::Tone;
use crate::tui::errors::{self, Failure, Subject};
use crate::tui::format::{self, text_of, Format};
use crate::tui::help::HelpSection;
use crate::tui::theme;
use crate::tui::ui::widgets::render_scrollbar;

/// The web shows this literal text when a table has no description.
const NO_DESCRIPTION: &str = "No description set";
/// Rows the SQL preview asks for.
const SQL_PREVIEW_ROWS: u32 = 100;

/// Which table, once the warehouse is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub warehouse: String,
    pub database: String,
    pub table: String,
}

impl TableRef {
    fn path(&self, route: &str) -> String {
        format!(
            "/lakehouse/{route}/{}/{}/{}",
            seg(&self.warehouse),
            seg(&self.database),
            seg(&self.table)
        )
    }

    /// The query the SQL preview runs.
    pub fn preview_sql(&self) -> String {
        format!(
            "SELECT * FROM `{}`.`{}` LIMIT {SQL_PREVIEW_ROWS}",
            self.database, self.table
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Part {
    Waiting,
    Loading(u64),
    Loaded(Value),
    /// The user may not see it; the text says why and what to do instead.
    Denied(String),
    Failed(String),
}

impl Part {
    fn is(&self, id: u64) -> bool {
        *self == Part::Loading(id)
    }
}

pub struct TablePreview {
    table: Option<TableRef>,
    /// While the warehouse is looked up from a lineage asset's connection:
    /// the request, and the asset's `namespace.table`.
    resolving: Option<(u64, String)>,
    pending: Option<(String, String)>,
    schema: Part,
    properties: Part,
    sample: Part,
    error: Option<String>,
    scroll: usize,
    viewport: usize,
    line_count: usize,
    /// The lookup of this table's lineage asset, while it runs.
    lineage: Option<u64>,
}

impl TablePreview {
    pub fn new(table: TableRef) -> Self {
        Self {
            table: Some(table),
            resolving: None,
            pending: None,
            schema: Part::Waiting,
            properties: Part::Waiting,
            sample: Part::Waiting,
            error: None,
            scroll: 0,
            viewport: 1,
            line_count: 0,
            lineage: None,
        }
    }

    /// A table named by a lineage asset: `qualified` is `namespace.table`,
    /// and the warehouse comes from connection `connection_id`'s config.
    pub fn from_connection(connection_id: String, qualified: String) -> Self {
        Self {
            table: None,
            pending: Some((connection_id, qualified)),
            ..Self::new(TableRef {
                warehouse: String::new(),
                database: String::new(),
                table: String::new(),
            })
        }
    }

    fn open_sql(&self, ctx: &mut Ctx) {
        if let Some(table) = &self.table {
            ctx.push(Box::new(SqlView::new(
                table.warehouse.clone(),
                table.preview_sql(),
            )));
        }
    }

    /// Finds this table's lineage asset, then opens its graph.
    fn open_lineage(&mut self, ctx: &mut Ctx) {
        let Some(table) = self.table.clone() else {
            return;
        };
        self.lineage = Some(ctx.run_task(
            "find the table's lineage asset",
            move |client: ApiClient| {
                let table = table.clone();
                async move { find_lineage_asset(client, &table).await }
            },
        ));
        ctx.toast(Tone::Info, "Looking up lineage…");
    }

    fn load(&mut self, ctx: &mut Ctx) {
        let Some(table) = self.table.clone() else {
            return;
        };
        self.error = None;
        self.schema =
            Part::Loading(ctx.fetch_raw("lakehouse", Method::GET, table.path("schema"), None));
        self.properties = Part::Loading(ctx.fetch_raw(
            "lakehouse",
            Method::GET,
            table.path("tableproperties"),
            None,
        ));
        self.sample = Part::Loading(ctx.fetch_raw(
            "lakehouse",
            Method::GET,
            table.path("samplerecords"),
            None,
        ));
    }

    fn resolve(&mut self, ctx: &mut Ctx) {
        let Some((connection, qualified)) = self.pending.clone() else {
            return self.load(ctx);
        };
        let path = format!("/connection/{}", seg(&connection));
        let id = ctx.fetch_raw("connections", Method::GET, path, None);
        self.resolving = Some((id, qualified));
    }

    fn resolved(&mut self, qualified: &str, result: Result<Value, String>, ctx: &mut Ctx) {
        let config = match result {
            Ok(answer) => match output::tabular(&answer) {
                Value::Array(items) => items.first().map(|c| c["config"].clone()),
                other => Some(other["config"].clone()),
            },
            Err(err) => {
                self.error = Some(errors::explain(
                    &err,
                    Subject {
                        what: "this table's connection",
                        service: "connections",
                    },
                ));
                return;
            }
        };
        let warehouse = config
            .as_ref()
            .and_then(|c| c["warehouseName"].as_str())
            .map(str::to_string);
        let split = qualified.split_once('.');
        match (warehouse, split) {
            (Some(warehouse), Some((database, table))) => {
                self.table = Some(TableRef {
                    warehouse,
                    database: database.to_string(),
                    table: table.to_string(),
                });
                self.pending = None;
                self.load(ctx);
            }
            (None, _) => {
                self.error =
                    Some("This asset's connection doesn't name a lakehouse warehouse.".to_string())
            }
            (_, None) => self.error = Some(format!("{qualified} isn't a database.table name.")),
        }
    }

    fn part_loaded(&mut self, which: Which, result: Result<Value, String>) {
        let part = match (which, result) {
            (Which::Properties, Ok(answer)) => match answer["response"]["error"].as_str() {
                // Unknown tables answer 200 with an error inside.
                Some(err) => Part::Failed(format!("The lakehouse can't find this table ({err}).")),
                None => Part::Loaded(output::tabular(&answer).clone()),
            },
            (Which::Sample, Ok(answer)) => Part::Loaded(sample_rows(&answer)),
            (_, Ok(answer)) => Part::Loaded(output::tabular(&answer).clone()),
            (Which::Sample, Err(err)) => sample_failure(&err),
            (_, Err(err)) => Part::Failed(errors::explain(
                &err,
                Subject {
                    what: "this table",
                    service: "lakehouse",
                },
            )),
        };
        match which {
            Which::Schema => self.schema = part,
            Which::Properties => self.properties = part,
            Which::Sample => self.sample = part,
        }
    }

    fn schema_columns(&self) -> Vec<(String, String)> {
        let Part::Loaded(Value::Array(rows)) = &self.schema else {
            return Vec::new();
        };
        let mut rows = rows.clone();
        rows.sort_by_key(|r| r["columnId"].as_i64().unwrap_or(i64::MAX));
        rows.iter()
            .map(|r| {
                (
                    text_of(&r["columnName"]),
                    r["dataType"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    fn lines(&self, ui: RenderCtx, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                err.clone(),
                theme::badge(theme::BadgeTone::Bad),
            )));
            return lines;
        }
        if self.table.is_none() {
            lines.push(loading_line(ui, "Finding the table's warehouse…"));
            return lines;
        }
        lines.extend(self.summary_lines(ui));
        lines.extend(self.schema_lines(ui, width));
        lines.extend(self.sample_lines(ui, width));
        lines.extend(self.history_lines(ui.now_ms, width));
        lines
    }

    fn summary_lines(&self, ui: RenderCtx) -> Vec<Line<'static>> {
        let props = match &self.properties {
            Part::Loaded(props) => props,
            Part::Failed(err) | Part::Denied(err) => {
                return vec![Line::from(Span::styled(
                    err.clone(),
                    theme::badge(theme::BadgeTone::Bad),
                ))]
            }
            _ => return vec![loading_line(ui, "Loading table properties…")],
        };
        let show = |key: &str, format: Format| format::show(props.get(key), format, ui.now_ms).text;
        let mut parts = vec![
            format!("{} rows", show("Row Count", Format::Number)),
            show("Total Size Bytes", Format::Bytes),
            format!("{} files", show("File Count", Format::Number)),
        ];
        let updated = props
            .get("Last Updated")
            .filter(|v| v.as_str() != Some("Unknown"));
        if updated.is_some() {
            parts.push(format!(
                "updated {}",
                format::show(updated, Format::RelTime, ui.now_ms).text
            ));
        }
        if let Some(version) = props.get("Format Version").filter(|v| !v.is_null()) {
            parts.push(format!("v{}", text_of(version)));
        }
        for key in ["Partitioned By", "Compression Codec"] {
            if let Some(value) = props.get(key).filter(|v| !v.is_null()) {
                parts.push(text_of(value).to_lowercase());
            }
        }
        let mut lines = vec![Line::from(Span::styled(parts.join(" · "), theme::title()))];
        let description = props["Description"].as_str().unwrap_or_default();
        if !description.is_empty() && description != NO_DESCRIPTION {
            lines.push(Line::from(description.to_string()));
        }
        if props["Row Count"].as_u64() == Some(0) {
            lines.push(Line::from(Span::styled(
                "This table is empty.",
                theme::muted(),
            )));
        }
        lines
    }

    fn schema_lines(&self, ui: RenderCtx, width: usize) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(""), heading("Schema")];
        match &self.schema {
            Part::Loaded(_) => {
                let columns = self.schema_columns();
                if columns.is_empty() {
                    lines.push(muted("No columns"));
                    return lines;
                }
                let rows: Vec<Vec<String>> = columns
                    .iter()
                    .enumerate()
                    .map(|(i, (name, kind))| {
                        vec![
                            (i + 1).to_string(),
                            name.clone(),
                            kind.clone(),
                            type_family(kind).into(),
                        ]
                    })
                    .collect();
                let headers = ["#", "Column", "Type", "Kind"].map(String::from);
                lines.extend(text_table::lines(&headers, &rows, width));
            }
            Part::Failed(err) | Part::Denied(err) => lines.push(bad(err)),
            _ => lines.push(loading_line(ui, "Loading columns…")),
        }
        lines
    }

    fn sample_lines(&self, ui: RenderCtx, width: usize) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(""), heading("Sample rows")];
        match &self.sample {
            Part::Loaded(Value::Array(rows)) if rows.is_empty() => {
                lines.push(muted("No sample rows found."))
            }
            Part::Loaded(Value::Array(rows)) => {
                let mut headers: Vec<String> =
                    self.schema_columns().into_iter().map(|(n, _)| n).collect();
                if headers.is_empty() {
                    headers = rows[0]
                        .as_object()
                        .map(|o| o.keys().cloned().collect())
                        .unwrap_or_default();
                }
                let cells: Vec<Vec<String>> = rows
                    .iter()
                    .map(|row| {
                        headers
                            .iter()
                            .map(|h| row.get(h).map_or(String::new(), text_of))
                            .collect()
                    })
                    .collect();
                lines.extend(text_table::lines(&headers, &cells, width));
            }
            Part::Loaded(_) => lines.push(muted("No sample rows found.")),
            Part::Denied(why) => {
                lines.push(Line::from(Span::styled(
                    why.clone(),
                    theme::badge(theme::BadgeTone::Warn),
                )));
                lines.push(muted(
                    "Press s to preview with SQL instead (starts a lakehouse session, a minute or more).",
                ));
            }
            Part::Failed(err) => lines.push(bad(err)),
            _ => lines.push(loading_line(ui, "Loading sample rows…")),
        }
        lines
    }

    fn history_lines(&self, now_ms: i64, width: usize) -> Vec<Line<'static>> {
        let Part::Loaded(props) = &self.properties else {
            return Vec::new();
        };
        let Some(history) = props["Snapshot History"]
            .as_array()
            .filter(|h| !h.is_empty())
        else {
            return Vec::new();
        };
        let rows: Vec<Vec<String>> = history
            .iter()
            .rev()
            .map(|snapshot| {
                vec![
                    format::show(snapshot.get("timestamp"), Format::RelTime, now_ms).text,
                    text_of(&snapshot["operation"]),
                    format!(
                        "+{}",
                        format::show(snapshot.get("added_records"), Format::Number, now_ms).text
                    ),
                    format!(
                        "−{}",
                        format::show(snapshot.get("deleted_records"), Format::Number, now_ms).text
                    ),
                ]
            })
            .collect();
        let headers = ["When", "Operation", "Added", "Deleted"].map(String::from);
        let mut lines = vec![Line::from(""), heading("Snapshots")];
        lines.extend(text_table::lines(&headers, &rows, width));
        lines
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.line_count.saturating_sub(self.viewport) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max.max(0)) as usize;
    }
}

/// The lineage asset of a lakehouse table.
///
/// No endpoint maps a table to its asset, so (as the research notes say)
/// this reads each GZLAKEHOUSE connection's config for its warehouse and
/// searches that connection's assets for `database.table`
/// (`GET etl /etl/lineage/assets?connectionId=&query=`, app.py:2227-2287).
async fn find_lineage_asset(client: ApiClient, table: &TableRef) -> Result<Value, String> {
    let fail = |e: anyhow::Error| format!("Can't look up lineage: {e:#}");
    let connections = client
        .request_json("connections", Method::GET, "/connection", &[], None)
        .await
        .map_err(fail)?;
    let qualified = format!("{}.{}", table.database, table.table);
    let lakehouses = crate::tui::shape::rows_of(&connections)
        .into_iter()
        .filter(|c| c["type"].as_str() == Some("GZLAKEHOUSE"));
    for connection in lakehouses {
        let id = text_of(&connection["Id"]);
        let path = format!("/connection/{}", seg(&id));
        let detail = client
            .request_json("connections", Method::GET, &path, &[], None)
            .await
            .map_err(fail)?;
        let warehouse = crate::tui::shape::rows_of(&detail)
            .first()
            .and_then(|d| d["config"]["warehouseName"].as_str().map(str::to_string));
        if !warehouse.is_some_and(|w| w.eq_ignore_ascii_case(&table.warehouse)) {
            continue;
        }
        let query = [
            ("connectionId".to_string(), id.clone()),
            ("query".to_string(), table.table.clone()),
            ("limit".to_string(), "100".to_string()),
        ];
        let assets = client
            .request_json("etl", Method::GET, "/etl/lineage/assets", &query, None)
            .await
            .map_err(fail)?;
        let found = crate::tui::shape::rows_of(&assets)
            .into_iter()
            .find(|asset| {
                asset["qualifiedName"]
                    .as_str()
                    .is_some_and(|q| q.eq_ignore_ascii_case(&qualified))
            });
        if let Some(asset) = found {
            return Ok(asset);
        }
    }
    Err(format!(
        "No lineage is recorded for {qualified} in {}.",
        table.warehouse
    ))
}

#[derive(Debug, Clone, Copy)]
enum Which {
    Schema,
    Properties,
    Sample,
}

/// Sample rows arrive as a JSON string holding an array of row objects.
fn sample_rows(answer: &Value) -> Value {
    let response = answer.get("response").unwrap_or(answer);
    match response {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Array(Vec::new())),
        other => other.clone(),
    }
}

/// 403 (not an Admin) and 400 (governed table) mean "use SQL instead".
fn sample_failure(err: &str) -> Part {
    match errors::parse(err) {
        Failure::Http { status: 403, .. } => {
            Part::Denied("Sample rows are only shown to lakehouse Admins.".to_string())
        }
        Failure::Http { status: 400, .. } => Part::Denied(errors::explain(
            err,
            Subject {
                what: "these sample rows",
                service: "lakehouse",
            },
        )),
        _ => Part::Failed(errors::explain(
            err,
            Subject {
                what: "these sample rows",
                service: "lakehouse",
            },
        )),
    }
}

/// The web's column type families (catalogModel.js:52-62).
fn type_family(data_type: &str) -> &'static str {
    let t = data_type.to_ascii_lowercase();
    if ["char", "string", "text", "uuid", "binary"]
        .iter()
        .any(|k| t.contains(k))
    {
        "text"
    } else if ["date", "time"].iter().any(|k| t.contains(k)) {
        "time"
    } else if t.contains("bool") {
        "bool"
    } else if [
        "int", "long", "double", "float", "decimal", "number", "numeric",
    ]
    .iter()
    .any(|k| t.contains(k))
    {
        "number"
    } else {
        "other"
    }
}

fn heading(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(text, theme::brand()))
}

fn muted(text: &str) -> Line<'static> {
    Line::from(Span::styled(format!("  {text}"), theme::muted()))
}

fn bad(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {text}"),
        theme::badge(theme::BadgeTone::Bad),
    ))
}

fn loading_line(ui: RenderCtx, text: &str) -> Line<'static> {
    let frame = theme::SPINNER[(ui.tick as usize) % theme::SPINNER.len()];
    Line::from(Span::styled(format!("  {frame} {text}"), theme::muted()))
}

impl View for TablePreview {
    fn title(&self) -> String {
        match &self.table {
            Some(t) => format!("{} › {} › {}", t.warehouse, t.database, t.table),
            None => "Table".to_string(),
        }
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        self.resolve(ctx);
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let page = self.viewport.max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::Char('s') => self.open_sql(ctx),
            KeyCode::Char('L') => self.open_lineage(ctx),
            KeyCode::Char('y') => match &self.table {
                Some(t) => ctx.copy(format!("{}.{}", t.database, t.table), "table name"),
                None => return Handled::No,
            },
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, ctx: &mut Ctx) {
        if self.lineage == Some(id) {
            self.lineage = None;
            return match result {
                Ok(asset) => {
                    let title = text_of(&asset["displayName"]);
                    match asset["assetId"].as_i64() {
                        Some(id) => ctx.push(Box::new(LineageGraph::new(id, title))),
                        None => ctx.toast(Tone::Error, "The lineage asset has no id"),
                    }
                }
                Err(err) => ctx.toast(Tone::Error, err),
            };
        }
        if let Some((waiting, qualified)) = self.resolving.clone() {
            if waiting == id {
                self.resolving = None;
                return self.resolved(&qualified, result, ctx);
            }
        }
        let which = if self.schema.is(id) {
            Which::Schema
        } else if self.properties.is(id) {
            Which::Properties
        } else if self.sample.is(id) {
            Which::Sample
        } else {
            return;
        };
        self.part_loaded(which, result);
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        self.resolve(ctx);
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
                Line::from(Span::styled(" s SQL · L lineage ", theme::muted()))
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
        frame.render_widget(Paragraph::new(shown), inner);
        if self.line_count > self.viewport {
            render_scrollbar(frame, area, self.line_count - self.viewport, self.scroll);
        }
    }

    fn help(&self) -> Vec<HelpSection> {
        vec![HelpSection::new(
            "Table",
            &[
                ("↑ ↓  PgUp PgDn", "scroll"),
                ("s", "query with SQL (starts a session the first time)"),
                ("L", "lineage of this table"),
                ("y", "copy database.table"),
                ("r", "reload"),
                ("esc", "back"),
            ],
        )]
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑↓", "scroll"),
            ("s", "SQL"),
            ("L", "lineage"),
            ("esc", "back"),
        ]
    }

    fn is_busy(&self) -> bool {
        self.resolving.is_some()
            || self.lineage.is_some()
            || [&self.schema, &self.properties, &self.sample]
                .iter()
                .any(|p| matches!(p, Part::Loading(_)))
    }
}

#[cfg(test)]
mod tests;
