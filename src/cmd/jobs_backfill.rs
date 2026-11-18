//! `gz jobs backfill <project> <job> --from <date> --to <date>`: run a
//! saved job once per hour, day or month, handing each run its slice.
//!
//! How a run gets its slice: the ETL provider replaces `$NAME` placeholders
//! in the job's code with Python literals before the run starts
//! (`utils/ExternalJobProvider.py` `createRunJobArtifact` →
//! `applyJobPlaceholders`). `POST /etl/job` reads the values from
//! `_externalJobPlaceholders` (`app.py` `submitETLJob`), the field
//! `/external/etl/job` fills from its public `placeholders` after checking
//! them (`ExternalJobProvider.py` `getPlaceholders`); gz applies the same
//! checks. So a job reads its slice as `run_date = $RUN_DATE`, which runs
//! as `run_date = '2026-11-01'`; `$RUN_DATE_END` is the next slice's start.
//!
//! Each run is submitted like `gz jobs run` and watched through its status
//! record only (`run_wait`). A state file per backfill records each slice's
//! run and how it ended, so running the same command again skips slices
//! that succeeded, picks up runs still going, and with `--retry` runs the
//! failed ones again.

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::BackfillArgs;
use crate::cmd::apps::saved_job_path;
use crate::cmd::civil::{civil_from_days, days_from_civil};
use crate::cmd::code_report::ReportedExit;
use crate::cmd::run_wait::{self, exit, is_final, SUCCESS};
use crate::cmd::{confirm, jobs, Runtime};
use crate::output::{self, OutputFormat};

const ETL: &str = "etl";
const PROJECTS: &str = "etlprojects";
const STATUS_POLL: Duration = Duration::from_secs(5);
/// More slices than this is almost always a mistyped year.
const MAX_SLICES: usize = 1000;
/// Recorded once a slice's run is submitted, before it has a final status.
const SUBMITTED: &str = "Submitted";
const END_SUFFIX: &str = "_END";

/// How long each slice is.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    Hour,
    Day,
    Month,
}

impl Step {
    fn word(self) -> &'static str {
        match self {
            Step::Hour => "hour",
            Step::Day => "day",
            Step::Month => "month",
        }
    }

    fn example(self) -> &'static str {
        match self {
            Step::Hour => "2026-11-01T05:00",
            Step::Day => "2026-11-01",
            Step::Month => "2026-11",
        }
    }

    /// A slice's start as hours, days or months since the epoch.
    fn parse(self, text: &str) -> Result<i64> {
        let parsed = match self {
            Step::Hour => parse_hour(text),
            Step::Day => parse_day(text),
            Step::Month => parse_month(text),
        };
        parsed.with_context(|| {
            format!(
                "{text:?} is not a {} for --step {}; write it like {}",
                self.word(),
                self.word(),
                self.example()
            )
        })
    }

    /// The value a run receives for the slice starting at `point`.
    fn format(self, point: i64) -> String {
        match self {
            Step::Hour => {
                let (y, m, d) = civil_from_days(point.div_euclid(24));
                format!("{y:04}-{m:02}-{d:02}T{:02}:00", point.rem_euclid(24))
            }
            Step::Day => {
                let (y, m, d) = civil_from_days(point);
                format!("{y:04}-{m:02}-{d:02}")
            }
            Step::Month => format!(
                "{:04}-{:02}",
                point.div_euclid(12),
                point.rem_euclid(12) + 1
            ),
        }
    }
}

fn parse_day(text: &str) -> Option<i64> {
    let mut parts = text.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    if day == 0 || day > crate::cmd::civil::days_in_month(year, month) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

/// `YYYY-MM-DD`, `YYYY-MM-DDTHH` or `YYYY-MM-DDTHH:00`.
fn parse_hour(text: &str) -> Option<i64> {
    let (date, clock) = text.split_once('T').unwrap_or((text, "00"));
    let hour = match clock.split_once(':') {
        Some((hour, "00")) => hour,
        Some(_) => return None,
        None => clock,
    };
    let hour: i64 = hour.parse().ok().filter(|h| (0..24).contains(h))?;
    Some(parse_day(date)? * 24 + hour)
}

/// `YYYY-MM`.
fn parse_month(text: &str) -> Option<i64> {
    let (year, month) = text.split_once('-')?;
    let year: i64 = year.parse().ok()?;
    let month: i64 = month.parse().ok().filter(|m| (1..=12).contains(m))?;
    Some(year * 12 + month - 1)
}

/// Every slice from `from` to `to`, both included.
fn slices(step: Step, from: &str, to: &str) -> Result<Vec<i64>> {
    let (first, last) = (step.parse(from)?, step.parse(to)?);
    if last < first {
        bail!("--to {to} is before --from {from}");
    }
    let count = (last - first + 1) as usize;
    if count > MAX_SLICES {
        bail!(
            "{count} {}s is more than {MAX_SLICES} runs; split the range",
            step.word()
        );
    }
    Ok((first..=last).collect())
}

/// The placeholder name a job writes, checked like the provider's
/// `PLACEHOLDER_PATTERN` (`\$[A-Za-z_][A-Za-z0-9_]*`).
fn placeholder(name: &str) -> Result<String> {
    let name = name.trim_start_matches('$');
    let mut chars = name.chars();
    let starts_well = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !starts_well || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("--param {name:?} must be letters, digits and _ and not start with a digit");
    }
    Ok(format!("${name}"))
}

/// The placeholder values one slice's run receives.
fn slice_values(step: Step, param: &str, point: i64) -> Value {
    json!({
        param: step.format(point),
        format!("{param}{END_SUFFIX}"): step.format(point + 1),
    })
}

/// Whether `code` reads `param` itself (not only as the start of a longer
/// name such as `$RUN_DATE_END`).
fn uses_placeholder(code: &str, param: &str) -> bool {
    code.match_indices(param).any(|(at, _)| {
        !code[at + param.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

// ---------- state ----------

/// What one slice's run did, as last seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SliceRecord {
    status: String,
    job_identifier: String,
}

/// The state file of one backfill.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackfillState {
    slices: BTreeMap<String, SliceRecord>,
}

impl BackfillState {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not a backfill state file; move it away to start over",
                path.display()
            )
        })
    }

    /// Written to a temporary file first, so an interrupted save never
    /// leaves half a file.
    fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let partial = path.with_extension("json.partial");
        std::fs::write(&partial, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("cannot write {}", partial.display()))?;
        std::fs::rename(&partial, path).with_context(|| format!("cannot write {}", path.display()))
    }

    fn record(&mut self, slice: &str, status: &str, execution: &str) {
        self.slices.insert(
            slice.to_string(),
            SliceRecord {
                status: status.to_string(),
                job_identifier: execution.to_string(),
            },
        );
    }
}

/// `<config dir>/backfills/<project>-<job>-<step>-<from>-<to>.json`.
fn state_path(args: &BackfillArgs, slices: &[i64]) -> Result<PathBuf> {
    let config = crate::config::config_path()?;
    let dir = config
        .parent()
        .context("the gz config file has no folder")?
        .join("backfills");
    let (first, last) = (slices[0], slices[slices.len() - 1]);
    let name = format!(
        "{}-{}-{}-{}-{}.json",
        args.project,
        args.job,
        args.step.word(),
        args.step.format(first),
        args.step.format(last)
    )
    .replace(':', "");
    Ok(dir.join(name))
}

// ---------- plan ----------

/// What this invocation does with one slice.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Run,
    /// A run submitted earlier whose end gz has not seen.
    Resume(String),
    Done,
    FailedBefore(String),
}

fn plan(points: &[i64], step: Step, state: &BackfillState, retry: bool) -> Vec<(i64, Action)> {
    points
        .iter()
        .map(|&point| {
            let action = match state.slices.get(&step.format(point)) {
                None => Action::Run,
                Some(r) if r.status == SUCCESS => Action::Done,
                Some(r) if r.status == SUBMITTED => Action::Resume(r.job_identifier.clone()),
                Some(_) if retry => Action::Run,
                Some(r) => Action::FailedBefore(r.status.clone()),
            };
            (point, action)
        })
        .collect()
}

// ---------- running ----------

pub async fn run(rt: &Runtime, args: &BackfillArgs) -> Result<()> {
    if args.concurrency == 0 {
        bail!("--concurrency must be at least 1");
    }
    let param = placeholder(&args.param)?;
    let points = slices(args.step, &args.from, &args.to)?;
    let path = state_path(args, &points)?;
    let mut state = BackfillState::load(&path)?;
    let actions = plan(&points, args.step, &state, args.retry);
    let job = SavedJob::load(rt, args).await?;
    let uses_param = uses_placeholder(&job.code, &param);
    if args.dry_run {
        return show_plan(rt, args, &param, &actions, &path, uses_param);
    }
    if !uses_param {
        bail!(
            "job {} never reads {param}, so every run would do the same thing. Read the slice in its code, e.g. run_date = {param} (and {param}{END_SUFFIX} for the end)",
            job.name
        );
    }
    let to_run = actions.iter().filter(|(_, a)| *a == Action::Run).count();
    if to_run > 0 {
        confirm(
            args.yes,
            &format!(
                "Run job {} {to_run} time(s), one per {}?",
                job.name,
                args.step.word()
            ),
        )?;
    }
    let mut backfill = Backfill {
        rt,
        args,
        param: &param,
        job: &job,
        path: &path,
        state: &mut state,
        total: actions
            .iter()
            .filter(|(_, a)| matches!(a, Action::Run | Action::Resume(_)))
            .count(),
    };
    let finished = tokio::select! {
        done = backfill.run_all(actions) => Some(done?),
        _ = tokio::signal::ctrl_c() => None,
    };
    let Some(()) = finished else {
        output::warn(&format!(
            "stopped waiting; runs already started keep going. Run the same command again to pick them up (state: {})",
            path.display()
        ));
        return Err(ReportedExit(exit::INTERRUPTED).into());
    };
    summarize(rt, args.step, &points, &state, &path)
}

/// The saved job a backfill runs and the submit body every slice shares.
struct SavedJob {
    name: String,
    code: String,
    spec: Value,
}

impl SavedJob {
    async fn load(rt: &Runtime, args: &BackfillArgs) -> Result<Self> {
        let name = jobs::project_jobs(rt, args.project)
            .await?
            .into_iter()
            .find(|j| j.id == args.job)
            .map(|j| j.name)
            .with_context(|| {
                format!(
                    "job {} not found in project {}; see `gz projects jobs {}`",
                    args.job, args.project, args.project
                )
            })?;
        let path = saved_job_path(&args.project.to_string(), &args.job.to_string());
        let saved = rt
            .http
            .request_json(PROJECTS, Method::GET, &path, &[], None)
            .await?;
        let spec = jobs::run_spec(
            &jobs::project_name(rt, args.project).await?,
            args.project,
            args.job,
            &name,
            &saved,
            &args.choice,
        );
        Ok(Self {
            name,
            code: saved["fileContent"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            spec,
        })
    }
}

struct Backfill<'a> {
    rt: &'a Runtime,
    args: &'a BackfillArgs,
    param: &'a str,
    job: &'a SavedJob,
    path: &'a Path,
    state: &'a mut BackfillState,
    total: usize,
}

/// A slice whose run is going.
struct InFlight {
    slice: String,
    execution: String,
}

impl Backfill<'_> {
    /// Keeps `--concurrency` runs going until every slice has ended.
    async fn run_all(&mut self, actions: Vec<(i64, Action)>) -> Result<()> {
        let mut queue: VecDeque<(i64, Action)> = actions.into();
        let mut running: Vec<InFlight> = Vec::new();
        let mut started = 0;
        loop {
            while running.len() < self.args.concurrency {
                let Some((point, action)) = queue.pop_front() else {
                    break;
                };
                let slice = self.args.step.format(point);
                match action {
                    Action::Run => {
                        started += 1;
                        running.push(self.submit(point, &slice, started).await?);
                    }
                    Action::Resume(execution) => {
                        started += 1;
                        self.progress(&format!(
                            "[{started}/{}] {slice} picked up (run {execution})",
                            self.total
                        ));
                        running.push(InFlight { slice, execution });
                    }
                    Action::Done | Action::FailedBefore(_) => {}
                }
            }
            if running.is_empty() {
                return Ok(());
            }
            running = self.settle(running).await?;
            if !running.is_empty() {
                tokio::time::sleep(STATUS_POLL).await;
            }
        }
    }

    async fn submit(&mut self, point: i64, slice: &str, started: usize) -> Result<InFlight> {
        let mut spec = self.job.spec.clone();
        spec["_externalJobPlaceholders"] = slice_values(self.args.step, self.param, point);
        let reply = self
            .rt
            .http
            .request_json(ETL, Method::POST, "/etl/job", &[], Some(spec))
            .await
            .with_context(|| {
                format!("slice {slice} could not start; runs already started keep going, and the same command picks them up")
            })?;
        let execution = reply["jobIdentifier"]
            .as_str()
            .with_context(|| format!("the ETL provider started slice {slice} but sent no run id"))?
            .to_string();
        self.state.record(slice, SUBMITTED, &execution);
        self.state.save(self.path)?;
        self.progress(&format!(
            "[{started}/{}] {slice} started (run {execution})",
            self.total
        ));
        Ok(InFlight {
            slice: slice.to_string(),
            execution,
        })
    }

    /// Records the runs that ended; returns the ones still going.
    async fn settle(&mut self, running: Vec<InFlight>) -> Result<Vec<InFlight>> {
        let mut still = Vec::new();
        for run in running {
            let status = run_wait::read_status(self.rt, &run.execution).await?;
            let Some(status) = status.filter(|s| is_final(Some(s))) else {
                still.push(run);
                continue;
            };
            self.state.record(&run.slice, &status, &run.execution);
            self.state.save(self.path)?;
            self.report_end(&run, &status);
        }
        Ok(still)
    }

    fn report_end(&self, run: &InFlight, status: &str) {
        if self.rt.format != OutputFormat::Table {
            return;
        }
        if status == SUCCESS {
            output::success(&format!("{} succeeded", run.slice));
        } else {
            output::warn(&format!(
                "{} ended {status}; its log: gz jobs logs --job {}",
                run.slice, run.execution
            ));
        }
    }

    fn progress(&self, line: &str) {
        if self.rt.format == OutputFormat::Table {
            output::info(line);
        }
    }
}

// ---------- output ----------

fn show_plan(
    rt: &Runtime,
    args: &BackfillArgs,
    param: &str,
    actions: &[(i64, Action)],
    path: &Path,
    uses_param: bool,
) -> Result<()> {
    let rows: Vec<Value> = actions
        .iter()
        .map(|(point, action)| {
            let values = slice_values(args.step, param, *point);
            let (plan, run) = match action {
                Action::Run => ("run".to_string(), Value::Null),
                Action::Resume(execution) => ("pick up running".to_string(), json!(execution)),
                Action::Done => ("skip: succeeded".to_string(), Value::Null),
                Action::FailedBefore(status) => {
                    (format!("skip: {status} (--retry runs it)"), Value::Null)
                }
            };
            json!({
                "slice": args.step.format(*point),
                "plan": plan,
                param: values[param],
                format!("{param}{END_SUFFIX}"): values[format!("{param}{END_SUFFIX}")],
                "run": run,
            })
        })
        .collect();
    if rt.format != OutputFormat::Table {
        let plan = json!({"slices": rows, "stateFile": path.display().to_string(),
                          "jobReadsParam": uses_param});
        return output::print_value(&plan, rt.format, rt.limit);
    }
    let end = format!("{param}{END_SUFFIX}");
    rt.show_columns(&Value::Array(rows), &["slice", "plan", param, &end, "run"])?;
    output::info(&format!("state file: {}", path.display()));
    if !uses_param {
        output::warn(&format!(
            "the job's code never reads {param}; a real backfill refuses until it does (e.g. run_date = {param})"
        ));
    }
    Ok(())
}

fn summarize(
    rt: &Runtime,
    step: Step,
    points: &[i64],
    state: &BackfillState,
    path: &Path,
) -> Result<()> {
    let rows: Vec<Value> = points
        .iter()
        .map(|&point| {
            let slice = step.format(point);
            let record = state.slices.get(&slice);
            json!({
                "slice": slice,
                "status": record.map(|r| r.status.as_str()),
                "run": record.map(|r| r.job_identifier.as_str()),
            })
        })
        .collect();
    let count = |status: &str| state.slices.values().filter(|r| r.status == status).count();
    let unsuccessful = rows
        .iter()
        .filter(|r| r["status"].as_str() != Some(SUCCESS))
        .count();
    if rt.format == OutputFormat::Table {
        rt.show_columns(&Value::Array(rows), &["slice", "status", "run"])?;
        output::info(&format!(
            "{} succeeded, {} failed, {} aborted of {} slice(s); state: {}",
            count(SUCCESS),
            count("Failed"),
            count("Aborted"),
            points.len(),
            path.display()
        ));
        if unsuccessful > 0 {
            output::warn("run the same command with --retry to run the failed slices again");
        }
    } else {
        let summary = json!({"slices": rows, "stateFile": path.display().to_string()});
        output::print_value(&summary, rt.format, rt.limit)?;
    }
    if unsuccessful > 0 {
        return Err(ReportedExit(exit::FAILED).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_hours_and_months_slice_their_range_inclusively() {
        let days = slices(Step::Day, "2026-02-27", "2026-03-01").unwrap();
        let days: Vec<String> = days.iter().map(|&p| Step::Day.format(p)).collect();
        assert_eq!(days, ["2026-02-27", "2026-02-28", "2026-03-01"]);

        let hours = slices(Step::Hour, "2026-11-01T22", "2026-11-02T01:00").unwrap();
        let hours: Vec<String> = hours.iter().map(|&p| Step::Hour.format(p)).collect();
        assert_eq!(
            hours,
            [
                "2026-11-01T22:00",
                "2026-11-01T23:00",
                "2026-11-02T00:00",
                "2026-11-02T01:00"
            ]
        );

        let months = slices(Step::Month, "2025-11", "2026-01").unwrap();
        let months: Vec<String> = months.iter().map(|&p| Step::Month.format(p)).collect();
        assert_eq!(months, ["2025-11", "2025-12", "2026-01"]);
    }

    #[test]
    fn bad_ranges_and_dates_are_refused() {
        assert!(slices(Step::Day, "2026-11-02", "2026-11-01").is_err());
        assert!(slices(Step::Day, "2026-02-30", "2026-03-01").is_err());
        assert!(slices(Step::Hour, "2026-11-01T24", "2026-11-02").is_err());
        assert!(slices(Step::Hour, "2026-11-01T05:30", "2026-11-02").is_err());
        assert!(slices(Step::Month, "2026-13", "2027-01").is_err());
        let error = slices(Step::Day, "2016-01-01", "2026-01-01").unwrap_err();
        assert!(error.to_string().contains("more than 1000"), "{error}");
    }

    #[test]
    fn each_run_gets_its_slice_and_where_it_ends() {
        let point = Step::Day.parse("2026-12-31").unwrap();
        assert_eq!(
            slice_values(Step::Day, "$RUN_DATE", point),
            json!({"$RUN_DATE": "2026-12-31", "$RUN_DATE_END": "2027-01-01"})
        );
        let month = Step::Month.parse("2026-12").unwrap();
        assert_eq!(
            slice_values(Step::Month, "$M", month),
            json!({"$M": "2026-12", "$M_END": "2027-01"})
        );
    }

    #[test]
    fn placeholder_names_follow_the_providers_pattern() {
        assert_eq!(placeholder("RUN_DATE").unwrap(), "$RUN_DATE");
        assert_eq!(placeholder("$day").unwrap(), "$day");
        assert!(placeholder("1day").is_err());
        assert!(placeholder("run-date").is_err());
    }

    #[test]
    fn only_a_whole_placeholder_counts_as_read() {
        assert!(uses_placeholder("d = $RUN_DATE\n", "$RUN_DATE"));
        assert!(!uses_placeholder("d = $RUN_DATE_END\n", "$RUN_DATE"));
        assert!(!uses_placeholder("print('hi')", "$RUN_DATE"));
    }

    #[test]
    fn a_rerun_skips_successes_picks_up_running_and_retries_only_when_asked() {
        let points = slices(Step::Day, "2026-11-01", "2026-11-04").unwrap();
        let mut state = BackfillState::default();
        state.record("2026-11-01", SUCCESS, "a");
        state.record("2026-11-02", "Failed", "b");
        state.record("2026-11-03", SUBMITTED, "c");
        let actions: Vec<Action> = plan(&points, Step::Day, &state, false)
            .into_iter()
            .map(|(_, a)| a)
            .collect();
        assert_eq!(
            actions,
            [
                Action::Done,
                Action::FailedBefore("Failed".into()),
                Action::Resume("c".into()),
                Action::Run,
            ]
        );
        let retried = plan(&points, Step::Day, &state, true);
        assert_eq!(retried[1].1, Action::Run);
        assert_eq!(retried[0].1, Action::Done);
    }

    #[test]
    fn the_state_file_survives_a_round_trip() {
        let dir = std::env::temp_dir().join(format!("gz-backfill-state-{}", std::process::id()));
        let path = dir.join("b.json");
        let mut state = BackfillState::default();
        state.record("2026-11-01", SUCCESS, "a");
        state.save(&path).unwrap();
        assert_eq!(BackfillState::load(&path).unwrap(), state);
        std::fs::remove_dir_all(&dir).ok();
    }
}
