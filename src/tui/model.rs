use serde_json::Value;

use crate::output;

fn cell_text(value: &Value) -> String {
    match value {
        Value::Null => "—".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) if items.is_empty() => "[]".to_string(),
        Value::Object(map) if map.is_empty() => "{}".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "?".to_string()),
    }
}

/// Scrollable, filterable, selectable data grid.
#[derive(Debug, Default, Clone)]
pub struct Grid {
    pub cols: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub raw: Vec<Value>,
    pub selected: usize,
    pub row_offset: usize,
    pub col_offset: usize,
    pub filter: String,
    pub matches: Vec<usize>,
    pub empty_note: String,
}

impl Grid {
    pub fn from_value(value: &Value, empty_note: &str) -> Self {
        let mut grid = Self {
            empty_note: empty_note.to_string(),
            ..Self::default()
        };
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
            Value::Array(items) => {
                for item in items {
                    if let Value::Object(map) = item {
                        for key in map.keys() {
                            if !self.cols.contains(key) {
                                self.cols.push(key.clone());
                            }
                        }
                    }
                }
                if self.cols.is_empty() {
                    self.cols.push("value".to_string());
                }
                for item in items {
                    if self.cols.len() == 1 && self.cols[0] == "value" && !item.is_object() {
                        self.rows.push(vec![cell_text(item)]);
                    } else {
                        self.rows.push(
                            self.cols
                                .iter()
                                .map(|c| {
                                    item.get(c)
                                        .map(cell_text)
                                        .unwrap_or_else(|| "—".to_string())
                                })
                                .collect(),
                        );
                    }
                    self.raw.push(item.clone());
                }
            }
            Value::Object(map) => {
                self.cols = vec!["key".to_string(), "value".to_string()];
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for key in keys {
                    self.rows.push(vec![key.clone(), cell_text(&map[key])]);
                    self.raw.push(map[key].clone());
                }
            }
            scalar => {
                self.cols = vec!["value".to_string()];
                self.rows = vec![vec![cell_text(scalar)]];
                self.raw = vec![scalar.clone()];
            }
        }
        self.apply_filter();
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
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    pub fn visible_len(&self) -> usize {
        self.matches.len()
    }

    pub fn move_sel(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, self.matches.len() as isize - 1) as usize;
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

    pub fn selected_row(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }

    pub fn selected_cells(&self) -> Option<&Vec<String>> {
        self.selected_row().and_then(|i| self.rows.get(i))
    }

    pub fn selected_raw(&self) -> Option<&Value> {
        self.selected_row().and_then(|i| self.raw.get(i))
    }

    pub fn visible_rows(&self) -> Vec<usize> {
        self.matches.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_grid_and_filters() {
        let mut grid = Grid::from_value(&json!([{"a": 1, "b": "x"}, {"a": 2, "b": "y"}]), "empty");
        assert_eq!(grid.cols, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(grid.visible_len(), 2);
        grid.filter = "y".to_string();
        grid.apply_filter();
        assert_eq!(grid.visible_len(), 1);
        assert_eq!(grid.selected_cells().unwrap()[1], "y");
    }

    #[test]
    fn unwraps_envelopes_and_clamps_selection() {
        let mut grid = Grid::from_value(&json!({"success": true, "data": [{"a": 1}]}), "empty");
        assert_eq!(grid.visible_len(), 1);
        grid.move_sel(99);
        assert_eq!(grid.selected, 0);
        grid.move_sel(-99);
        assert_eq!(grid.selected, 0);
    }
}
