//! What a failed run leaves behind, cut down to what explains it: the end
//! of its log, the lines around errors and tracebacks, its status and its
//! job's config, within a size budget the model can take in one message.

use regex::Regex;
use serde_json::Value;
use std::ops::Range;
use std::sync::OnceLock;

use super::history::RunRow;
use crate::assistant::secret_text::redact;
use crate::cmd::jobs::JobRef;

/// How much of a log the excerpt keeps.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Lines from the end of the log, always kept.
    pub tail_lines: usize,
    /// Lines kept before and after an error line.
    pub before_error: usize,
    pub after_error: usize,
    /// A traceback runs on below its header; keep this many lines of it.
    pub after_traceback: usize,
    /// Longer lines are cut.
    pub max_line_chars: usize,
    /// The whole excerpt stays under this.
    pub max_chars: usize,
}

pub const LIMITS: Limits = Limits {
    tail_lines: 60,
    before_error: 3,
    after_error: 3,
    after_traceback: 25,
    max_line_chars: 400,
    max_chars: 16_000,
};

/// The evidence never carries more of the run's `output` than this.
const MAX_OUTPUT_CHARS: usize = 2_000;

fn error_line() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"\b\w*(?:Error|Exception)\b|\bERROR\b|(?i:\bfatal\b|\bfailed\b)")
            .expect("the error-line pattern is a valid regex")
    })
}

const TRACEBACK: &str = "Traceback (most recent call last)";

/// The kept lines of a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogExcerpt {
    /// Kept lines, with `… N lines left out …` where lines were skipped.
    pub text: String,
    pub total_lines: usize,
    pub kept_lines: usize,
}

/// A merged run of kept lines and how much it matters when space is short.
#[derive(Debug, Clone)]
struct Chunk {
    lines: Range<usize>,
    /// Lower is kept first.
    rank: usize,
    /// The tail keeps its end when cut; error chunks keep their start.
    is_tail: bool,
}

/// The end of the log plus every error line and traceback with some
/// context. When that is over budget the tail, then the first error (often
/// the root cause), then the last one win; other errors are left out.
pub fn excerpt(lines: &[String], limits: &Limits) -> LogExcerpt {
    let lines: Vec<String> = lines
        .iter()
        .map(|line| cut_line(line, limits.max_line_chars))
        .collect();
    let mut chunks = chunks(&lines, limits);
    budget(&mut chunks, &lines, limits.max_chars);
    chunks.sort_by_key(|c| c.lines.start);
    render(&lines, &chunks)
}

fn cut_line(line: &str, max: usize) -> String {
    let line = line.trim_end();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let kept: String = line.chars().take(max).collect();
    format!("{kept}…")
}

/// Kept ranges, merged where they touch, ranked tail first, then the first
/// and last errors.
fn chunks(lines: &[String], limits: &Limits) -> Vec<Chunk> {
    let total = lines.len();
    let mut ranges: Vec<Range<usize>> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(TRACEBACK) || error_line().is_match(line))
        .map(|(at, line)| {
            let after = if line.contains(TRACEBACK) {
                limits.after_traceback
            } else {
                limits.after_error
            };
            at.saturating_sub(limits.before_error)..(at + after + 1).min(total)
        })
        .collect();
    ranges.push(total.saturating_sub(limits.tail_lines)..total);
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges.into_iter().filter(|r| !r.is_empty()) {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    let count = merged.len();
    merged
        .into_iter()
        .enumerate()
        .map(|(at, lines)| {
            let is_tail = lines.end == total;
            let rank = match at {
                _ if is_tail => 0,
                0 => 1,
                _ if at + 2 == count => 2,
                _ => 3 + at,
            };
            Chunk {
                lines,
                rank,
                is_tail,
            }
        })
        .collect()
}

/// Drops and cuts chunks, least important first, until they fit `max_chars`.
fn budget(chunks: &mut Vec<Chunk>, lines: &[String], max_chars: usize) {
    chunks.sort_by_key(|c| c.rank);
    let mut left = max_chars;
    let mut kept = Vec::new();
    for mut chunk in chunks.drain(..) {
        let cost = |range: &Range<usize>| lines[range.clone()].iter().map(|l| l.len() + 1).sum();
        let full: usize = cost(&chunk.lines);
        if full <= left {
            left -= full;
            kept.push(chunk);
            continue;
        }
        // Cut the chunk to what fits, then stop: nothing ranked lower is kept.
        while !chunk.lines.is_empty() && cost(&chunk.lines) > left {
            if chunk.is_tail {
                chunk.lines.start += 1;
            } else {
                chunk.lines.end -= 1;
            }
        }
        if !chunk.lines.is_empty() {
            kept.push(chunk);
        }
        break;
    }
    *chunks = kept;
}

fn render(lines: &[String], chunks: &[Chunk]) -> LogExcerpt {
    let mut out = Vec::new();
    let mut next = 0;
    let mut kept = 0;
    for chunk in chunks {
        if chunk.lines.start > next {
            out.push(skipped(chunk.lines.start - next));
        }
        out.extend(lines[chunk.lines.clone()].iter().cloned());
        kept += chunk.lines.len();
        next = chunk.lines.end;
    }
    if next < lines.len() {
        out.push(skipped(lines.len() - next));
    }
    LogExcerpt {
        text: out.join("\n"),
        total_lines: lines.len(),
        kept_lines: kept,
    }
}

fn skipped(count: usize) -> String {
    let noun = if count == 1 { "line" } else { "lines" };
    format!("… {count} {noun} left out …")
}

/// The run's own record (`POST /etl/jobStatus`): the status the log
/// service acts on, its times and its `output`.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusRecord {
    pub status: String,
    pub started_at: Option<String>,
    pub stopped_at: Option<String>,
    pub output: Option<String>,
}

impl StatusRecord {
    /// From `{success, data: [{jobstatus, sessionId, startedat, stoppedat, output}]}`.
    pub fn from_reply(reply: &Value) -> Option<Self> {
        let row = reply["data"].as_array()?.first()?;
        let text = |key: &str| {
            row[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        let output = match &row["output"] {
            Value::Null => None,
            Value::String(s) if s.is_empty() => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        };
        Some(Self {
            status: text("jobstatus")?,
            started_at: text("startedat"),
            stopped_at: text("stoppedat"),
            output: output.map(|o| cut_line(&o, MAX_OUTPUT_CHARS)),
        })
    }

    /// `Success`, `Failed` and `Aborted` are final; only then is reading the
    /// run's logs free of side effects (the log service fails runs it finds
    /// stale only while they are `Queued` or `Running`).
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "Success" | "Failed" | "Aborted")
    }
}

/// Everything gz code is told about the failure.
#[derive(Debug, Clone)]
pub struct Evidence {
    pub project: i64,
    pub job: JobRef,
    pub run: RunRow,
    pub record: StatusRecord,
    pub log: LogExcerpt,
    /// The job's saved `config`.
    pub config: Value,
    /// A newer run succeeded.
    pub newer_success: bool,
}

impl Evidence {
    /// The evidence as plain text, credentials redacted.
    pub fn render(&self) -> String {
        let run = &self.run;
        let mut out = vec![
            format!(
                "Job: {} (id {}) in project {}",
                self.job.name, self.job.id, self.project
            ),
            format!("Run: {} · status {}", run.execution_id, self.record.status),
        ];
        let mut push = |label: &str, value: Option<String>| {
            if let Some(value) = value {
                out.push(format!("{label}: {value}"));
            }
        };
        push("Session", run.session_id.clone());
        push(
            "Started",
            self.record.started_at.clone().or(run.started_at.clone()),
        );
        push(
            "Stopped",
            self.record.stopped_at.clone().or(run.completed_at.clone()),
        );
        push("Duration", run.duration_seconds.map(|s| format!("{s:.0}s")));
        push("Started by", run.started_by.clone());
        push("Usage", usage_line(&run.item));
        push("Run output", self.record.output.clone());
        if self.newer_success {
            out.push("Note: a newer run of this job succeeded".into());
        }
        out.push(format!(
            "Job config: {}",
            serde_json::to_string(&self.config).unwrap_or_default()
        ));
        out.push(String::new());
        out.push(format!(
            "Log ({} of {} lines):",
            self.log.kept_lines, self.log.total_lines
        ));
        out.push(if self.log.total_lines == 0 {
            "(the run wrote no log lines)".to_string()
        } else {
            self.log.text.clone()
        });
        redact(&out.join("\n"))
    }
}

/// Cost, CPU and memory from the history item, when it has them.
fn usage_line(item: &Value) -> Option<String> {
    let number = |path: &[&str]| {
        path.iter()
            .try_fold(item, |value, key| value.get(key))
            .and_then(Value::as_f64)
    };
    let parts: Vec<String> = [
        number(&["usage", "cost", "totalUsd"]).map(|v| format!("${v:.4}")),
        number(&["usage", "cpu", "averageCores"]).map(|v| format!("{v:.2} avg CPU cores")),
        number(&["usage", "memory", "peakGiB"]).map(|v| format!("{v:.2} GiB peak memory")),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn log(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    fn numbered(count: usize) -> Vec<String> {
        (0..count).map(|i| format!("step {i} ok")).collect()
    }

    #[test]
    fn a_short_log_is_kept_whole() {
        let lines = log(&["start", "rows: 3", "done"]);
        let excerpt = excerpt(&lines, &LIMITS);
        assert_eq!(excerpt.text, "start\nrows: 3\ndone");
        assert_eq!((excerpt.kept_lines, excerpt.total_lines), (3, 3));
    }

    #[test]
    fn keeps_the_traceback_far_above_the_tail() {
        let mut lines = numbered(500);
        lines[100] = TRACEBACK.to_string();
        lines[101] = "  File \"job.py\", line 7, in <module>".to_string();
        lines[102] = "KeyError: 'order_id'".to_string();
        let excerpt = excerpt(&lines, &LIMITS);
        assert!(
            excerpt.text.contains("KeyError: 'order_id'"),
            "{}",
            excerpt.text
        );
        assert!(excerpt.text.contains("step 97 ok"), "context before");
        assert!(
            !excerpt.text.contains("step 300 ok"),
            "the middle is left out"
        );
        assert!(excerpt.text.contains("step 499 ok"), "the tail is kept");
        assert!(excerpt.text.contains("lines left out"));
        assert!(excerpt.kept_lines < 120, "{}", excerpt.kept_lines);
    }

    #[test]
    fn stays_under_the_budget_keeping_the_first_error_and_the_tail() {
        let mut lines: Vec<String> = (0..3000)
            .map(|i| format!("line {i} {}", "x".repeat(80)))
            .collect();
        for at in [200, 1200, 2200] {
            lines[at] = format!("ValueError: bad row {at}");
        }
        let limits = Limits {
            max_chars: 6_000,
            ..LIMITS
        };
        let excerpt = excerpt(&lines, &limits);
        assert!(excerpt.text.len() <= 6_000 + 200, "{}", excerpt.text.len());
        assert!(excerpt.text.contains("line 2999"), "tail first");
        assert!(excerpt.text.contains("bad row 200"), "then the first error");
        assert!(
            !excerpt.text.contains("bad row 1200"),
            "a middle error gives way"
        );
    }

    #[test]
    fn cuts_very_long_lines() {
        let lines = vec!["y".repeat(5_000)];
        let excerpt = excerpt(&lines, &LIMITS);
        assert_eq!(excerpt.text.chars().count(), LIMITS.max_line_chars + 1);
        assert!(excerpt.text.ends_with('…'));
    }

    #[test]
    fn reads_the_status_record_and_knows_which_states_are_final() {
        let reply = json!({"success": true, "data": [{
            "jobstatus": "Failed", "sessionId": "s1", "startedat": "2026-11-01T10:00:00",
            "stoppedat": null, "output": {"error": "boom"},
        }]});
        let record = StatusRecord::from_reply(&reply).unwrap();
        assert_eq!(record.status, "Failed");
        assert_eq!(record.output.as_deref(), Some(r#"{"error":"boom"}"#));
        assert!(record.is_terminal());
        for status in ["Running", "Queued", "Submitted"] {
            let reply = json!({"data": [{"jobstatus": status}]});
            assert!(
                !StatusRecord::from_reply(&reply).unwrap().is_terminal(),
                "{status}"
            );
        }
        assert_eq!(StatusRecord::from_reply(&json!({"data": []})), None);
    }

    #[test]
    fn renders_the_run_and_redacts_credentials() {
        let item = json!({"executionId": "e1", "status": "failed", "sessionId": "s1",
            "usage": {"cost": {"totalUsd": 0.25}}});
        let evidence = Evidence {
            project: 7,
            job: JobRef {
                id: 9,
                name: "clean-orders".into(),
            },
            run: RunRow::from_item(&item).unwrap(),
            record: StatusRecord {
                status: "Failed".into(),
                started_at: None,
                stopped_at: None,
                output: None,
            },
            log: excerpt(
                &log(&["using postgres://etl:s3cretpw@db/app", "KeyError: 'x'"]),
                &LIMITS,
            ),
            config: json!({"minimumWorkers": 1}),
            newer_success: false,
        };
        let text = evidence.render();
        assert!(
            text.contains("Job: clean-orders (id 9) in project 7"),
            "{text}"
        );
        assert!(text.contains("Run: e1 · status Failed"), "{text}");
        assert!(text.contains("Usage: $0.2500"), "{text}");
        assert!(
            text.contains(r#"Job config: {"minimumWorkers":1}"#),
            "{text}"
        );
        assert!(text.contains("KeyError: 'x'"), "{text}");
        assert!(!text.contains("s3cretpw"), "{text}");
    }
}
