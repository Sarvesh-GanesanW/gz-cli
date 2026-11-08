use std::cmp::Ordering;

use serde_json::Value;

use crate::output;
use crate::tui::fields::{self, FieldPath};
use crate::tui::format::{self, epoch_ms};
use crate::tui::screen::{Column, ScreenSpec};
use crate::tui::theme::BadgeTone;

const MIN_COL_WIDTH: usize = 4;
const MAX_COL_WIDTH: usize = 48;
/// Rows sampled when sizing columns; enough to be representative, cheap on big pages.
const WIDTH_SAMPLE: usize = 200;

/// How deep nested objects are spread into `parent.child` rows.
const FLATTEN_DEPTH: usize = 3;

fn cell_text(value: &Value) -> String {
    match value {
        Value::Null => "—".to_string(),
        Value::Number(n) => group_thousands(&n.to_string()),
        Value::Array(items) if items.iter().any(|v| v.is_object() || v.is_array()) => {
            format!("{} item(s) · ⏎ to view", items.len())
        }
        Value::Array(items) => items
            .iter()
            .map(output::scalar_text)
            .collect::<Vec<_>>()
            .join(", "),
        other => output::scalar_text(other),
    }
}

/// `17503151` → `17,503,151`; decimals and numbers under five digits (years,
/// small counts) are left alone.
fn group_thousands(number: &str) -> String {
    let digits = number.trim_start_matches('-');
    if digits.len() < 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return number.to_string();
    }
    format::group_digits(number)
}

/// Spreads nested objects into dotted keys so summaries read as rows
/// (`usage.inputTokens  1,204`) instead of JSON blobs.
fn flatten_into(prefix: &str, value: &Value, depth: usize, out: &mut Vec<(String, Value)>) {
    match value {
        Value::Object(map) if depth < FLATTEN_DEPTH && !map.is_empty() => {
            for (key, inner) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_into(&path, inner, depth + 1, out);
            }
        }
        _ => out.push((prefix.to_string(), value.clone())),
    }
}

/// Orders two rows by the value at `path`: missing values last, numbers and
/// timestamps by magnitude, text ignoring case.
fn compare_raw(a: &Value, b: &Value, path: &[&str], descending: bool) -> Ordering {
    let key = |row: &Value| {
        fields::lookup_any(row, path)
            .filter(|v| !v.is_null())
            .cloned()
    };
    match (key(a), key(b)) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let order = compare_values(&x, &y);
            if descending {
                order.reverse()
            } else {
                order
            }
        }
    }
}

/// Numbers by magnitude, then timestamps by time, then sizes and numeric
/// text (`"4MB"`, `"1.25"`) by magnitude, else text ignoring case.
fn compare_values(a: &Value, b: &Value) -> Ordering {
    if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
        return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
    }
    if let (Value::String(_), Value::String(_)) = (a, b) {
        if let (Some(x), Some(y)) = (epoch_ms(a), epoch_ms(b)) {
            return x.cmp(&y);
        }
    }
    if let (Some(x), Some(y)) = (format::byte_count(a), format::byte_count(b)) {
        return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
    }
    format::text_of(a)
        .to_lowercase()
        .cmp(&format::text_of(b).to_lowercase())
}

/// Scrollable, filterable, selectable data grid.
#[derive(Debug, Default, Clone)]
pub struct Grid {
    pub cols: Vec<String>,
    pub widths: Vec<u16>,
    pub rows: Vec<Vec<String>>,
    /// Badge tone per cell, same shape as `rows` (None = plain text).
    pub tones: Vec<Vec<Option<BadgeTone>>>,
    pub raw: Vec<Value>,
    /// The spec columns these rows were built from, for sorting by raw value.
    spec_columns: Option<&'static [Column]>,
    /// The user's sort: column and whether it is descending.
    pub sorted: Option<(usize, bool)>,
    pub selected: usize,
    pub row_offset: usize,
    pub col_offset: usize,
    pub filter: String,
    pub matches: Vec<usize>,
}

impl Grid {
    #[cfg(test)]
    pub fn from_value(value: &Value) -> Self {
        let mut grid = Self::default();
        grid.replace(value);
        grid
    }

    /// Rebuilds the grid from an API answer: through `spec` when there is
    /// one and the answer is a list, else with generic columns.
    pub fn load(&mut self, value: &Value, spec: Option<&'static ScreenSpec>, now_ms: i64) {
        let rows = match spec.and_then(|s| s.rows) {
            Some(path) => fields::lookup_any(value, path).unwrap_or(&Value::Null),
            None => output::tabular(value),
        };
        match (spec, rows) {
            (Some(spec), Value::Array(items)) if !spec.columns.is_empty() => match spec.shape {
                Some(shape) => self.replace_with_spec(&shape(items.clone(), now_ms), spec, now_ms),
                None => self.replace_with_spec(items, spec, now_ms),
            },
            _ => self.replace(rows),
        }
    }

    fn clear(&mut self) {
        self.cols.clear();
        self.rows.clear();
        self.tones.clear();
        self.raw.clear();
        self.spec_columns = None;
        self.sorted = None;
        self.selected = 0;
        self.row_offset = 0;
        self.col_offset = 0;
    }

    fn replace_with_spec(&mut self, items: &[Value], spec: &'static ScreenSpec, now_ms: i64) {
        self.clear();
        self.spec_columns = Some(spec.columns);
        self.cols = spec.columns.iter().map(|c| c.header.to_string()).collect();
        let enriched: Vec<Value> = items.iter().map(|i| spec.enriched(i, now_ms)).collect();
        let mut items: Vec<&Value> = enriched.iter().collect();
        if let Some(sort) = spec.sort {
            items.sort_by(|a, b| compare_raw(a, b, sort.field, sort.descending));
        }
        for item in items {
            let shown: Vec<format::Shown> = spec
                .columns
                .iter()
                .map(|c| format::show(fields::lookup_any(item, c.field), c.format, now_ms))
                .collect();
            self.rows
                .push(shown.iter().map(|s| s.text.clone()).collect());
            self.tones.push(shown.iter().map(|s| s.tone).collect());
            self.raw.push(item.clone());
        }
        self.widths = self.measure_widths();
        for (width, column) in self.widths.iter_mut().zip(spec.columns) {
            if let Some(fixed) = column.width {
                *width = fixed;
            }
        }
        self.apply_filter();
    }

    pub fn replace(&mut self, value: &Value) {
        self.clear();
        match output::tabular(value) {
            Value::Null => {}
            Value::Array(items) if items.iter().all(Value::is_object) => {
                self.cols = output::columns_of(items);
                for item in items {
                    self.rows.push(
                        self.cols
                            .iter()
                            .map(|c| item.get(c).map(cell_text).unwrap_or_else(|| "—".into()))
                            .collect(),
                    );
                    self.raw.push(item.clone());
                }
            }
            Value::Array(items) => {
                self.cols = vec!["value".to_string()];
                for item in items {
                    self.rows.push(vec![cell_text(item)]);
                    self.raw.push(item.clone());
                }
            }
            object @ Value::Object(_) => {
                self.cols = vec!["key".to_string(), "value".to_string()];
                let mut entries = Vec::new();
                flatten_into("", object, 0, &mut entries);
                for (key, val) in entries {
                    self.rows.push(vec![key, cell_text(&val)]);
                    self.raw.push(val);
                }
            }
            scalar => {
                self.cols = vec!["value".to_string()];
                self.rows = vec![vec![cell_text(scalar)]];
                self.raw = vec![scalar.clone()];
            }
        }
        self.tones = self.rows.iter().map(|r| vec![None; r.len()]).collect();
        self.widths = self.measure_widths();
        self.apply_filter();
    }

    /// `o`: sorts by the next column, ascending (the first column shown
    /// when nothing is sorted yet).
    pub fn sort_next_column(&mut self) {
        if self.cols.is_empty() {
            return;
        }
        let col = match self.sorted {
            Some((col, _)) => (col + 1) % self.cols.len(),
            None => self.first_shown_col(),
        };
        self.sort_by_column(col, false);
        // Keep the sorted column on screen when cycling wraps back left.
        self.col_offset = self.col_offset.min(col);
    }

    /// `O`: reverses the sort (descending by the first column shown when
    /// nothing is sorted yet).
    pub fn reverse_sort(&mut self) {
        let (col, descending) = self.sorted.unwrap_or((self.first_shown_col(), false));
        self.sort_by_column(col, !descending);
    }

    /// "Started ▼" for the header, when the user sorted.
    pub fn sort_label(&self) -> Option<String> {
        let (col, descending) = self.sorted?;
        let arrow = if descending { "▼" } else { "▲" };
        Some(format!("{} {arrow}", self.cols.get(col)?))
    }

    fn first_shown_col(&self) -> usize {
        self.col_offset.min(self.cols.len().saturating_sub(1))
    }

    /// Orders rows by column `col`, comparing raw values (numbers as
    /// numbers, timestamps as times) rather than the formatted text.
    pub fn sort_by_column(&mut self, col: usize, descending: bool) {
        let Some(header) = self.cols.get(col) else {
            return;
        };
        let path: Vec<&str> = match self.spec_columns {
            Some(columns) => columns[col].field.to_vec(),
            None => vec![header.as_str()],
        };
        let mut order: Vec<usize> = (0..self.raw.len()).collect();
        order.sort_by(|&a, &b| compare_raw(&self.raw[a], &self.raw[b], &path, descending));
        let selected_raw = self.matches.get(self.selected).copied();
        self.rows = order.iter().map(|&i| self.rows[i].clone()).collect();
        self.tones = order.iter().map(|&i| self.tones[i].clone()).collect();
        self.raw = order.iter().map(|&i| self.raw[i].clone()).collect();
        self.sorted = Some((col, descending));
        self.apply_filter();
        if let Some(was) = selected_raw {
            if let Some(now) = order.iter().position(|&i| i == was) {
                self.select_raw(now);
            }
        }
    }

    /// Selects the row at raw index `index` if the filter shows it.
    pub fn select_raw(&mut self, index: usize) -> bool {
        match self.matches.iter().position(|&i| i == index) {
            Some(position) => {
                self.selected = position;
                true
            }
            None => false,
        }
    }

    /// The raw index of the first row whose `field` reads as `text`.
    pub fn find_raw(&self, field: FieldPath, text: &str) -> Option<usize> {
        self.raw.iter().position(|row| {
            fields::lookup_any(row, field)
                .map(format::text_of)
                .is_some_and(|value| value == text)
        })
    }

    fn measure_widths(&self) -> Vec<u16> {
        self.cols
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let widest_cell = self
                    .rows
                    .iter()
                    .take(WIDTH_SAMPLE)
                    .map(|r| r[i].chars().count())
                    .max()
                    .unwrap_or(0);
                widest_cell
                    .max(col.chars().count())
                    .clamp(MIN_COL_WIDTH, MAX_COL_WIDTH) as u16
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Keeps the rows whose shown cells hold every word of the filter,
    /// in any cells and ignoring case: "failed etl" finds failed ETL jobs.
    pub fn apply_filter(&mut self) {
        let lowered = self.filter.to_lowercase();
        let words: Vec<&str> = lowered.split_whitespace().collect();
        self.matches = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                let cells: Vec<String> = row.iter().map(|c| c.to_lowercase()).collect();
                words
                    .iter()
                    .all(|word| cells.iter().any(|cell| cell.contains(word)))
            })
            .map(|(i, _)| i)
            .collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.trim().to_string();
        self.apply_filter();
    }

    pub fn visible_len(&self) -> usize {
        self.matches.len()
    }

    pub fn move_sel(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    pub fn page(&mut self, height: usize, down: bool) {
        let step = height.saturating_sub(1).max(1) as isize;
        self.move_sel(if down { step } else { -step });
    }

    pub fn home(&mut self) {
        self.selected = 0;
    }

    pub fn end(&mut self) {
        self.selected = self.matches.len().saturating_sub(1);
    }

    pub fn scroll_cols(&mut self, delta: isize) {
        let last = self.cols.len().saturating_sub(1) as isize;
        self.col_offset = (self.col_offset as isize + delta).clamp(0, last) as usize;
    }

    /// Keeps the selected row inside a viewport of `height` rows.
    pub fn follow_selection(&mut self, height: usize) {
        let height = height.max(1);
        if self.selected >= self.row_offset + height {
            self.row_offset = self.selected + 1 - height;
        }
        if self.selected < self.row_offset {
            self.row_offset = self.selected;
        }
    }

    pub fn selected_raw(&self) -> Option<&Value> {
        self.matches
            .get(self.selected)
            .and_then(|i| self.raw.get(*i))
    }

    #[cfg(test)]
    fn selected_cells(&self) -> Option<&Vec<String>> {
        self.matches
            .get(self.selected)
            .and_then(|i| self.rows.get(*i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    static MODELS: ScreenSpec = ScreenSpec {
        columns: &[crate::tui::screen::Column::new(
            "Model",
            &["modelId"],
            crate::tui::format::Format::Text,
        )],
        rows: Some(&["modelMetrics"]),
        ..ScreenSpec::BLANK
    };

    fn add_label(row: &mut Value, _now_ms: i64) {
        let label = format!("{}-{}", row["a"], row["b"].as_str().unwrap_or(""));
        row["label"] = Value::String(label);
    }

    static ENRICHED: ScreenSpec = ScreenSpec {
        columns: &[crate::tui::screen::Column::new(
            "Label",
            &["label"],
            format::Format::Text,
        )],
        enrich: Some(add_label),
        ..ScreenSpec::BLANK
    };

    #[test]
    fn specs_read_rows_from_a_named_array_in_the_answer() {
        let mut grid = Grid::default();
        let answer =
            json!({"site": "preview", "modelMetrics": [{"modelId": "a"}, {"modelId": "b"}]});
        grid.load(&answer, Some(&MODELS), 0);
        assert_eq!(grid.visible_len(), 2);
        assert_eq!(grid.rows[1][0], "b");
        grid.load(&json!({"site": "preview"}), Some(&MODELS), 0);
        assert!(
            grid.is_empty(),
            "a missing array means no rows, not the envelope"
        );
    }

    #[test]
    fn screens_can_show_fields_derived_from_several_others() {
        let mut grid = Grid::default();
        grid.load(&json!([{"a": 1, "b": "x"}]), Some(&ENRICHED), 0);
        assert_eq!(grid.rows[0], vec!["1-x"]);
        assert_eq!(grid.selected_raw().unwrap()["label"], "1-x");
    }

    #[test]
    fn builds_grid_and_filters() {
        let mut grid = Grid::from_value(&json!([{"a": 1, "b": "x"}, {"a": 2, "b": "y"}]));
        assert_eq!(grid.cols, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(grid.visible_len(), 2);
        grid.set_filter("y");
        assert_eq!(grid.visible_len(), 1);
        assert_eq!(grid.selected_cells().unwrap()[1], "y");
    }

    #[test]
    fn filter_words_must_all_match_in_any_cells() {
        let mut grid = Grid::from_value(&json!([
            {"name": "Nightly ETL", "status": "FAILED"},
            {"name": "Nightly ETL", "status": "SUCCEEDED"},
            {"name": "Hourly sync", "status": "FAILED"},
        ]));
        grid.set_filter("failed  etl");
        assert_eq!(grid.visible_len(), 1);
        assert_eq!(grid.selected_cells().unwrap()[0], "Nightly ETL");
        grid.set_filter("FAILED");
        assert_eq!(grid.visible_len(), 2, "case is ignored");
    }

    #[test]
    fn unwraps_envelopes_and_clamps_selection() {
        let mut grid = Grid::from_value(&json!({"success": true, "data": [{"a": 1}]}));
        assert_eq!(grid.visible_len(), 1);
        grid.move_sel(99);
        assert_eq!(grid.selected, 0);
        grid.move_sel(-99);
        assert_eq!(grid.selected, 0);
    }

    #[test]
    fn sizes_columns_from_content() {
        let grid = Grid::from_value(&json!([{"id": 1, "description": "a much longer cell"}]));
        assert_eq!(grid.cols, vec!["id", "description"]);
        assert_eq!(grid.widths, vec![4, 18]);
    }

    #[test]
    fn flattens_nested_summaries_into_rows() {
        let grid = Grid::from_value(&json!({
            "site": "preview",
            "usage": {"inputTokens": 17503151, "cost": {"usd": "1.244280580800000000"}},
            "agentMetrics": [{"id": 1}, {"id": 2}],
        }));
        let rows: Vec<(String, String)> = grid
            .rows
            .iter()
            .map(|r| (r[0].clone(), r[1].clone()))
            .collect();
        assert!(rows.contains(&("usage.inputTokens".into(), "17,503,151".into())));
        assert!(rows.contains(&("usage.cost.usd".into(), "1.24428".into())));
        assert!(rows.contains(&("agentMetrics".into(), "2 item(s) · ⏎ to view".into())));
    }

    #[test]
    fn groups_thousands_only_for_long_integers() {
        assert_eq!(group_thousands("1234"), "1234");
        assert_eq!(group_thousands("-1234567"), "-1,234,567");
        assert_eq!(group_thousands("12.5"), "12.5");
    }

    #[test]
    fn sorts_by_raw_values_not_formatted_text() {
        let mut grid = Grid::from_value(&json!([
            {"n": 9, "at": "2026-01-02T00:00:00Z"},
            {"n": 10, "at": "2025-12-31T00:00:00Z"},
            {"n": null, "at": "2026-03-01T00:00:00Z"},
        ]));
        let n = grid.cols.iter().position(|c| c == "n").unwrap();
        let at = grid.cols.iter().position(|c| c == "at").unwrap();
        grid.move_sel(1);
        grid.sort_by_column(n, false);
        let col: Vec<&str> = grid.rows.iter().map(|r| r[n].as_str()).collect();
        assert_eq!(col, vec!["9", "10", "—"], "numeric order, missing last");
        assert_eq!(
            grid.selected_raw().unwrap()["n"],
            10,
            "selection follows its row"
        );
        grid.sort_by_column(at, true);
        assert_eq!(grid.raw[0]["n"], Value::Null, "newest first");
    }

    #[test]
    fn finds_and_selects_rows_by_field() {
        let mut grid = Grid::from_value(&json!([{"Id": 1}, {"Id": 2}]));
        let index = grid.find_raw(&["Id"], "2").unwrap();
        assert!(grid.select_raw(index));
        assert_eq!(grid.selected, 1);
        grid.set_filter("zzz");
        assert!(!grid.select_raw(index));
    }

    #[test]
    fn null_is_an_empty_grid() {
        assert!(Grid::from_value(&Value::Null).is_empty());
    }
}
