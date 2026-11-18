//! Running checks on several tables in one temporary Spark job, and what
//! each table's report says.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::file::TableRules;
use super::script::{self, ScriptParams, TableJob, TableReport};
use crate::cmd::code_report::ReportedExit;
use crate::cmd::lakehouse_table::LakehouseTable;
use crate::cmd::quality::args::FailOn;
use crate::cmd::quality::check::script::ConnectedTable;
use crate::cmd::quality::check::{
    self, lakehouse_connections, pick_connection, LakehouseConnection, RunOutcome,
};
use crate::cmd::Runtime;

/// Exit code when checks fail, as `gz quality check` uses.
pub const CHECKS_FAILED_EXIT: i32 = 2;

/// What to check on one table.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckSpec {
    pub table: LakehouseTable,
    pub branch: Option<String>,
    pub fail_on: FailOn,
    /// `None` keeps the runtime's default checks.
    pub config: Option<Value>,
    pub checks: Vec<Value>,
    pub fingerprint: Option<String>,
    pub results: Option<LakehouseTable>,
}

impl CheckSpec {
    /// The rules of a `quality.toml` table, on `branch` (stamped with the
    /// rules' fingerprint) or on `main`.
    pub fn from_rules(rules: &TableRules, branch: Option<&str>) -> Self {
        Self {
            table: rules.table.clone(),
            branch: branch.map(str::to_string),
            fail_on: rules.fail_on(),
            config: Some(rules.qualipilot_config()),
            checks: rules.registered_checks(),
            fingerprint: branch.map(|_| rules.fingerprint()),
            results: None,
        }
    }
}

/// Where the job runs.
pub struct Compute<'a> {
    pub project: i64,
    pub compute: i64,
    pub rte: &'a str,
    /// The lakehouse connection to read the tables through (default: one
    /// per warehouse).
    pub connection: Option<&'a str>,
}

/// A table's outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct TableOutcome {
    pub table: LakehouseTable,
    pub fail_on: FailOn,
    pub report: TableReport,
}

impl TableOutcome {
    pub fn passed(&self) -> bool {
        matches!(&self.report, TableReport::Checked(printed) if printed.gate_failures.is_empty())
    }

    /// The checks that failed, or why the table could not be checked.
    pub fn problem(&self) -> Option<String> {
        match &self.report {
            TableReport::Checked(printed) if printed.gate_failures.is_empty() => None,
            TableReport::Checked(printed) => Some(format!(
                "failed at {}: {}",
                self.fail_on.as_str(),
                printed.gate_failures.join(", ")
            )),
            TableReport::NotChecked(why) => Some(format!("could not be checked: {why}")),
        }
    }
}

/// The run and every table's outcome, in the order asked.
pub struct ChecksRun {
    pub execution: String,
    pub tables: Vec<TableOutcome>,
}

impl ChecksRun {
    pub fn passed(&self) -> bool {
        self.tables.iter().all(TableOutcome::passed)
    }

    /// Exit code 2 when a table failed, as `gz quality check` exits.
    pub fn exit(&self) -> Result<()> {
        if self.passed() {
            Ok(())
        } else {
            Err(ReportedExit(CHECKS_FAILED_EXIT).into())
        }
    }
}

/// The script for `specs`, with the connections it needs.
pub struct PlannedJob {
    pub code: String,
    pub connections: Vec<LakehouseConnection>,
    pub marker: String,
}

/// Picks each table's connection and writes the script.
pub async fn plan(
    rt: &Runtime,
    specs: &[CheckSpec],
    connection: Option<&str>,
    marker: String,
    fail_run: bool,
) -> Result<PlannedJob> {
    let site = rt
        .http
        .resolved()
        .site()
        .context("no site is set; run `gz configure` first")?;
    let registry = lakehouse_connections(rt).await?;
    let mut connections: Vec<LakehouseConnection> = Vec::new();
    let mut add = |picked: &LakehouseConnection| {
        if !connections.iter().any(|c| c.id == picked.id) {
            connections.push(picked.clone());
        }
        picked.name.clone()
    };
    let mut jobs = Vec::new();
    for spec in specs {
        let checked = pick_connection(&registry, &spec.table, connection)?;
        let results = match &spec.results {
            Some(results) => {
                let wanted = results
                    .warehouse
                    .eq_ignore_ascii_case(&spec.table.warehouse)
                    .then_some(checked.name.as_str());
                let picked = pick_connection(&registry, results, wanted)?;
                Some(ConnectedTable {
                    connection: add(picked),
                    table: results.name.clone(),
                })
            }
            None => None,
        };
        jobs.push(TableJob {
            shown: spec.table.to_string(),
            connection: add(checked),
            table: spec.table.name.clone(),
            branch: spec.branch.clone(),
            fail_on: spec.fail_on.as_str(),
            config: spec.config.clone(),
            checks: spec.checks.clone(),
            fingerprint: spec.fingerprint.clone(),
            results,
        });
    }
    let code = script::script(&ScriptParams {
        tables: &jobs,
        site: &site,
        marker: &marker,
        fail_run,
    });
    Ok(PlannedJob {
        code,
        connections,
        marker,
    })
}

/// Runs the checks of `specs` as one temporary job and reads each table's
/// report.
pub async fn run(rt: &Runtime, specs: &[CheckSpec], compute: &Compute<'_>) -> Result<ChecksRun> {
    if !compute.rte.to_lowercase().contains("spark") {
        bail!(
            "--rte {} is not a Spark runtime; checks from quality.toml need one, e.g. gz-rte-spark-sklearn:1.0.0",
            compute.rte
        );
    }
    let marker = format!("GZR-{}", check::short_id());
    let planned = plan(rt, specs, compute.connection, marker, false).await?;
    let outcome = check::run_temporary_job(
        rt,
        &check::JobToRun {
            project: compute.project,
            code: &planned.code,
            connections: &planned.connections,
            compute: compute.compute,
            rte: compute.rte,
        },
    )
    .await?;
    let tables = read_outcomes(specs, &outcome, &planned.marker)?;
    Ok(ChecksRun {
        execution: outcome.execution,
        tables,
    })
}

/// Each table's report from the run's log, in the order of `specs`.
pub fn read_outcomes(
    specs: &[CheckSpec],
    outcome: &RunOutcome,
    marker: &str,
) -> Result<Vec<TableOutcome>> {
    let mut tables = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        let lines = outcome.messages.iter().map(String::as_str);
        let report = match script::read_table_report(lines, marker, index + 1)? {
            Some(report) => report,
            None if tables.is_empty() => bail!("{}", check::no_report_message(outcome)),
            None => TableReport::NotChecked(format!(
                "run {} ended {} before checking it",
                outcome.execution, outcome.status
            )),
        };
        tables.push(TableOutcome {
            table: spec.table.clone(),
            fail_on: spec.fail_on,
            report,
        });
    }
    Ok(tables)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::quality::rules::file::parse;

    fn outcome(lines: &[&str]) -> RunOutcome {
        RunOutcome {
            execution: "run1".into(),
            status: "Success".into(),
            messages: lines.iter().map(|l| l.to_string()).collect(),
        }
    }

    fn specs() -> Vec<CheckSpec> {
        let rules = parse(
            "[tables.\"w.d.a\"]\nrows = { min = 1 }\n[tables.\"w.d.b\"]\nunique = [\"id\"]\nfail_on = \"warn\"\n",
        )
        .unwrap();
        rules
            .iter()
            .map(|r| CheckSpec::from_rules(r, Some("try-1")))
            .collect()
    }

    #[test]
    fn rules_on_a_branch_carry_their_fingerprint() {
        let specs = specs();
        assert_eq!(specs[0].branch.as_deref(), Some("try-1"));
        assert_eq!(specs[0].fingerprint.as_ref().unwrap().len(), 16);
        assert_eq!(specs[1].fail_on, FailOn::Warn);
        let on_main = CheckSpec::from_rules(
            &parse("[tables.\"w.d.a\"]\nrows = { min = 1 }\n").unwrap()[0],
            None,
        );
        assert_eq!(on_main.fingerprint, None);
    }

    #[test]
    fn each_table_gets_its_own_outcome() {
        let run = outcome(&[
            "GZR-x-001 1/1 {\"report\":{\"results\":[]},\"gateFailures\":[]}",
            "GZR-x-002 1/1 {\"report\":{\"results\":[]},\"gateFailures\":[\"unique\"]}",
        ]);
        let tables = read_outcomes(&specs(), &run, "GZR-x").unwrap();
        assert!(tables[0].passed());
        assert!(!tables[1].passed());
        assert_eq!(tables[1].problem().unwrap(), "failed at warn: unique");
    }

    #[test]
    fn a_run_that_stopped_part_way_says_so() {
        let run = outcome(&["GZR-x-001 1/1 {\"report\":{\"results\":[]},\"gateFailures\":[]}"]);
        let tables = read_outcomes(&specs(), &run, "GZR-x").unwrap();
        assert!(tables[1].problem().unwrap().contains("before checking it"));
        let nothing = outcome(&["Traceback: boom"]);
        assert!(read_outcomes(&specs(), &nothing, "GZR-x")
            .unwrap_err()
            .to_string()
            .contains("without printing a report"));
    }
}
