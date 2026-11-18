//! `quality.toml` on `gz jobs try` and `gz jobs publish`.
//!
//! The rules plug into the lakehouse's own publish gate (iceberg-provider
//! `iceberg/quality_gate.py`): `gz jobs try` turns the gate on for each
//! table the file declares (`PUT …/quality-gate {"required": true}`), then
//! runs the rules on the try's branch, which records the branch's verdict
//! (`gz.quality.branch.<branch>`) stamped with the rules' fingerprint. From
//! then on the provider refuses any publish of a branch without a passing
//! verdict for its current head, from gz and the web app alike.
//!
//! `gz jobs publish` also refuses, before asking the provider, when a
//! declared table's verdict is missing, failed, for an older head, or from
//! other rules than the ones in the folder now (`GET …/quality` returns the
//! verdict as recorded, `rules` included).

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::path::Path;

use super::file::{QualityFile, TableRules};
use super::run::{self, CheckSpec, ChecksRun, Compute};
use crate::cmd::lakehouse_table::LakehouseTable;
use crate::cmd::Runtime;
use crate::output;

const LAKEHOUSE: &str = "lakehouse";
/// The runtime the checks run on when the try's runtime isn't Spark.
pub const DEFAULT_SPARK_RTE: &str = "gz-rte-spark-sklearn:1.0.0";

/// The rules of the folder that apply to the tables a job writes.
#[derive(Debug, Clone)]
pub struct FolderRules {
    pub file: QualityFile,
    pub rules: Vec<TableRules>,
}

impl FolderRules {
    /// `quality.toml` in `dir`, narrowed to `tables`; `None` when there is
    /// no file. A file with problems is an error that lists them.
    pub fn for_tables(dir: &Path, tables: &[&LakehouseTable]) -> Result<Option<Self>> {
        let Some(file) = QualityFile::find(dir)? else {
            return Ok(None);
        };
        let rules = tables
            .iter()
            .filter_map(|table| file.rules_for(table).cloned())
            .collect();
        Ok(Some(Self { file, rules }))
    }

    /// Declared tables the job doesn't write, which the try leaves alone.
    pub fn unused(&self, tables: &[&LakehouseTable]) -> Vec<String> {
        self.file
            .tables
            .iter()
            .filter(|rules| !tables.iter().any(|t| t.key() == rules.table.key()))
            .map(|rules| rules.table.to_string())
            .collect()
    }
}

/// The checks of a try ran, or why they could not.
pub enum TryChecks {
    Ran(ChecksRun),
    Failed(String),
}

impl TryChecks {
    pub fn passed(&self) -> bool {
        matches!(self, TryChecks::Ran(run) if run.passed())
    }
}

/// Turns the publish gate on for the declared tables and runs their rules
/// on `branch`. Never an `Err`: the try already ran, so a problem here is
/// reported as failed checks next to what the run changed.
pub async fn check_try(
    rt: &Runtime,
    folder: &FolderRules,
    branch: &str,
    project: i64,
    compute: i64,
    try_rte: &str,
) -> TryChecks {
    match run_try_checks(rt, folder, branch, project, compute, try_rte).await {
        Ok(run) => TryChecks::Ran(run),
        Err(err) => TryChecks::Failed(format!("{err:#}")),
    }
}

async fn run_try_checks(
    rt: &Runtime,
    folder: &FolderRules,
    branch: &str,
    project: i64,
    compute: i64,
    try_rte: &str,
) -> Result<ChecksRun> {
    for rules in &folder.rules {
        require_gate(rt, &rules.table).await?;
    }
    let specs: Vec<CheckSpec> = folder
        .rules
        .iter()
        .map(|rules| CheckSpec::from_rules(rules, Some(branch)))
        .collect();
    let rte = checks_runtime(try_rte);
    if !rt.quiet {
        output::info(&format!(
            "checking {} on branch {branch} with {} ({rte})",
            folder
                .rules
                .iter()
                .map(|r| r.table.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            folder.file.path.display()
        ));
    }
    run::run(
        rt,
        &specs,
        &Compute {
            project,
            compute,
            rte,
            connection: None,
        },
    )
    .await
}

/// The try's runtime when it is Spark, which the checks need, else the
/// default Spark runtime.
pub fn checks_runtime(try_rte: &str) -> &str {
    if try_rte.to_lowercase().contains("spark") {
        try_rte
    } else {
        DEFAULT_SPARK_RTE
    }
}

/// Turns the table's publish gate on when it is off, saying so.
async fn require_gate(rt: &Runtime, table: &LakehouseTable) -> Result<()> {
    let quality = read_quality(rt, table).await?;
    if quality["required"] == json!(true) {
        return Ok(());
    }
    rt.http
        .request_json(
            LAKEHOUSE,
            Method::PUT,
            &table.path("quality-gate"),
            &[],
            Some(json!({"required": true})),
        )
        .await
        .with_context(|| format!("cannot turn on the quality gate of {table}"))?;
    output::info(&format!(
        "{table}: publishing a branch now needs a passing quality verdict (quality.toml declares it)"
    ));
    Ok(())
}

/// The provider's `/quality` reply: `{required, branches: [{name,
/// headSnapshotId, verdict, current}]}`.
async fn read_quality(rt: &Runtime, table: &LakehouseTable) -> Result<Value> {
    let reply = rt
        .http
        .request_json(LAKEHOUSE, Method::GET, &table.path("quality"), &[], None)
        .await
        .with_context(|| format!("cannot read the quality gate of {table}"))?;
    Ok(reply["response"].clone())
}

/// Why a declared table's branch may not be published, if it may not.
pub fn publish_problem(rules: &TableRules, quality: &Value, branch: &str) -> Option<String> {
    let table = &rules.table;
    let named = quality["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["name"] == json!(branch));
    let verdict = named.map(|b| &b["verdict"]).filter(|v| v.is_object());
    let Some(verdict) = verdict else {
        return Some(format!(
            "{table}: branch {branch} has no quality.toml checks recorded; `gz jobs try` runs them"
        ));
    };
    if named.is_some_and(|b| b["current"] != json!(true)) {
        return Some(format!(
            "{table}: the checks on branch {branch} are for an older head ({})",
            verdict["snapshotId"].as_str().unwrap_or("?")
        ));
    }
    if verdict["rules"].as_str() != Some(rules.fingerprint().as_str()) {
        return Some(format!(
            "{table}: the verdict on branch {branch} is not from the rules in quality.toml now"
        ));
    }
    if verdict["passed"] != json!(true) {
        let failed: Vec<&str> = verdict["failedChecks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        return Some(format!(
            "{table}: branch {branch} failed its quality.toml checks{}",
            if failed.is_empty() {
                String::new()
            } else {
                format!(": {}", failed.join(", "))
            }
        ));
    }
    None
}

/// The declared tables on `branch` that may not be published, and why.
pub async fn publish_problems(
    rt: &Runtime,
    folder: &FolderRules,
    branch: &str,
) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for rules in &folder.rules {
        let quality = read_quality(rt, &rules.table).await?;
        problems.extend(publish_problem(rules, &quality, branch));
    }
    Ok(problems)
}

/// The command that runs the rules on a branch again without a new try.
pub fn recheck_command(folder: &FolderRules, project: i64, branch: &str) -> Vec<String> {
    folder
        .rules
        .iter()
        .map(|rules| {
            format!(
                "gz quality check {} --project {project} --branch {branch} --rules {}",
                rules.table,
                folder.file.path.display()
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::quality::rules::file::parse;

    fn rules() -> TableRules {
        parse("[tables.\"w.d.t\"]\nrows = { min = 1 }\n")
            .unwrap()
            .remove(0)
    }

    fn quality(verdict: Value, current: bool) -> Value {
        json!({"required": true, "branches": [
            {"name": "try-1", "headSnapshotId": "9", "verdict": verdict, "current": current},
        ]})
    }

    fn verdict(passed: bool, fingerprint: &str) -> Value {
        json!({"snapshotId": "9", "passed": passed, "failedChecks": ["row_count"], "rules": fingerprint})
    }

    #[test]
    fn a_current_passing_verdict_from_these_rules_publishes() {
        let rules = rules();
        let fingerprint = rules.fingerprint();
        assert_eq!(
            publish_problem(&rules, &quality(verdict(true, &fingerprint), true), "try-1"),
            None
        );
    }

    #[test]
    fn missing_failed_stale_and_foreign_verdicts_are_refused() {
        let rules = rules();
        let fingerprint = rules.fingerprint();
        let refused = |quality: Value| publish_problem(&rules, &quality, "try-1").unwrap();
        assert!(refused(quality(Value::Null, false)).contains("no quality.toml checks recorded"));
        assert!(
            refused(json!({"required": false, "branches": []})).contains("no quality.toml checks")
        );
        assert!(refused(quality(verdict(true, &fingerprint), false)).contains("older head (9)"));
        assert!(
            refused(quality(verdict(true, "something-else"), true)).contains("not from the rules")
        );
        assert!(
            refused(quality(json!({"snapshotId": "9", "passed": true}), true))
                .contains("not from the rules")
        );
        assert!(refused(quality(verdict(false, &fingerprint), true))
            .ends_with("failed its quality.toml checks: row_count"));
    }

    #[test]
    fn checks_need_a_spark_runtime() {
        assert_eq!(
            checks_runtime("gz-rte-spark-pytorch:1.0.0"),
            "gz-rte-spark-pytorch:1.0.0"
        );
        assert_eq!(
            checks_runtime("gz-rte-duckdb-sklearn:1.0.0"),
            DEFAULT_SPARK_RTE
        );
    }

    #[test]
    fn only_the_tables_the_job_writes_are_checked() {
        let dir = std::env::temp_dir().join(format!("gz-quality-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("quality.toml"),
            "[tables.\"w.d.a\"]\nrows = { min = 1 }\n[tables.\"w.d.b\"]\nrows = { min = 1 }\n",
        )
        .unwrap();
        let written = LakehouseTable::parse("W.D.A").unwrap();
        let folder = FolderRules::for_tables(&dir, &[&written]).unwrap().unwrap();
        assert_eq!(folder.rules.len(), 1);
        assert_eq!(folder.unused(&[&written]), ["w.d.b"]);
        std::fs::write(
            dir.join("quality.toml"),
            "[tables.\"w.d.a\"]\nrows = { min = -1 }\n",
        )
        .unwrap();
        let err = FolderRules::for_tables(&dir, &[&written]).unwrap_err();
        assert!(
            err.to_string()
                .contains("quality.toml:2:8: rows.min can't be negative"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(FolderRules::for_tables(&dir, &[&written])
            .unwrap()
            .is_none());
    }
}
