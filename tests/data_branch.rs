//! `gz jobs try`, `gz jobs publish` and `gz jobs discard` against stub
//! ETL, connection and lakehouse providers: which tables get a branch, what
//! the run is sent, the diff shown, and what is undone when a step fails.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const TABLE: &str = "/lakehouse/tables/Manufacturing/mfg_ops/oee_daily";
const FLASK_404: &str = "<!doctype html>\n<html lang=en>\n<title>404 Not Found</title>\n<h1>Not Found</h1>\n<p>The requested URL was not found on the server.</p>";

/// How the stub providers behave in one test.
#[derive(Clone, Copy, Default)]
struct Stub {
    /// The ETL provider predates `writeBranch`.
    old_etl: bool,
    /// The ETL provider refuses the run.
    refuse_run: bool,
    /// Someone committed to main after the branch was made.
    main_moved: bool,
    /// What `…/quality` says about the table.
    gate: Gate,
    /// The provider refuses the publish with its quality 409.
    refuse_publish: bool,
}

#[derive(Clone, Copy, Default)]
enum Gate {
    /// The provider predates the route.
    #[default]
    Missing,
    /// Required, and try-1's head failed its checks.
    Failed,
    /// Required, and try-1's head passed.
    Passed,
}

fn quality(gate: Gate) -> Option<Value> {
    let passed = match gate {
        Gate::Missing => return None,
        Gate::Failed => false,
        Gate::Passed => true,
    };
    let failed: &[&str] = if passed {
        &[]
    } else {
        &["ranges", "missing_values"]
    };
    Some(
        json!({"status": 200, "response": {"required": true, "branches": [
            {"name": "try-1", "headSnapshotId": "13", "current": true, "verdict": {
                "version": 1, "snapshotId": "13", "passed": passed, "failOn": "error",
                "worstSeverity": if passed { "ok" } else { "error" }, "failedChecks": failed,
                "checkedAt": "2026-11-11T09:04:00Z", "runId": "r1", "qualipilotVersion": "3.6.0",
            }},
        ]}}),
    )
}

fn snapshot(id: u64, parent: Option<u64>, minute: u32, added: u64, total: u64) -> Value {
    json!({
        "snapshotId": id.to_string(),
        "parentSnapshotId": parent.map(|p| p.to_string()),
        "committedAt": format!("2026-11-11T09:{minute:02}:00+00:00"),
        "operation": "append",
        "summary": {"added-records": added.to_string(), "total-records": total.to_string()},
    })
}

/// `oee_daily`: main at 11 (or 12 when it moved on), the branch at 13.
fn versions(stub: Stub) -> Value {
    let main = if stub.main_moved { "12" } else { "11" };
    json!({
        "currentSnapshotId": main,
        "snapshots": [
            snapshot(13, Some(11), 3, 5, 15),
            snapshot(12, Some(11), 2, 1, 11),
            snapshot(11, None, 1, 10, 10),
        ],
        "branches": [
            {"name": "main", "snapshotId": main},
            {"name": "try-1", "snapshotId": "13"},
        ],
        "tags": [],
    })
}

fn lineage() -> Value {
    json!({"graph": {
        "assets": [],
        "relations": [],
        "compilerGraph": {"assets": [
            {"platform": "gzlakehouse", "directions": ["source"], "connection": {"id": "1360"},
             "attributes": {"qualifiedName": "MFG_OPS.PRODUCTION_RUNS"}},
            {"platform": "gzlakehouse", "directions": ["sink"], "connection": {"id": "1360"},
             "attributes": {"qualifiedName": "MFG_OPS.OEE_DAILY"}},
        ]},
    }})
}

fn reply(stub: Stub, request: &Recorded) -> Option<(&'static str, String)> {
    let path = request.target.split('?').next().unwrap_or_default();
    let ok = |value: Value| Some(("200 OK", value.to_string()));
    match (request.method.as_str(), path) {
        ("GET", "/etl/etlProjects") => ok(json!([{"Id": 1138, "Name": "Manufacturing"}])),
        ("GET", "/etl/etlJobs/1138/jobs") => ok(json!([{"Id": 1324, "Name": "build-oee"}])),
        ("GET", "/etl/lineage/jobs/1324") => ok(lineage()),
        ("GET", "/connection/1360") => ok(json!([{"name": "manufacturingops", "type": "GZLAKEHOUSE",
            "config": {"warehouseName": "Manufacturing", "databaseName": "mfg_ops"}}])),
        ("GET", "/etl/job/options") if stub.old_etl => {
            Some(("404 Not Found", FLASK_404.to_string()))
        }
        ("GET", "/etl/job/options") => ok(json!({"writeBranch": {"runtimes": ["spark"]}})),
        ("GET", "/etl/etlJobs/jobs/1138/1324") => ok(json!({
            "config": {"connectionNames": ["manufacturingops"], "minimumWorkers": 2},
            "fileContent": "print('saved')\n",
        })),
        ("PUT", p) if p == format!("{TABLE}/write-branches/try-1") => Some((
            "201 Created",
            json!({"status": 201, "response": {"name": "try-1", "snapshotId": "11", "writable": true}})
                .to_string(),
        )),
        ("POST", "/etl/job") if stub.refuse_run => ok(json!({"status": 400, "response": {
            "message": "This run can't write to branch try-1: mfg.downtime has no branch try-1"
        }})),
        ("POST", "/etl/job") => ok(json!({"jobIdentifier": "run-1", "writeBranch": "try-1"})),
        ("POST", "/etl/logs") => ok(json!({
            "events": [{"timestamp": "2026-11-11T09:03:00Z", "message": "wrote 5 rows"}],
            "hasMoreLogs": false,
            "jobStatus": [{"jobstatus": "Success"}],
        })),
        ("POST", "/etl/jobStatus") => ok(json!({"success": true, "data": [{"jobstatus": "Success"}]})),
        ("GET", p) if p == format!("{TABLE}/versions") => {
            ok(json!({"status": 200, "response": versions(stub)}))
        }
        ("GET", p) if p == format!("{TABLE}/quality") => quality(stub.gate).and_then(ok),
        ("POST", p) if p == format!("{TABLE}/write-branches/try-1/publish") && stub.refuse_publish => {
            Some(("409 CONFLICT", json!({"status": 409, "response": {
                "message": "Branch try-1 of mfg_ops.oee_daily has no passing quality verdict for its head 13: run runTableDataQualityChecks on it",
                "reason": "quality",
                "quality": {"verdict": null, "headSnapshotId": "13"},
            }}).to_string()))
        }
        ("POST", p) if p == format!("{TABLE}/write-branches/try-1/publish") => {
            ok(json!({"status": 200, "response": {
                "name": "try-1", "published": true, "previousSnapshotId": "11",
                "currentSnapshotId": "13",
                "snapshots": [{"snapshotId": "13", "operation": "append"}],
            }}))
        }
        ("DELETE", p) if p == format!("{TABLE}/branches/try-1") => {
            ok(json!({"status": 200, "response": {"name": "try-1", "deleted": true}}))
        }
        _ => None,
    }
}

fn stub_providers(stub: Stub) -> (String, Arc<Mutex<Vec<Recorded>>>) {
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
            match reply(stub, &request) {
                Some((status, body)) if body.starts_with('<') => {
                    respond(&mut stream, status, "text/html", &body)
                }
                Some((status, body)) => respond(&mut stream, status, "application/json", &body),
                None => respond(&mut stream, "404 Not Found", "text/html", FLASK_404),
            }
        }
    });
    (base, log)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-branch-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "branch-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .args(args)
        .output()
        .unwrap()
}

/// What gz said: with `-o json` an error is printed as `{"error": …}` on stdout.
fn said(out: &Output) -> String {
    format!("{}{}", plain(&out.stdout), plain(&out.stderr))
}

/// `METHOD /path` for each request after the job and its tables were found.
fn changes(log: &Arc<Mutex<Vec<Recorded>>>) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|r| r.method != "GET" && r.target != "/etl/jobStatus")
        .map(|r| format!("{} {}", r.method, r.target))
        .collect()
}

const TRY: &[&str] = &[
    "-o",
    "json",
    "jobs",
    "try",
    "build-oee",
    "--branch",
    "try-1",
    "--compute",
    "1003",
    "--rte",
    "gz-rte-spark-pytorch:1.0.0",
];

#[test]
fn try_runs_the_synced_code_on_a_branch_of_each_written_table_and_shows_the_diff() {
    let dir = temp_dir("try");
    std::fs::write(
        dir.join(".gz-sync.json"),
        r#"{"project": 1138, "jobs": {}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("build-oee.py"), "print('changed')\r\n").unwrap();
    let (base, log) = stub_providers(Stub::default());

    let out = gz(&dir, &base, TRY);

    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert_eq!(
        changes(&log),
        [
            format!("PUT {TABLE}/write-branches/try-1"),
            "POST /etl/job".to_string()
        ],
        "the branch exists before the run that writes it"
    );
    let submitted = log
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.target == "/etl/job")
        .unwrap()
        .body
        .clone();
    assert_eq!(submitted["jobId"], json!(1324));
    assert_eq!(submitted["writeBranch"], json!("try-1"));
    assert_eq!(
        submitted["writeBranchTables"],
        json!([{"warehouseName": "Manufacturing", "databaseName": "mfg_ops", "tableName": "oee_daily"}])
    );
    assert_eq!(
        submitted["code"],
        json!("print('changed')\n"),
        "the local file, not the saved code"
    );

    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        shown["run"],
        json!({"jobIdentifier": "run-1", "status": "Success"})
    );
    assert_eq!(shown["code"], json!("build-oee.py"));
    let table = &shown["tables"][0];
    assert_eq!(table["table"], json!("mfg_ops.oee_daily"));
    assert_eq!(table["changed"], json!(true));
    assert_eq!(table["from"]["snapshotId"], json!("11"));
    assert_eq!(table["to"]["snapshotId"], json!("13"));
    assert_eq!(
        shown["next"],
        json!([
            "gz jobs publish 1324 --project 1138 --branch try-1",
            "gz jobs discard 1324 --project 1138 --branch try-1",
        ])
    );
}

#[test]
fn without_a_synced_file_the_saved_code_runs() {
    let dir = temp_dir("saved");
    let (base, log) = stub_providers(Stub::default());

    let out = gz(&dir, &base, TRY);

    assert!(out.status.success(), "{}", plain(&out.stderr));
    let submitted = log
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.target == "/etl/job")
        .unwrap()
        .body
        .clone();
    assert!(submitted.get("code").is_none(), "{submitted}");
    assert_eq!(submitted["writeBranch"], json!("try-1"));
}

#[test]
fn an_etl_provider_without_branch_runs_is_refused_before_anything_changes() {
    let dir = temp_dir("old");
    let (base, log) = stub_providers(Stub {
        old_etl: true,
        ..Stub::default()
    });

    let out = gz(&dir, &base, TRY);

    assert!(!out.status.success());
    let stderr = said(&out);
    assert!(
        stderr.contains("the ETL provider doesn't support writeBranch yet: it needs the lakehouse and ETL provider updates deployed"),
        "{stderr}"
    );
    assert!(changes(&log).is_empty(), "{:?}", changes(&log));
}

#[test]
fn a_refused_run_removes_the_branches_it_made() {
    let dir = temp_dir("refused");
    let (base, log) = stub_providers(Stub {
        refuse_run: true,
        ..Stub::default()
    });

    let out = gz(&dir, &base, TRY);

    assert!(!out.status.success());
    let stderr = said(&out);
    assert!(
        stderr.contains("mfg.downtime has no branch try-1"),
        "{stderr}"
    );
    assert!(stderr.contains("gz removed branch try-1 again"), "{stderr}");
    assert_eq!(
        changes(&log),
        [
            format!("PUT {TABLE}/write-branches/try-1"),
            "POST /etl/job".to_string(),
            format!("DELETE {TABLE}/branches/try-1"),
        ]
    );
}

#[test]
fn publish_moves_main_on_each_table_with_the_branch() {
    let dir = temp_dir("publish");
    let (base, log) = stub_providers(Stub::default());

    let out = gz(
        &dir,
        &base,
        &[
            "-o",
            "json",
            "jobs",
            "publish",
            "1324",
            "--project",
            "1138",
            "--branch",
            "try-1",
            "--yes",
        ],
    );

    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert_eq!(
        changes(&log),
        [format!("POST {TABLE}/write-branches/try-1/publish")]
    );
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        shown["tables"][0]["table"],
        json!("Manufacturing.mfg_ops.oee_daily")
    );
    assert_eq!(shown["tables"][0]["currentSnapshotId"], json!("13"));
}

#[test]
fn publish_asks_first_and_refuses_without_an_answer() {
    let dir = temp_dir("ask");
    let (base, log) = stub_providers(Stub::default());

    let out = gz(
        &dir,
        &base,
        &[
            "jobs",
            "publish",
            "1324",
            "--project",
            "1138",
            "--branch",
            "try-1",
        ],
    );

    assert!(!out.status.success());
    assert!(
        plain(&out.stderr).contains("pass --yes"),
        "{}",
        plain(&out.stderr)
    );
    assert!(changes(&log).is_empty());
}

#[test]
fn publish_refuses_when_main_moved_on_without_touching_any_table() {
    let dir = temp_dir("moved");
    let (base, log) = stub_providers(Stub {
        main_moved: true,
        ..Stub::default()
    });

    let out = gz(
        &dir,
        &base,
        &[
            "jobs",
            "publish",
            "1324",
            "--project",
            "1138",
            "--branch",
            "try-1",
            "--yes",
        ],
    );

    assert!(!out.status.success());
    let stderr = plain(&out.stderr);
    assert!(stderr.contains("nothing was published"), "{stderr}");
    assert!(stderr.contains("main has moved on (now 12)"), "{stderr}");
    assert!(changes(&log).is_empty());
}

#[test]
fn discard_drops_the_branch_and_leaves_main() {
    let dir = temp_dir("discard");
    let (base, log) = stub_providers(Stub::default());

    let out = gz(
        &dir,
        &base,
        &[
            "-o",
            "json",
            "jobs",
            "discard",
            "1324",
            "--project",
            "1138",
            "--branch",
            "try-1",
        ],
    );

    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert_eq!(changes(&log), [format!("DELETE {TABLE}/branches/try-1")]);
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["dropped"], json!(["Manufacturing.mfg_ops.oee_daily"]));
}

#[test]
fn try_prints_the_logs_the_change_per_table_and_what_to_do_next() {
    let dir = temp_dir("text");
    let (base, _log) = stub_providers(Stub::default());

    let out = gz(&dir, &base, &TRY[2..]);

    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    for part in [
        "wrote 5 rows",
        "mfg_ops.oee_daily · Manufacturing",
        "Next:",
        "  gz jobs publish 1324 --project 1138 --branch try-1",
        "  gz jobs discard 1324 --project 1138 --branch try-1",
    ] {
        assert!(stdout.contains(part), "missing {part:?} in\n{stdout}");
    }
}

const PUBLISH: &[&str] = &[
    "jobs",
    "publish",
    "1324",
    "--project",
    "1138",
    "--branch",
    "try-1",
    "--yes",
];

#[test]
fn publish_stops_before_any_table_when_the_quality_gate_refuses_the_branch() {
    let dir = temp_dir("gate-failed");
    let (base, log) = stub_providers(Stub {
        gate: Gate::Failed,
        ..Stub::default()
    });

    let out = gz(&dir, &base, PUBLISH);

    assert!(!out.status.success());
    let stderr = plain(&out.stderr);
    assert!(
        stderr.contains("nothing was published: the quality gate refuses 1 table(s):\n  Manufacturing.mfg_ops.oee_daily: branch try-1 failed its quality checks (ranges, missing_values)\n"),
        "{stderr}"
    );
    assert!(changes(&log).is_empty());
}

#[test]
fn publish_goes_ahead_with_a_passing_verdict_on_the_head() {
    let dir = temp_dir("gate-passed");
    let (base, log) = stub_providers(Stub {
        gate: Gate::Passed,
        ..Stub::default()
    });

    let out = gz(&dir, &base, PUBLISH);

    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert!(log
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.target == format!("{TABLE}/quality")));
    assert_eq!(
        changes(&log),
        [format!("POST {TABLE}/write-branches/try-1/publish")]
    );
}

#[test]
fn a_quality_refusal_from_the_server_shows_its_message() {
    let dir = temp_dir("gate-409");
    let (base, _log) = stub_providers(Stub {
        refuse_publish: true,
        ..Stub::default()
    });

    let out = gz(&dir, &base, PUBLISH);

    assert!(!out.status.success());
    let stderr = plain(&out.stderr);
    assert!(
        stderr.contains("Manufacturing.mfg_ops.oee_daily was not published: HTTP 409 Conflict: Branch try-1 of mfg_ops.oee_daily has no passing quality verdict for its head 13: run runTableDataQualityChecks on it. Published already: none"),
        "{stderr}"
    );
}
