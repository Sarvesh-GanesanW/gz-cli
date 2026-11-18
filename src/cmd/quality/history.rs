//! The SQL `gz quality history` runs over a results table: the rows
//! qualipilot's `QualityReport.to_rows()` appended (`run_id`, `checked_at`,
//! `source`, `source_version`, `source_ref`, `engine`, `row_count`,
//! `check_name`, `severity`, `status`, `duration_seconds`, `error`,
//! `payload_json`, `package_version`, `config_hash`).

use anyhow::{bail, Result};

use crate::table_history::{ident, TableName};

/// Shown newest first; `payload_json` is left out, it can be 64 KB a row.
/// Durations are rounded to hundredths: stored as measured, they read like
/// `0.000012239999932717184`.
const COLUMNS: [&str; 10] = [
    "checked_at",
    "check_name",
    "severity",
    "status",
    "source_ref",
    "source_version",
    "row_count",
    "round(duration_seconds, 2) AS duration_seconds",
    "error",
    "run_id",
];

/// Names the results table each row came from when several are read.
const RESULTS_TABLE_COLUMN: &str = "results_table";

pub struct HistoryQuery<'a> {
    /// One or more results tables of one warehouse.
    pub results: &'a [TableName],
    pub checked: &'a TableName,
    pub limit: usize,
    pub failed_only: bool,
}

/// Letters, digits, `_` and `-`: a name with anything else never reaches
/// the SQL, quoted or not.
fn check_identifier(part: &str) -> Result<()> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if part.is_empty() || !part.chars().all(allowed) {
        bail!("table names may only use letters, digits, '_' and '-' (got '{part}')");
    }
    Ok(())
}

/// A Spark SQL string literal.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

impl HistoryQuery<'_> {
    /// Rows whose `source` names the checked table, with or without a
    /// catalog in front, whatever its case. Several results tables are read
    /// together, newest first across all of them, each row naming its table.
    pub fn sql(&self) -> Result<String> {
        let checked = [&self.checked.database, &self.checked.table];
        let results = self.results.iter().flat_map(|t| [&t.database, &t.table]);
        for part in checked.into_iter().chain(results) {
            check_identifier(part)?;
        }
        let filters = self.filters().join("\n  AND ");
        let [only] = self.results else {
            return Ok(self.union_sql(&filters));
        };
        Ok(format!(
            "SELECT {}\nFROM {}\nWHERE {filters}\nORDER BY checked_at DESC\nLIMIT {}",
            COLUMNS.join(", "),
            from(only),
            self.limit
        ))
    }

    fn filters(&self) -> Vec<String> {
        let source = self.checked.to_string();
        let suffix = format!(".{source}");
        let mut filters = vec![format!(
            "(lower(source) = lower({}) OR right(lower(source), {}) = lower({}))",
            literal(&source),
            suffix.chars().count(),
            literal(&suffix)
        )];
        if self.failed_only {
            filters.push("(severity <> 'ok' OR status = 'failed')".to_string());
        }
        filters
    }

    fn union_sql(&self, filters: &str) -> String {
        let parts: Vec<String> = self
            .results
            .iter()
            .map(|table| {
                format!(
                    "  SELECT {} AS {RESULTS_TABLE_COLUMN}, {}\n  FROM {}\n  WHERE {}",
                    literal(&table.to_string()),
                    COLUMNS.join(", "),
                    from(table),
                    filters.replace('\n', "\n  ")
                )
            })
            .collect();
        format!(
            "SELECT *\nFROM (\n{}\n) AS history\nORDER BY checked_at DESC\nLIMIT {}",
            parts.join("\n  UNION ALL\n"),
            self.limit
        )
    }
}

fn from(table: &TableName) -> String {
    format!("{}.{}", ident(&table.database), ident(&table.table))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> TableName {
        TableName {
            database: text.split('.').next().unwrap().to_string(),
            table: text.split('.').nth(1).unwrap().to_string(),
        }
    }

    #[test]
    fn selects_the_checked_tables_rows_newest_first() {
        let (results, checked) = (name("dq.results"), name("Sales.orders"));
        let query = HistoryQuery {
            results: std::slice::from_ref(&results),
            checked: &checked,
            limit: 20,
            failed_only: false,
        };
        assert_eq!(
            query.sql().unwrap(),
            "SELECT checked_at, check_name, severity, status, source_ref, source_version, row_count, round(duration_seconds, 2) AS duration_seconds, error, run_id\n\
             FROM `dq`.`results`\n\
             WHERE (lower(source) = lower('Sales.orders') OR right(lower(source), 13) = lower('.Sales.orders'))\n\
             ORDER BY checked_at DESC\n\
             LIMIT 20"
        );
    }

    #[test]
    fn failed_keeps_warnings_errors_and_checks_that_did_not_run() {
        let (results, checked) = (name("dq.results"), name("sales.orders"));
        let query = HistoryQuery {
            results: std::slice::from_ref(&results),
            checked: &checked,
            limit: 5,
            failed_only: true,
        };
        let sql = query.sql().unwrap();
        assert!(
            sql.contains("\n  AND (severity <> 'ok' OR status = 'failed')\n"),
            "{sql}"
        );
        assert!(sql.ends_with("LIMIT 5"), "{sql}");
    }

    #[test]
    fn names_that_could_break_out_of_the_sql_are_refused() {
        let results = name("dq.results");
        for bad in [
            "sales.orders') OR 1=1 --",
            "sales.ord`ers",
            "sales.or ders",
            "sa'les.orders",
        ] {
            let checked = TableName {
                database: bad.split_once('.').unwrap().0.to_string(),
                table: bad.split_once('.').unwrap().1.to_string(),
            };
            let query = HistoryQuery {
                results: std::slice::from_ref(&results),
                checked: &checked,
                limit: 20,
                failed_only: false,
            };
            assert!(query.sql().is_err(), "{bad:?} should be refused");
        }
        let checked = name("sales.orders");
        let bad_results = name("dq.results;drop");
        let query = HistoryQuery {
            results: std::slice::from_ref(&bad_results),
            checked: &checked,
            limit: 20,
            failed_only: false,
        };
        assert!(query.sql().is_err());
    }

    #[test]
    fn several_results_tables_are_read_together_newest_first() {
        let results = [name("dq.results"), name("Ops.orders_quality")];
        let checked = name("sales.orders");
        let query = HistoryQuery {
            results: &results,
            checked: &checked,
            limit: 20,
            failed_only: true,
        };
        assert_eq!(
            query.sql().unwrap(),
            "SELECT *\n\
             FROM (\n\
             \x20 SELECT 'dq.results' AS results_table, checked_at, check_name, severity, status, source_ref, source_version, row_count, round(duration_seconds, 2) AS duration_seconds, error, run_id\n\
             \x20 FROM `dq`.`results`\n\
             \x20 WHERE (lower(source) = lower('sales.orders') OR right(lower(source), 13) = lower('.sales.orders'))\n\
             \x20   AND (severity <> 'ok' OR status = 'failed')\n\
             \x20 UNION ALL\n\
             \x20 SELECT 'Ops.orders_quality' AS results_table, checked_at, check_name, severity, status, source_ref, source_version, row_count, round(duration_seconds, 2) AS duration_seconds, error, run_id\n\
             \x20 FROM `Ops`.`orders_quality`\n\
             \x20 WHERE (lower(source) = lower('sales.orders') OR right(lower(source), 13) = lower('.sales.orders'))\n\
             \x20   AND (severity <> 'ok' OR status = 'failed')\n\
             ) AS history\n\
             ORDER BY checked_at DESC\n\
             LIMIT 20"
        );
    }

    #[test]
    fn literals_double_their_quotes() {
        assert_eq!(literal("o'brien"), "'o''brien'");
        assert_eq!(literal("') OR 1=1 --"), "''') OR 1=1 --'");
    }
}
