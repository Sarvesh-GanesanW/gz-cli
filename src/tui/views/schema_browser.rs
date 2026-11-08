//! A connection's databases, tables and columns, read through the data
//! provider with the connection's stored credentials.
//!
//! Contract (TUI research, "Connection actions › Browse schema"):
//! `POST data /getSchema {"ConnectionName"}` (plus `?siteName=` for
//! GZLAKEHOUSE) → `{success, data: [one row per column]}` with
//! `databaseName, tableName, columnName, dataType` (and `totalBytes`,
//! `updatedAt` for lakehouse); unsupported types answer 400
//! (groundzero-data-provider-api app.py:61-87; web etlUtlis.js:41-54,
//! designer/Dataset.js:3073-3128). It reaches the external source, so it
//! loads only when asked, and the API gateway gives up after 29 s.

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
    Frame,
};
use reqwest::Method;
use serde_json::{json, Value};

use super::{text_table, Ctx, Handled, RenderCtx, View};
use crate::tui::errors::{self, Subject};
use crate::tui::format::{self, text_of, Format};
use crate::tui::help::HelpSection;
use crate::tui::shape;
use crate::tui::theme;
use crate::tui::ui::widgets::render_scrollbar;

#[derive(Debug, Clone, PartialEq)]
enum Load {
    Loading(u64),
    Ready,
    Failed(String),
}

pub struct SchemaBrowser {
    connection: String,
    /// `?siteName=` for lakehouse connections.
    site: Option<String>,
    load: Load,
    tables: Vec<Value>,
    selected: usize,
    expanded: BTreeSet<usize>,
    scroll: usize,
    viewport: usize,
}

impl SchemaBrowser {
    pub fn new(connection: impl Into<String>, site: Option<String>) -> Self {
        Self {
            connection: connection.into(),
            site,
            load: Load::Ready,
            tables: Vec::new(),
            selected: 0,
            expanded: BTreeSet::new(),
            scroll: 0,
            viewport: 1,
        }
    }

    fn fetch(&mut self, ctx: &mut Ctx) {
        let path = match &self.site {
            Some(site) => format!("/getSchema?siteName={}", crate::cmd::seg(site)),
            None => "/getSchema".to_string(),
        };
        let body = json!({"ConnectionName": self.connection});
        self.load = Load::Loading(ctx.fetch_raw("data", Method::POST, path, Some(body)));
    }

    fn lines(&self, ui: RenderCtx, width: usize) -> (Vec<Line<'static>>, usize) {
        let mut lines = Vec::new();
        let mut selected_line = 0;
        match &self.load {
            Load::Loading(_) => {
                let frame = theme::SPINNER[(ui.tick as usize) % theme::SPINNER.len()];
                lines.push(Line::from(Span::styled(
                    format!("{frame} Reading tables from {}…", self.connection),
                    theme::muted(),
                )));
                return (lines, 0);
            }
            Load::Failed(err) => {
                lines.push(Line::from(Span::styled(
                    err.clone(),
                    theme::badge(theme::BadgeTone::Bad),
                )));
                return (lines, 0);
            }
            Load::Ready if self.tables.is_empty() => {
                lines.push(Line::from(Span::styled(
                    "This connection has no tables to show.",
                    theme::muted(),
                )));
                return (lines, 0);
            }
            Load::Ready => {}
        }
        lines.push(Line::from(Span::styled(
            format!("{} tables · ⏎ shows a table's columns", self.tables.len()),
            theme::muted(),
        )));
        for (index, table) in self.tables.iter().enumerate() {
            let columns = table["columns"].as_array().map_or(0, Vec::len);
            let mut text = format!(
                "{} {}.{}  {} columns",
                if self.expanded.contains(&index) {
                    "▾"
                } else {
                    "▸"
                },
                text_of(&table["databaseName"]),
                text_of(&table["tableName"]),
                columns
            );
            if let Some(bytes) = table.get("totalBytes").filter(|v| !v.is_null()) {
                text.push_str(&format!(
                    " · {}",
                    format::show(Some(bytes), Format::Bytes, ui.now_ms).text
                ));
            }
            if let Some(kind) = table["objectType"].as_str() {
                text.push_str(&format!(" · {}", kind.to_lowercase()));
            }
            let mut line = Line::from(text);
            if index == self.selected {
                selected_line = lines.len();
                line = line.patch_style(theme::selected_row());
            }
            lines.push(line);
            if self.expanded.contains(&index) {
                let rows: Vec<Vec<String>> = table["columns"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| vec![text_of(&c["columnName"]), text_of(&c["dataType"])])
                    .collect();
                let headers = ["Column", "Type"].map(String::from);
                lines.extend(
                    text_table::lines(&headers, &rows, width.saturating_sub(2))
                        .into_iter()
                        .map(|l| {
                            let mut spans = vec![Span::raw("  ")];
                            spans.extend(l.spans);
                            Line::from(spans)
                        }),
                );
            }
        }
        (lines, selected_line)
    }
}

/// One row per table from the per-column rows.
fn tables_of(answer: &Value) -> Vec<Value> {
    shape::group_by(
        shape::rows_of(answer),
        &["databaseName", "tableName"],
        "columns",
    )
}

impl View for SchemaBrowser {
    fn title(&self) -> String {
        format!("Tables in {}", self.connection)
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        self.fetch(ctx);
    }

    fn on_key(&mut self, key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        let last = self.tables.len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.selected = last,
            KeyCode::Enter | KeyCode::Char(' ') => {
                if !self.expanded.remove(&self.selected) {
                    self.expanded.insert(self.selected);
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, _ctx: &mut Ctx) {
        if self.load != Load::Loading(id) {
            return;
        }
        match result {
            Ok(answer) => {
                self.tables = tables_of(&answer);
                self.selected = 0;
                self.expanded.clear();
                self.load = Load::Ready;
            }
            Err(err) => {
                self.load = Load::Failed(errors::explain(
                    &err,
                    Subject {
                        what: "this connection's tables",
                        service: "data",
                    },
                ))
            }
        }
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        self.fetch(ctx);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ui: RenderCtx) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border(true))
            .title(Span::styled(format!(" {} ", self.title()), theme::title()))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let (lines, selected_line) = self.lines(ui, inner.width as usize);
        self.viewport = inner.height.max(1) as usize;
        if selected_line >= self.scroll + self.viewport {
            self.scroll = selected_line + 1 - self.viewport;
        }
        if selected_line < self.scroll {
            self.scroll = selected_line;
        }
        let total = lines.len();
        let shown: Vec<Line> = lines
            .into_iter()
            .skip(self.scroll)
            .take(self.viewport)
            .collect();
        frame.render_widget(Paragraph::new(shown), inner);
        if total > self.viewport {
            render_scrollbar(frame, area, total - self.viewport, self.scroll);
        }
    }

    fn help(&self) -> Vec<HelpSection> {
        vec![HelpSection::new(
            "Tables",
            &[
                ("↑ ↓", "move"),
                ("⏎  space", "show or hide the columns"),
                ("r", "read the tables again"),
                ("esc", "back"),
            ],
        )]
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("↑↓", "move"), ("⏎", "columns"), ("esc", "back")]
    }

    fn is_busy(&self) -> bool {
        matches!(self.load, Load::Loading(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn groups_column_rows_by_database_and_table() {
        let answer = json!({"success": true, "data": [
            {"databaseName": "public", "tableName": "orders", "columnName": "id", "dataType": "int"},
            {"databaseName": "public", "tableName": "orders", "columnName": "total", "dataType": "numeric"},
            {"databaseName": "sales", "tableName": "orders", "columnName": "id", "dataType": "int"},
        ]});
        let tables = tables_of(&answer);
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0]["columns"].as_array().unwrap().len(), 2);
        assert_eq!(tables[1]["databaseName"], "sales");
    }
}
