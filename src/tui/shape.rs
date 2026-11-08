//! Turning API answers into the rows a screen shows: grouping flat rows
//! (one per column → one per table) and paging through long lists with
//! `limit` / `offset`.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::output;
use crate::tui::fields;
use crate::tui::format::text_of;

/// One row per distinct value of the `by` fields, in first-seen order. Each
/// group row keeps the first member's fields and lists every member under
/// `into`, so `Format::Count` on `into` shows how many there are.
///
/// The lakehouse schema routes answer one row per column; grouping by
/// `tableName` turns them into tables.
pub fn group_by(rows: Vec<Value>, by: &[&str], into: &str) -> Vec<Value> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, (Map<String, Value>, Vec<Value>)> = HashMap::new();
    for row in rows {
        let key = by
            .iter()
            .map(|field| fields::lookup(&row, field).map(text_of).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\u{1f}");
        let entry = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            (row.as_object().cloned().unwrap_or_default(), Vec::new())
        });
        entry.1.push(row);
    }
    order
        .into_iter()
        .filter_map(|key| groups.remove(&key))
        .map(|(mut first, members)| {
            first.insert(into.to_string(), Value::Array(members));
            Value::Object(first)
        })
        .collect()
}

/// How a long list pages: `limit` and `offset` query parameters.
#[derive(Debug, Clone, Copy)]
pub struct Paging {
    /// Rows asked for per page.
    pub size: usize,
    /// The most the server accepts in one page; a reload asks for every row
    /// already shown (up to this), so a refresh keeps the user's place.
    pub max: usize,
}

impl Paging {
    /// `path` with `limit` and `offset` added to its query.
    pub fn page_path(&self, path: &str, offset: usize, limit: usize) -> String {
        let joiner = if path.contains('?') { '&' } else { '?' };
        format!("{path}{joiner}limit={limit}&offset={offset}")
    }

    /// The page size for a reload when `shown` rows are on screen.
    pub fn reload_limit(&self, shown: usize) -> usize {
        shown.max(self.size).min(self.max)
    }
}

/// Where the next page starts, or None when this was the last. Reads the
/// envelope's `hasMore` / `nextOffset` (lineage), then `total` (log
/// exports), and otherwise assumes a full page means there is more.
pub fn next_offset(answer: &Value, offset: usize, limit: usize) -> Option<usize> {
    let returned = rows_of(answer).len();
    let after = offset + returned;
    if let Some(has_more) = answer.get("hasMore").and_then(Value::as_bool) {
        let next = answer
            .get("nextOffset")
            .and_then(Value::as_u64)
            .map_or(after, |n| n as usize);
        return (has_more && returned > 0).then_some(next);
    }
    if let Some(total) = total_of(answer) {
        return (returned > 0 && (after as u64) < total).then_some(after);
    }
    (returned >= limit && returned > 0).then_some(after)
}

/// The list's full length when the envelope says it (`total`).
pub fn total_of(answer: &Value) -> Option<u64> {
    answer.get("total").and_then(Value::as_u64)
}

/// The rows in an answer, unwrapped from its envelope.
pub fn rows_of(answer: &Value) -> Vec<Value> {
    match output::tabular(answer) {
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn groups_column_rows_into_tables_in_first_seen_order() {
        let rows = vec![
            json!({"tableName": "b", "columnName": "x", "totalBytes": 10}),
            json!({"tableName": "a", "columnName": "y", "totalBytes": 5}),
            json!({"tableName": "b", "columnName": "z", "totalBytes": 10}),
        ];
        let tables = group_by(rows, &["tableName"], "columns");
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0]["tableName"], "b");
        assert_eq!(tables[0]["columns"].as_array().unwrap().len(), 2);
        assert_eq!(
            tables[0]["totalBytes"], 10,
            "the first member's fields stay"
        );
        assert_eq!(tables[1]["columns"][0]["columnName"], "y");
    }

    #[test]
    fn groups_by_several_fields() {
        let rows = vec![
            json!({"databaseName": "d1", "tableName": "t"}),
            json!({"databaseName": "d2", "tableName": "t"}),
            json!({"databaseName": "d1", "tableName": "t"}),
        ];
        let tables = group_by(rows, &["databaseName", "tableName"], "columns");
        assert_eq!(tables.len(), 2);
    }

    #[test]
    fn page_paths_add_to_an_existing_query() {
        let paging = Paging { size: 50, max: 100 };
        assert_eq!(
            paging.page_path("/a?connectionId=1", 50, 50),
            "/a?connectionId=1&limit=50&offset=50"
        );
        assert_eq!(paging.page_path("/a", 0, 50), "/a?limit=50&offset=0");
        assert_eq!(paging.reload_limit(10), 50);
        assert_eq!(paging.reload_limit(70), 70);
        assert_eq!(paging.reload_limit(700), 100);
    }

    #[test]
    fn next_offset_follows_has_more_then_total_then_full_pages() {
        let lineage = json!({"assets": [1, 2], "hasMore": true, "nextOffset": 2});
        assert_eq!(next_offset(&lineage, 0, 2), Some(2));
        let last = json!({"assets": [1], "hasMore": false, "nextOffset": null});
        assert_eq!(next_offset(&last, 2, 2), None);

        let runs = json!({"runs": [1, 2], "total": 5, "limit": 2, "offset": 2});
        assert_eq!(next_offset(&runs, 2, 2), Some(4));
        let end = json!({"runs": [5], "total": 5, "limit": 2, "offset": 4});
        assert_eq!(next_offset(&end, 4, 2), None);
        // Past the end the server reports total 0 and no rows.
        assert_eq!(next_offset(&json!({"runs": [], "total": 0}), 6, 2), None);

        assert_eq!(next_offset(&json!([1, 2]), 0, 2), Some(2));
        assert_eq!(next_offset(&json!([1]), 0, 2), None);
    }
}
