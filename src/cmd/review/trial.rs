//! Trying a job for review: `gz jobs try` and `gz jobs discard` exactly as a
//! user would run them, in a child gz with `-o json`. Running the command
//! itself (not a copy of its steps) means every check `try` makes, today
//! and later, is part of the review.
//!
//! The child gets the job's code from a scratch folder that maps only that
//! job, so it runs exactly the code under review whatever state the user's
//! folder is in.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::model::{Step, Tried};
use crate::cmd::devloop::MAPPING_FILE;
use crate::cmd::Runtime;

/// The run options passed on to `gz jobs try`.
pub struct RunChoice<'a> {
    pub compute: i64,
    pub rte: &'a str,
    pub workers: Option<u32>,
    pub gpu: Option<u32>,
}

/// The job being tried.
pub struct TryTarget<'a> {
    pub project: i64,
    pub job_id: i64,
    pub job: &'a str,
    pub code: &'a str,
    pub branch: &'a str,
}

/// Starts of `gz jobs try` errors that mean the job can't be tried as it
/// is, as opposed to gz failing (see `cmd::data_branch`).
const CANNOT_TRY: &[&str] = &[
    // No lakehouse output: `try_job` before anything is made.
    "shows no lakehouse table it writes",
    // The ETL provider's analyzer refused the branch run (`refusal::explain`).
    "won't run the job on branch",
    "Not running on branch",
    // The platform predates branch runs.
    "doesn't support writeBranch yet",
    "doesn't support write branches yet",
];

/// Starts of `gz jobs try` errors after which try itself removed every
/// branch it made (`drop_made` in `cmd::data_branch`).
const TRY_CLEANED_UP: &[&str] = &["cannot make branch", "the run was not started"];

/// `discard`'s error when no table has the branch: nothing to clean up.
const NO_BRANCH: &str = "has a branch named";

/// `review-<job id>-<UTC time>`: valid for `--branch` (letters, digits, '-'
/// and at most 64 characters) and unique per job and second.
pub fn branch_name(job_id: i64, epoch_secs: i64) -> String {
    let (year, month, day) = crate::cmd::civil::civil_from_days(epoch_secs.div_euclid(86_400));
    let seconds = epoch_secs.rem_euclid(86_400);
    format!(
        "review-{job_id}-{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

/// A child gz with the parent's profile and overrides, printing JSON.
fn child(rt: &Runtime) -> Result<tokio::process::Command> {
    let exe =
        std::env::current_exe().context("cannot find the running gz to start `gz jobs try`")?;
    let resolved = rt.http.resolved();
    let mut command = tokio::process::Command::new(exe);
    command
        .env("GZ_PROFILE", &resolved.profile_name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if rt.asked_quiet {
            Stdio::null()
        } else {
            Stdio::inherit()
        })
        .args(["-o", "json"]);
    // Secrets go through the environment, never argv.
    if let Some(host) = &resolved.flag_host {
        command.env("GZ_HOST", host);
    }
    if let Some(token) = &resolved.flag_token {
        command.env("GZ_TOKEN", token);
    }
    if rt.asked_quiet {
        command.arg("-q");
    }
    Ok(command)
}

/// What a child gz printed: its report and its error, either may be missing.
#[derive(Debug, Default, PartialEq)]
pub struct Printed {
    pub report: Option<Value>,
    pub error: Option<String>,
}

/// With `-o json` a command prints its report, then `{"error": …}` if it
/// failed after that; both are JSON documents on stdout.
pub fn parse_printed(stdout: &str) -> Result<Printed> {
    let mut printed = Printed::default();
    for value in serde_json::Deserializer::from_str(stdout).into_iter::<Value>() {
        let value = value.with_context(|| {
            format!(
                "gz printed something that isn't JSON: {}",
                stdout.chars().take(300).collect::<String>()
            )
        })?;
        match value.get("error").and_then(Value::as_str) {
            Some(error) if value.as_object().is_some_and(|o| o.len() == 1) => {
                printed.error = Some(error.to_string());
            }
            _ => printed.report = Some(value),
        }
    }
    Ok(printed)
}

async fn run_child(rt: &Runtime, args: &[String]) -> Result<(Printed, Option<i32>)> {
    let output = child(rt)?
        .args(args)
        .output()
        .await
        .context("cannot start gz")?;
    let printed = parse_printed(&String::from_utf8_lossy(&output.stdout))?;
    Ok((printed, output.status.code()))
}

/// A scratch folder holding only this job's code, mapped to the job.
struct Scratch(PathBuf);

impl Scratch {
    fn new(target: &TryTarget) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "gz-review-{}-{}",
            std::process::id(),
            target.branch
        ));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let file = format!("{}.py", target.job);
        let mapping = json!({"jobs": {file.clone(): target.job}});
        let mapping = toml::to_string(&mapping).context("cannot write the job mapping")?;
        std::fs::write(dir.join(MAPPING_FILE), mapping)
            .and_then(|_| std::fs::write(dir.join(&file), target.code))
            .with_context(|| format!("cannot write the job's code to {}", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn try_args(target: &TryTarget, dir: &Path, choice: &RunChoice) -> Vec<String> {
    let mut args = vec![
        "jobs".to_string(),
        "try".to_string(),
        target.job_id.to_string(),
        "--project".to_string(),
        target.project.to_string(),
        "--branch".to_string(),
        target.branch.to_string(),
        "--dir".to_string(),
        dir.display().to_string(),
        "--compute".to_string(),
        choice.compute.to_string(),
        "--rte".to_string(),
        choice.rte.to_string(),
    ];
    if let Some(workers) = choice.workers {
        args.extend(["--workers".to_string(), workers.to_string()]);
    }
    if let Some(gpu) = choice.gpu {
        args.extend(["--gpu".to_string(), gpu.to_string()]);
    }
    args
}

/// Runs `gz jobs try` on the job's code.
pub async fn try_job(rt: &Runtime, target: &TryTarget<'_>, choice: &RunChoice<'_>) -> Step<Tried> {
    let scratch = match Scratch::new(target) {
        Ok(scratch) => scratch,
        Err(err) => return Step::Failed(format!("{err:#}")),
    };
    match run_child(rt, &try_args(target, scratch.path(), choice)).await {
        Ok((printed, code)) => tried_from(printed, code, target.branch),
        Err(err) => Step::Failed(format!("{err:#}")),
    }
}

/// Reads what `gz jobs try` printed into a review step.
pub fn tried_from(printed: Printed, exit: Option<i32>, branch: &str) -> Step<Tried> {
    let Some(report) = printed.report else {
        return match printed.error {
            Some(error) if CANNOT_TRY.iter().any(|start| error.contains(start)) => {
                Step::Skipped(format!("not tried: {error}"))
            }
            Some(error) => Step::Failed(error),
            None => Step::Failed(format!(
                "gz jobs try exited with {} and printed nothing",
                exit.map_or("a signal".to_string(), |c| format!("code {c}"))
            )),
        };
    };
    let text_list = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    };
    Step::Done(Tried {
        branch: report["branch"].as_str().unwrap_or(branch).to_string(),
        run_id: report["run"]["jobIdentifier"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        status: report["run"]["status"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        tables: report["tables"].as_array().cloned().unwrap_or_default(),
        quality_blocks: text_list(&report["qualityBlocks"]),
        error: printed.error,
        report,
    })
}

/// Whether the try may have left branches behind: it started a run, or it
/// stopped somewhere it does not clean up after itself.
pub fn may_have_left_branches(tried: &Step<Tried>) -> bool {
    match tried {
        Step::Done(_) => true,
        Step::NotAsked(_) | Step::Skipped(_) => false,
        Step::Failed(error) => !TRY_CLEANED_UP.iter().any(|start| error.contains(start)),
    }
}

/// Drops the branch with `gz jobs discard`; says what was dropped.
pub async fn discard(rt: &Runtime, project: i64, job_id: i64, branch: &str) -> Step<String> {
    let args = [
        "jobs",
        "discard",
        &job_id.to_string(),
        "--project",
        &project.to_string(),
        "--branch",
        branch,
    ]
    .map(str::to_string);
    match run_child(rt, &args).await {
        Ok((printed, _)) => discarded_from(printed, branch),
        Err(err) => Step::Failed(format!("{err:#}")),
    }
}

pub fn discarded_from(printed: Printed, branch: &str) -> Step<String> {
    if let Some(error) = printed.error {
        if error.contains(NO_BRANCH) {
            return Step::Done(format!("no table has branch {branch}; nothing to drop"));
        }
        return Step::Failed(format!(
            "{error}; drop it later with `gz jobs discard … --branch {branch}`"
        ));
    }
    let dropped: Vec<&str> = printed
        .report
        .as_ref()
        .and_then(|r| r["dropped"].as_array())
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    Step::Done(format!(
        "branch {branch} dropped from {}",
        if dropped.is_empty() {
            "no table".to_string()
        } else {
            dropped.join(", ")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_are_valid_try_branches() {
        let name = branch_name(3013, 1_794_960_000);
        assert_eq!(name, "review-3013-20261118-000000");
        assert!(name.len() <= 64);
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn reads_a_report_followed_by_its_error() {
        let printed = parse_printed(
            "{\n  \"run\": {\"jobIdentifier\": \"r\", \"status\": \"Failed\"}\n}\n{\n  \"error\": \"run r ended\"\n}\n",
        )
        .unwrap();
        assert_eq!(printed.report.unwrap()["run"]["status"], json!("Failed"));
        assert_eq!(printed.error.as_deref(), Some("run r ended"));
        assert_eq!(parse_printed("").unwrap(), Printed::default());
        assert!(parse_printed("Trying job").is_err());
    }

    #[test]
    fn a_job_that_cannot_be_tried_is_skipped_and_gz_errors_are_errors() {
        let error = |text: &str| Printed {
            report: None,
            error: Some(text.to_string()),
        };
        let skipped = tried_from(
            error("the lineage of job x shows no lakehouse table it writes; pass each one with --table"),
            Some(1),
            "b",
        );
        assert!(matches!(skipped, Step::Skipped(ref r) if r.contains("no lakehouse table")));
        let refused = tried_from(
            error("the ETL provider won't run the job on branch b, because …"),
            Some(1),
            "b",
        );
        assert!(matches!(refused, Step::Skipped(_)));
        assert!(matches!(
            tried_from(error("HTTP 500"), Some(1), "b"),
            Step::Failed(_)
        ));
        assert!(matches!(
            tried_from(Printed::default(), Some(1), "b"),
            Step::Failed(ref r) if r.contains("code 1")
        ));
    }

    #[test]
    fn a_report_carries_the_run_tables_and_quality_blocks() {
        let printed = Printed {
            report: Some(json!({
                "branch": "b",
                "run": {"jobIdentifier": "r", "status": "Success"},
                "tables": [
                    {"table": "db.t", "warehouse": "wh", "changed": true},
                    {"table": "wh.db.u", "changed": false},
                ],
                "qualityBlocks": ["wh.db.t: failed"],
            })),
            error: None,
        };
        let Step::Done(tried) = tried_from(printed, Some(0), "b") else {
            panic!("not done");
        };
        assert!(tried.succeeded());
        assert_eq!(
            tried.changed_tables(),
            vec![("wh".to_string(), "db.t".to_string())]
        );
        assert_eq!(tried.unchanged_tables(), vec!["wh.db.u"]);
        assert_eq!(tried.quality_blocks, vec!["wh.db.t: failed"]);
    }

    #[test]
    fn branches_are_dropped_unless_try_never_left_any() {
        let failed = |text: &str| Step::<Tried>::Failed(text.to_string());
        assert!(!may_have_left_branches(&failed(
            "cannot make branch b of wh.db.t: HTTP 403 Forbidden"
        )));
        assert!(!may_have_left_branches(&failed(
            "HTTP 500: the run was not started, and gz removed branch b again"
        )));
        assert!(!may_have_left_branches(&Step::Skipped(
            "no lakehouse table".into()
        )));
        assert!(may_have_left_branches(&failed(
            "cannot read the run's status"
        )));
    }

    #[test]
    fn discarding_a_branch_no_table_has_is_nothing_to_do() {
        let gone = discarded_from(
            Printed {
                report: None,
                error: Some("no table job x writes has a branch named b; …".into()),
            },
            "b",
        );
        assert!(matches!(gone, Step::Done(ref t) if t.contains("nothing to drop")));
        let dropped = discarded_from(
            Printed {
                report: Some(json!({"branch": "b", "dropped": ["wh.db.t"]})),
                error: None,
            },
            "b",
        );
        assert!(matches!(dropped, Step::Done(ref t) if t.ends_with("wh.db.t")));
    }
}
