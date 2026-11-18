//! Waiting on an ETL run until it ends, optionally streaming its log.
//!
//! The ETL provider settles a running run when its log or its job's
//! current run is read: `POST /etl/logs` and `GET /etl/jobs/{id}/current-run`
//! inspect the run's pods and may mark it Failed (`utils/etl_logs/
//! stale_run.py`). So while a run is going gz only reads:
//! - its status from its own record, `POST /etl/jobStatus` →
//!   `data[0].jobstatus` (`app.py` `etlJobStatusHandler`, a plain
//!   `fn_r_jobExecutionStatus` read);
//! - with `--follow`, its live lines from the session pod's buffer,
//!   `POST /etl/ekslogs` `{jobIdentifier, nextToken}` → `{events,
//!   nextToken}` (`utils/etl_logs/pod_logs.py`), which reads nothing else.
//!
//! Once the status is final the durable CloudWatch log is read
//! (`POST /etl/logs`, paged by `cursor` until `hasMoreLogs` is false), and
//! only the lines the pod buffer had not already shown are printed.

use anyhow::{bail, Context, Result};
use owo_colors::OwoColorize;
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::cli::WaitArgs;
use crate::cmd::code_report::ReportedExit;
use crate::cmd::Runtime;
use crate::output::{self, OutputFormat};

const ETL: &str = "etl";
/// `utils/etl_logs/completion.py` TERMINAL_STATUSES.
pub const FINAL_STATUSES: [&str; 3] = [SUCCESS, FAILED, ABORTED];
pub const SUCCESS: &str = "Success";
const FAILED: &str = "Failed";
const ABORTED: &str = "Aborted";
const RUNNING: &str = "Running";
const STATUS_POLL: Duration = Duration::from_secs(3);
/// Failed pod reads in a row before gz stops asking the pod. A job run's
/// pod may have no service to read it through.
const POD_TRIES: u32 = 5;
const THROTTLE_WAIT: Duration = Duration::from_secs(5);
/// The provider holds a finished run's log open for a minute of grace
/// (`COMPLETION_GRACE`) when the run logged no end marker; past this gz
/// stops waiting for more lines.
const FINISHED_LOG_WAIT: Duration = Duration::from_secs(180);

/// How `--wait` ends, as the process exit code.
pub mod exit {
    pub const SUCCEEDED: i32 = 0;
    pub const FAILED: i32 = 2;
    pub const ABORTED: i32 = 3;
    pub const TIMED_OUT: i32 = 4;
    /// 128 + SIGINT, as shells report a command stopped with Ctrl-C.
    pub const INTERRUPTED: i32 = 130;
}

/// The exit codes, for `--help` and the README.
pub const EXIT_CODES_HELP: &str = "\
Exit codes with --wait or --follow:
  0    the run succeeded
  1    gz could not start or check the run (bad input, network, auth)
  2    the run failed
  3    the run was aborted (cancelled)
  4    --wait-timeout passed first; the run keeps going
  130  you pressed Ctrl-C; the run keeps going";

/// What a caller asked for: nothing, waiting, or waiting with the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitPlan {
    pub follow: bool,
    pub timeout: Option<Duration>,
}

impl WaitPlan {
    /// `None` when the caller does not wait at all. `--wait-timeout` alone
    /// implies `--wait`.
    pub fn from_args(args: &WaitArgs) -> Option<Self> {
        let waits = args.wait || args.follow || args.wait_timeout.is_some();
        waits.then_some(Self {
            follow: args.follow,
            timeout: args.wait_timeout,
        })
    }

    pub fn follow() -> Self {
        Self {
            follow: true,
            timeout: None,
        }
    }
}

/// How waiting on a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitEnd {
    Final(String),
    TimedOut(Duration),
    Interrupted,
}

impl WaitEnd {
    pub fn exit_code(&self) -> i32 {
        match self {
            WaitEnd::Final(status) if status == SUCCESS => exit::SUCCEEDED,
            WaitEnd::Final(status) if status == ABORTED => exit::ABORTED,
            WaitEnd::Final(_) => exit::FAILED,
            WaitEnd::TimedOut(_) => exit::TIMED_OUT,
            WaitEnd::Interrupted => exit::INTERRUPTED,
        }
    }

    fn word(&self) -> &str {
        match self {
            WaitEnd::Final(status) => status,
            WaitEnd::TimedOut(_) => "timedOut",
            WaitEnd::Interrupted => "interrupted",
        }
    }
}

/// Parses `--wait-timeout`: a whole number with an optional unit (`s`, `m`,
/// `h`, `d`); a bare number is seconds.
pub fn parse_timeout(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let amount: u64 = number
        .parse()
        .map_err(|_| format!("{text:?} is not a duration; try 90s, 30m, 2h or 1d"))?;
    let seconds = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("unknown unit {unit:?}; use s, m, h or d")),
    };
    if amount == 0 {
        return Err("the timeout must be more than zero".into());
    }
    Ok(Duration::from_secs(amount * seconds))
}

/// The run's status from its own record; `None` before it has one.
pub async fn read_status(rt: &Runtime, execution: &str) -> Result<Option<String>> {
    let reply = rt
        .http
        .request_json(
            ETL,
            Method::POST,
            "/etl/jobStatus",
            &[],
            Some(json!({"jobIdentifier": execution})),
        )
        .await
        .with_context(|| format!("cannot read the status of run {execution}"))?;
    Ok(reply["data"][0]["jobstatus"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string))
}

fn is_final(status: Option<&str>) -> bool {
    status.is_some_and(|s| FINAL_STATUSES.contains(&s))
}

/// Polls the run's status until it is final, without touching its log.
pub async fn wait_until_final(rt: &Runtime, execution: &str) -> Result<String> {
    loop {
        if let Some(status) = read_status(rt, execution).await? {
            if is_final(Some(&status)) {
                return Ok(status);
            }
        }
        tokio::time::sleep(STATUS_POLL).await;
    }
}

/// Waits on the run per `plan`, stopping at `--wait-timeout` or Ctrl-C without
/// stopping the run.
pub async fn wait(rt: &Runtime, execution: &str, plan: WaitPlan) -> Result<WaitEnd> {
    let watched = async {
        if plan.follow {
            follow(rt, execution).await
        } else {
            wait_until_final(rt, execution).await
        }
    };
    let deadline = async {
        match plan.timeout {
            Some(limit) => tokio::time::sleep(limit).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        status = watched => Ok(WaitEnd::Final(status?)),
        _ = deadline => Ok(WaitEnd::TimedOut(plan.timeout.unwrap_or_default())),
        _ = tokio::signal::ctrl_c() => Ok(WaitEnd::Interrupted),
    }
}

/// Prints how the wait ended; anything but success becomes the process's
/// exit code.
pub fn report(rt: &Runtime, execution: &str, end: &WaitEnd) -> Result<()> {
    let code = end.exit_code();
    if rt.format != OutputFormat::Table {
        let summary = json!({"jobIdentifier": execution, "status": end.word(), "exitCode": code});
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_end(execution, end);
    }
    if code == exit::SUCCEEDED {
        return Ok(());
    }
    Err(ReportedExit(code).into())
}

fn print_end(execution: &str, end: &WaitEnd) {
    let again = follow_again(execution);
    match end {
        WaitEnd::Final(status) if status == SUCCESS => {
            output::success(&format!("run {execution} succeeded"));
        }
        WaitEnd::Final(status) => {
            eprintln!("gz: error: run {execution} ended with status {status}");
        }
        WaitEnd::TimedOut(limit) => output::warn(&format!(
            "stopped waiting after {}; run {execution} is still going (gz did not stop it). {again}",
            describe(*limit)
        )),
        WaitEnd::Interrupted => output::warn(&format!(
            "stopped waiting; run {execution} is still going (Ctrl-C does not stop it). {again}"
        )),
    }
}

fn follow_again(execution: &str) -> String {
    format!("Follow it again: gz jobs logs --job {execution} --follow")
}

fn describe(limit: Duration) -> String {
    let seconds = limit.as_secs();
    match seconds {
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

// ---------- following the log ----------

/// Lines already printed from the pod buffer, by message, so the durable
/// log read after the run only adds what the buffer missed.
#[derive(Default)]
pub struct Printed(HashMap<String, usize>);

impl Printed {
    fn add(&mut self, message: &str) {
        *self.0.entry(key(message)).or_default() += 1;
    }

    /// True (and forgotten once) when `message` was printed before.
    fn take(&mut self, message: &str) -> bool {
        match self.0.get_mut(&key(message)) {
            Some(count) if *count > 0 => {
                *count -= 1;
                true
            }
            _ => false,
        }
    }
}

fn key(message: &str) -> String {
    message.trim_end().to_string()
}

/// Streams the run's log until it ends and returns its final status.
pub async fn follow(rt: &Runtime, execution: &str) -> Result<String> {
    let mut printed = Printed::default();
    let mut pod = PodLog::default();
    let status = loop {
        let status = read_status(rt, execution).await?;
        if is_final(status.as_deref()) {
            break status.unwrap_or_default();
        }
        if status.as_deref() == Some(RUNNING) && pod.is_worth_reading() {
            let page = pod_page(rt, execution, pod.next_line).await;
            for event in pod.take(page) {
                print_event(rt, &event)?;
                printed.add(message(&event));
            }
            if !pod.is_worth_reading() && rt.format == OutputFormat::Table {
                output::info(&format!(
                    "the pod of run {execution} does not serve its lines while it runs; they print when it ends"
                ));
            }
        }
        tokio::time::sleep(STATUS_POLL).await;
    };
    print_finished_log(rt, execution, None, &mut printed).await?;
    Ok(status)
}

/// Where the run's pod buffer is read from, and whether it still answers.
#[derive(Default)]
struct PodLog {
    next_line: u64,
    failures: u32,
}

impl PodLog {
    fn is_worth_reading(&self) -> bool {
        self.failures < POD_TRIES
    }

    /// The new lines of one read, counting failed reads in a row.
    fn take(&mut self, page: PodPage) -> Vec<Value> {
        match page {
            PodPage::Lines(page) => {
                self.failures = 0;
                self.next_line = next_pod_line(&page, self.next_line);
                events(&page).cloned().collect()
            }
            PodPage::Empty => Vec::new(),
            PodPage::Unreachable => {
                self.failures += 1;
                Vec::new()
            }
        }
    }
}

enum PodPage {
    Lines(Value),
    /// The pod answers but has no lines for the run yet.
    Empty,
    Unreachable,
}

/// One read of the pod's buffer from `start`. Lines it cannot serve are
/// read from the durable log once the run ends, so a failure here is not
/// an error.
async fn pod_page(rt: &Runtime, execution: &str, start: u64) -> PodPage {
    let mut body = json!({"jobIdentifier": execution});
    if start > 0 {
        body["nextToken"] = json!(start.to_string());
    }
    match rt
        .http
        .request_json(ETL, Method::POST, "/etl/ekslogs", &[], Some(body))
        .await
    {
        Ok(page) => PodPage::Lines(page),
        // `pod_logs.py` answers 404 "No logs found" from a pod with none yet.
        Err(err) if err.to_string().contains("HTTP 404") => PodPage::Empty,
        Err(_) => PodPage::Unreachable,
    }
}

fn next_pod_line(page: &Value, current: u64) -> u64 {
    page["nextToken"]
        .as_str()
        .and_then(|token| token.parse().ok())
        .unwrap_or(current)
}

/// Prints the finished run's durable log after `cursor`, skipping lines in
/// `printed`. Only for a run whose status is final.
pub async fn print_finished_log(
    rt: &Runtime,
    execution: &str,
    mut cursor: Option<String>,
    printed: &mut Printed,
) -> Result<()> {
    let started = Instant::now();
    loop {
        let mut body = json!({"jobIdentifier": execution});
        if let Some(cursor) = &cursor {
            body["cursor"] = json!(cursor);
        }
        let page = match rt
            .http
            .request_json(ETL, Method::POST, "/etl/logs", &[], Some(body))
            .await
        {
            Ok(page) => page,
            Err(err) if err.to_string().contains("HTTP 429") => {
                tokio::time::sleep(THROTTLE_WAIT).await;
                continue;
            }
            Err(err) => return Err(err.context(format!("cannot read the log of run {execution}"))),
        };
        for event in events(&page) {
            if !printed.take(message(event)) {
                print_event(rt, event)?;
            }
        }
        cursor = page["cursor"].as_str().map(str::to_string).or(cursor);
        if page["hasMoreLogs"] != Value::Bool(true) {
            if let Some(reason) = page["staleReason"].as_str() {
                output::warn(reason);
            }
            return Ok(());
        }
        if started.elapsed() > FINISHED_LOG_WAIT {
            output::warn(&format!(
                "the log of run {execution} is still settling; read the rest later: gz jobs logs --job {execution}"
            ));
            return Ok(());
        }
        tokio::time::sleep(STATUS_POLL).await;
    }
}

fn events(page: &Value) -> impl Iterator<Item = &Value> {
    page["events"].as_array().into_iter().flatten()
}

fn message(event: &Value) -> &str {
    event["message"].as_str().unwrap_or_default()
}

fn print_event(rt: &Runtime, event: &Value) -> Result<()> {
    if rt.format != OutputFormat::Table {
        println!("{}", serde_json::to_string(event)?);
        return Ok(());
    }
    let time = event["timestamp"].as_str().unwrap_or_default();
    println!("{} {}", time.dimmed(), message(event).trim_end());
    Ok(())
}

/// `gz jobs logs --follow`: a run that is still going is followed through
/// its pod; a finished one's log is read from `cursor` on.
pub async fn follow_logs(rt: &Runtime, execution: &str, cursor: Option<String>) -> Result<()> {
    let status = read_status(rt, execution).await?;
    if status.is_none() {
        bail!("run {execution} not found; see `gz projects job-runs <project> <job>`");
    }
    if is_final(status.as_deref()) {
        return print_finished_log(rt, execution, cursor, &mut Printed::default()).await;
    }
    if cursor.is_some() {
        // A cursor points into the durable log, which is read only once
        // the run is over.
        wait_until_final(rt, execution).await?;
        return print_finished_log(rt, execution, cursor, &mut Printed::default()).await;
    }
    match wait(rt, execution, WaitPlan::follow()).await? {
        WaitEnd::Final(_) => Ok(()),
        stopped => report(rt, execution, &stopped),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_take_seconds_minutes_hours_and_days() {
        assert_eq!(parse_timeout("90"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_timeout("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_timeout("30m"), Ok(Duration::from_secs(1800)));
        assert_eq!(parse_timeout("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_timeout("1d"), Ok(Duration::from_secs(86_400)));
        assert!(parse_timeout("0").is_err());
        assert!(parse_timeout("5w").is_err());
        assert!(parse_timeout("soon").is_err());
    }

    #[test]
    fn each_ending_has_its_own_exit_code() {
        assert_eq!(WaitEnd::Final("Success".into()).exit_code(), 0);
        assert_eq!(WaitEnd::Final("Failed".into()).exit_code(), 2);
        assert_eq!(WaitEnd::Final("Aborted".into()).exit_code(), 3);
        assert_eq!(WaitEnd::TimedOut(Duration::from_secs(1)).exit_code(), 4);
        assert_eq!(WaitEnd::Interrupted.exit_code(), 130);
    }

    #[test]
    fn a_timeout_alone_means_wait() {
        let args = WaitArgs {
            wait: false,
            follow: false,
            wait_timeout: Some(Duration::from_secs(5)),
        };
        assert_eq!(
            WaitPlan::from_args(&args),
            Some(WaitPlan {
                follow: false,
                timeout: Some(Duration::from_secs(5))
            })
        );
        let none = WaitArgs {
            wait: false,
            follow: false,
            wait_timeout: None,
        };
        assert_eq!(WaitPlan::from_args(&none), None);
    }

    #[test]
    fn lines_shown_from_the_pod_are_not_shown_again() {
        let mut printed = Printed::default();
        printed.add("rows written: 42\n");
        printed.add("done");
        printed.add("done");
        assert!(printed.take("rows written: 42"));
        assert!(!printed.take("rows written: 42"), "once per printed line");
        assert!(printed.take("done") && printed.take("done"));
        assert!(!printed.take("done"));
        assert!(!printed.take("never printed"));
    }

    #[test]
    fn a_pod_that_keeps_failing_is_no_longer_read() {
        let mut pod = PodLog::default();
        for _ in 1..POD_TRIES {
            assert!(pod.take(PodPage::Unreachable).is_empty());
            assert!(pod.is_worth_reading());
        }
        let page = json!({"events": [{"message": "a"}], "nextToken": "1"});
        assert_eq!(pod.take(PodPage::Lines(page)).len(), 1);
        assert_eq!(pod.next_line, 1);
        for _ in 0..POD_TRIES {
            pod.take(PodPage::Unreachable);
        }
        assert!(!pod.is_worth_reading(), "{POD_TRIES} failures in a row");
        pod.take(PodPage::Empty);
        assert_eq!(pod.failures, POD_TRIES, "an empty buffer is not a failure");
    }

    #[test]
    fn the_pod_cursor_moves_only_when_the_pod_says() {
        assert_eq!(next_pod_line(&json!({"nextToken": "7"}), 3), 7);
        assert_eq!(next_pod_line(&json!({"events": []}), 3), 3);
    }

    #[test]
    fn durations_are_described_in_their_largest_whole_unit() {
        assert_eq!(describe(Duration::from_secs(7200)), "2h");
        assert_eq!(describe(Duration::from_secs(120)), "2m");
        assert_eq!(describe(Duration::from_secs(90)), "90s");
    }
}
