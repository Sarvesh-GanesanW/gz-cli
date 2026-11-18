//! `gz status` against a stub of every backend it reads: the request
//! shapes, a section that fails standing alone, and the last look kept per
//! profile.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

fn reply(method: &str, path: &str) -> Option<Value> {
    Some(match (method, path) {
        ("GET", "/etl/consumption-metrics") => json!({
            "items": [
                {"status": "failed", "jobName": "nightly-load", "projectName": "ETL", "jobId": 7,
                 "projectId": 1000, "executionId": "run-f1", "startedAt": "2026-11-18T08:00:00Z",
                 "completedAt": "2026-11-18T08:01:00Z"}
            ],
            "pagination": {"limit": 100, "offset": 0, "total": 1}
        }),
        ("POST", "/etl/jobStatus") => json!({"success": true, "data": [
            {"jobstatus": "Failed", "output": "Py4JJavaError: Table ETL.sales not found\nTraceback", "sessionId": "s"}
        ]}),
        ("GET", "/v1/approval-requests") => json!({"items": [
            {"id": 271, "agentVersion": 3, "requestedBy": "sarvesh",
             "requestedAt": "2026-11-18T07:00:00Z", "definitionSnapshot": {"name": "pair-judge"}}
        ], "limit": 100, "offset": 0}),
        ("GET", "/v1/tool-approvals") => json!({"total": 0, "items": []}),
        ("GET", "/chat/conversations") => json!({"conversations": [
            {"conversationId": "c1", "conversationName": "Clean orders"},
            {"conversationId": "c2", "conversationName": "Other"}
        ]}),
        ("GET", "/chat/conversations/c1") => json!({"conversationId": "c1", "messages": [],
            "pendingApproval": {"runId": "run-7", "interruptId": "int-3", "interruptName": "delete_dataset"}}),
        ("GET", "/chat/conversations/c2") => json!({"conversationId": "c2", "messages": []}),
        ("GET", "/etl/etlProjects") => {
            json!([{"Id": 1000, "Name": "ETL"}, {"Id": 1001, "Name": "Empty"}])
        }
        ("GET", "/jupyter/1000/notebook") => {
            json!({"success": true, "notebooks": [{"id": 55, "name": "explore"}]})
        }
        ("GET", "/jupyter/1001/notebook") => json!({"success": true, "notebooks": []}),
        ("POST", "/jupyter/getRunningNotebooks") => json!({"success": true, "data": [
            {"notebookId": "55", "status": "Running", "podId": "p1", "rteName": "gz-rte-sklearn:1.0.0"}
        ]}),
        ("GET", "/chat/metrics") => json!({"username": "me", "costUsd": {"total": "3.10"},
            "userMetrics": [{"username": "me", "totalCostUsd": "1.25"}],
            "budget": {"usedCents": 0, "limitCents": null, "usedUsd": "0", "windowDays": 30}}),
        ("GET", "/repos/acme/gz/releases/latest") => json!({"tag_name": "v99.0.0"}),
        _ => return None,
    })
}

fn stub_server() -> (String, Arc<Mutex<Vec<Recorded>>>) {
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
            match reply(&request.method, path) {
                Some(body) => respond(&mut stream, "200 OK", "application/json", &body.to_string()),
                None => respond(
                    &mut stream,
                    "500 Internal Server Error",
                    "application/json",
                    r#"{"detail": "stub has no such route"}"#,
                ),
            }
        }
    });
    (base, log)
}

fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-status-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "status-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .env("GZ_UPDATE_API", base)
        .env("GZ_UPDATE_REPO", "acme/gz")
        .args(args)
        .output()
        .unwrap()
}

fn find<'a>(log: &'a [Recorded], method: &str, path: &str) -> Vec<&'a Recorded> {
    log.iter()
        .filter(|r| r.method == method && r.target.split('?').next() == Some(path))
        .collect()
}

#[test]
fn shows_every_section_with_the_backends_own_shapes() {
    let (base, log) = stub_server();
    let dir = folder("all");
    let out = gz(&dir, &base, &["status"]);
    let text = plain(&out.stdout);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    for part in [
        "since 20",
        "(the last day; first look)",
        "nightly-load",
        "reason: Py4JJavaError: Table ETL.sales not found",
        "gz fix 7 --project 1000 --run run-f1",
        "pair-judge v3",
        "chat waiting to delete_dataset · gz chat approve run-7 --interrupt int-3",
        "explore",
        "Running · project ETL · gz-rte-sklearn:1.0.0",
        "$1.25",
        "v99.0.0 is out",
    ] {
        assert!(text.contains(part), "missing {part:?} in\n{text}");
    }

    let log = log.lock().unwrap();
    let runs = find(&log, "GET", "/etl/consumption-metrics");
    assert!(runs.iter().any(|r| r.target.contains("status=failed")
        && r.target.contains("workloadType=job")
        && r.target.contains("from=")));
    assert_eq!(
        find(&log, "POST", "/etl/jobStatus")[0].body,
        json!({"jobIdentifier": "run-f1"})
    );
    assert!(find(&log, "GET", "/v1/approval-requests")[0]
        .target
        .contains("status=waiting_for_approval"));
    assert!(find(&log, "GET", "/chat/conversations")[0]
        .target
        .contains("assistantMode=chat"));
    let running = find(&log, "POST", "/jupyter/getRunningNotebooks");
    assert_eq!(running.len(), 1, "a project without notebooks isn't asked");
    assert_eq!(running[0].body, json!({"projectId": 1000}));
    assert!(find(&log, "GET", "/chat/metrics")[0]
        .target
        .contains("from="));
}

#[test]
fn a_failing_section_says_so_and_the_rest_still_show() {
    let (base, _log) = stub_server();
    let dir = folder("broken");
    // The stub has no route for this repo, so the release check fails.
    let out = Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(&dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "status-test")
        .env("GZ_HOST", &base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .env("GZ_UPDATE_API", &base)
        .env("GZ_UPDATE_REPO", "acme/missing")
        .args(["status", "-o", "json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value["release"]["error"].is_string(), "{value}");
    assert_eq!(value["failedRuns"]["total"], 1);
    assert_eq!(value["pausedChats"]["pending"][0]["conversationId"], "c1");
}

#[test]
fn the_last_look_moves_unless_since_is_given() {
    let (base, _log) = stub_server();
    let dir = folder("look");
    let state = dir.join("config/gz/status_state.json");
    let out = gz(&dir, &base, &["status", "--since", "2h"]);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert!(!state.exists(), "--since doesn't move the last look");

    let out = gz(&dir, &base, &["status"]);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert!(saved["last_look_ms"]["status-test"].is_i64(), "{saved}");

    let out = gz(&dir, &base, &["status"]);
    assert!(
        plain(&out.stdout).contains("since your last look"),
        "{}",
        plain(&out.stdout)
    );
}
