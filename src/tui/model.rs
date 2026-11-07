use serde_json::Value;

use crate::output;

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

/// `17503151` → `17,503,151`; decimals and short numbers are left alone.
fn group_thousands(number: &str) -> String {
    let (sign, digits) = match number.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", number),
    };
    if digits.len() < 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return number.to_string();
    }
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    format!("{sign}{out}")
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

/// Scrollable, filterable, selectable data grid.
#[derive(Debug, Default, Clone)]
pub struct Grid {
    pub cols: Vec<String>,
    pub widths: Vec<u16>,
    pub rows: Vec<Vec<String>>,
    pub raw: Vec<Value>,
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

    pub fn replace(&mut self, value: &Value) {
        self.cols.clear();
        self.rows.clear();
        self.raw.clear();
        self.selected = 0;
        self.row_offset = 0;
        self.col_offset = 0;
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
        self.widths = self.measure_widths();
        self.apply_filter();
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

    pub fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        self.matches = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                needle.is_empty() || row.iter().any(|c| c.to_lowercase().contains(&needle))
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
    fn null_is_an_empty_grid() {
        assert!(Grid::from_value(&Value::Null).is_empty());
    }
}
