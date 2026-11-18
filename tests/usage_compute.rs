//! `gz usage compute` against a stub: the consumption-metrics queries it
//! sends (summaries, groups, run pages), the bill request, and the report.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

fn query_has(target: &str, pair: &str) -> bool {
    target
        .split_once('?')
        .is_some_and(|(_, query)| query.split('&').any(|p| p == pair))
}

fn summary(runs: u64, cost: f64) -> Value {
    json!({"executionCount": runs, "sessionCount": runs, "totalCostUsd": cost,
           "totalCpuCoreHours": 0.5, "totalMemoryGiBHours": 2.0,
           "observedCostUsd": cost, "observedCpuCoreHours": 0.5, "observedMemoryGiBHours": 2.0})
}

fn consumption(target: &str) -> Value {
    if query_has(target, "groupBy=project") {
        return json!({"items": [{"groupType": "project", "projectId": 1510, "projectName": "Invoices",
            "summary": summary(3, 0.9)}], "pagination": {"total": 1}});
    }
    if query_has(target, "groupBy=job") {
        return json!({"items": [{"groupType": "job", "jobId": 2125, "jobName": "ChallanReceiptBulk",
            "projectName": "Invoices", "summary": summary(3, 0.9)}], "pagination": {"total": 1}});
    }
    if query_has(target, "groupBy=dataset") {
        return json!({"items": [], "pagination": {"total": 0}});
    }
    if query_has(target, "limit=1") {
        return json!({"items": [], "summary": summary(3, 0.9), "pagination": {"total": 3}});
    }
    json!({"items": [
        {"executionId": "e-dear", "jobName": "ChallanReceiptBulk", "collectionStatus": "complete",
         "usage": {"cost": {"totalUsd": 0.7}, "cpu": {"coreHours": 0.3}, "memory": {"gibHours": 1.5}}},
        {"executionId": "e-cheap", "jobName": "ChallanReceiptBulk", "collectionStatus": "complete",
         "usage": {"cost": {"totalUsd": 0.2}, "cpu": {"coreHours": 0.2}, "memory": {"gibHours": 0.5}}}
    ], "pagination": {"total": 2}})
}

fn reply(method: &str, target: &str) -> Option<Value> {
    let path = target.split('?').next().unwrap_or_default();
    Some(match (method, path) {
        ("GET", "/etl/consumption-metrics") => consumption(target),
        ("POST", "/admin/client/bill") => json!({"month": 11, "year": 2026, "totalCost": 50.0,
            "bill": [{"name": "vCPU", "unit": "GB-Hour", "consumption": "180.67", "cost": "45.17"}]}),
        ("GET", "/etl/etlProjects") => json!([{"Id": 1510, "Name": "Invoices"}]),
        ("GET", "/etl/etlJobs/1510/jobs") => json!([{"Id": 2125, "Name": "ChallanReceiptBulk"}]),
        ("GET", "/jupyter/1510/notebook") => json!({"success": true, "notebooks": []}),
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
            match reply(&request.method, &request.target) {
                Some(body) => respond(&mut stream, "200 OK", "application/json", &body.to_string()),
                None => respond(&mut stream, "404 Not Found", "application/json", "{}"),
            }
        }
    });
    (base, log)
}

fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-compute-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "compute-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn site_report_asks_summaries_groups_runs_and_the_bill() {
    let (base, log) = stub_server();
    let out = gz(
        &folder("site"),
        &base,
        &["usage", "compute", "--since", "7d"],
    );
    let text = plain(&out.stdout);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    for part in [
        "Job runs",
        "$0.90",
        "Top projects",
        "Invoices",
        "ChallanReceiptBulk",
        "$0.70 · 0.30 core-h · 1.50 GiB-h · run e-dear",
        "180.67 GB-Hour of pod memory · $45.17",
        "no SQL session kept by gz",
    ] {
        assert!(text.contains(part), "missing {part:?} in\n{text}");
    }
    let dear = text.find("run e-dear").unwrap();
    let cheap = text.find("run e-cheap").unwrap();
    assert!(dear < cheap, "costliest run first:\n{text}");

    let log = log.lock().unwrap();
    let metrics: Vec<&Recorded> = log
        .iter()
        .filter(|r| r.target.starts_with("/etl/consumption-metrics"))
        .collect();
    assert!(metrics
        .iter()
        .all(|r| r.method == "GET" && r.target.contains("from=") && r.target.contains("&to=")));
    assert!(metrics
        .iter()
        .any(|r| query_has(&r.target, "workloadType=job") && query_has(&r.target, "limit=1")));
    assert!(metrics
        .iter()
        .any(|r| query_has(&r.target, "workloadType=dataset") && query_has(&r.target, "limit=1")));
    assert!(
        metrics
            .iter()
            .any(|r| query_has(&r.target, "groupBy=job") && query_has(&r.target, "projectId=1510")),
        "job groups need their project"
    );
    let bills: Vec<&Recorded> = log
        .iter()
        .filter(|r| r.target == "/admin/client/bill")
        .collect();
    assert!(!bills.is_empty());
    assert_eq!(bills[0].body["siteName"], "acme");
    assert!(bills[0].body["year"].is_i64() && bills[0].body["month"].is_i64());
}

#[test]
fn a_job_report_filters_by_the_job_and_skips_site_wide_parts() {
    let (base, log) = stub_server();
    let out = gz(
        &folder("job"),
        &base,
        &[
            "usage",
            "compute",
            "--job",
            "ChallanReceiptBulk",
            "-o",
            "json",
        ],
    );
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["job"]["id"], 2125);
    assert!(value["bill"].is_null() && value["topProjects"].is_null());
    assert_eq!(value["topRuns"]["runs"][0]["costUsd"], 0.7);
    let log = log.lock().unwrap();
    assert!(log
        .iter()
        .filter(|r| r.target.starts_with("/etl/consumption-metrics"))
        .all(|r| query_has(&r.target, "jobId=2125")));
    assert!(!log.iter().any(|r| r.target == "/admin/client/bill"));
}
