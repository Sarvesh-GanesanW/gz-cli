//! `\d table`: Spark's `DESCRIBE TABLE EXTENDED` answer read into columns,
//! partitioning and a properties summary, and drawn as one block.
//!
//! The answer is rows of `{col_name, data_type, comment}`: the columns, a
//! blank row, then sections headed `# …` (Spark's `DescribeTableExec`):
//! `# Partition Information` (identity partitions, as columns) or
//! `# Partitioning` (`Part 0 | days(ts)`), `# Metadata Columns`, and
//! `# Detailed Table Information` (`Name`, `Type`, `Location`, `Provider`,
//! `Owner`, `Table Properties [k=v,…]`, `Statistics`). A table without
//! partitions has no partition section at all.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use serde_json::{json, Value};

use super::results::{self, Page};
use crate::tui::theme;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableDescription {
    pub name: Option<String>,
    /// `(name, type, comment)` in table order.
    pub columns: Vec<(String, String, String)>,
    /// Partition fields as Spark describes them: `days(ts)`, `country`.
    pub partitioning: Vec<String>,
    /// `Type`, `Provider`, `Location`, `Owner`, `Statistics`, … in order.
    pub details: Vec<(String, String)>,
    pub properties: Vec<(String, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Columns,
    Partitions,
    Detail,
    Skipped,
}

const NAME: &str = "col_name";
const TYPE: &str = "data_type";
const COMMENT: &str = "comment";

impl TableDescription {
    pub fn from_page(page: &Page) -> Self {
        let mut found = Self::default();
        let mut section = Section::Columns;
        for row in &page.rows {
            let field = |key: &str| row[key].as_str().unwrap_or_default().trim().to_string();
            let (name, kind, comment) = (field(NAME), field(TYPE), field(COMMENT));
            if name.is_empty() && kind.is_empty() {
                continue;
            }
            if let Some(header) = name.strip_prefix('#') {
                section = section_named(header.trim(), section);
                continue;
            }
            match section {
                Section::Columns => found.columns.push((name, kind, comment)),
                Section::Partitions if name.starts_with("Part ") => found.partitioning.push(kind),
                Section::Partitions => found.partitioning.push(name),
                Section::Detail => found.add_detail(name, kind),
                Section::Skipped => {}
            }
        }
        found
    }

    fn add_detail(&mut self, key: String, value: String) {
        match key.as_str() {
            "Name" => self.name = Some(value),
            "Table Properties" => self.properties = parse_properties(&value),
            _ => self.details.push((key, value)),
        }
    }

    /// The block `\d` prints.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some(name) = &self.name {
            lines.push(Line::from(Span::styled(name.clone(), theme::title())));
        }
        lines.extend(results::render(&self.columns_page(), width, false));
        lines.push(Line::from(""));
        let partitioning = if self.partitioning.is_empty() {
            "not partitioned".to_string()
        } else {
            self.partitioning.join(", ")
        };
        lines.push(labelled("Partitioned by", &partitioning));
        for (key, value) in &self.details {
            lines.push(labelled(key, value));
        }
        if !self.properties.is_empty() {
            lines.push(labelled(
                "Properties",
                &format!("{} set", self.properties.len()),
            ));
            for (key, value) in &self.properties {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {key}"), theme::muted()),
                    Span::raw(format!(" = {value}")),
                ]));
            }
        }
        lines
    }

    fn columns_page(&self) -> Page {
        let has_comments = self.columns.iter().any(|(_, _, c)| !c.is_empty());
        let mut columns = vec!["column".to_string(), "type".to_string()];
        if has_comments {
            columns.push("comment".to_string());
        }
        let rows = self
            .columns
            .iter()
            .map(|(name, kind, comment)| json!({"column": name, "type": kind, "comment": comment}))
            .collect::<Vec<Value>>();
        Page {
            columns,
            column_types: Vec::new(),
            rows,
            page_number: 1,
            has_more: false,
            total_records: None,
        }
    }
}

fn section_named(header: &str, current: Section) -> Section {
    match header {
        "Partition Information" | "Partitioning" => Section::Partitions,
        "Detailed Table Information" => Section::Detail,
        // `# col_name data_type comment` heads the partition columns.
        "col_name" => current,
        _ => Section::Skipped,
    }
}

fn labelled(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key}: "),
            theme::muted().add_modifier(Modifier::BOLD),
        ),
        Span::raw(value.to_string()),
    ])
}

/// `[a=1,b=x,y]` → `[(a, 1), (b, "x,y")]`: a piece without `=` belongs to
/// the value before it.
fn parse_properties(text: &str) -> Vec<(String, String)> {
    let inner = text.trim().trim_start_matches('[').trim_end_matches(']');
    let mut properties: Vec<(String, String)> = Vec::new();
    for piece in inner.split(',').filter(|p| !p.is_empty()) {
        match (piece.split_once('='), properties.last_mut()) {
            (Some((key, value)), _) => properties.push((key.trim().into(), value.trim().into())),
            (None, Some((_, value))) => {
                value.push(',');
                value.push_str(piece);
            }
            (None, None) => properties.push((piece.trim().into(), String::new())),
        }
    }
    properties
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, kind: &str) -> Value {
        json!({"col_name": name, "data_type": kind, "comment": ""})
    }

    fn page(rows: Vec<Value>) -> Page {
        results::parse_page(&json!({"data": rows, "pageNumber": 1}))
    }

    fn unpartitioned() -> Page {
        page(vec![
            row("plant_id", "string"),
            row("units_produced", "int"),
            row("", ""),
            row("# Metadata Columns", ""),
            row("_file", "string"),
            row("", ""),
            row("# Detailed Table Information", ""),
            row("Name", "WH.DemoDB.ProductionMetrics"),
            row("Type", "MANAGED"),
            row("Provider", "iceberg"),
            row(
                "Table Properties",
                "[current-snapshot-id=88,format=iceberg/parquet,write.metadata.metrics.default=truncate(16),x]",
            ),
            row("Statistics", "38000000 bytes, 250000 rows"),
        ])
    }

    #[test]
    fn reads_columns_details_and_properties_without_metadata_columns() {
        let described = TableDescription::from_page(&unpartitioned());
        assert_eq!(
            described.name.as_deref(),
            Some("WH.DemoDB.ProductionMetrics")
        );
        let names: Vec<&str> = described.columns.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(names, vec!["plant_id", "units_produced"]);
        assert!(described.partitioning.is_empty());
        assert_eq!(described.details[0], ("Type".into(), "MANAGED".into()));
        assert_eq!(
            described.properties[2],
            (
                "write.metadata.metrics.default".into(),
                "truncate(16),x".into()
            )
        );
    }

    #[test]
    fn reads_both_partition_layouts() {
        let transforms = page(vec![
            row("ts", "timestamp"),
            row("", ""),
            row("# Partitioning", ""),
            row("Part 0", "days(ts)"),
            row("Part 1", "bucket(16, id)"),
        ]);
        assert_eq!(
            TableDescription::from_page(&transforms).partitioning,
            vec!["days(ts)", "bucket(16, id)"]
        );
        let identity = page(vec![
            row("country", "string"),
            row("# Partition Information", ""),
            row("# col_name", "data_type"),
            row("country", "string"),
        ]);
        let described = TableDescription::from_page(&identity);
        assert_eq!(described.partitioning, vec!["country"]);
        assert_eq!(described.columns.len(), 1);
    }

    #[test]
    fn draws_one_block() {
        let lines = TableDescription::from_page(&unpartitioned()).lines(80);
        let text = crate::sql::render::plain(&lines);
        assert!(
            text.starts_with("WH.DemoDB.ProductionMetrics\n column"),
            "{text}"
        );
        assert!(text.contains("Partitioned by: not partitioned"), "{text}");
        assert!(
            text.contains("Statistics: 38000000 bytes, 250000 rows"),
            "{text}"
        );
        assert!(text.contains("Properties: 3 set"), "{text}");
        assert!(text.contains("  format = iceberg/parquet"), "{text}");
    }
}
