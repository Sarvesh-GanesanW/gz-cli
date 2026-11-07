//! `gz run` against a stub ETL backend: upload the local file as the job's
//! code → submit the run → stream logs → exit by the final status.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::json;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Answers the routes `gz run` uses; `final_status` is what the logs report.
fn stub_etl_server(final_status: &'static str) -> (String, Arc<Mutex<Vec<Recorded>>>) {
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
                    "config": {"connectionNames": ["GZS3"], "minimumWorkers": 1, "gpuSize": 0},
                    "fileContent": "print('old')\r\n",
                }),
                ("PUT", "/etl/etlJobs/jobs/7/9") => json!({"success": true}),
                ("POST", "/etl/job") => {
                    json!({"jobIdentifier": "0123456789abcdef0123456789abcdef"})
                }
                ("POST", "/etl/logs") => json!({
                    "events": [{"timestamp": "12:00:01", "message": "rows written: 42"}],
                    "jobStatus": [{"jobstatus": final_status}],
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
    let dir = std::env::temp_dir().join(format!("gz-devloop-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("clean_orders.py"), "print('new')\n").unwrap();
    dir
}

fn gz_run(dir: &PathBuf, base: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "devloop-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args([
            "run",
            "clean_orders.py",
            "--project",
            "7",
            "--job",
            "clean-orders",
            "--compute",
            "1003",
            "--rte",
            "gz-rte-spark-pytorch:1.0.0",
            "--follow",
            "--yes",
        ])
        .output()
        .unwrap()
}

fn find<'a>(log: &'a [Recorded], method: &str, path: &str) -> &'a Recorded {
    log.iter()
        .find(|r| r.method == method && r.target.starts_with(path))
        .unwrap_or_else(|| panic!("no {method} {path} in {log:#?}"))
}

#[test]
fn uploads_the_file_runs_it_and_streams_logs() {
    let (base, log) = stub_etl_server("Success");
    let dir = workspace("ok");
    let out = gz_run(&dir, &base);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("rows written: 42"), "{stdout}");

    let log = log.lock().unwrap();
    let update = find(&log, "PUT", "/etl/etlJobs/jobs/7/9");
    assert_eq!(update.body["code"], "print('new')\n");
    assert_eq!(update.body["projectId"], 7);
    assert_eq!(
        update.body["config"]["connectionNames"],
        json!(["GZS3"]),
        "config kept"
    );
    let submit = find(&log, "POST", "/etl/job");
    assert_eq!(submit.body["jobId"], 9);
    assert_eq!(submit.body["jobName"], "clean-orders");
    assert_eq!(submit.body["projectName"], "ETL");
    assert_eq!(submit.body["computeId"], 1003);
    let logs = find(&log, "POST", "/etl/logs");
    assert_eq!(
        logs.body["jobIdentifier"],
        "0123456789abcdef0123456789abcdef"
    );
    let put_at = log.iter().position(|r| r.method == "PUT").unwrap();
    let submit_at = log
        .iter()
        .position(|r| r.target.starts_with("/etl/job"))
        .unwrap();
    assert!(put_at < submit_at, "code is saved before the run starts");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_failed_run_exits_non_zero() {
    let (base, _log) = stub_etl_server("Failed");
    let dir = workspace("fail");
    let out = gz_run(&dir, &base);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ended with status Failed"));
    std::fs::remove_dir_all(&dir).ok();
}

fn gz_sync(dir: &PathBuf, base: &str, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "devloop-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(["sync", ".", "--project", "7", "--dry-run"])
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn dry_run_shows_the_plan_and_changes_nothing() {
    let (base, log) = stub_etl_server("Success");
    let dir = workspace("dry");
    let out = gz_sync(&dir, &base, &["--force"]);
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains("clean_orders.py → clean-orders  would be updated"),
        "{stdout}"
    );
    let log = log.lock().unwrap();
    assert!(
        log.iter().all(|r| r.method == "GET"),
        "dry run only reads: {log:#?}"
    );
    assert!(
        !dir.join(".gz-sync.json").exists(),
        "dry run keeps no state"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sync_refuses_to_overwrite_code_it_has_never_seen() {
    let (base, _log) = stub_etl_server("Success");
    let dir = workspace("conflict");
    let out = gz_sync(&dir, &base, &[]);
    let stdout = plain(&out.stdout);
    let stderr = plain(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stdout.contains("changed on both sides · skipped"),
        "{stdout}"
    );
    assert!(stderr.contains("gz sync --pull"), "{stderr}");
    std::fs::remove_dir_all(&dir).ok();
}
