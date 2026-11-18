//! Rows and schema of each table a try run wrote, branch against main: the
//! comparison `gz diff --rows` makes, on one SQL session per warehouse.

use anyhow::{Context, Result};
use std::collections::BTreeMap;

use super::model::{Step, TableRows};
use crate::cli::ShellArgs;
use crate::cmd::table_history::report::{self, Subject};
use crate::cmd::Runtime;
use crate::sql::Runner;
use crate::table_history::diff::{self, Comparison, RowDiff, RowRequest};
use crate::table_history::{History, TableName};

/// Sample rows kept per kind of change.
const SAMPLE_ROWS: u32 = 3;
/// `gz diff`'s pointer to its --key flag, which review doesn't have.
const KEY_HINT: &str = "; --key tells them apart)";
/// Width of the sample tables in the text report.
const TEXT_WIDTH: usize = 120;

/// Compares each `(warehouse, database.table)` on `branch` against main.
pub async fn compare(
    rt: &Runtime,
    sql_compute: Option<&str>,
    branch: &str,
    tables: &[(String, String)],
) -> Vec<(String, Step<TableRows>)> {
    let mut by_warehouse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (warehouse, table) in tables {
        by_warehouse.entry(warehouse).or_default().push(table);
    }
    let mut compared = Vec::new();
    for (warehouse, names) in by_warehouse {
        let label = |name: &str| format!("{warehouse}.{name}");
        let session = match open_session(rt, warehouse, sql_compute).await {
            Ok(session) => session,
            Err(err) => {
                let reason = format!("{err:#}");
                compared.extend(
                    names
                        .iter()
                        .map(|name| (label(name), Step::Failed(reason.clone()))),
                );
                continue;
            }
        };
        for name in names {
            let step = match compare_table(rt, &session.runner, warehouse, name, branch).await {
                Ok(rows) => Step::Done(rows),
                Err(err) => Step::Failed(format!("{err:#}")),
            };
            compared.push((label(name), step));
        }
        session.close(rt).await;
    }
    compared
}

/// A SQL session, and whether review started it (and so stops it).
struct Session {
    runner: Runner,
    started_here: bool,
}

impl Session {
    async fn close(self, rt: &Runtime) {
        self.runner.close(rt, !self.started_here).await;
    }
}

/// The SQL session the profile remembers, read fresh from the config file.
fn remembered_session(rt: &Runtime) -> Option<String> {
    let config = crate::config::load_config().ok()?;
    config.profile(&rt.http.resolved().profile_name).sql_session
}

/// Opens the session `gz sql` would use. One that was already running is
/// reused and left running: review only stops a session it started.
async fn open_session(rt: &Runtime, warehouse: &str, compute: Option<&str>) -> Result<Session> {
    let before = remembered_session(rt);
    let options = ShellArgs {
        warehouse: Some(warehouse.to_string()),
        compute: compute.map(str::to_string),
        keep_session: false,
    };
    let runner = Runner::open(rt, &options).await.context(
        "comparing rows needs a SQL session: pass --sql-compute NAME (or --no-rows to skip it)",
    )?;
    let started_here = remembered_session(rt) != before;
    Ok(Session {
        runner,
        started_here,
    })
}

async fn compare_table(
    rt: &Runtime,
    session: &Runner,
    warehouse: &str,
    name: &str,
    branch: &str,
) -> Result<TableRows> {
    let table = TableName::parse(name)?;
    let history = History::load(&rt.http, warehouse, &table).await?;
    let comparison = Comparison::between(&history, "main", branch)?;
    let request = RowRequest {
        table: &table,
        from: &comparison.from,
        to: &comparison.to,
        key: &[],
        sample: SAMPLE_ROWS,
    };
    let rows = diff::compare_rows(session, &request).await?;
    let subject = Subject {
        table: &table,
        warehouse,
        history: &history,
    };
    Ok(table_rows(&subject, &comparison, &rows))
}

fn table_rows(subject: &Subject, comparison: &Comparison, rows: &RowDiff) -> TableRows {
    let names = |columns: &[diff::Column]| columns.iter().map(|c| c.name.clone()).collect();
    TableRows {
        text: report::row_lines(rows, TEXT_WIDTH)
            .join("\n")
            .replace(KEY_HINT, ")"),
        json: report::diff_json(subject, comparison, Some(rows)),
        added_rows: rows.counts.added,
        removed_rows: rows.counts.removed,
        added_columns: names(&rows.schema.added),
        removed_columns: names(&rows.schema.removed),
        retyped_columns: rows
            .schema
            .retyped
            .iter()
            .map(|c| format!("{}: {} → {}", c.name, c.from, c.to))
            .collect(),
    }
}
