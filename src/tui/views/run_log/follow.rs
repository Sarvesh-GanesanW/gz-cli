//! Following one run's logs, as pure state: what each `/etl/logs` page or
//! failure changes, and when to ask again. The timing copies the web's job
//! editor (`jobLogStreamModel.js`, `useJobLogStream.js`): 1 s after a page
//! with new lines, 3 s when idle, the server's wait on 429, backing off on
//! errors, and giving up after two minutes without an answer.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::tui::errors::{self, Failure};

pub const BUSY_POLL_MS: i64 = 1_000;
pub const IDLE_POLL_MS: i64 = 3_000;
/// The wait on 429 when the server doesn't say.
pub const THROTTLE_WAIT_MS: i64 = 2_000;
const BACKOFF_MS: [i64; 5] = [1_000, 2_000, 4_000, 8_000, 15_000];
pub const GIVE_UP_AFTER_MS: i64 = 2 * 60_000;
pub const QUIET_AFTER_MS: i64 = 15 * 60_000;

static SUB_JOB_STARTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^EXECUTING JOB: (.+) \((\d+)/(\d+)\)$")
        .expect("the sub-job start pattern is a valid regex")
});
static SUB_JOB_FINISHED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Job (.+) completed successfully$")
        .expect("the sub-job end pattern is a valid regex")
});
static LEADING_LEVEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:\[?\d[\d\-/:., TZ+]*\]?\s*)?[\[(]?(DEBUG|INFO|WARN(?:ING)?|ERROR|CRITICAL|FATAL)\b",
    )
    .expect("the log level pattern is a valid regex")
});
static RUNTIME_FAILURE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:Error occurred:|User code error at line \d+|Traceback \(most recent call last\))",
    )
    .expect("the runtime failure pattern is a valid regex")
});

/// How a log line is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warning,
    Error,
    Other,
}

impl Level {
    /// A leading level name, or a runtime failure line (`Error occurred:`).
    pub fn of(text: &str) -> Level {
        if let Some(caps) = LEADING_LEVEL.captures(text) {
            return match &caps[1] {
                "DEBUG" => Level::Debug,
                "INFO" => Level::Info,
                "WARN" | "WARNING" => Level::Warning,
                _ => Level::Error,
            };
        }
        if RUNTIME_FAILURE.is_match(text) {
            Level::Error
        } else {
            Level::Other
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub at_ms: Option<i64>,
    pub text: String,
    pub level: Level,
}

/// A multi-job run's progress from the runtime's markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubJob {
    pub index: u32,
    pub total: u32,
    pub name: String,
    pub finished: bool,
}

/// Why following stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// The run ended with this status (`Success`, `Failed`, `Aborted`).
    Finished(String),
    NotFound,
    /// No answer for two minutes; the run may still be going.
    GaveUp,
    /// Access or request errors that retrying won't fix.
    Refused(String),
}

impl Stopped {
    /// The web's closing notice.
    pub fn notice(&self) -> String {
        match self {
            Stopped::Finished(status) => match status.as_str() {
                "Success" => "Job completed successfully".to_string(),
                "Failed" => "Job failed. Check the logs for the cause.".to_string(),
                "Aborted" => "Job was aborted".to_string(),
                other => format!("Job ended: {other}"),
            },
            Stopped::NotFound => "The run could not be found.".to_string(),
            Stopped::GaveUp => "Log streaming stopped after 2 minutes without a response. \
                The job may still be running. Press R to resume."
                .to_string(),
            Stopped::Refused(reason) => reason.clone(),
        }
    }
}

/// With no usual run length to compare, an active run this old may be stuck.
pub const STUCK_WITHOUT_HISTORY_MS: i64 = 6 * 3_600_000;
/// A run this many times longer than usual may be stuck…
const STUCK_FACTOR: i64 = 10;
/// …but never before this, since pods alone can take minutes to start.
const STUCK_FLOOR_MS: i64 = 30 * 60_000;

/// Whether an active run looks stuck: running far past the job's usual
/// length (or 6 h without history), or "running" with an end time set.
/// Stage has ~185 such rows, months old.
pub fn may_be_stuck(elapsed_ms: i64, usual_ms: Option<i64>, has_end_time: bool) -> bool {
    let limit = usual_ms.map_or(STUCK_WITHOUT_HISTORY_MS, |usual| {
        (usual * STUCK_FACTOR).max(STUCK_FLOOR_MS)
    });
    has_end_time || elapsed_ms > limit
}

const TERMINAL: [&str; 3] = ["Success", "Failed", "Aborted"];

pub fn is_terminal(status: &str) -> bool {
    TERMINAL.iter().any(|t| t.eq_ignore_ascii_case(status))
}

#[derive(Debug, Clone, Default)]
pub struct Follower {
    pub lines: Vec<LogLine>,
    seen: HashSet<String>,
    pub cursor: Option<String>,
    /// `jobStatus[0].jobstatus`: Queued, Running, Success, Failed, Aborted.
    pub status: Option<String>,
    pub started_ms: Option<i64>,
    pub stopped_ms: Option<i64>,
    pub sub_job: Option<SubJob>,
    pub stopped: Option<Stopped>,
    /// The latest retryable failure, shown while retrying.
    pub retrying: Option<String>,
    /// The provider gave up on a dead pod (`staleReason`).
    pub stale_reason: Option<String>,
    failing_since: Option<i64>,
    failures: usize,
    last_new_line_ms: Option<i64>,
    /// When the next request is due (epoch ms).
    pub next_poll_ms: Option<i64>,
}

impl Follower {
    pub fn new(now_ms: i64) -> Self {
        Self {
            last_new_line_ms: Some(now_ms),
            next_poll_ms: Some(now_ms),
            ..Self::default()
        }
    }

    /// The request body for the next page.
    pub fn body(&self, execution_id: &str, session_id: Option<&str>) -> Value {
        let mut body = serde_json::json!({ "jobIdentifier": execution_id });
        if let Some(session) = session_id {
            body["sessionId"] = Value::String(session.to_string());
        }
        if let Some(cursor) = &self.cursor {
            body["cursor"] = Value::String(cursor.clone());
        }
        body
    }

    pub fn is_following(&self) -> bool {
        self.stopped.is_none()
    }

    /// True when a request should go out now.
    pub fn due(&self, now_ms: i64) -> bool {
        self.is_following() && self.next_poll_ms.is_some_and(|at| now_ms >= at)
    }

    /// A request is on its way; nothing more until it answers.
    pub fn sent(&mut self) {
        self.next_poll_ms = None;
    }

    /// No new lines for 15 minutes while the run still looks active.
    pub fn is_quiet(&self, now_ms: i64) -> bool {
        self.is_following()
            && self
                .last_new_line_ms
                .is_some_and(|at| now_ms - at >= QUIET_AFTER_MS)
    }

    /// Lands one page; returns how many new lines it added.
    pub fn page(&mut self, page: &Value, now_ms: i64) -> usize {
        self.failing_since = None;
        self.failures = 0;
        self.retrying = None;
        let added = self.add_events(page["events"].as_array().map_or(&[][..], Vec::as_slice));
        if added > 0 {
            self.last_new_line_ms = Some(now_ms);
        }
        if let Some(cursor) = page["cursor"].as_str().or(page["nextToken"].as_str()) {
            self.cursor = Some(cursor.to_string());
        }
        self.read_status(&page["jobStatus"][0]);
        if let Some(reason) = page["staleReason"].as_str() {
            self.stale_reason = Some(reason.to_string());
        }
        let more = page["hasMoreLogs"].as_bool();
        let ended = self.status.as_deref().is_some_and(is_terminal);
        match more {
            Some(false) if ended => {
                self.stopped = Some(Stopped::Finished(self.status.clone().unwrap_or_default()));
                self.next_poll_ms = None;
            }
            // A page without `hasMoreLogs` is malformed: retry like an error.
            None => self.failed_retryable("The log service sent an incomplete page", now_ms),
            Some(_) => {
                let wait = if added > 0 {
                    BUSY_POLL_MS
                } else {
                    IDLE_POLL_MS
                };
                self.next_poll_ms = Some(now_ms + wait);
            }
        }
        added
    }

    /// Lands a failed request.
    pub fn failed(&mut self, raw: &str, explained: String, now_ms: i64) {
        match errors::parse(raw) {
            Failure::Http { status: 404, .. } => {
                self.stopped = Some(Stopped::NotFound);
                self.next_poll_ms = None;
            }
            Failure::Http { status: 429, .. } => {
                self.retrying = Some("The log service is busy; waiting".to_string());
                let wait =
                    errors::retry_after_secs(raw).map_or(THROTTLE_WAIT_MS, |s| s as i64 * 1_000);
                self.next_poll_ms = Some(now_ms + wait);
            }
            Failure::Http {
                status: 400 | 401 | 403,
                ..
            } => {
                self.stopped = Some(Stopped::Refused(explained));
                self.next_poll_ms = None;
            }
            _ => self.failed_retryable(&explained, now_ms),
        }
    }

    fn failed_retryable(&mut self, reason: &str, now_ms: i64) {
        let since = *self.failing_since.get_or_insert(now_ms);
        if now_ms - since >= GIVE_UP_AFTER_MS {
            self.stopped = Some(Stopped::GaveUp);
            self.next_poll_ms = None;
            return;
        }
        let wait = BACKOFF_MS[self.failures.min(BACKOFF_MS.len() - 1)];
        self.failures += 1;
        self.retrying = Some(reason.to_string());
        self.next_poll_ms = Some(now_ms + wait);
    }

    /// Starts following again after giving up, keeping the lines and cursor.
    pub fn resume(&mut self, now_ms: i64) {
        self.stopped = None;
        self.failing_since = None;
        self.failures = 0;
        self.retrying = None;
        self.last_new_line_ms = Some(now_ms);
        self.next_poll_ms = Some(now_ms);
    }

    fn add_events(&mut self, events: &[Value]) -> usize {
        let mut added = 0;
        for event in events {
            let text = event["message"].as_str().unwrap_or_default().trim_end();
            let id = event["eventId"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("{}:{text}", event["timestampMs"]));
            if !self.seen.insert(id) {
                continue;
            }
            self.track_sub_job(text);
            self.lines.push(LogLine {
                at_ms: event["timestampMs"].as_i64(),
                text: text.to_string(),
                level: Level::of(text),
            });
            added += 1;
        }
        added
    }

    fn track_sub_job(&mut self, text: &str) {
        if let Some(caps) = SUB_JOB_STARTED.captures(text) {
            self.sub_job = Some(SubJob {
                name: caps[1].to_string(),
                index: caps[2].parse().unwrap_or(0),
                total: caps[3].parse().unwrap_or(0),
                finished: false,
            });
        } else if let Some(caps) = SUB_JOB_FINISHED.captures(text) {
            if let Some(sub) = self.sub_job.as_mut().filter(|s| s.name == caps[1]) {
                sub.finished = true;
            }
        }
    }

    fn read_status(&mut self, row: &Value) {
        if let Some(status) = row["jobstatus"].as_str() {
            self.status = Some(status.to_string());
        }
        if let Some(started) = row["startedat"].as_str().and_then(rfc1123_ms) {
            self.started_ms = Some(started);
        }
        if let Some(stopped) = row["stoppedat"].as_str().and_then(rfc1123_ms) {
            self.stopped_ms = Some(stopped);
        }
    }

    pub fn last_line(&self) -> Option<&LogLine> {
        self.lines.last()
    }
}

/// `Fri, 30 Oct 2026 03:54:54 GMT`, which `/etl/logs` status rows use.
pub fn rfc1123_ms(text: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut words = text.split_whitespace().skip(1);
    let day: u32 = words.next()?.parse().ok()?;
    let month = MONTHS
        .iter()
        .position(|m| Some(*m) == words.clone().next())? as u32
        + 1;
    words.next();
    let year: i64 = words.next()?.parse().ok()?;
    let clock = words.next()?;
    let mut parts = clock.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (parts.next()??, parts.next()??, parts.next()??);
    let days = crate::tui::format::days_from_civil(year, month, day);
    Some(days * 86_400_000 + (h * 3_600 + m * 60 + s) * 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T0: i64 = 1_794_139_200_000;

    fn page(ids: &[&str], status: &str, more: bool) -> Value {
        let events: Vec<Value> = ids
            .iter()
            .map(|id| json!({"eventId": id, "message": format!("line {id}"), "timestampMs": T0}))
            .collect();
        json!({"events": events, "jobStatus": [{"jobstatus": status,
               "startedat": "Sun, 08 Nov 2026 11:58:00 GMT", "stoppedat": null}],
               "hasMoreLogs": more, "cursor": format!("c-{}", ids.len())})
    }

    #[test]
    fn polls_fast_while_lines_arrive_and_slowly_when_idle() {
        let mut f = Follower::new(T0);
        assert!(f.due(T0));
        f.sent();
        assert!(!f.due(T0 + 60_000), "one request in flight at a time");
        assert_eq!(f.page(&page(&["1", "2"], "Running", true), T0), 2);
        assert_eq!(f.next_poll_ms, Some(T0 + BUSY_POLL_MS));
        assert_eq!(f.page(&page(&["1", "2"], "Running", true), T0 + 1_000), 0);
        assert_eq!(f.next_poll_ms, Some(T0 + 1_000 + IDLE_POLL_MS));
        assert_eq!(f.lines.len(), 2, "repeated events are dropped");
        assert_eq!(f.cursor.as_deref(), Some("c-2"));
        assert_eq!(
            f.body("abc", Some("s1")),
            json!({"jobIdentifier": "abc", "sessionId": "s1", "cursor": "c-2"})
        );
        assert_eq!(f.started_ms, Some(T0 - 120_000));
    }

    #[test]
    fn stops_when_the_run_ends_and_no_more_logs_remain() {
        let mut f = Follower::new(T0);
        f.page(&page(&["1"], "Success", true), T0);
        assert!(f.is_following(), "more logs may still come");
        f.page(&page(&["2"], "Success", false), T0 + 1_000);
        assert_eq!(f.stopped, Some(Stopped::Finished("Success".into())));
        assert_eq!(f.stopped.unwrap().notice(), "Job completed successfully");
    }

    #[test]
    fn waits_as_long_as_a_throttled_server_asks() {
        let mut f = Follower::new(T0);
        let throttled = "POST u → HTTP 429 Too Many Requests (retry after 5s): CloudWatch";
        f.failed(throttled, String::new(), T0);
        assert_eq!(f.next_poll_ms, Some(T0 + 5_000));
        f.failed(
            "POST u → HTTP 429 Too Many Requests: CloudWatch",
            String::new(),
            T0,
        );
        assert_eq!(f.next_poll_ms, Some(T0 + THROTTLE_WAIT_MS));
        assert!(f.is_following());
    }

    #[test]
    fn backs_off_on_errors_then_gives_up_after_two_minutes() {
        let mut f = Follower::new(T0);
        let err = "POST u → HTTP 500 Internal Server Error: boom";
        let mut now = T0;
        let mut waits = Vec::new();
        for _ in 0..6 {
            f.failed(err, "The etl service had an error.".into(), now);
            let next = f.next_poll_ms.unwrap();
            waits.push(next - now);
            now = next;
        }
        assert_eq!(waits, vec![1_000, 2_000, 4_000, 8_000, 15_000, 15_000]);
        assert_eq!(f.retrying.as_deref(), Some("The etl service had an error."));
        f.failed(err, String::new(), T0 + GIVE_UP_AFTER_MS);
        assert_eq!(f.stopped, Some(Stopped::GaveUp));
        f.resume(T0 + GIVE_UP_AFTER_MS + 1);
        assert!(f.due(T0 + GIVE_UP_AFTER_MS + 1));
    }

    #[test]
    fn a_missing_run_stops_at_once() {
        let mut f = Follower::new(T0);
        f.failed(
            "POST u → HTTP 404 Not Found: Execution not found",
            String::new(),
            T0,
        );
        assert_eq!(
            f.stopped.as_ref().unwrap().notice(),
            "The run could not be found."
        );
    }

    #[test]
    fn notices_quiet_runs_after_fifteen_minutes() {
        let mut f = Follower::new(T0);
        f.page(&page(&["1"], "Running", true), T0);
        assert!(!f.is_quiet(T0 + QUIET_AFTER_MS - 1));
        assert!(f.is_quiet(T0 + QUIET_AFTER_MS));
    }

    #[test]
    fn tracks_sub_jobs_from_runtime_markers() {
        let mut f = Follower::new(T0);
        let events = json!({"events": [
            {"eventId": "a", "message": "EXECUTING JOB: load (2/5)"},
            {"eventId": "b", "message": "Job load completed successfully"},
        ], "jobStatus": [{"jobstatus": "Running"}], "hasMoreLogs": true});
        f.page(&events, T0);
        assert_eq!(
            f.sub_job,
            Some(SubJob {
                index: 2,
                total: 5,
                name: "load".into(),
                finished: true
            })
        );
    }

    #[test]
    fn runs_far_past_their_usual_length_may_be_stuck() {
        let hour = 3_600_000;
        assert!(!may_be_stuck(5 * hour, None, false));
        assert!(may_be_stuck(7 * hour, None, false));
        // Usually 10 s: still fine at 20 min (pod start-up), stuck after 30.
        assert!(!may_be_stuck(20 * 60_000, Some(10_000), false));
        assert!(may_be_stuck(31 * 60_000, Some(10_000), false));
        // Usually an hour: stuck only past ten.
        assert!(!may_be_stuck(9 * hour, Some(hour), false));
        assert!(may_be_stuck(11 * hour, Some(hour), false));
        assert!(
            may_be_stuck(60_000, None, true),
            "running rows with an end time"
        );
    }

    #[test]
    fn colours_lines_by_level_and_failure_markers() {
        assert_eq!(Level::of("2026-11-08 10:00:00 INFO starting"), Level::Info);
        assert_eq!(Level::of("[WARNING] slow"), Level::Warning);
        assert_eq!(
            Level::of("Traceback (most recent call last):"),
            Level::Error
        );
        assert_eq!(Level::of("plain"), Level::Other);
    }
}
