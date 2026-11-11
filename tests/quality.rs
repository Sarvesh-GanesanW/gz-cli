//! `gz quality` against a stub lakehouse provider: the branch verdicts
//! table, turning the gate on and off, a provider without the routes, and
//! history read through a `gz sql` session under the scan limit.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const TABLE: &str = "/lakehouse/tables/Lineage/sales/orders";
const FLASK_404: &str = "<!doctype html>\n<html lang=en>\n<title>404 Not Found</title>\n<h1>Not Found</h1>\n<p>The requested URL was not found on the server.</p>";

#[derive(Clone, Copy, PartialEq)]
enum Provider {
    /// Has the quality routes; `orders` has the branches below.
    Current,
    /// Has the routes; `orders` has no branch besides main.
    NoBranches,
    /// Predates the quality routes.
    Older,
}

fn quality(provider: Provider) -> Value {
    let branches = if provider == Provider::NoBranches {
        json!([])
    } else {
        json!([
            {"name": "try-bad", "headSnapshotId": "17", "current": true, "verdict": {
                "version": 1, "snapshotId": "17", "passed": false, "failOn": "error",
                "worstSeverity": "error", "failedChecks": ["ranges", "missing_values"],
                "checkedAt": "2026-11-11T11:05:00Z", "runId": "r2", "qualipilotVersion": "3.6.0",
            }},
            {"name": "try-fresh", "headSnapshotId": "13", "verdict": null, "current": false},
            {"name": "try-moved", "headSnapshotId": "16", "current": false, "verdict": {
                "version": 1, "snapshotId": "15", "passed": true, "failOn": "error",
                "worstSeverity": "ok", "failedChecks": [], "checkedAt": "2026-11-11T09:00:00Z",
                "runId": "r1", "qualipilotVersion": "3.6.0",
            }},
        ])
    };
    json!({"status": 200, "response": {"required": true, "branches": branches}})
}

/// Big enough to trip a 1 KB scan limit.
fn results_versions() -> Value {
    json!({"status": 200, "response": {
        "currentSnapshotId": "7",
        "snapshots": [{"snapshotId": "7", "committedAt": "2026-11-11T00:00:00+00:00",
            "operation": "append", "summary": {
                "total-files-size": "5000000", "total-data-files": "3", "total-records": "900",
            }}],
        "branches": [], "tags": [],
    }})
}

fn reply(provider: Provider, request: &Recorded) -> Option<Value> {
    let path = request.target.split('?').next().unwrap_or_default();
    if provider == Provider::Older && path.starts_with(TABLE) {
        return None;
    }
    let reply = match (request.method.as_str(), path) {
        ("GET", p) if p == format!("{TABLE}/quality") => quality(provider),
        ("PUT", p) if p == format!("{TABLE}/quality-gate") => {
            json!({"status": 200, "response": {"required": request.body["required"]}})
        }
        ("GET", "/lakehouse/warehouse") => json!({"status": 200, "response": [
            {"id": 1108, "name": "GZWarehousePROD"},
            {"id": 1114, "name": "Lineage"},
        ]}),
        ("GET", "/rte/compute") => json!([{"Id": 1000, "Name": "mini", "Description": "Mini"}]),
        ("GET", "/lakehouse/tables/Lineage/dq/results/versions") => results_versions(),
        ("POST", "/lakehouse/startsession") => {
            json!({"status": 200, "response": {"sessionId": "s1"}})
        }
        ("POST", "/lakehouse/submitquery") => json!({"status": 200, "response": {
            "status": "SUCCEEDED",
            "schema": [{"name": "checked_at", "type": "timestamp"}, {"name": "check_name", "type": "string"},
                       {"name": "severity", "type": "string"}, {"name": "status", "type": "string"}],
            "data": [{"checked_at": "2026-11-11 11:05:00", "check_name": "ranges",
                      "severity": "error", "status": "completed"}],
            "pageNumber": 1, "hasMore": false, "totalRecords": 1,
        }, "queryExecutionId": "q1"}),
        ("POST", "/lakehouse/stopsession") => json!({"status": 200, "response": {}}),
        _ => return None,
    };
    Some(reply)
}

fn stub(provider: Provider) -> (String, Arc<Mutex<Vec<Recorded>>>) {
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
            match reply(provider, &request) {
                Some(body) => respond(&mut stream, "200 OK", "application/json", &body.to_string()),
                None => respond(&mut stream, "404 NOT FOUND", "text/html", FLASK_404),
            }
        }
    });
    (base, log)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-quality-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "quality-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .args(args)
        .output()
        .unwrap()
}

fn requests(log: &Arc<Mutex<Vec<Recorded>>>) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .map(|r| format!("{} {}", r.method, r.target))
        .collect()
}

#[test]
fn show_lists_each_branch_with_its_verdict_and_whether_it_is_for_the_head() {
    let dir = temp_dir("show");
    let (base, log) = stub(Provider::Current);

    let out = gz(&dir, &base, &["quality", "show", "Lineage.sales.orders"]);

    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert_eq!(requests(&log), [format!("GET {TABLE}/quality")]);
    let stdout = plain(&out.stdout);
    // Cells only, without the table's borders and padding.
    let row = |first: &str| {
        stdout
            .lines()
            .map(|line| {
                line.split(['│', '┆'])
                    .map(str::trim)
                    .filter(|cell| !cell.is_empty())
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .find(|cells| cells.starts_with(first))
            .unwrap_or_else(|| panic!("no row for {first} in\n{stdout}"))
    };
    assert_eq!(
        row("branch"),
        "branch | verdict | snapshot | failed checks | checked"
    );
    assert_eq!(
        row("try-bad"),
        "try-bad | failed | current | ranges, missing_values | 2026-11-11 11:05 UTC"
    );
    assert_eq!(row("try-fresh"), "try-fresh | none | — | — | —");
    assert_eq!(
        row("try-moved"),
        "try-moved | passed | stale | — | 2026-11-11 09:00 UTC"
    );
    assert!(
        plain(&out.stderr).contains("publishing a branch needs a passing verdict"),
        "{}",
        plain(&out.stderr)
    );
}

#[test]
fn show_as_json_prints_the_providers_reply() {
    let dir = temp_dir("json");
    let (base, _log) = stub(Provider::Current);

    let out = gz(
        &dir,
        &base,
        &["-o", "json", "quality", "show", "Lineage.sales.orders"],
    );

    assert!(out.status.success(), "{}", plain(&out.stderr));
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown, quality(Provider::Current));
}

#[test]
fn show_without_branches_says_how_a_branch_gets_a_verdict() {
    let dir = temp_dir("empty");
    let (base, _log) = stub(Provider::NoBranches);

    let out = gz(&dir, &base, &["quality", "show", "Lineage.sales.orders"]);

    assert!(out.status.success(), "{}", plain(&out.stderr));
    let said = plain(&out.stderr);
    assert!(said.contains("has no branches besides main"), "{said}");
    assert!(
        said.contains("`gz jobs try` run calls runTableDataQualityChecks"),
        "{said}"
    );
}

#[test]
fn an_older_provider_is_named_as_the_reason() {
    let dir = temp_dir("older");
    let (base, _log) = stub(Provider::Older);

    for args in [
        &["quality", "show", "Lineage.sales.orders"][..],
        &["quality", "require", "Lineage.sales.orders"][..],
    ] {
        let out = gz(&dir, &base, args);
        assert!(!out.status.success());
        let said = plain(&out.stderr);
        assert!(
            said.contains("needs the lakehouse provider update deployed"),
            "{said}"
        );
    }
}

#[test]
fn require_turns_the_gate_on_and_off() {
    let dir = temp_dir("require");
    let (base, log) = stub(Provider::Current);

    let on = gz(&dir, &base, &["quality", "require", "Lineage.sales.orders"]);
    let off = gz(
        &dir,
        &base,
        &["quality", "require", "Lineage.sales.orders", "--off"],
    );

    assert!(on.status.success(), "{}", plain(&on.stderr));
    assert!(off.status.success(), "{}", plain(&off.stderr));
    let log = log.lock().unwrap();
    let bodies: Vec<(String, Value)> = log
        .iter()
        .map(|r| (format!("{} {}", r.method, r.target), r.body.clone()))
        .collect();
    assert_eq!(
        bodies,
        [
            (
                format!("PUT {TABLE}/quality-gate"),
                json!({"required": true})
            ),
            (
                format!("PUT {TABLE}/quality-gate"),
                json!({"required": false})
            ),
        ]
    );
    assert!(plain(&off.stdout).contains("not required to publish"));
}

#[test]
fn history_queries_the_results_table_on_its_warehouse() {
    let dir = temp_dir("history");
    let (base, log) = stub(Provider::Current);

    let out = gz(
        &dir,
        &base,
        &[
            "quality",
            "history",
            "Lineage.sales.orders",
            "--results",
            "Lineage.dq.results",
            "--failed",
            "--limit",
            "5",
            "--compute",
            "mini",
        ],
    );

    assert!(out.status.success(), "{}", plain(&out.stderr));
    let stdout = plain(&out.stdout);
    assert!(stdout.contains("ranges"), "{stdout}");
    let log = log.lock().unwrap();
    let submit = log
        .iter()
        .find(|r| r.target == "/lakehouse/submitquery")
        .expect("a query was submitted");
    assert_eq!(submit.body["catalog"], json!("Lineage"));
    assert_eq!(
        submit.body["query"],
        json!(
            "SELECT checked_at, check_name, severity, status, source_ref, source_version, row_count, round(duration_seconds, 2) AS duration_seconds, error, run_id\n\
             FROM `dq`.`results`\n\
             WHERE (lower(source) = lower('sales.orders') OR right(lower(source), 13) = lower('.sales.orders'))\n  \
             AND (severity <> 'ok' OR status = 'failed')\n\
             ORDER BY checked_at DESC\n\
             LIMIT 5"
        )
    );
    assert!(log.iter().any(|r| r.target == "/lakehouse/stopsession"));
}

#[test]
fn history_is_refused_over_the_scan_limit_before_a_session_starts() {
    let dir = temp_dir("scan");
    let (base, log) = stub(Provider::Current);

    let out = gz(
        &dir,
        &base,
        &[
            "quality",
            "history",
            "Lineage.sales.orders",
            "--results",
            "Lineage.dq.results",
            "--max-scan",
            "1KB",
        ],
    );

    assert!(!out.status.success());
    assert!(
        plain(&out.stderr).contains("--max-scan"),
        "{}",
        plain(&out.stderr)
    );
    assert!(
        !requests(&log).iter().any(|r| r.contains("startsession")),
        "{:?}",
        requests(&log)
    );
}

#[test]
fn history_refuses_a_table_name_that_could_change_the_sql() {
    let dir = temp_dir("inject");
    let (base, log) = stub(Provider::Current);

    let out = gz(
        &dir,
        &base,
        &[
            "quality",
            "history",
            "Lineage.sales.orders')--",
            "--results",
            "Lineage.dq.results",
        ],
    );

    assert!(!out.status.success());
    assert!(
        plain(&out.stderr).contains("may only use letters, digits"),
        "{}",
        plain(&out.stderr)
    );
    assert!(requests(&log).is_empty());
}
