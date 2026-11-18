//! quality.toml and scheduled watches against stub platform services:
//! `gz quality lint`, the rules a `gz jobs try` runs on its branch (and the
//! publish gate they feed), `gz quality check --rules`, and `gz quality
//! watch` with its schedule, failure email, runs and removal. No email is
//! sent anywhere: the Schedule API is a stub.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const TABLE: &str = "/lakehouse/tables/Manufacturing/mfg_ops/oee_daily";
const CHECK_JOB: i64 = 4242;
const SCHEDULE: i64 = 1041;
const RULES: &str = r#"
[tables."Manufacturing.mfg_ops.oee_daily"]
rows = { min = 1 }
not_null = ["line_id"]
unique = [["line_id", "day"]]

[[tables."Manufacturing.mfg_ops.oee_daily".sql]]
name = "no_negative_oee"
query = "SELECT * FROM {table} WHERE oee < 0"
"#;

#[derive(Default)]
struct Platform {
    requests: Vec<Recorded>,
    /// Checks the check job's report fails.
    failures: Vec<&'static str>,
    gate_required: bool,
    /// try-1's verdict, as the provider's `/quality` returns it.
    verdict: Value,
    check_code: String,
    check_config: Value,
    schedule: Option<Value>,
    email: Option<Value>,
    watch_runs: Vec<Value>,
}

fn params(code: &str) -> Value {
    let line = code
        .lines()
        .find(|l| l.starts_with("PARAMS = json.loads("))
        .expect("the script loads its PARAMS");
    let literal = &line["PARAMS = json.loads(".len()..line.len() - 1];
    serde_json::from_str(&serde_json::from_str::<String>(literal).unwrap()).unwrap()
}

fn report_lines(platform: &Platform) -> Value {
    let params = params(&platform.check_code);
    let marker = params["marker"].as_str().unwrap();
    let report = json!({
        "dataset": {"row_count": 15, "source_version": "13", "source_ref": params["tables"][0]["branch"]},
        "results": [
            {"name": "row_count", "severity": "ok", "status": "completed", "error": null,
             "payload": {"rows": 15, "min": 1, "max": null}},
            {"name": "unique", "severity": if platform.failures.contains(&"unique") { "error" } else { "ok" },
             "status": "completed", "error": null,
             "payload": {"keys": [{"columns": ["line_id", "day"], "duplicateRows": 2}]}},
        ],
    });
    let text = json!({"report": report, "gateFailures": platform.failures}).to_string();
    json!([
        {"timestamp": "t", "message": "Spark session ready\n"},
        {"timestamp": "t", "message": format!("{marker}-001 1/1 {text}\n")},
    ])
}

/// What the check job's run records on the table: the runtime's verdict,
/// stamped with the rules' fingerprint.
fn record_verdict(platform: &mut Platform) {
    let params = params(&platform.check_code);
    let table = &params["tables"][0];
    if table["branch"].is_null() {
        return;
    }
    platform.verdict = json!({
        "version": 1, "snapshotId": "13", "passed": platform.failures.is_empty(),
        "failOn": "error", "failedChecks": platform.failures, "rules": table["fingerprint"],
    });
}

fn reply(platform: &mut Platform, request: &Recorded) -> Option<Value> {
    let path = request
        .target
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();
    let check_job = format!("/etl/etlJobs/jobs/1999/{CHECK_JOB}");
    let try_check_job = format!("/etl/etlJobs/jobs/1138/{CHECK_JOB}");
    let schedule_named = platform
        .schedule
        .as_ref()
        .map(|s| format!("/schedule/{SCHEDULE}/{}", s["name"].as_str().unwrap()));
    let reply = match (request.method.as_str(), path.as_str()) {
        ("GET", "/etl/etlProjects") => json!([
            {"Id": 1138, "Name": "Manufacturing"},
            {"Id": 1999, "Name": "DataQuality"},
        ]),
        ("GET", "/etl/etlJobs/1138/jobs") => json!([{"Id": 1324, "Name": "build-oee"}]),
        ("GET", "/etl/lineage/jobs/1324") => json!({"graph": {"assets": [], "relations": [],
        "compilerGraph": {"assets": [
            {"platform": "gzlakehouse", "directions": ["sink"], "connection": {"id": "1360"},
             "attributes": {"qualifiedName": "MFG_OPS.OEE_DAILY"}},
        ]}}}),
        ("GET", "/connection") => json!([
            {"Id": 1360, "Name": "manufacturingops", "type": "GZLAKEHOUSE"},
        ]),
        ("GET", "/connection/1360") => json!([{"name": "manufacturingops", "type": "GZLAKEHOUSE",
            "config": {"warehouseName": "Manufacturing", "databaseName": "mfg_ops"}}]),
        ("GET", "/rte/compute") => json!([{"Id": 1006, "Name": "small"}]),
        ("GET", "/etl/job/options") => json!({"writeBranch": {"runtimes": ["spark"]}}),
        ("GET", "/etl/etlJobs/jobs/1138/1324") => json!({
            "config": {"connectionNames": ["manufacturingops"]}, "fileContent": "print('saved')\n",
        }),
        ("PUT", p) if p == format!("{TABLE}/write-branches/try-1") => {
            json!({"status": 201, "response": {"name": "try-1", "snapshotId": "11"}})
        }
        ("PUT", p) if p == format!("{TABLE}/quality-gate") => {
            platform.gate_required = request.body["required"] == json!(true);
            json!({"status": 200, "response": {"required": platform.gate_required}})
        }
        ("GET", p) if p == format!("{TABLE}/quality") => json!({"status": 200, "response": {
            "required": platform.gate_required,
            "branches": [{"name": "try-1", "headSnapshotId": "13", "current": true,
                          "verdict": platform.verdict}],
        }}),
        ("GET", p) if p == format!("{TABLE}/versions") => json!({"status": 200, "response": {
            "currentSnapshotId": "11",
            "snapshots": [
                {"snapshotId": "13", "parentSnapshotId": "11", "committedAt": "2026-11-11T09:03:00+00:00",
                 "operation": "append", "summary": {"added-records": "5", "total-records": "15"}},
                {"snapshotId": "11", "parentSnapshotId": null, "committedAt": "2026-11-11T09:01:00+00:00",
                 "operation": "append", "summary": {"added-records": "10", "total-records": "10"}},
            ],
            "branches": [{"name": "main", "snapshotId": "11"}, {"name": "try-1", "snapshotId": "13"}],
            "tags": [],
        }}),
        ("POST", p) if p == format!("{TABLE}/write-branches/try-1/publish") => {
            json!({"status": 200, "response": {"name": "try-1", "published": true,
                "previousSnapshotId": "11", "currentSnapshotId": "13", "snapshots": []}})
        }
        ("POST", "/etl/etlJobs/1138") | ("POST", "/etl/etlJobs/1999") => {
            platform.check_code = request.body["fileContent"].as_str()?.to_string();
            json!({"Id": CHECK_JOB})
        }
        ("GET", p) if p == check_job || p == try_check_job => {
            json!({"config": platform.check_config, "fileContent": platform.check_code})
        }
        ("PUT", p) if p == check_job || p == try_check_job => {
            platform.check_config = request.body["config"].clone();
            json!({"status": 200})
        }
        ("POST", "/etl/job") if request.body["jobId"] == json!(1324) => {
            json!({"jobIdentifier": "run-1", "writeBranch": "try-1"})
        }
        ("POST", "/etl/job") => {
            record_verdict(platform);
            json!({"jobIdentifier": "run-check"})
        }
        ("POST", "/etl/jobStatus") => json!({"data": [{"jobstatus": "Success"}]}),
        ("POST", "/etl/logs") if request.body["jobIdentifier"] == json!("run-1") => {
            json!({"events": [{"timestamp": "t", "message": "wrote 5 rows"}], "hasMoreLogs": false})
        }
        ("POST", "/etl/logs") => {
            json!({"events": report_lines(platform), "hasMoreLogs": false, "cursor": "c1"})
        }
        ("DELETE", p) if p == format!("/etl/etlJobs/jobs/{CHECK_JOB}") => json!({"status": 200}),
        ("GET", "/schedule") => json!(platform.schedule.iter().cloned().collect::<Vec<_>>()),
        ("POST", "/schedule") => {
            let body = &request.body;
            platform.schedule = Some(json!({
                "Id": SCHEDULE, "name": body["name"], "cron": body["cron"],
                "description": body["description"], "scheduleType": body["scheduleType"],
                "projectId": body["jobDetails"]["projectId"], "projectName": body["jobDetails"]["projectName"],
                "jobId": body["jobDetails"]["jobId"], "jobName": body["jobDetails"]["jobName"],
                "computeId": body["jobDetails"]["computeId"], "rteName": body["jobDetails"]["rteName"],
            }));
            json!({"schedule": SCHEDULE})
        }
        ("GET", "/schedule/notificationTypes") => json!([
            {"name": "Success", "notificationTypeId": 1000},
            {"name": "Failure", "notificationTypeId": 1003},
        ]),
        ("POST", p) if p == format!("/schedule/{SCHEDULE}/email") => {
            let mut email = request.body.clone();
            email["notificationId"] = json!(7);
            email["type"] = json!("Failure");
            platform.email = Some(email);
            json!({"status": "success"})
        }
        ("GET", p) if p == format!("/schedule/{SCHEDULE}/email") => {
            json!(platform.email.iter().cloned().collect::<Vec<_>>())
        }
        ("DELETE", p) if p == format!("/schedule/{SCHEDULE}/email/7") => {
            platform.email = None;
            json!({})
        }
        ("GET", p) if p == format!("/schedule/{SCHEDULE}") => {
            json!({"enabled": true, "cron": platform.schedule.as_ref()?["cron"]})
        }
        ("GET", p) if p == format!("/schedule/{SCHEDULE}/runs") => json!(platform.watch_runs),
        ("PUT", p)
            if Some(p.replacen("/schedule/run", "/schedule", 1)) == schedule_named
                && p.starts_with("/schedule/run/") =>
        {
            platform
                .watch_runs
                .push(json!({"executionId": "run-check", "status": "Success",
                "startTime": "2026-11-18T10:00:00.000Z", "stopTime": "2026-11-18T10:02:00.000Z"}));
            json!({"status": "success"})
        }
        ("DELETE", p) if Some(p.to_string()) == schedule_named => {
            platform.schedule = None;
            json!({})
        }
        _ => return None,
    };
    Some(reply)
}

fn stub(failures: Vec<&'static str>) -> (String, Arc<Mutex<Platform>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let platform = Arc::new(Mutex::new(Platform {
        failures,
        ..Platform::default()
    }));
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
    let dir = std::env::temp_dir().join(format!("gz-quality-rules-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "quality-rules-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .args(args)
        .output()
        .unwrap()
}

fn said(out: &Output) -> String {
    format!("{}{}", plain(&out.stdout), plain(&out.stderr))
}

fn changes(platform: &Arc<Mutex<Platform>>) -> Vec<String> {
    platform
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.method != "GET" && r.target != "/etl/jobStatus" && r.target != "/etl/logs")
        .map(|r| format!("{} {}", r.method, r.target))
        .collect()
}

fn body_of(platform: &Arc<Mutex<Platform>>, method: &str, target: &str) -> Value {
    platform
        .lock()
        .unwrap()
        .requests
        .iter()
        .find(|r| r.method == method && r.target == target)
        .unwrap_or_else(|| panic!("no {method} {target}"))
        .body
        .clone()
}

const TRY: &[&str] = &[
    "-o",
    "json",
    "jobs",
    "try",
    "build-oee",
    "--project",
    "1138",
    "--branch",
    "try-1",
    "--compute",
    "1003",
    "--rte",
    "gz-rte-duckdb-sklearn:1.0.0",
];

// ---------- lint ----------

#[test]
fn lint_lists_every_problem_with_its_line_and_column() {
    let dir = temp_dir("lint");
    std::fs::write(
        dir.join("quality.toml"),
        "[tables.\"w.d.t\"]\nrows = { min = 5, max = 1 }\nnot_null = [\" id\"]\n",
    )
    .unwrap();
    let out = gz(&dir, "http://127.0.0.1:9", &["quality", "lint"]);
    assert!(!out.status.success());
    let text = said(&out);
    assert!(
        text.contains("quality.toml:2:8: rows.min is larger than rows.max"),
        "{text}"
    );
    assert!(
        text.contains("quality.toml:3:13: a column name can't be empty"),
        "{text}"
    );

    let help = said(&gz(&dir, "http://127.0.0.1:9", &["quality", "--help"]));
    assert!(
        help.contains("lint     Check a quality.toml and list every problem"),
        "{help}"
    );

    std::fs::write(dir.join("quality.toml"), RULES).unwrap();
    let out = gz(&dir, "http://127.0.0.1:9", &["quality", "lint", "."]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(
        said(&out).contains("1 table(s), 4 check(s), no problems"),
        "{}",
        said(&out)
    );
}

// ---------- try and publish ----------

#[test]
fn try_runs_the_rules_on_its_branch_after_turning_the_gate_on() {
    let dir = temp_dir("try-pass");
    std::fs::write(dir.join("quality.toml"), RULES).unwrap();
    let (base, platform) = stub(vec![]);

    let out = gz(&dir, &base, TRY);

    assert!(out.status.success(), "{}", said(&out));
    assert_eq!(
        changes(&platform),
        [
            format!("PUT {TABLE}/write-branches/try-1"),
            "POST /etl/job".to_string(),
            format!("PUT {TABLE}/quality-gate"),
            "POST /etl/etlJobs/1138".to_string(),
            format!("PUT /etl/etlJobs/jobs/1138/{CHECK_JOB}"),
            "POST /etl/job".to_string(),
            format!("DELETE /etl/etlJobs/jobs/{CHECK_JOB}"),
        ],
        "the gate is on and the checks run after the try's run"
    );
    let state = platform.lock().unwrap();
    let script = params(&state.check_code);
    let table = &script["tables"][0];
    assert_eq!(table["branch"], "try-1");
    assert_eq!(table["connection"], "manufacturingops");
    assert_eq!(table["fingerprint"].as_str().unwrap().len(), 16);
    assert_eq!(table["config"]["checks"]["outliers"], json!(false));
    let checks: Vec<&str> = table["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["check"].as_str().unwrap())
        .collect();
    assert_eq!(
        checks,
        ["row_count", "null_limits", "unique", "sql:no_negative_oee"]
    );
    assert_eq!(script["failRun"], json!(false));
    drop(state);
    let check_run = platform
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.target == "/etl/job")
        .nth(1)
        .unwrap()
        .body
        .clone();
    assert_eq!(
        check_run["rteName"], "gz-rte-spark-sklearn:1.0.0",
        "checks need Spark even when the try ran on DuckDB"
    );
    assert_eq!(check_run["computeId"], json!(1003));

    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["qualityChecks"]["passed"], json!(true));
    assert_eq!(
        shown["qualityChecks"]["tables"][0]["table"],
        "Manufacturing.mfg_ops.oee_daily"
    );
    assert_eq!(
        shown["next"][0],
        "gz jobs publish 1324 --project 1138 --branch try-1"
    );
}

#[test]
fn failing_rules_fail_the_try_and_never_suggest_publishing() {
    let dir = temp_dir("try-fail");
    std::fs::write(dir.join("quality.toml"), RULES).unwrap();
    let (base, _platform) = stub(vec!["unique"]);

    let out = gz(&dir, &base, TRY);

    assert_eq!(out.status.code(), Some(2), "{}", said(&out));
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["qualityChecks"]["passed"], json!(false));
    assert_eq!(
        shown["qualityChecks"]["tables"][0]["gateFailures"],
        json!(["unique"])
    );
    let next = shown["next"].to_string();
    assert!(!next.contains("publish"), "{next}");
    assert!(next.contains("gz jobs discard"), "{next}");
}

#[test]
fn try_shows_the_checks_in_text_mode() {
    let dir = temp_dir("try-text");
    std::fs::write(dir.join("quality.toml"), RULES).unwrap();
    let (base, _platform) = stub(vec!["unique"]);

    let out = gz(&dir, &base, &TRY[2..]);

    let text = said(&out);
    assert!(text.contains("Quality checks (quality.toml):"), "{text}");
    assert!(text.contains("line_id+day: 2 duplicates"), "{text}");
    assert!(
        text.contains("Manufacturing.mfg_ops.oee_daily: failed at error: unique"),
        "{text}"
    );
}

#[test]
fn a_broken_quality_toml_stops_the_try_before_anything_changes() {
    let dir = temp_dir("try-broken");
    std::fs::write(
        dir.join("quality.toml"),
        "[tables.\"Manufacturing.mfg_ops.oee_daily\"]\nrows = { min = -1 }\n",
    )
    .unwrap();
    let (base, platform) = stub(vec![]);

    let out = gz(&dir, &base, TRY);

    assert!(!out.status.success());
    assert!(
        said(&out).contains("quality.toml:2:8: rows.min can't be negative"),
        "{}",
        said(&out)
    );
    assert!(changes(&platform).is_empty(), "{:?}", changes(&platform));
}

#[test]
fn publish_goes_ahead_once_the_branch_passed_these_rules() {
    let dir = temp_dir("publish-pass");
    std::fs::write(dir.join("quality.toml"), RULES).unwrap();
    let (base, platform) = stub(vec![]);
    assert!(gz(&dir, &base, TRY).status.success());

    let out = gz(
        &dir,
        &base,
        &[
            "jobs",
            "publish",
            "build-oee",
            "--project",
            "1138",
            "--branch",
            "try-1",
            "--yes",
        ],
    );

    assert!(out.status.success(), "{}", said(&out));
    assert!(changes(&platform).contains(&format!("POST {TABLE}/write-branches/try-1/publish")));
}

#[test]
fn publish_refuses_when_the_rules_changed_since_the_try() {
    let dir = temp_dir("publish-changed");
    std::fs::write(dir.join("quality.toml"), RULES).unwrap();
    let (base, platform) = stub(vec![]);
    assert!(gz(&dir, &base, TRY).status.success());
    std::fs::write(
        dir.join("quality.toml"),
        RULES.replace("min = 1", "min = 10"),
    )
    .unwrap();

    let out = gz(
        &dir,
        &base,
        &[
            "jobs",
            "publish",
            "build-oee",
            "--project",
            "1138",
            "--branch",
            "try-1",
            "--yes",
        ],
    );

    assert!(!out.status.success());
    let text = said(&out);
    assert!(
        text.contains("not from the rules in quality.toml now"),
        "{text}"
    );
    assert!(
        text.contains("gz quality check Manufacturing.mfg_ops.oee_daily --project 1138 --branch try-1 --rules"),
        "{text}"
    );
    assert!(!changes(&platform).iter().any(|c| c.ends_with("/publish")));
}

#[test]
fn publish_refuses_a_branch_whose_checks_failed_or_never_ran() {
    let dir = temp_dir("publish-failed");
    std::fs::write(dir.join("quality.toml"), RULES).unwrap();
    let (base, platform) = stub(vec![]);
    let publish = [
        "jobs",
        "publish",
        "build-oee",
        "--project",
        "1138",
        "--branch",
        "try-1",
        "--yes",
    ];

    let out = gz(&dir, &base, &publish);
    assert!(
        said(&out).contains("has no quality.toml checks recorded"),
        "{}",
        said(&out)
    );

    platform.lock().unwrap().failures = vec!["unique"];
    assert_eq!(gz(&dir, &base, TRY).status.code(), Some(2));
    let out = gz(&dir, &base, &publish);
    assert!(
        said(&out).contains("failed its quality.toml checks: unique"),
        "{}",
        said(&out)
    );
    assert!(!changes(&platform).iter().any(|c| c.ends_with("/publish")));
}

// ---------- check --rules ----------

#[test]
fn check_with_rules_runs_the_files_rules_and_exits_2_when_they_fail() {
    let dir = temp_dir("check");
    std::fs::write(dir.join("quality.toml"), RULES).unwrap();
    let (base, platform) = stub(vec!["unique"]);

    let out = gz(
        &dir,
        &base,
        &[
            "-o",
            "json",
            "quality",
            "check",
            "Manufacturing.mfg_ops.oee_daily",
            "--project",
            "1999",
            "--rules",
        ],
    );

    assert_eq!(out.status.code(), Some(2), "{}", said(&out));
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["tables"][0]["gateFailures"], json!(["unique"]));
    let script = params(&platform.lock().unwrap().check_code);
    assert_eq!(script["tables"][0]["branch"], Value::Null);
    assert_eq!(
        script["tables"][0]["fingerprint"],
        Value::Null,
        "main has no verdict to stamp"
    );
}

// ---------- watch ----------

#[test]
fn a_watch_is_a_job_and_a_schedule_with_a_failure_email_and_stops_cleanly() {
    let dir = temp_dir("watch");
    let (base, platform) = stub(vec![]);

    let out = gz(
        &dir,
        &base,
        &[
            "quality",
            "watch",
            "Manufacturing.mfg_ops.oee_daily",
            "--project",
            "1999",
            "--every",
            "1h",
            "--max-age",
            "24h",
            "--notify-email",
            "ops@acme.example",
            "--notify-email",
            "lead@acme.example",
            "--run-now",
        ],
    );
    assert!(out.status.success(), "{}", said(&out));
    assert!(
        said(&out).contains("run run-check started"),
        "{}",
        said(&out)
    );

    let schedule = body_of(&platform, "POST", "/schedule");
    let name = schedule["name"].as_str().unwrap().to_string();
    assert!(
        name.starts_with("gz-quality-watch-manufacturing-mfg-ops-oee-daily-"),
        "{name}"
    );
    assert!(
        schedule["cron"]
            .as_str()
            .unwrap()
            .ends_with(" 0/1 * * ? *)"),
        "{schedule}"
    );
    assert_eq!(schedule["scheduleType"], "BATCH");
    assert_eq!(schedule["jobDetails"]["jobId"], json!(CHECK_JOB));
    assert_eq!(schedule["jobDetails"]["computeId"], json!(1006));
    assert_eq!(
        schedule["jobDetails"]["rteName"],
        "gz-rte-spark-sklearn:1.0.0"
    );
    assert_eq!(
        schedule["config"]["gzQualityWatch"]["table"],
        "Manufacturing.mfg_ops.oee_daily"
    );

    let email = body_of(&platform, "POST", &format!("/schedule/{SCHEDULE}/email"));
    assert_eq!(email["notificationTypeId"], json!(1003));
    assert_eq!(email["toEmail"], "ops@acme.example");
    assert_eq!(email["ccEmail"], "lead@acme.example");
    assert_eq!(email["fromEmail"], "ops@acme.example");
    assert_eq!(email["attachLogs"], json!(false));

    let state = platform.lock().unwrap();
    let record = &state.check_config["gzQualityWatch"];
    assert_eq!(record["maxAge"], "24h");
    assert_eq!(
        state.check_config["connectionNames"],
        json!(["manufacturingops"])
    );
    let script = params(&state.check_code);
    assert_eq!(
        script["failRun"],
        json!(true),
        "a failing check fails the run, which emails"
    );
    assert_eq!(
        script["tables"][0]["config"],
        Value::Null,
        "the default checks"
    );
    assert_eq!(
        script["tables"][0]["checks"],
        json!([{"check": "commit_age", "maxHours": 24.0}])
    );
    assert_eq!(script["marker"], record["marker"]);
    drop(state);
    assert!(
        !changes(&platform).iter().any(|c| c.starts_with("DELETE")),
        "the watch's job stays"
    );

    let out = gz(&dir, &base, &["-o", "json", "quality", "watch", "list"]);
    let listed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(listed[0]["id"], json!(SCHEDULE));
    assert_eq!(listed[0]["table"], "Manufacturing.mfg_ops.oee_daily");
    assert_eq!(listed[0]["last status"], "Success");

    let out = gz(
        &dir,
        &base,
        &[
            "quality",
            "watch",
            "show",
            "manufacturing.MFG_OPS.oee_daily",
        ],
    );
    let text = said(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("a failing run emails ops@acme.example"),
        "{text}"
    );
    assert!(
        text.contains("Manufacturing.mfg_ops.oee_daily: passed"),
        "{text}"
    );
    platform.lock().unwrap().failures = vec!["unique"];
    let out = gz(&dir, &base, &["quality", "watch", "show", "1041"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a failed latest run exits like `gz quality check`"
    );
    platform.lock().unwrap().failures.clear();

    let again = gz(
        &dir,
        &base,
        &[
            "quality",
            "watch",
            "Manufacturing.mfg_ops.oee_daily",
            "--project",
            "1999",
            "--every",
            "1h",
        ],
    );
    assert!(
        said(&again).contains("is already watched (1041"),
        "{}",
        said(&again)
    );

    let out = gz(&dir, &base, &["quality", "watch", "stop", "1041", "--yes"]);
    assert!(out.status.success(), "{}", said(&out));
    let deletes: Vec<String> = changes(&platform)
        .into_iter()
        .filter(|c| c.starts_with("DELETE"))
        .collect();
    assert_eq!(
        deletes,
        [
            format!("DELETE /schedule/{SCHEDULE}/email/7"),
            format!("DELETE /schedule/{SCHEDULE}/{name}"),
            format!("DELETE /etl/etlJobs/jobs/{CHECK_JOB}"),
        ]
    );
}

#[test]
fn a_watch_with_rules_runs_the_files_rules_on_main() {
    let dir = temp_dir("watch-rules");
    std::fs::write(dir.join("quality.toml"), RULES).unwrap();
    let (base, platform) = stub(vec![]);

    let out = gz(
        &dir,
        &base,
        &[
            "quality",
            "watch",
            "Manufacturing.mfg_ops.oee_daily",
            "--project",
            "1999",
            "--every",
            "15m",
            "--rules",
        ],
    );

    assert!(out.status.success(), "{}", said(&out));
    let script = params(&platform.lock().unwrap().check_code);
    assert_eq!(script["tables"][0]["branch"], Value::Null);
    assert_eq!(script["tables"][0]["checks"].as_array().unwrap().len(), 4);
    assert!(
        !changes(&platform).iter().any(|c| c.contains("/email")),
        "no email without --notify-email"
    );
}

#[test]
fn intervals_a_schedule_cant_keep_are_refused_before_anything_is_made() {
    let dir = temp_dir("watch-bad");
    let (base, platform) = stub(vec![]);
    let out = gz(
        &dir,
        &base,
        &[
            "quality",
            "watch",
            "Manufacturing.mfg_ops.oee_daily",
            "--project",
            "1999",
            "--every",
            "7m",
        ],
    );
    assert!(!out.status.success());
    assert!(
        said(&out).contains("--every 7m can't be a schedule"),
        "{}",
        said(&out)
    );
    assert!(changes(&platform).is_empty());
}

// ---------- notify setting ----------

#[test]
fn configure_saves_the_notify_setting() {
    let dir = temp_dir("notify");
    let out = gz(&dir, "http://127.0.0.1:9", &["configure", "--notify", "on"]);
    assert!(out.status.success(), "{}", said(&out));
    let config = std::fs::read_to_string(dir.join("config/gz/config.toml")).unwrap();
    assert!(config.contains("notify = true"), "{config}");
    let out = gz(
        &dir,
        "http://127.0.0.1:9",
        &["configure", "--notify", "maybe"],
    );
    assert!(!out.status.success());
}
