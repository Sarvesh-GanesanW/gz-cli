//! A small local record of recent request failures, read by `gz bug-report`.
//!
//! Each failure is one JSON line in `<config dir>/errors.jsonl`, capped at
//! [`MAX_ENTRIES`]. Only the request's shape is kept (method, URL without its
//! query, status, request id) — never bodies, headers or tokens, so the file
//! is safe to attach to a report.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How many failures the file keeps; older ones drop off.
pub const MAX_ENTRIES: usize = 50;
const FILE_NAME: &str = "errors.jsonl";
/// Response headers that carry a request id, in the order gz prefers them.
const REQUEST_ID_HEADERS: [&str; 5] = [
    "x-request-id",
    "x-amzn-requestid",
    "apigw-requestid",
    "x-amz-apigw-id",
    "x-amz-cf-id",
];
/// Request ids are short tokens; anything longer is not one.
const MAX_REQUEST_ID_CHARS: usize = 128;

static ARGS: OnceLock<Vec<String>> = OnceLock::new();
#[cfg(not(test))]
static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One failed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEntry {
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    pub time: String,
    /// The gz subcommand path (`gz jobs list`), without arguments or values.
    pub command: String,
    pub method: String,
    /// Scheme, host and path only.
    pub url: String,
    /// The HTTP status; absent when no response came back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// What kind of failure: `http`, `envelope` (HTTP 200 carrying an
    /// error), `timeout`, `connect` or `request`.
    pub kind: String,
}

/// The kind of failure, as stored in [`ErrorEntry::kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Http,
    Envelope,
    Timeout,
    Connect,
    Request,
}

impl FailureKind {
    fn as_str(self) -> &'static str {
        match self {
            FailureKind::Http => "http",
            FailureKind::Envelope => "envelope",
            FailureKind::Timeout => "timeout",
            FailureKind::Connect => "connect",
            FailureKind::Request => "request",
        }
    }

    /// The kind of a transport failure (no response at all).
    pub fn of_transport(err: &reqwest::Error) -> Self {
        if err.is_timeout() {
            FailureKind::Timeout
        } else if err.is_connect() {
            FailureKind::Connect
        } else {
            FailureKind::Request
        }
    }
}

/// What the HTTP layer knows about a failed request.
pub struct Failure<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub status: Option<u16>,
    pub headers: Option<&'a reqwest::header::HeaderMap>,
    pub kind: FailureKind,
}

/// Remembers this process's arguments, so [`record`] can name the command.
/// Only subcommand names are ever kept from them (see [`command_path`]).
pub fn remember_args(args: impl IntoIterator<Item = String>) {
    let _ = ARGS.set(args.into_iter().collect());
}

/// The running command's path, worked out only when a failure is recorded.
fn running_command() -> String {
    use clap::CommandFactory;
    let args = ARGS.get().map(Vec::as_slice).unwrap_or_default();
    command_path(&crate::cli::Cli::command(), args)
}

/// The subcommand path in `args` (`["gz", "--profile", "p", "jobs", "list",
/// "42"]` → `gz jobs list`): only names clap knows as subcommands are kept,
/// so argument values (which may be secrets or data) never get in.
pub fn command_path(root: &clap::Command, args: &[String]) -> String {
    let mut words = vec![root.get_name().to_string()];
    let mut current = root;
    for arg in args.iter().skip(1) {
        if arg.starts_with('-') {
            continue;
        }
        match current.find_subcommand(arg) {
            Some(sub) => {
                words.push(sub.get_name().to_string());
                current = sub;
            }
            None if current.has_subcommands() => continue,
            None => break,
        }
    }
    words.join(" ")
}

/// Records one failure in the ring file. Recording is best effort: a
/// failure to write it must never turn into a second error for the user.
pub fn record(failure: &Failure) {
    // Unit tests make failing requests on purpose; they must not write to
    // the developer's real config folder. Integration tests run the real
    // binary with an isolated config and do exercise this.
    #[cfg(test)]
    test_log::push(failure.url);
    #[cfg(not(test))]
    write_entry(failure);
}

#[cfg(not(test))]
fn write_entry(failure: &Failure) {
    let Ok(path) = log_path() else {
        return;
    };
    let entry = entry_for(failure, crate::config::now_unix());
    // The TUI fails requests in parallel; one writer at a time keeps their
    // shared temporary file whole.
    let _writer = WRITER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = append(&path, &entry, MAX_ENTRIES);
}

fn entry_for(failure: &Failure, now_unix: i64) -> ErrorEntry {
    ErrorEntry {
        time: utc_timestamp(now_unix),
        command: running_command(),
        method: failure.method.to_string(),
        url: strip_url(failure.url),
        status: failure.status,
        request_id: failure.headers.and_then(request_id),
        kind: failure.kind.as_str().to_string(),
    }
}

/// `<config dir>/errors.jsonl`, next to `config.toml`.
pub fn log_path() -> Result<PathBuf> {
    let config = crate::config::config_path()?;
    let dir = config
        .parent()
        .context("the config path has no folder")?
        .to_path_buf();
    Ok(dir.join(FILE_NAME))
}

/// The most recent `limit` failures, oldest first. A missing file means
/// nothing failed yet; lines this build cannot read are skipped.
pub fn recent(path: &Path, limit: usize) -> Result<Vec<ErrorEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let entries: Vec<ErrorEntry> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let skip = entries.len().saturating_sub(limit);
    Ok(entries.into_iter().skip(skip).collect())
}

/// Appends `entry`, keeping only the newest `cap` lines. Written to a
/// temporary file and renamed, so a reader never sees half a file.
fn append(path: &Path, entry: &ErrorEntry, cap: usize) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    lines.push(serde_json::to_string(entry)?);
    let keep_from = lines.len().saturating_sub(cap);
    let mut text = lines[keep_from..].join("\n");
    text.push('\n');

    let dir = path.parent().context("the error log has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let temp = dir.join(format!(".{FILE_NAME}.{}", std::process::id()));
    write_owner_only(&temp, &text)?;
    std::fs::rename(&temp, path).with_context(|| format!("cannot write {}", path.display()))
}

fn write_owner_only(path: &Path, text: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("cannot write {}", path.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("cannot write {}", path.display()))
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
    }
}

/// Scheme, host, port and path: no query string (presigned URLs carry
/// signatures there), no fragment, no user info.
pub fn strip_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut url) => {
            url.set_query(None);
            url.set_fragment(None);
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.to_string()
        }
        Err(_) => raw.split(['?', '#']).next().unwrap_or_default().to_string(),
    }
}

fn request_id(headers: &reqwest::header::HeaderMap) -> Option<String> {
    REQUEST_ID_HEADERS.iter().find_map(|name| {
        let value = headers.get(*name)?.to_str().ok()?.trim();
        let looks_like_id = !value.is_empty()
            && value.len() <= MAX_REQUEST_ID_CHARS
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=' | '.' | ':'));
        looks_like_id.then(|| value.to_string())
    })
}

/// Unix seconds → `YYYY-MM-DDTHH:MM:SSZ`.
pub fn utc_timestamp(unix: i64) -> String {
    let (year, month, day) = crate::tui::format::civil_from_days(unix.div_euclid(86_400));
    let secs = unix.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// What unit tests would have logged, per test thread, so a test can
/// check that a request was (or was not) reported.
#[cfg(test)]
pub mod test_log {
    use std::cell::RefCell;

    thread_local! {
        static URLS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn push(url: &str) {
        URLS.with(|urls| urls.borrow_mut().push(url.to_string()));
    }

    /// The URLs of the failures recorded on this thread so far.
    pub fn urls() -> Vec<String> {
        URLS.with(|urls| urls.borrow().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn temp_log(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gz-errlog-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir.join(FILE_NAME)
    }

    fn entry(n: usize) -> ErrorEntry {
        ErrorEntry {
            time: utc_timestamp(n as i64),
            command: "gz jobs list".into(),
            method: "GET".into(),
            url: format!("https://api.example.com/jobs/{n}"),
            status: Some(500),
            request_id: None,
            kind: "http".into(),
        }
    }

    #[test]
    fn the_ring_keeps_only_the_newest_entries() {
        let path = temp_log("ring");
        for n in 0..7 {
            append(&path, &entry(n), 5).unwrap();
        }
        let kept = recent(&path, 100).unwrap();
        assert_eq!(kept.len(), 5);
        assert_eq!(kept.first().unwrap().url, "https://api.example.com/jobs/2");
        assert_eq!(kept.last().unwrap().url, "https://api.example.com/jobs/6");
        assert_eq!(recent(&path, 2).unwrap().len(), 2);
        assert_eq!(
            recent(&path, 2).unwrap()[1].url,
            "https://api.example.com/jobs/6"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_log_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_log("mode");
        append(&path, &entry(1), 5).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_missing_log_means_no_errors_and_unreadable_lines_are_skipped() {
        let path = temp_log("garbage");
        assert!(recent(&path, 10).unwrap().is_empty());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let good = serde_json::to_string(&entry(3)).unwrap();
        std::fs::write(&path, format!("not json\n{good}\n")).unwrap();
        assert_eq!(recent(&path, 10).unwrap(), vec![entry(3)]);
    }

    #[test]
    fn urls_lose_their_query_fragment_and_user_info() {
        assert_eq!(
            strip_url("https://user:pw@bucket.s3.amazonaws.com/a/b?X-Amz-Signature=abc#frag"),
            "https://bucket.s3.amazonaws.com/a/b"
        );
        assert_eq!(
            strip_url("https://x.example.com:8443/jobs?token=secret"),
            "https://x.example.com:8443/jobs"
        );
        assert_eq!(strip_url("not a url?token=1"), "not a url");
    }

    #[test]
    fn request_ids_come_from_the_first_known_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-amz-cf-id", HeaderValue::from_static("cf-1"));
        headers.insert("x-amzn-requestid", HeaderValue::from_static("abc-123"));
        assert_eq!(request_id(&headers).as_deref(), Some("abc-123"));
        headers.insert(
            "x-request-id",
            HeaderValue::from_static("req 1 with spaces"),
        );
        assert_eq!(
            request_id(&headers).as_deref(),
            Some("abc-123"),
            "a value that is not an id is passed over"
        );
        assert_eq!(request_id(&HeaderMap::new()), None);
    }

    #[test]
    fn an_entry_never_carries_the_query_string() {
        let failure = Failure {
            method: "POST",
            url: "https://h.example.com/p?api_key=sk_live_secret",
            status: Some(401),
            headers: None,
            kind: FailureKind::Http,
        };
        let line = serde_json::to_string(&entry_for(&failure, 0)).unwrap();
        assert!(!line.contains("sk_live_secret"), "{line}");
        assert!(line.contains("\"time\":\"1970-01-01T00:00:00Z\""), "{line}");
    }

    #[test]
    fn command_paths_keep_subcommand_names_only() {
        use clap::CommandFactory;
        let root = crate::cli::Cli::command();
        let args = |line: &str| -> Vec<String> { line.split(' ').map(String::from).collect() };
        assert_eq!(
            command_path(
                &root,
                &args("gz --profile prod jobs status job-42 --limit 5")
            ),
            "gz jobs status"
        );
        assert_eq!(
            command_path(&root, &args("gz sql run select-secret-stuff")),
            "gz sql run"
        );
        assert_eq!(command_path(&root, &args("gz --token sk_abc")), "gz");
    }

    #[test]
    fn timestamps_are_utc_iso8601() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(1_763_424_000), "2025-11-18T00:00:00Z");
    }
}
