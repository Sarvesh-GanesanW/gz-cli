//! Quality rules as code: `quality.toml` in a `gz sync` folder declares the
//! checks of the tables its jobs write ([`file`]). `gz jobs try` runs them
//! on the try's branch and records the branch's verdict, which the
//! lakehouse publish gate reads ([`branch`]); `gz quality lint` checks the
//! file, and `gz quality check --rules` and `gz quality watch --rules` run
//! it on demand or on a schedule.

pub mod branch;
pub mod file;
pub mod run;
pub mod script;

use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;

use crate::cmd::quality::check::report;
use crate::cmd::Runtime;
use crate::output::{self, OutputFormat};
use file::QualityFile;
use run::{ChecksRun, TableOutcome};
use script::TableReport;

/// `gz quality lint [PATH]`: every problem with its line and column.
pub async fn lint(rt: &Runtime, path: Option<&Path>) -> Result<()> {
    let default = Path::new(file::FILE_NAME);
    let path = path.unwrap_or(default);
    let path = if path.is_dir() {
        path.join(file::FILE_NAME)
    } else {
        path.to_path_buf()
    };
    let checked = QualityFile::load(&path)?;
    if rt.format != OutputFormat::Table {
        let tables: Vec<Value> = checked
            .tables
            .iter()
            .map(|rules| {
                json!({
                    "table": rules.table.to_string(),
                    "failOn": rules.fail_on,
                    "checks": rules.check_count(),
                    "fingerprint": rules.fingerprint(),
                })
            })
            .collect();
        return rt
            .show(json!({"file": path.display().to_string(), "valid": true, "tables": tables}))
            .await;
    }
    let checks: usize = checked.tables.iter().map(|t| t.check_count()).sum();
    output::success(&format!(
        "{}: {} table(s), {checks} check(s), no problems",
        path.display(),
        checked.tables.len()
    ));
    Ok(())
}

/// One line per table: passed, or what failed.
pub fn outcome_line(outcome: &TableOutcome) -> String {
    match outcome.problem() {
        None => format!("{}: passed", outcome.table),
        Some(problem) => format!("{}: {problem}", outcome.table),
    }
}

/// The checks of each table, for tables and terminals.
pub fn show_outcomes(rt: &Runtime, run: &ChecksRun) -> Result<()> {
    for outcome in &run.tables {
        match &outcome.report {
            TableReport::Checked(printed) => {
                println!(
                    "{}",
                    report::headline(&outcome.table.to_string(), &printed.report)
                );
                rt.show_columns(&report::check_rows(&printed.report), &report::CHECK_COLUMNS)?;
            }
            TableReport::NotChecked(why) => {
                println!("{}: could not be checked: {why}", outcome.table)
            }
        }
        if outcome.passed() {
            output::success(&outcome_line(outcome));
        } else {
            output::warn(&outcome_line(outcome));
        }
    }
    Ok(())
}

/// Each table's outcome for `-o json`.
pub fn outcomes_json(run: &ChecksRun) -> Value {
    let tables: Vec<Value> = run
        .tables
        .iter()
        .map(|outcome| {
            let (report, failures, error) = match &outcome.report {
                TableReport::Checked(printed) => (
                    printed.report.clone(),
                    json!(printed.gate_failures),
                    Value::Null,
                ),
                TableReport::NotChecked(why) => (Value::Null, json!([]), json!(why)),
            };
            json!({
                "table": outcome.table.to_string(),
                "failOn": outcome.fail_on.as_str(),
                "passed": outcome.passed(),
                "gateFailures": failures,
                "error": error,
                "report": report,
            })
        })
        .collect();
    json!({"run": run.execution, "passed": run.passed(), "tables": tables})
}
