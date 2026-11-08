//! Picking the run to fix from the job's run history.
//!
//! The history is `GET /etl/consumption-metrics?jobId=…&workloadType=job`,
//! newest first, the list the web app's run-history panel shows. It only
//! reads, so it is safe for any run, live ones included.

use serde_json::Value;

/// A run's state as the history reports it (`status`, lower case; the web
/// labels `cancelled` "Aborted").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

impl RunState {
    fn parse(status: &str) -> Self {
        match status {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Unknown,
        }
    }

    /// Whether the run is still going (its logs must not be read).
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    /// Whether the run is over; only then may its logs be read.
    pub fn is_over(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "aborted",
            Self::Unknown => "in an unknown state",
        }
    }
}

/// One run from the history.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub execution_id: String,
    pub session_id: Option<String>,
    pub state: RunState,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub duration_seconds: Option<f64>,
    pub started_by: Option<String>,
    pub project_id: Option<i64>,
    /// The history item as listed, for its usage figures.
    pub item: Value,
}

impl RunRow {
    pub fn from_item(item: &Value) -> Option<Self> {
        let text = |key: &str| {
            item[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        Some(Self {
            execution_id: text("executionId")?,
            session_id: text("sessionId"),
            state: RunState::parse(item["status"].as_str().unwrap_or_default()),
            started_at: text("startedAt"),
            completed_at: text("completedAt"),
            duration_seconds: item["durationSeconds"].as_f64(),
            started_by: text("startedBy"),
            project_id: item["projectId"].as_i64(),
            item: item.clone(),
        })
    }
}

/// The rows of one history page.
pub fn rows(page: &Value) -> Vec<RunRow> {
    page["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(RunRow::from_item)
        .collect()
}

/// The run to fix.
#[derive(Debug, Clone, PartialEq)]
pub struct Chosen {
    pub run: RunRow,
    /// A newer run succeeded, so the failure may already be fixed.
    pub newer_success: bool,
}

/// Why there is no run to fix.
#[derive(Debug, Clone, PartialEq)]
pub enum NoRun {
    /// The latest run is still going; `failed` is the newest failure before it.
    LatestActive {
        latest: RunRow,
        failed: Option<RunRow>,
    },
    /// The run asked for (`--run`) is still going, or its state is unknown.
    RequestedNotOver(RunRow),
    /// The run asked for isn't in the history.
    NotFound(String),
    /// No run in the history failed.
    NoFailure { latest: Option<RunRow> },
}

/// The newest failed run, or `wanted` (`--run`). Only a run that is over is
/// chosen: reading the logs of one still going can change its state.
pub fn choose(runs: &[RunRow], wanted: Option<&str>) -> Result<Chosen, Box<NoRun>> {
    if let Some(wanted) = wanted {
        let at = runs
            .iter()
            .position(|r| r.execution_id == wanted)
            .ok_or_else(|| Box::new(NoRun::NotFound(wanted.to_string())))?;
        let run = &runs[at];
        if !run.state.is_over() {
            return Err(Box::new(NoRun::RequestedNotOver(run.clone())));
        }
        return Ok(chosen(runs, at));
    }
    let latest_failed = runs.iter().position(|r| r.state == RunState::Failed);
    match runs.first() {
        Some(latest) if latest.state.is_active() => Err(Box::new(NoRun::LatestActive {
            latest: latest.clone(),
            failed: latest_failed.map(|at| runs[at].clone()),
        })),
        _ => match latest_failed {
            Some(at) => Ok(chosen(runs, at)),
            None => Err(Box::new(NoRun::NoFailure {
                latest: runs.first().cloned(),
            })),
        },
    }
}

fn chosen(runs: &[RunRow], at: usize) -> Chosen {
    Chosen {
        run: runs[at].clone(),
        newer_success: runs[..at].iter().any(|r| r.state == RunState::Succeeded),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(id: &str, status: &str) -> RunRow {
        RunRow::from_item(&json!({"executionId": id, "status": status, "sessionId": "s"})).unwrap()
    }

    #[test]
    fn picks_the_newest_failed_run() {
        let runs = [
            run("a", "cancelled"),
            run("b", "failed"),
            run("c", "failed"),
        ];
        let chosen = choose(&runs, None).unwrap();
        assert_eq!(chosen.run.execution_id, "b");
        assert!(!chosen.newer_success);
    }

    #[test]
    fn says_when_a_newer_run_already_succeeded() {
        let runs = [run("a", "succeeded"), run("b", "failed")];
        let chosen = choose(&runs, None).unwrap();
        assert_eq!(chosen.run.execution_id, "b");
        assert!(chosen.newer_success);
    }

    #[test]
    fn refuses_while_the_latest_run_is_going_and_names_the_last_failure() {
        for status in ["running", "queued"] {
            let runs = [run("live", status), run("old", "failed")];
            let why = choose(&runs, None).expect_err("the latest run is going");
            match *why {
                NoRun::LatestActive { latest, failed } => {
                    assert_eq!(latest.execution_id, "live");
                    assert_eq!(failed.unwrap().execution_id, "old");
                }
                other => panic!("{status}: {other:?}"),
            }
        }
    }

    #[test]
    fn an_explicit_run_must_be_listed_and_finished() {
        let runs = [
            run("live", "running"),
            run("odd", "mystery"),
            run("ok", "succeeded"),
            run("bad", "failed"),
        ];
        assert_eq!(
            choose(&runs, Some("live")),
            Err(Box::new(NoRun::RequestedNotOver(runs[0].clone())))
        );
        assert_eq!(
            choose(&runs, Some("odd")),
            Err(Box::new(NoRun::RequestedNotOver(runs[1].clone()))),
            "an unknown state isn't over"
        );
        assert_eq!(
            choose(&runs, Some("nope")),
            Err(Box::new(NoRun::NotFound("nope".into())))
        );
        let chosen = choose(&runs, Some("bad")).unwrap();
        assert_eq!(chosen.run.execution_id, "bad");
        assert!(chosen.newer_success);
        assert_eq!(
            choose(&runs, Some("ok")).unwrap().run.state,
            RunState::Succeeded
        );
    }

    #[test]
    fn no_failure_is_reported_with_the_latest_run() {
        let runs = [run("ok", "succeeded")];
        assert_eq!(
            choose(&runs, None),
            Err(Box::new(NoRun::NoFailure {
                latest: Some(runs[0].clone())
            }))
        );
        assert_eq!(
            choose(&[], None),
            Err(Box::new(NoRun::NoFailure { latest: None }))
        );
    }

    #[test]
    fn reads_rows_and_skips_items_without_an_id() {
        let page = json!({"items": [
            {"executionId": "a", "status": "failed", "projectId": 7, "durationSeconds": 12.5},
            {"status": "failed"},
        ]});
        let rows = rows(&page);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].project_id, Some(7));
        assert_eq!(rows[0].duration_seconds, Some(12.5));
        assert_eq!(rows[0].state, RunState::Failed);
    }
}
