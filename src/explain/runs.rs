//! Which job run wrote a table snapshot, and with which version of its code.
//!
//! The platform does not stamp the run id on a table commit, so the match
//! is built from what each side does record:
//!
//! - the ETL lineage store keeps the Iceberg `snapshotId` of commits that
//!   went through the GZ Lakehouse REST catalog with lineage headers
//!   (`CATALOG_EVENT` observations, groundzero-iceberg-provider-api
//!   iceberg/rest_catalog/audit.py:135); when present, that is the answer;
//! - otherwise, time: the commit falls inside a run's start and end, and
//!   the Spark application id (`app-YYYYMMDDHHMMSS-NNNN`, which standalone
//!   Spark derives from the time the application started) must have started
//!   inside the same run;
//! - the snapshot's `app-name` is whatever the job code passed to its Spark
//!   session (gz-base-rte-spark GZ/SparkUtils.py:528), so it only
//!   corroborates when it equals the job's name.
//!
//! Every match says how sure it is and why.

use serde_json::Value;

use crate::tui::format::epoch_ms;

/// A commit and a run's start or end, recorded by different services, may
/// differ by this much.
pub const CLOCK_SLACK_MS: i64 = 2 * 60 * 1000;

/// One run as `GET /etl/consumption-metrics` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub execution_id: String,
    pub job_id: Option<i64>,
    pub job_name: Option<String>,
    pub project_id: Option<i64>,
    pub project_name: Option<String>,
    /// `queued`, `running`, `succeeded`, `failed` or `cancelled`.
    pub status: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub provider: Option<String>,
}

impl Run {
    pub fn from_item(item: &Value) -> Option<Self> {
        let text = |key: &str| {
            item[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        Some(Self {
            execution_id: text("executionId")?,
            job_id: item["jobId"].as_i64(),
            job_name: text("jobName"),
            project_id: item["projectId"].as_i64(),
            project_name: text("projectName"),
            status: text("status").unwrap_or_else(|| "unknown".to_string()),
            started_at: text("startedAt")?,
            completed_at: text("completedAt"),
            provider: text("provider"),
        })
    }

    fn started_ms(&self) -> Option<i64> {
        epoch_ms(&Value::String(self.started_at.clone()))
    }

    /// A run with no end yet is taken to last until now; one listed as
    /// over without an end time cannot be placed and is left out.
    fn ended_ms(&self, now_ms: i64) -> Option<i64> {
        match &self.completed_at {
            Some(at) => epoch_ms(&Value::String(at.clone())),
            None if matches!(self.status.as_str(), "queued" | "running") => Some(now_ms),
            None => None,
        }
    }

    fn contains(&self, moment_ms: i64, now_ms: i64) -> bool {
        let (Some(start), Some(end)) = (self.started_ms(), self.ended_ms(now_ms)) else {
            return false;
        };
        start - CLOCK_SLACK_MS <= moment_ms && moment_ms <= end + CLOCK_SLACK_MS
    }
}

/// What a snapshot says about the commit that made it.
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub snapshot_id: String,
    pub committed_ms: i64,
    pub app_name: Option<String>,
    pub spark_app_id: Option<String>,
}

/// When a standalone Spark application started, read from its id
/// (`app-20261105073019-0000` → 2026-11-05 07:30:19 UTC). The master's
/// clock is UTC on the platform's pods.
pub fn spark_app_started_ms(app_id: &str) -> Option<i64> {
    let stamp = app_id.strip_prefix("app-")?.split('-').next()?;
    if stamp.len() != 14 || !stamp.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let iso = format!(
        "{}-{}-{}T{}:{}:{}Z",
        &stamp[..4],
        &stamp[4..6],
        &stamp[6..8],
        &stamp[8..10],
        &stamp[10..12],
        &stamp[12..14]
    );
    epoch_ms(&Value::String(iso))
}

/// How sure a match is, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Certainty {
    /// The lineage store recorded this snapshot against the run.
    Recorded,
    /// The only run that was going at commit time, and the Spark
    /// application that committed started inside it.
    Strong,
    /// The only run that fits the commit time, with nothing more to
    /// confirm it.
    Likely,
    /// Several runs fit equally; this is the one that started last
    /// before the commit.
    Ambiguous,
}

impl Certainty {
    pub fn label(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Strong => "strong",
            Self::Likely => "likely",
            Self::Ambiguous => "ambiguous",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunMatch {
    pub run: Run,
    pub certainty: Certainty,
    /// Why, in plain words, one item per piece of evidence.
    pub reasons: Vec<String>,
    /// Other runs that fit as well as this one.
    pub alternatives: Vec<String>,
}

/// The run that most likely made `commit`, from `runs` (the runs listed
/// around the commit time). `recorded` is the execution id the lineage
/// store recorded for this snapshot, when it did.
pub fn match_run(
    commit: &Commit,
    runs: &[Run],
    recorded: Option<&str>,
    now_ms: i64,
) -> Option<RunMatch> {
    if let Some(id) = recorded {
        if let Some(run) = runs.iter().find(|run| run.execution_id == id) {
            return Some(RunMatch {
                run: run.clone(),
                certainty: Certainty::Recorded,
                reasons: vec![format!(
                    "the lineage store recorded snapshot {} for this run",
                    commit.snapshot_id
                )],
                alternatives: Vec::new(),
            });
        }
    }
    let going: Vec<&Run> = runs
        .iter()
        .filter(|run| run.contains(commit.committed_ms, now_ms))
        .collect();
    if going.is_empty() {
        return None;
    }
    let spark_started = commit
        .spark_app_id
        .as_deref()
        .and_then(spark_app_started_ms);
    let scored: Vec<(u8, &Run)> = going
        .iter()
        .map(|run| (score(commit, run, spark_started, now_ms), *run))
        .collect();
    let best_score = scored.iter().map(|(s, _)| *s).max().unwrap_or_default();
    let mut best: Vec<&Run> = scored
        .iter()
        .filter(|(s, _)| *s == best_score)
        .map(|(_, run)| *run)
        .collect();
    // The run that started last before the commit is the most plausible
    // of equals: an earlier one would have committed earlier.
    best.sort_by_key(|run| std::cmp::Reverse(run.started_ms()));
    let chosen = best[0];
    let spark_fits = spark_started.is_some_and(|at| chosen.contains(at, now_ms));
    let names_match = names_match(commit, chosen);
    let certainty = match (best.len(), spark_fits) {
        (1, true) => Certainty::Strong,
        (1, false) => Certainty::Likely,
        _ => Certainty::Ambiguous,
    };
    Some(RunMatch {
        run: chosen.clone(),
        certainty,
        reasons: reasons(commit, chosen, spark_fits, names_match, going.len()),
        alternatives: best[1..]
            .iter()
            .map(|run| run.execution_id.clone())
            .collect(),
    })
}

fn names_match(commit: &Commit, run: &Run) -> bool {
    match (&commit.app_name, &run.job_name) {
        (Some(app), Some(job)) => app.eq_ignore_ascii_case(job),
        _ => false,
    }
}

/// Evidence beyond the time window: the Spark application started inside
/// the run (worth most), and the app name is the job's name.
fn score(commit: &Commit, run: &Run, spark_started: Option<i64>, now_ms: i64) -> u8 {
    let spark_fits = spark_started.is_some_and(|at| run.contains(at, now_ms));
    u8::from(spark_fits) * 2 + u8::from(names_match(commit, run))
}

fn reasons(
    commit: &Commit,
    run: &Run,
    spark_fits: bool,
    names_match: bool,
    overlapping: usize,
) -> Vec<String> {
    let mut reasons = vec![match overlapping {
        1 => "the only run going when the snapshot was committed".to_string(),
        n => format!("{n} runs were going when the snapshot was committed"),
    }];
    if spark_fits {
        if let Some(app) = &commit.spark_app_id {
            reasons.push(format!("Spark application {app} started during this run"));
        }
    }
    if names_match {
        reasons.push("the snapshot's app-name is the job's name".to_string());
    } else if let (Some(app), Some(job)) = (&commit.app_name, &run.job_name) {
        reasons.push(format!(
            "the snapshot's app-name ({app}) is not the job's name ({job}); the job's code names its Spark session"
        ));
    }
    reasons
}

/// A version of a job's code, as the ETL lineage store keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeVersion {
    pub version_id: i64,
    pub number: Option<i64>,
    /// SHA-256 of the code as it ran (placeholders resolved).
    pub code_hash: Option<String>,
    pub compiled_on: Option<String>,
    pub source_uri: Option<String>,
}

/// From `GET /etl/lineage/jobs/{id}` (one job version's graph): that
/// version, when `execution_id` is among its runs.
pub fn version_running(job_lineage: &Value, execution_id: &str) -> Option<CodeVersion> {
    let graph = &job_lineage["graph"];
    let ran_here = graph["runs"]
        .as_array()?
        .iter()
        .any(|run| run["jobExecutionId"].as_str() == Some(execution_id));
    if !ran_here {
        return None;
    }
    let version = &graph["jobVersion"];
    let text = |key: &str| version[key].as_str().map(String::from);
    Some(CodeVersion {
        version_id: version["jobVersionId"].as_i64()?,
        number: version["versionNumber"].as_i64(),
        code_hash: text("codeHash"),
        compiled_on: text("compiledOn"),
        source_uri: text("sourceUri"),
    })
}

/// The id of the version compiled last before `started_at`, from the
/// `versions` list of the same reply: the one a run would have used.
pub fn version_before(job_lineage: &Value, started_at: &str) -> Option<i64> {
    let started = epoch_ms(&Value::String(started_at.to_string()))?;
    job_lineage["versions"]
        .as_array()?
        .iter()
        .filter_map(|version| {
            let compiled = epoch_ms(&version["compiled_on"])?;
            (compiled <= started).then_some((compiled, version["job_version_id"].as_i64()?))
        })
        .max()
        .map(|(_, id)| id)
}

/// The execution id the lineage store recorded for `snapshot_id`: a
/// `CATALOG_EVENT` observation whose metadata names the snapshot, joined
/// to its run.
pub fn recorded_execution(job_lineage: &Value, snapshot_id: &str) -> Option<String> {
    let graph = &job_lineage["graph"];
    let run_id = graph["observations"]
        .as_array()?
        .iter()
        .find(|observation| {
            observation["observationType"] == "CATALOG_EVENT"
                && crate::output::scalar_text(&observation["metadata"]["snapshotId"]) == snapshot_id
        })?["runId"]
        .as_i64()?;
    graph["runs"]
        .as_array()?
        .iter()
        .find(|run| run["runId"].as_i64() == Some(run_id))?["jobExecutionId"]
        .as_str()
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NOW: &str = "2026-11-06T00:00:00Z";

    fn ms(iso: &str) -> i64 {
        epoch_ms(&json!(iso)).unwrap()
    }

    fn run(id: &str, job: &str, started: &str, completed: Option<&str>, status: &str) -> Run {
        Run::from_item(&json!({
            "executionId": id, "jobId": 1825, "jobName": job, "projectId": 1405,
            "projectName": "Invoices", "status": status, "startedAt": started,
            "completedAt": completed, "provider": "spark",
        }))
        .unwrap()
    }

    /// The commit of DemoDB.INVOICE_EXTRACTS snapshot 7209706686888924249 on
    /// stage, and the runs around it.
    fn commit() -> Commit {
        Commit {
            snapshot_id: "7209706686888924249".to_string(),
            committed_ms: ms("2026-11-05T07:46:49.239+00:00"),
            app_name: Some("InvoiceBulkExtract".to_string()),
            spark_app_id: Some("app-20261105073019-0000".to_string()),
        }
    }

    fn runs() -> Vec<Run> {
        vec![
            run(
                "6e36a1953ce3491fa4b4afcb78e9545f",
                "InvoicePDFToLakehouse",
                "2026-11-05T07:30:12.487+00:00",
                Some("2026-11-05T07:46:52.277+00:00"),
                "succeeded",
            ),
            run(
                "eb5c544a58a34c3885b7e3ba8795de46",
                "InvoicePDFToLakehouse",
                "2026-11-05T07:21:17.730+00:00",
                Some("2026-11-05T07:30:09.573+00:00"),
                "cancelled",
            ),
        ]
    }

    #[test]
    fn reads_the_start_time_out_of_a_spark_application_id() {
        assert_eq!(
            spark_app_started_ms("app-20261105073019-0000"),
            Some(ms("2026-11-05T07:30:19Z"))
        );
        assert_eq!(spark_app_started_ms("local-1730790000000"), None);
        assert_eq!(spark_app_started_ms("app-2026110507-0000"), None);
    }

    #[test]
    fn a_commit_inside_one_run_whose_spark_app_started_there_is_strong() {
        let found = match_run(&commit(), &runs(), None, ms(NOW)).unwrap();
        assert_eq!(found.run.execution_id, "6e36a1953ce3491fa4b4afcb78e9545f");
        assert_eq!(found.certainty, Certainty::Strong);
        assert!(found.reasons[1].contains("app-20261105073019-0000 started during this run"));
        assert!(
            found.reasons[2].contains("is not the job's name"),
            "{:?}",
            found.reasons
        );
    }

    #[test]
    fn without_a_spark_id_the_only_run_in_the_window_is_likely() {
        let duckdb = Commit {
            spark_app_id: None,
            app_name: None,
            ..commit()
        };
        let found = match_run(&duckdb, &runs(), None, ms(NOW)).unwrap();
        assert_eq!(found.certainty, Certainty::Likely);
    }

    #[test]
    fn a_commit_outside_every_run_matches_none() {
        let later = Commit {
            committed_ms: ms("2026-11-05T09:00:00Z"),
            ..commit()
        };
        assert_eq!(match_run(&later, &runs(), None, ms(NOW)), None);
    }

    #[test]
    fn overlapping_runs_are_ambiguous_and_the_spark_app_breaks_ties() {
        let mut overlapping = runs();
        overlapping.push(run(
            "other",
            "NightlyLoad",
            "2026-11-05T07:00:00Z",
            Some("2026-11-05T08:00:00Z"),
            "succeeded",
        ));
        let found = match_run(&commit(), &overlapping, None, ms(NOW)).unwrap();
        assert_eq!(found.run.execution_id, "6e36a1953ce3491fa4b4afcb78e9545f");
        assert_eq!(
            found.certainty,
            Certainty::Ambiguous,
            "the other run also contains the Spark start"
        );
        assert_eq!(found.alternatives, ["other"]);
        assert!(found.reasons[0].starts_with("2 runs were going"));

        let no_spark = Commit {
            spark_app_id: None,
            ..commit()
        };
        let found = match_run(&no_spark, &overlapping, None, ms(NOW)).unwrap();
        assert_eq!(found.certainty, Certainty::Ambiguous);
        assert_eq!(
            found.run.execution_id, "6e36a1953ce3491fa4b4afcb78e9545f",
            "of equals, the one that started last before the commit"
        );
    }

    #[test]
    fn the_app_name_breaks_a_tie_but_does_not_make_it_strong() {
        let mut overlapping = runs();
        overlapping.push(run(
            "bulk",
            "InvoiceBulkExtract",
            "2026-11-05T07:00:00Z",
            Some("2026-11-05T08:00:00Z"),
            "succeeded",
        ));
        let no_spark = Commit {
            spark_app_id: None,
            ..commit()
        };
        let found = match_run(&no_spark, &overlapping, None, ms(NOW)).unwrap();
        assert_eq!(found.run.execution_id, "bulk");
        assert_eq!(found.certainty, Certainty::Likely);
        assert!(found
            .reasons
            .iter()
            .any(|r| r.contains("is the job's name")));
    }

    #[test]
    fn a_recorded_snapshot_wins_over_time() {
        let found = match_run(
            &commit(),
            &runs(),
            Some("eb5c544a58a34c3885b7e3ba8795de46"),
            ms(NOW),
        )
        .unwrap();
        assert_eq!(found.certainty, Certainty::Recorded);
        assert_eq!(found.run.execution_id, "eb5c544a58a34c3885b7e3ba8795de46");
    }

    #[test]
    fn a_running_run_lasts_until_now() {
        let live = vec![run(
            "live",
            "Stream",
            "2026-11-05T07:40:00Z",
            None,
            "running",
        )];
        let found = match_run(&commit(), &live, None, ms(NOW)).unwrap();
        assert_eq!(found.run.execution_id, "live");
        let over_without_end = vec![run("x", "Stream", "2026-11-05T07:40:00Z", None, "failed")];
        assert_eq!(match_run(&commit(), &over_without_end, None, ms(NOW)), None);
    }

    fn job_lineage() -> Value {
        json!({
            "versions": [
                {"job_version_id": 1399, "version_number": 6, "compiled_on": "2026-11-05T07:21:17.618+00:00"},
                {"job_version_id": 1398, "version_number": 5, "compiled_on": "2026-11-05T06:40:00+00:00"},
            ],
            "graph": {
                "jobVersion": {
                    "jobVersionId": 1399, "versionNumber": 6, "codeHash": "3cf7e274",
                    "compiledOn": "2026-11-05T07:21:17.618+00:00",
                    "sourceUri": "s3://bucket/preview/etlJob/1405/1825/resolved/3cf7e274.py",
                },
                "runs": [
                    {"runId": 2542, "jobExecutionId": "6e36a1953ce3491fa4b4afcb78e9545f"},
                    {"runId": 2541, "jobExecutionId": "eb5c544a58a34c3885b7e3ba8795de46"},
                ],
                "observations": [
                    {"runId": 2542, "observationType": "RUNTIME_SCHEMA", "role": "TARGET", "metadata": {}},
                    {"runId": 2541, "observationType": "CATALOG_EVENT", "role": "TARGET",
                     "metadata": {"snapshotId": 7209706686888924249u64}},
                ],
            },
        })
    }

    #[test]
    fn finds_the_code_version_a_run_used() {
        let version = version_running(&job_lineage(), "6e36a1953ce3491fa4b4afcb78e9545f").unwrap();
        assert_eq!(version.version_id, 1399);
        assert_eq!(version.number, Some(6));
        assert_eq!(version.code_hash.as_deref(), Some("3cf7e274"));
        assert_eq!(version_running(&job_lineage(), "unknown"), None);
        assert_eq!(
            version_before(&job_lineage(), "2026-11-05T07:00:00Z"),
            Some(1398)
        );
        assert_eq!(version_before(&job_lineage(), "2026-11-01T00:00:00Z"), None);
    }

    #[test]
    fn finds_the_run_the_lineage_store_recorded_for_a_snapshot() {
        assert_eq!(
            recorded_execution(&job_lineage(), "7209706686888924249").as_deref(),
            Some("eb5c544a58a34c3885b7e3ba8795de46")
        );
        assert_eq!(recorded_execution(&job_lineage(), "1"), None);
    }
}
