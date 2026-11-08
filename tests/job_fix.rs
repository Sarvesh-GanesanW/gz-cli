//! `gz jobs fix` against a stub Groundzero: which run it picks, which
//! endpoints it calls (and never calls for a live run), the job file it
//! puts in the folder, and the prompt it prints.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const FAILED: &str = "0123456789abcdef0123456789abcdef";
const LIVE: &str = "fedcba9876543210fedcba9876543210";
const SAVED_CODE: &str = "rows = load()\nprint(rows['order_id'])\n";

/// What the stub reports.
#[derive(Clone, Copy)]
struct Platform {
    /// The newest run in the history, before the failed one.
    latest: Option<&'static str>,
    /// What the failed run's own record says.
    record: &'static str,
}

const FAILED_RUN: Platform = Platform {
    latest: None,
    record: "Failed",
};

fn history(platform: Platform) -> Value {
    let mut items = Vec::new();
    if let Some(status) = platform.latest {
        items.push(
            json!({"executionId": LIVE, "sessionId": "s-live", "status": status,
                          "jobId": 9, "projectId": 7, "startedAt": "2026-11-08T10:00:00+00:00"}),
        );
    }
    items.push(
        json!({"executionId": FAILED, "sessionId": "s-failed", "status": "failed",
                      "jobId": 9, "projectId": 7, "startedAt": "2026-11-08T09:00:00+00:00",
                      "durationSeconds": 42.0, "usage": {"cost": {"totalUsd": 0.0123}}}),
    );
    json!({"items": items, "pagination": {"total": items.len()}})
}

fn stub(platform: Platform) -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            seen.lock().unwrap().push(request.clone());
            let path = request.target.split('?').next().unwrap_or_default();
            let body = match (request.method.as_str(), path) {
                ("GET", "/etl/etlProjects") => json!([{"Id": 7, "Name": "ETL"}]),
                ("GET", "/etl/etlJobs/7/jobs") => json!([{"Id": 9, "Name": "clean-orders"}]),
                ("GET", "/etl/etlJobs/jobs/7/9") => json!({
                    "config": {"connectionNames": ["GZS3"], "minimumWorkers": 1},
                    "fileContent": SAVED_CODE,
                }),
                ("GET", "/etl/consumption-metrics") => history(platform),
                ("POST", "/etl/jobStatus") => json!({"success": true, "data": [{
                    "jobstatus": platform.record, "sessionId": "s-failed",
                    "startedat": "2026-11-08T09:00:00", "stoppedat": "2026-11-08T09:00:42",
                    "output": null,
                }]}),
                ("POST", "/etl/logs") => json!({
                    "events": [
                        {"timestamp": "09:00:01", "message": "loading orders"},
                        {"timestamp": "09:00:40", "message": "Traceback (most recent call last):"},
                        {"timestamp": "09:00:40", "message": "  File \"clean-orders.py\", line 2"},
                        {"timestamp": "09:00:40", "message": "KeyError: 'order_id'"},
                    ],
                    "jobStatus": [{"jobstatus": platform.record}],
                    "hasMoreLogs": false,
                    "cursor": "c1",
                }),
                _ => {
                    respond(&mut stream, "404 Not Found", "application/json", "{}");
                    continue;
                }
            };
            respond(&mut stream, "200 OK", "application/json", &body.to_string());
        }
    });
    (base, log)
}

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-job-fix-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("HOME", dir.join("home"))
        .env("GZ_PROFILE", "job-fix-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(args)
        .output()
        .unwrap()
}

fn called(log: &[Recorded], method: &str, path: &str) -> Vec<Recorded> {
    log.iter()
        .filter(|r| r.method == method && r.target.split('?').next() == Some(path))
        .cloned()
        .collect()
}

fn assert_never_touches_live_state(log: &[Recorded]) {
    assert!(
        !log.iter().any(|r| r.target.contains("current-run")),
        "current-run settles stale runs: {log:#?}"
    );
    assert!(
        log.iter()
            .all(|r| r.method == "GET" || r.target == "/etl/jobStatus" || r.target == "/etl/logs"),
        "only reads: {log:#?}"
    );
}

#[test]
fn prints_the_evidence_and_prompt_for_the_newest_failed_run() {
    let (base, log) = stub(FAILED_RUN);
    let dir = workspace("print");
    let out = gz(
        &dir,
        &base,
        &["jobs", "fix", "clean-orders", "--project", "7", "--print"],
    );
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", plain(&out.stderr));

    assert!(
        stdout.contains(&format!(
            "job clean-orders (id 9, project 7) failed in run {FAILED}"
        )),
        "{stdout}"
    );
    assert!(stdout.contains("KeyError: 'order_id'"), "{stdout}");
    assert!(
        stdout.contains("Traceback (most recent call last):"),
        "{stdout}"
    );
    assert!(stdout.contains("Usage: $0.0123"), "{stdout}");
    assert!(
        stdout.contains("`gz sync . --project 7 --only clean-orders.py`"),
        "{stdout}"
    );
    assert!(
        stdout.contains("gz jobs run --project 7 --job 9 --compute <compute-id>"),
        "{stdout}"
    );

    assert_eq!(
        std::fs::read_to_string(dir.join("clean-orders.py")).unwrap(),
        SAVED_CODE
    );
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(".gz-sync.json")).unwrap()).unwrap();
    assert_eq!(
        state["project"], 7,
        "a later `gz sync` knows the file is in step"
    );

    let log = log.lock().unwrap();
    let history = &called(&log, "GET", "/etl/consumption-metrics")[0];
    assert!(history.target.contains("jobId=9"), "{}", history.target);
    assert!(
        history.target.contains("workloadType=job"),
        "{}",
        history.target
    );
    assert_eq!(
        called(&log, "POST", "/etl/jobStatus")[0].body,
        json!({"jobIdentifier": FAILED})
    );
    let logs = called(&log, "POST", "/etl/logs");
    assert_eq!(logs.len(), 1);
    assert_eq!(
        logs[0].body,
        json!({"jobIdentifier": FAILED, "sessionId": "s-failed"})
    );
    let status_at = log
        .iter()
        .position(|r| r.target == "/etl/jobStatus")
        .unwrap();
    let logs_at = log.iter().position(|r| r.target == "/etl/logs").unwrap();
    assert!(
        status_at < logs_at,
        "the record is checked before any log read"
    );
    assert_eq!(called(&log, "GET", "/etl/etlJobs/jobs/7/9").len(), 1);
    assert_never_touches_live_state(&log);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stops_without_reading_logs_while_the_latest_run_is_going() {
    let (base, log) = stub(Platform {
        latest: Some("running"),
        ..FAILED_RUN
    });
    let dir = workspace("running");
    let out = gz(
        &dir,
        &base,
        &["jobs", "fix", "9", "--project", "7", "--print"],
    );
    let stderr = plain(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains(&format!(
            "the latest run {LIVE} of clean-orders is still running"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("gz jobs logs --job {LIVE} --follow")),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("--run {FAILED}")),
        "offers the earlier failure: {stderr}"
    );

    let log = log.lock().unwrap();
    assert!(called(&log, "POST", "/etl/logs").is_empty(), "{log:#?}");
    assert!(
        called(&log, "POST", "/etl/jobStatus").is_empty(),
        "{log:#?}"
    );
    assert_never_touches_live_state(&log);
    assert!(!dir.join("clean-orders.py").exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn trusts_the_runs_own_record_over_the_history_before_reading_logs() {
    let (base, log) = stub(Platform {
        latest: None,
        record: "Running",
    });
    let dir = workspace("record");
    let out = gz(
        &dir,
        &base,
        &["jobs", "fix", "clean-orders", "--project", "7", "--print"],
    );
    let stderr = plain(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("is Running by its own record"), "{stderr}");
    let log = log.lock().unwrap();
    assert!(called(&log, "POST", "/etl/logs").is_empty(), "{log:#?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn keeps_local_edits_and_says_so_in_the_prompt() {
    let (base, _log) = stub(FAILED_RUN);
    let dir = workspace("edits");
    let edited = "rows = load()\nprint(rows.get('order_id'))\n";
    std::fs::write(dir.join("clean-orders.py"), edited).unwrap();
    let out = gz(
        &dir,
        &base,
        &["fix", "clean-orders", "--project", "7", "--print"],
    );
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", plain(&out.stderr));
    assert!(plain(&out.stderr).contains("has local edits; keeping them"));
    assert!(
        stdout.contains("`clean-orders.py` has local edits"),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("clean-orders.py")).unwrap(),
        edited
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn takes_the_project_from_the_synced_folder() {
    let (base, log) = stub(FAILED_RUN);
    let dir = workspace("synced");
    std::fs::write(dir.join(".gz-sync.json"), r#"{"project": 7, "jobs": {}}"#).unwrap();
    let out = gz(&dir, &base, &["fix", "9", "--print"]);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let log = log.lock().unwrap();
    assert!(
        called(&log, "GET", "/etl/etlProjects").is_empty(),
        "no project search"
    );
    assert_eq!(called(&log, "GET", "/etl/etlJobs/7/jobs").len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn without_a_terminal_it_asks_for_print_before_calling_anything() {
    let (base, log) = stub(FAILED_RUN);
    let dir = workspace("notty");
    let out = gz(&dir, &base, &["fix", "clean-orders", "--project", "7"]);
    assert!(!out.status.success());
    assert!(
        plain(&out.stderr).contains("--print"),
        "{}",
        plain(&out.stderr)
    );
    assert!(log.lock().unwrap().is_empty());
    std::fs::remove_dir_all(&dir).ok();
}
