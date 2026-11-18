//! `gz jobs run --wait/--follow/--timeout` and `gz jobs logs --follow`
//! against a stub ETL provider: the exit code of each ending, and that a
//! run still going is read only through its status record and its pod's
//! buffer, never through `/etl/logs` or `current-run`.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RUN: &str = "0123456789abcdef0123456789abcdef";

struct Platform {
    requests: Vec<Recorded>,
    /// Statuses `/etl/jobStatus` answers in turn; the last one repeats.
    statuses: Vec<&'static str>,
    polled: usize,
}

impl Platform {
    fn status(&self) -> &'static str {
        self.statuses[self.polled.min(self.statuses.len()) - 1]
    }

    fn is_final(&self) -> bool {
        self.polled > 0 && ["Success", "Failed", "Aborted"].contains(&self.status())
    }
}

fn reply(platform: &mut Platform, request: &Recorded) -> Option<Value> {
    let reply = match (request.method.as_str(), request.target.as_str()) {
        ("GET", "/etl/etlProjects") => json!([{"Id": 1999, "Name": "DataQuality"}]),
        ("GET", "/etl/etlJobs/1999/jobs") => json!([{"Id": 3013, "Name": "DQC"}]),
        ("GET", "/etl/etlJobs/jobs/1999/3013") => json!({"config": {"connectionNames": []}}),
        ("POST", "/etl/job") => json!({"jobIdentifier": RUN}),
        ("POST", "/etl/jobStatus") => {
            platform.polled += 1;
            json!({"success": true, "data": [{"jobstatus": platform.status()}]})
        }
        ("POST", "/etl/ekslogs") => {
            let start = request.body["nextToken"].as_str().unwrap_or("0");
            if start == "0" {
                json!({"events": [
                    {"timestamp": "t1", "message": "Spark Session initialized"},
                    {"timestamp": "t2", "message": "rows written: 42"},
                ], "hasMoreLogs": true, "nextToken": "2"})
            } else {
                json!({"events": [], "hasMoreLogs": true, "nextToken": start})
            }
        }
        ("POST", "/etl/logs") => json!({
            "events": [
                {"timestamp": "t1", "message": "Spark Session initialized\n"},
                {"timestamp": "t2", "message": "rows written: 42\n"},
                {"timestamp": "t3", "message": "Execution complete."},
            ],
            "jobStatus": [{"jobstatus": platform.status()}],
            "hasMoreLogs": false,
            "cursor": "c1",
        }),
        _ => return None,
    };
    Some(reply)
}

fn stub(statuses: Vec<&'static str>) -> (String, Arc<Mutex<Platform>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let platform = Arc::new(Mutex::new(Platform {
        requests: Vec::new(),
        statuses,
        polled: 0,
    }));
    let shared = platform.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            let mut platform = shared.lock().unwrap();
            // A log read while the run is going is what these tests forbid.
            let unsafe_read = (request.target == "/etl/logs" && !platform.is_final())
                || request.target.contains("current-run");
            platform.requests.push(request.clone());
            if unsafe_read {
                respond(
                    &mut stream,
                    "418 I'M A TEAPOT",
                    "text/plain",
                    "read while running",
                );
                continue;
            }
            match reply(&mut platform, &request) {
                Some(body) => respond(&mut stream, "200 OK", "application/json", &body.to_string()),
                None => respond(&mut stream, "404 NOT FOUND", "text/plain", "no such route"),
            }
        }
    });
    (base, platform)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-run-wait-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn command(dir: &Path, base: &str, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gz"));
    command
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "run-wait-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(Stdio::null())
        .args(args);
    command
}

fn gz(name: &str, base: &str, args: &[&str]) -> Output {
    command(&temp_dir(name), base, args).output().unwrap()
}

const RUN_JOB: [&str; 8] = [
    "jobs",
    "run",
    "1999",
    "3013",
    "--compute",
    "1006",
    "--rte",
    "gz-rte-spark-sklearn:1.0.0",
];

fn run_job(name: &str, base: &str, extra: &[&str]) -> Output {
    let args: Vec<&str> = RUN_JOB
        .iter()
        .copied()
        .chain(extra.iter().copied())
        .collect();
    gz(name, base, &args)
}

fn targets(platform: &Arc<Mutex<Platform>>) -> Vec<String> {
    platform
        .lock()
        .unwrap()
        .requests
        .iter()
        .map(|r| format!("{} {}", r.method, r.target))
        .collect()
}

#[test]
fn waiting_on_a_run_that_succeeds_exits_zero_without_reading_its_log() {
    let (base, platform) = stub(vec!["Queued", "Running", "Success"]);
    let out = run_job("ok", &base, &["--wait"]);
    assert_eq!(out.status.code(), Some(0), "{}", plain(&out.stderr));
    assert!(plain(&out.stdout).contains(&format!("run {RUN} succeeded")));
    let calls = targets(&platform);
    assert_eq!(
        calls.iter().filter(|c| *c == "POST /etl/jobStatus").count(),
        3
    );
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("/etl/logs") || c.contains("ekslogs")),
        "{calls:?}"
    );
}

#[test]
fn a_failed_run_exits_two_and_an_aborted_one_three() {
    let (base, _) = stub(vec!["Failed"]);
    let out = run_job("failed", &base, &["--wait"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(plain(&out.stderr).contains("ended with status Failed"));

    let (base, _) = stub(vec!["Aborted"]);
    let out = run_job("aborted", &base, &["--wait"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(plain(&out.stderr).contains("ended with status Aborted"));
}

#[test]
fn json_output_reports_how_the_wait_ended() {
    let (base, _) = stub(vec!["Failed"]);
    let out = run_job("json", &base, &["--wait", "-o", "json"]);
    assert_eq!(out.status.code(), Some(2));
    let summary: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        summary,
        json!({"jobIdentifier": RUN, "status": "Failed", "exitCode": 2})
    );
}

#[test]
fn follow_streams_the_pod_while_running_then_only_the_missing_lines() {
    let (base, platform) = stub(vec!["Running", "Success"]);
    let out = run_job("follow", &base, &["--follow"]);
    assert_eq!(out.status.code(), Some(0), "{}", plain(&out.stderr));
    let stdout = plain(&out.stdout);
    for line in [
        "Spark Session initialized",
        "rows written: 42",
        "Execution complete.",
    ] {
        assert_eq!(stdout.matches(line).count(), 1, "{line} once in\n{stdout}");
    }
    let calls = targets(&platform);
    let status_at = |n: usize| {
        calls
            .iter()
            .enumerate()
            .filter(|(_, c)| *c == "POST /etl/jobStatus")
            .nth(n)
            .unwrap()
            .0
    };
    let pod_at = calls.iter().position(|c| c == "POST /etl/ekslogs").unwrap();
    let log_at = calls.iter().position(|c| c == "POST /etl/logs").unwrap();
    assert!(status_at(0) < pod_at && pod_at < status_at(1) && status_at(1) < log_at);
    assert!(
        !calls.iter().any(|c| c.contains("current-run")),
        "{calls:?}"
    );
}

#[test]
fn a_timeout_stops_waiting_but_not_the_run() {
    let (base, platform) = stub(vec!["Running"]);
    let out = run_job("timeout", &base, &["--wait-timeout", "1s"]);
    assert_eq!(out.status.code(), Some(4));
    let stderr = plain(&out.stderr);
    assert!(stderr.contains("stopped waiting after 1s"), "{stderr}");
    assert!(stderr.contains("still going"), "{stderr}");
    assert!(
        stderr.contains(&format!("gz jobs logs --job {RUN} --follow")),
        "{stderr}"
    );
    let calls = targets(&platform);
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("abort") || c.contains("/etl/logs")),
        "{calls:?}"
    );
}

#[test]
fn ctrl_c_stops_waiting_and_says_how_to_follow_again() {
    let (base, platform) = stub(vec!["Running"]);
    let args: Vec<&str> = RUN_JOB.iter().copied().chain(["--wait"]).collect();
    let child = command(&temp_dir("interrupt"), &base, &args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while platform.lock().unwrap().polled == 0 {
        assert!(started.elapsed() < Duration::from_secs(20), "never polled");
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: signalling a child process this test owns.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(130));
    let stderr = plain(&out.stderr);
    assert!(stderr.contains("Ctrl-C does not stop it"), "{stderr}");
    assert!(stderr.contains(&format!("gz jobs logs --job {RUN} --follow")));
}

#[test]
fn logs_follow_reads_a_running_run_only_through_its_pod() {
    let (base, platform) = stub(vec!["Running", "Running", "Success"]);
    let out = gz(
        "logs-follow",
        &base,
        &["jobs", "logs", "--job", RUN, "--follow"],
    );
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let stdout = plain(&out.stdout);
    assert_eq!(stdout.matches("rows written: 42").count(), 1, "{stdout}");
    assert!(stdout.contains("Execution complete."));
    let calls = targets(&platform);
    assert!(calls.contains(&"POST /etl/ekslogs".to_string()));
}

#[test]
fn spec_only_cannot_wait() {
    let (base, _) = stub(vec!["Success"]);
    let out = run_job("spec", &base, &["--spec-only", "--wait"]);
    assert_eq!(out.status.code(), Some(2), "clap usage error");
    assert!(plain(&out.stderr).contains("cannot be used with"));
}
