//! `gz jobs compare` and `gz jobs backfill` against a stub ETL provider:
//! the calls each makes, what they refuse, and the backfill's state file.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

const RUN_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const RUN_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[derive(Default)]
struct Platform {
    requests: Vec<Recorded>,
    /// Status of each run id; submitted runs are added as they start.
    statuses: HashMap<String, &'static str>,
    /// What a backfill run of each `$RUN_DATE` ends as (default Success).
    slice_endings: HashMap<String, &'static str>,
    code: &'static str,
    submitted: usize,
}

fn metrics(run: &str, cores: f64, rows_written: u64) -> Value {
    json!({
        "identity": {"jobId": 3013, "jobName": "DQC", "startedAt": "2026-11-16T03:56:22+00:00",
                     "completedAt": "2026-11-16T03:57:13+00:00", "durationSeconds": 51.2,
                     "startedBy": "srijith", "provider": "spark", "executionId": run},
        "runtime": {"engineVersion": "3.5.6",
                    "summary": {"inputRows": 1250007, "outputRows": rows_written,
                                "inputBytes": 42914, "outputBytes": 8321}},
        "usage": {"allocation": {"cpuRequestCores": cores, "memoryRequestGiB": 4.0},
                  "memory": {"peakGiB": 0.87}, "cpu": {"coreHours": 0.0015}},
    })
}

fn log(run: &str) -> Value {
    let tasks = if run == RUN_A {
        "28 completed"
    } else {
        "33 completed"
    };
    json!({"events": [
        {"message": "Execution started."},
        {"message": "===== JOB METRICS ====="},
        {"message": format!("  Tasks            : {tasks}")},
        {"message": "  Input            : 1,250,007 records (41.91 KB)"},
        {"message": "========================="},
        {"message": "Execution complete."},
    ], "hasMoreLogs": false, "cursor": "c"})
}

/// Version 2 recorded run A, version 1 run B (on its second page of runs).
fn lineage(query: &str) -> Value {
    let versions = json!([{"job_version_id": 22}, {"job_version_id": 11}]);
    let graph = if query.contains("versionId=22") {
        json!({"jobVersion": {"versionNumber": 2, "codeHash": "1111aaaa2222bbbb"},
               "runs": [{"runId": 7, "jobExecutionId": RUN_A}],
               "assets": [{"assetId": 1, "qualifiedName": "lake.sales.orders"},
                          {"assetId": 2, "qualifiedName": "lake.dq.history"}],
               "observations": [{"runId": 7, "assetId": 1, "role": "SOURCE"},
                                {"runId": 7, "assetId": 2, "role": "TARGET"}],
               "pagination": {"runs": {"hasMore": false, "nextOffset": null}}})
    } else if query.contains("versionId=11") && query.contains("runOffset=0") {
        json!({"jobVersion": {"versionNumber": 1, "codeHash": "9999cccc"},
               "runs": [{"runId": 3, "jobExecutionId": "cccccccccccccccccccccccccccccccc"}],
               "assets": [], "observations": [],
               "pagination": {"runs": {"hasMore": true, "nextOffset": 100}}})
    } else if query.contains("versionId=11") {
        json!({"jobVersion": {"versionNumber": 1, "codeHash": "9999cccc"},
               "runs": [{"runId": 4, "jobExecutionId": RUN_B}],
               "assets": [{"assetId": 2, "qualifiedName": "lake.dq.history"}],
               "observations": [{"runId": 4, "assetId": 2, "role": "TARGET"}],
               "pagination": {"runs": {"hasMore": false, "nextOffset": null}}})
    } else {
        return json!({"versions": versions, "graph": {}});
    };
    json!({"versions": versions, "graph": graph})
}

fn reply(platform: &mut Platform, request: &Recorded) -> Option<Value> {
    let (path, query) = request
        .target
        .split_once('?')
        .unwrap_or((request.target.as_str(), ""));
    let run = request.body["jobIdentifier"].as_str().unwrap_or_default();
    let reply = match (request.method.as_str(), path) {
        ("POST", "/etl/jobStatus") => {
            let status = platform.statuses.get(run)?;
            json!({"data": [{"jobstatus": status, "startedat": "Mon, 16 Nov 2026 03:56:13 GMT",
                             "stoppedat": "Mon, 16 Nov 2026 03:57:13 GMT"}]})
        }
        ("POST", "/etl/logs") => log(run),
        ("GET", p) if p == format!("/etl/consumption-metrics/{RUN_A}") => metrics(RUN_A, 2.0, 8),
        ("GET", p) if p == format!("/etl/consumption-metrics/{RUN_B}") => metrics(RUN_B, 8.0, 9),
        ("GET", "/etl/lineage/jobs/3013") => lineage(query),
        ("GET", "/etl/etlProjects") => json!([{"Id": 1999, "Name": "DataQuality"}]),
        ("GET", "/etl/etlJobs/1999/jobs") => json!([{"Id": 3013, "Name": "DQC"}]),
        ("GET", "/etl/etlJobs/jobs/1999/3013") => {
            json!({"config": {"connectionNames": ["GZLakehouseProd"]}, "fileContent": platform.code})
        }
        ("POST", "/etl/job") => {
            platform.submitted += 1;
            let id = format!("{:032x}", platform.submitted);
            let slice = request.body["_externalJobPlaceholders"]["$RUN_DATE"]
                .as_str()
                .unwrap_or_default();
            let ending = platform
                .slice_endings
                .get(slice)
                .copied()
                .unwrap_or("Success");
            platform.statuses.insert(id.clone(), ending);
            json!({"jobIdentifier": id})
        }
        _ => return None,
    };
    Some(reply)
}

fn stub(platform: Platform) -> (String, Arc<Mutex<Platform>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let platform = Arc::new(Mutex::new(platform));
    let shared = platform.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            let mut platform = shared.lock().unwrap();
            platform.requests.push(request.clone());
            match reply(&mut platform, &request) {
                Some(body) => respond(&mut stream, "200 OK", "application/json", &body.to_string()),
                None => respond(&mut stream, "404 NOT FOUND", "text/plain", "no such route"),
            }
        }
    });
    (base, platform)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-jobs-ops-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "jobs-ops-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(Stdio::null())
        .args(args)
        .output()
        .unwrap()
}

fn calls(platform: &Arc<Mutex<Platform>>) -> Vec<String> {
    platform
        .lock()
        .unwrap()
        .requests
        .iter()
        .map(|r| format!("{} {}", r.method, r.target))
        .collect()
}

fn finished_runs() -> Platform {
    Platform {
        statuses: HashMap::from([
            (RUN_A.to_string(), "Success"),
            (RUN_B.to_string(), "Failed"),
        ]),
        ..Platform::default()
    }
}

// ---------- compare ----------

#[test]
fn compare_shows_two_runs_side_by_side_in_json() {
    let (base, _) = stub(finished_runs());
    let out = gz(
        &temp_dir("compare"),
        &base,
        &["jobs", "compare", RUN_A, RUN_B, "-o", "json"],
    );
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let row = |field: &str| {
        report["comparison"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["field"] == field)
            .unwrap_or_else(|| panic!("no {field} row"))
            .clone()
    };
    assert_eq!(row("status")["first"], "Success");
    assert_eq!(row("status")["second"], "Failed");
    assert_eq!(row("requested CPU cores")["changed"], true);
    assert_eq!(row("rows written")["first"], "8");
    assert_eq!(row("rows read")["changed"], false);
    assert_eq!(row("tables read")["first"], "lake.sales.orders");
    assert_eq!(row("tables written")["second"], "lake.dq.history");
    assert_eq!(row("job version")["first"], "v2");
    assert_eq!(row("job version")["second"], "v1");
    assert_eq!(row("Tasks")["first"], "28 completed");
    assert_eq!(row("Tasks")["second"], "33 completed");
    assert_eq!(
        row("runtime image")["first"],
        "not returned by the platform"
    );
    let notes = report["notes"].to_string();
    assert!(
        notes.contains("the runs executed different code"),
        "{notes}"
    );
    assert!(notes.contains("config is not kept per run"), "{notes}");
    assert_eq!(
        report["runs"][1]["lineage"]["tablesWritten"],
        json!(["lake.dq.history"])
    );
}

#[test]
fn compare_pages_through_a_versions_runs_to_find_one() {
    let (base, platform) = stub(finished_runs());
    let out = gz(
        &temp_dir("pages"),
        &base,
        &["jobs", "compare", RUN_A, RUN_B],
    );
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let calls = calls(&platform);
    assert!(calls.iter().any(|c| c.contains("versionId=11")
        && c.contains("runOffset=100")
        && c.contains("runLimit=100")));
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("current-run") || c.contains("ekslogs")),
        "{calls:?}"
    );
}

#[test]
fn compare_refuses_a_run_still_going_before_reading_its_log() {
    let mut platform = finished_runs();
    platform.statuses.insert(RUN_B.to_string(), "Running");
    let (base, platform) = stub(platform);
    let out = gz(
        &temp_dir("running"),
        &base,
        &["jobs", "compare", RUN_A, RUN_B],
    );
    assert!(!out.status.success());
    let stderr = plain(&out.stderr);
    assert!(
        stderr.contains(&format!("run {RUN_B} is still Running")),
        "{stderr}"
    );
    let calls = calls(&platform);
    let platform = platform.lock().unwrap();
    let logs_of_b = platform
        .requests
        .iter()
        .any(|r| r.target == "/etl/logs" && r.body["jobIdentifier"] == RUN_B);
    assert!(!logs_of_b, "{calls:?}");
}

#[test]
fn compare_refuses_what_is_not_a_run_id() {
    let (base, platform) = stub(finished_runs());
    let out = gz(
        &temp_dir("bad-id"),
        &base,
        &["jobs", "compare", "3013", RUN_B],
    );
    assert!(!out.status.success());
    assert!(plain(&out.stderr).contains("is not a run id"));
    assert!(calls(&platform).is_empty());
}

// ---------- backfill ----------

const BACKFILL: [&str; 13] = [
    "jobs",
    "backfill",
    "1999",
    "3013",
    "--from",
    "2026-11-01",
    "--to",
    "2026-11-03",
    "--compute",
    "1006",
    "--rte",
    "gz-rte-spark-sklearn:1.0.0",
    "--yes",
];

fn backfill(dir: &Path, base: &str, extra: &[&str]) -> Output {
    let args: Vec<&str> = BACKFILL
        .iter()
        .copied()
        .chain(extra.iter().copied())
        .collect();
    gz(dir, base, &args)
}

fn reads_the_date() -> Platform {
    Platform {
        code: "run_date = $RUN_DATE\nend = $RUN_DATE_END\n",
        ..Platform::default()
    }
}

fn submitted_slices(platform: &Arc<Mutex<Platform>>) -> Vec<Value> {
    platform
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.target == "/etl/job")
        .map(|r| r.body["_externalJobPlaceholders"].clone())
        .collect()
}

#[test]
fn a_dry_run_lists_slices_and_starts_nothing() {
    let dir = temp_dir("dry");
    let (base, platform) = stub(reads_the_date());
    let out = backfill(&dir, &base, &["--dry-run", "-o", "json"]);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let plan: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(plan["jobReadsParam"], true);
    let slices = plan["slices"].as_array().unwrap();
    assert_eq!(slices.len(), 3);
    assert_eq!(slices[2]["$RUN_DATE"], "2026-11-03");
    assert_eq!(slices[2]["$RUN_DATE_END"], "2026-11-04");
    assert!(calls(&platform).iter().all(|c| c.starts_with("GET")));
    assert!(
        !dir.join("config/gz/backfills").exists(),
        "a dry run writes no state"
    );
}

#[test]
fn each_slice_runs_with_its_date_and_a_rerun_skips_the_done_ones() {
    let dir = temp_dir("run");
    let (base, platform) = stub(reads_the_date());
    let out = backfill(&dir, &base, &["--concurrency", "2"]);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert_eq!(
        submitted_slices(&platform),
        [
            json!({"$RUN_DATE": "2026-11-01", "$RUN_DATE_END": "2026-11-02"}),
            json!({"$RUN_DATE": "2026-11-02", "$RUN_DATE_END": "2026-11-03"}),
            json!({"$RUN_DATE": "2026-11-03", "$RUN_DATE_END": "2026-11-04"}),
        ]
    );
    let submit = platform
        .lock()
        .unwrap()
        .requests
        .iter()
        .find(|r| r.target == "/etl/job")
        .unwrap()
        .body
        .clone();
    assert_eq!(submit["jobId"], 3013);
    assert_eq!(submit["computeId"], 1006);
    assert!(plain(&out.stderr).contains("3 succeeded, 0 failed"));
    let state = dir.join("config/gz/backfills/1999-3013-day-2026-11-01-2026-11-03.json");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(saved["slices"]["2026-11-02"]["status"], "Success");

    let again = backfill(&dir, &base, &[]);
    assert!(again.status.success(), "{}", plain(&again.stderr));
    assert_eq!(submitted_slices(&platform).len(), 3, "nothing ran twice");
}

#[test]
fn a_failed_slice_exits_two_and_retry_runs_only_it_again() {
    let dir = temp_dir("retry");
    let mut platform = reads_the_date();
    platform.slice_endings.insert("2026-11-02".into(), "Failed");
    let (base, platform) = stub(platform);
    let out = backfill(&dir, &base, &[]);
    assert_eq!(out.status.code(), Some(2), "{}", plain(&out.stderr));
    assert!(plain(&out.stderr).contains("--retry"));

    let skipped = backfill(&dir, &base, &[]);
    assert_eq!(skipped.status.code(), Some(2));
    assert_eq!(
        submitted_slices(&platform).len(),
        3,
        "a failure is not rerun unasked"
    );

    platform.lock().unwrap().slice_endings.clear();
    let retried = backfill(&dir, &base, &["--retry"]);
    assert!(retried.status.success(), "{}", plain(&retried.stderr));
    let slices = submitted_slices(&platform);
    assert_eq!(slices.len(), 4);
    assert_eq!(slices[3]["$RUN_DATE"], "2026-11-02");
}

#[test]
fn a_run_left_going_is_picked_up_instead_of_started_again() {
    let dir = temp_dir("resume");
    let state = dir.join("config/gz/backfills");
    std::fs::create_dir_all(&state).unwrap();
    let going = "dddddddddddddddddddddddddddddddd";
    std::fs::write(
        state.join("1999-3013-day-2026-11-01-2026-11-03.json"),
        json!({"slices": {"2026-11-01": {"status": "Submitted", "jobIdentifier": going}}})
            .to_string(),
    )
    .unwrap();
    let mut platform = reads_the_date();
    platform.statuses.insert(going.into(), "Success");
    let (base, platform) = stub(platform);
    let out = backfill(&dir, &base, &[]);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let slices = submitted_slices(&platform);
    assert_eq!(slices.len(), 2);
    assert!(slices.iter().all(|s| s["$RUN_DATE"] != "2026-11-01"));
    assert!(plain(&out.stderr).contains(&format!("2026-11-01 picked up (run {going})")));
}

#[test]
fn a_job_that_never_reads_its_date_is_refused() {
    let (base, platform) = stub(Platform {
        code: "print('same every time')\n",
        ..Platform::default()
    });
    let out = backfill(&temp_dir("no-param"), &base, &[]);
    assert!(!out.status.success());
    assert!(plain(&out.stderr).contains("never reads $RUN_DATE"));
    assert!(submitted_slices(&platform).is_empty());
}
