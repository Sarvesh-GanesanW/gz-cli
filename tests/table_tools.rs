//! `gz table profile`, `gz table rollback` and `gz jobs lineage graph`
//! against a stub provider: the routes each calls, what it sends, and what
//! it prints.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

fn snapshot(id: u64, parent: Option<u64>, day: u32, total: u64) -> Value {
    json!({
        "snapshotId": id.to_string(),
        "parentSnapshotId": parent.map(|p| p.to_string()),
        "committedAt": format!("2026-09-{day:02}T09:00:00+00:00"),
        "operation": "append",
        "summary": {
            "added-records": "10",
            "total-records": total.to_string(),
            "total-data-files": "1",
            "total-files-size": (total * 1000).to_string(),
            "app-name": "load_orders",
        },
    })
}

/// main 101 → 104, and the branch `audit` (201) off 102.
fn versions() -> Value {
    json!({
        "currentSnapshotId": "104",
        "snapshots": [
            snapshot(201, Some(102), 5, 25),
            snapshot(104, Some(103), 4, 40),
            snapshot(103, Some(102), 3, 30),
            snapshot(102, Some(101), 2, 20),
            snapshot(101, None, 1, 10),
        ],
        "branches": [
            {"name": "main", "snapshotId": "104"},
            {"name": "audit", "snapshotId": "201"},
        ],
        "tags": [],
    })
}

/// What the session answers: the columns, the aggregates, the top values.
fn query_reply(query: &str) -> Value {
    let page = |schema: Value, data: Value| {
        json!({"status": 200, "queryExecutionId": "q1", "response": {
            "schema": schema, "data": data, "pageNumber": 1, "hasMore": false,
        }})
    };
    if query.ends_with("LIMIT 0") {
        return page(
            json!([{"name": "amount", "type": "double"}, {"name": "region", "type": "string"}]),
            json!([]),
        );
    }
    if query.contains("stack(") {
        return page(
            json!([{"name": "col", "type": "int"}, {"name": "value", "type": "string"}, {"name": "n", "type": "long"}]),
            json!([{"col": 1, "value": "eu", "n": 30}, {"col": 0, "value": "9.5", "n": 4}]),
        );
    }
    page(
        json!([{"name": "total_rows", "type": "long"}]),
        json!([{"total_rows": 40, "c0_values": 38, "c0_distinct": 31, "c0_min": 1.0,
                "c0_max": 99.0, "c0_mean": 12.5, "c1_values": 40, "c1_distinct": 3,
                "c1_minlen": 2, "c1_maxlen": 4}]),
    )
}

fn lineage_graph() -> Value {
    json!({"graph": {
        "centerAssetId": 7,
        "assets": [
            {"assetId": 6, "assetType": "OBJECT", "displayName": "raw/orders.csv", "qualifiedName": "raw/orders.csv"},
            {"assetId": 7, "assetType": "TABLE", "displayName": "sales.orders", "qualifiedName": "sales.orders"},
            {"assetId": 8, "assetType": "TABLE", "displayName": "sales.daily", "qualifiedName": "sales.daily"},
        ],
        "relations": [
            {"sourceAssetId": 6, "targetAssetId": 7, "jobVersionId": 1},
            {"sourceAssetId": 7, "targetAssetId": 8, "jobVersionId": 2},
        ],
        "jobVersions": [
            {"jobVersionId": 1, "jobName": "LoadOrders", "workloadType": "job"},
            {"jobVersionId": 2, "jobName": "Daily", "workloadType": "job"},
        ],
        "dashboards": [],
        "maskedSummary": {"assets": 0},
    }})
}

fn stub() -> (String, Arc<Mutex<Vec<Recorded>>>) {
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
            let (status, reply) = match (request.method.as_str(), path) {
                ("GET", "/lakehouse/warehouse") => (
                    "200 OK",
                    json!({"status": 200, "response": [{"id": 1114, "name": "Lineage"}]}),
                ),
                ("GET", "/rte/compute") => (
                    "200 OK",
                    json!([{"Id": 1000, "Name": "mini", "Description": "Mini"}]),
                ),
                ("GET", "/lakehouse/tables/Lineage/sales/orders/versions") => {
                    ("200 OK", json!({"status": 200, "response": versions()}))
                }
                ("POST", "/lakehouse/tables/Lineage/sales/orders/rollback") => {
                    if request.body["snapshotId"] == "101" {
                        (
                            "409 Conflict",
                            json!({"status": 409, "response": {
                            "message": "main has moved since the rollback was previewed"}}),
                        )
                    } else {
                        (
                            "200 OK",
                            json!({"status": 200, "response": {
                                "currentSnapshotId": request.body["snapshotId"],
                                "previousSnapshotId": "104",
                            }}),
                        )
                    }
                }
                ("POST", "/lakehouse/startsession") => (
                    "200 OK",
                    json!({"status": 200, "response": {"sessionId": "s1"}}),
                ),
                ("POST", "/lakehouse/submitquery") => (
                    "200 OK",
                    query_reply(request.body["query"].as_str().unwrap_or_default()),
                ),
                ("POST", "/lakehouse/stopsession") => {
                    ("200 OK", json!({"status": 200, "response": {}}))
                }
                ("GET", "/etl/lineage/connections") => {
                    ("200 OK", json!({"connections": [{"connectionId": 5}]}))
                }
                ("GET", "/etl/lineage/assets") => (
                    "200 OK",
                    json!({"assets": [
                        {"assetId": 7, "assetType": "TABLE", "displayName": "sales.orders",
                         "qualifiedName": "sales.orders", "bindings": [{"connectionName": "Lake"}]},
                    ]}),
                ),
                ("GET", "/etl/lineage/assets/7") => ("200 OK", lineage_graph()),
                _ => ("404 Not Found", json!({})),
            };
            respond(&mut stream, status, "application/json", &reply.to_string());
        }
    });
    (base, log)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-tabletools-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gz"));
    cmd.env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "tabletools-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(Stdio::null());
    cmd
}

fn requests(log: &Arc<Mutex<Vec<Recorded>>>) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{} {}",
                r.method,
                r.target.split('?').next().unwrap_or_default()
            )
        })
        .collect()
}

fn rollback_bodies(log: &Arc<Mutex<Vec<Recorded>>>) -> Vec<Value> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|r| r.target.ends_with("/rollback"))
        .map(|r| r.body.clone())
        .collect()
}

#[test]
fn rollback_previews_then_moves_main_naming_the_main_it_previewed() {
    let dir = temp_dir("rollback");
    let (base, log) = stub();
    let out = gz(&dir, &base)
        .args([
            "table",
            "rollback",
            "Lineage.sales.orders",
            "--to",
            "102",
            "--yes",
        ])
        .output()
        .unwrap();
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    for part in [
        "Roll back sales.orders · Lineage",
        "  main now  104 · 2026-09-04 09:00 UTC · current, branch main",
        "  back to   102 · 2026-09-02 09:00 UTC",
        "Undoes 2 commit(s)",
        "Records  40 → 20 (−20)",
    ] {
        assert!(stdout.contains(part), "missing {part:?} in\n{stdout}");
    }
    assert!(
        stdout.contains("main of sales.orders is now at snapshot 102 (was 104). To undo: gz table rollback Lineage.sales.orders --to 104 --allow-non-ancestor"),
        "{stdout}"
    );
    assert_eq!(
        rollback_bodies(&log),
        [json!({"snapshotId": "102", "expectedSnapshotId": "104", "allowNonAncestor": false})]
    );
    assert!(
        !requests(&log).iter().any(|r| r.contains("session")),
        "the preview needs no session without --rows"
    );
}

#[test]
fn rollback_without_a_terminal_or_yes_changes_nothing() {
    let dir = temp_dir("rollback-ask");
    let (base, log) = stub();
    let out = gz(&dir, &base)
        .args([
            "table",
            "rollback",
            "sales.orders",
            "--warehouse",
            "Lineage",
            "--to",
            "previous",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        plain(&out.stderr).contains("pass --yes"),
        "{}",
        plain(&out.stderr)
    );
    assert!(rollback_bodies(&log).is_empty());
}

#[test]
fn rollback_to_another_branch_needs_the_explicit_flag() {
    let dir = temp_dir("rollback-branch");
    let (base, log) = stub();
    let refused = gz(&dir, &base)
        .args([
            "table",
            "rollback",
            "Lineage.sales.orders",
            "--to",
            "audit",
            "--yes",
        ])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let stderr = plain(&refused.stderr);
    assert!(
        stderr.contains("snapshot 201 is not an earlier snapshot of main"),
        "{stderr}"
    );
    assert!(rollback_bodies(&log).is_empty());

    let allowed = gz(&dir, &base)
        .args(["table", "rollback", "Lineage.sales.orders", "--to", "audit"])
        .args(["--allow-non-ancestor", "--yes"])
        .output()
        .unwrap();
    assert!(allowed.status.success(), "{}", plain(&allowed.stderr));
    let stdout = plain(&allowed.stdout);
    assert!(
        stdout.contains("Brings in 1 commit(s) main never had"),
        "{stdout}"
    );
    assert_eq!(rollback_bodies(&log)[0]["allowNonAncestor"], true);
}

#[test]
fn a_refused_rollback_shows_the_servers_reason() {
    let dir = temp_dir("rollback-409");
    let (base, _log) = stub();
    let out = gz(&dir, &base)
        .args([
            "table",
            "rollback",
            "Lineage.sales.orders",
            "--to",
            "101",
            "--yes",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = plain(&out.stderr);
    assert!(
        stderr.contains("rolling back sales.orders failed; `gz table versions sales.orders` shows where main is now"),
        "{stderr}"
    );
    assert!(
        stderr.contains("main has moved since the rollback was previewed"),
        "{stderr}"
    );
}

#[test]
fn profile_reads_one_snapshot_in_aggregate_passes_on_a_session() {
    let dir = temp_dir("profile");
    let (base, log) = stub();
    let out = gz(&dir, &base)
        .args([
            "-o",
            "json",
            "table",
            "profile",
            "Lineage.sales.orders",
            "--at",
            "103",
        ])
        .args(["--top", "2", "--compute", "mini"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let profile: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(profile["snapshotId"], "103");
    assert_eq!(profile["rows"], 40);
    assert_eq!(profile["columns"][0]["nulls"], 2);
    assert_eq!(
        profile["columns"][0]["top"],
        json!([{"value": "9.5", "count": 4}])
    );
    assert_eq!(profile["columns"][1]["maxLength"], 4);

    let queries: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.target == "/lakehouse/submitquery")
        .map(|r| r.body["query"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        queries.len(),
        3,
        "columns, one aggregate pass, one top-values pass"
    );
    assert!(
        queries
            .iter()
            .all(|q| q.contains("`sales`.`orders` VERSION AS OF 103")),
        "{queries:?}"
    );
    let seen = requests(&log);
    assert_eq!(
        seen[1],
        "GET /lakehouse/tables/Lineage/sales/orders/versions"
    );
    assert_eq!(seen.last().unwrap(), "POST /lakehouse/stopsession");
}

#[test]
fn profile_over_the_scan_limit_starts_nothing() {
    let dir = temp_dir("profile-limit");
    let (base, log) = stub();
    let out = gz(&dir, &base)
        .args([
            "table",
            "profile",
            "Lineage.sales.orders",
            "--max-scan",
            "1KB",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = plain(&out.stderr);
    assert!(
        stderr.contains("scan check: Reads up to 39.1 KB"),
        "{stderr}"
    );
    assert!(
        stderr.contains("This profile reads up to 39.1 KB, over the 1.0 KB limit"),
        "{stderr}"
    );
    assert!(
        !requests(&log).iter().any(|r| r.contains("session")),
        "no session started"
    );
}

#[test]
fn lineage_graph_finds_an_asset_by_name_and_walks_both_ways() {
    let dir = temp_dir("lineage");
    let (base, log) = stub();
    let out = gz(&dir, &base)
        .args(["jobs", "lineage", "graph", "sales.orders", "--depth", "3"])
        .output()
        .unwrap();
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert_eq!(
        stdout,
        "\
sales.orders  TABLE 7
├── upstream (1)
│   └── raw/orders.csv  OBJECT 6  via LoadOrders (job)
└── downstream (1)
    └── sales.daily  TABLE 8  via Daily (job)
"
    );
    let graph_read = log
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.target.starts_with("/etl/lineage/assets/7"))
        .map(|r| r.target.clone())
        .unwrap();
    assert!(graph_read.contains("upstreamDepth=3"), "{graph_read}");
    assert!(graph_read.contains("downstreamDepth=3"), "{graph_read}");
    let seen = requests(&log);
    assert_eq!(
        seen[..2],
        ["GET /etl/lineage/connections", "GET /etl/lineage/assets"]
    );
}
