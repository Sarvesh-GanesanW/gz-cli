//! Query cost preview against a stub lakehouse provider: `gz sql estimate`
//! from the planned route and from snapshot totals where the route is not
//! deployed, and `--max-scan` / `sql_max_scan` refusing a query before any
//! session starts.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const GB: u64 = 1 << 30;
/// `DemoDB.inventory`, 9 GB in 350 files.
const INVENTORY_BYTES: u64 = 9 * GB;
/// `DemoDB.sales`, 25 KB.
const SALES_BYTES: u64 = 25_116;

#[derive(Clone, Copy, PartialEq)]
enum Lakehouse {
    /// Answers `/lakehouse/scanestimate`.
    Plans,
    /// Has no estimate route: Flask's HTML 404.
    Older,
}

fn versions(bytes: u64, files: u64, rows: u64) -> Value {
    json!({"status": 200, "response": {
        "currentSnapshotId": "7",
        "snapshots": [{"snapshotId": "7", "committedAt": "2026-11-01T00:00:00+00:00",
            "operation": "append", "summary": {
                "total-files-size": bytes.to_string(),
                "total-data-files": files.to_string(),
                "total-records": rows.to_string(),
            }}],
        "branches": [], "tags": [],
    }})
}

fn stub(kind: Lakehouse) -> (String, Arc<Mutex<Vec<Recorded>>>) {
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
            let reply = match (request.method.as_str(), path) {
                ("GET", "/lakehouse/warehouse") => {
                    json!({"status": 200, "response": [{"id": 1108, "name": "GZWarehousePROD"}]})
                }
                ("GET", "/rte/compute") => {
                    json!([{"Id": 1000, "Name": "mini", "Description": "Mini"}])
                }
                ("GET", "/lakehouse/tables/GZWarehousePROD/DemoDB/inventory/versions") => {
                    versions(INVENTORY_BYTES, 350, 400_000_000)
                }
                ("GET", "/lakehouse/tables/GZWarehousePROD/DemoDB/sales/versions") => {
                    versions(SALES_BYTES, 1, 700)
                }
                ("POST", "/lakehouse/scanestimate") if kind == Lakehouse::Older => {
                    let page = "<!doctype html>\n<html lang=en>\n<title>404 Not Found</title>";
                    respond(&mut stream, "404 NOT FOUND", "text/html", page);
                    continue;
                }
                ("POST", "/lakehouse/scanestimate") => json!({"status": 200, "response": {
                    "bytes": 3 * GB, "files": 117, "rows": 133_000_000,
                    "tables": [{"table": "DemoDB.inventory", "bytes": 3 * GB, "files": 117,
                        "rows": 133_000_000, "tableBytes": INVENTORY_BYTES, "tableFiles": 350,
                        "tableRows": 400_000_000, "basis": "planned",
                        "filter": "region == 'eu'", "note": null}],
                }}),
                ("POST", "/lakehouse/startsession") => {
                    json!({"status": 200, "response": {"sessionId": "s1"}})
                }
                ("POST", "/lakehouse/submitquery") => json!({"status": 200, "response": {
                    "status": "SUCCEEDED", "data": [{"n": 1}], "schema": [{"name": "n", "type": "int"}],
                    "pageNumber": 1, "hasMore": false, "totalRecords": 1,
                }, "queryExecutionId": "q1"}),
                ("POST", "/lakehouse/stopsession") => json!({"status": 200, "response": {}}),
                _ => {
                    let body = json!({"status": 404, "response": {"message": "Table not found"}});
                    respond(
                        &mut stream,
                        "404 NOT FOUND",
                        "application/json",
                        &body.to_string(),
                    );
                    continue;
                }
            };
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                &reply.to_string(),
            );
        }
    });
    (base, log)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-scan-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gz"));
    cmd.env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "scan-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme");
    cmd
}

fn paths(log: &Arc<Mutex<Vec<Recorded>>>) -> Vec<String> {
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

const BIG_QUERY: &str = "WITH s AS (SELECT * FROM DemoDB.sales) \
                         SELECT * FROM DemoDB.inventory i JOIN s ON s.id = i.id";

#[test]
fn estimate_shows_the_lakehouses_planned_scan() {
    let dir = temp_dir("planned");
    let (base, log) = stub(Lakehouse::Plans);
    let out = gz(&dir, &base)
        .args([
            "sql",
            "estimate",
            "SELECT * FROM DemoDB.inventory WHERE region = 'eu'",
        ])
        .args(["--warehouse", "GZWarehousePROD", "-o", "json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let estimate: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(estimate["basis"], "planned");
    assert_eq!(estimate["bytes"], 3 * GB);
    assert_eq!(estimate["tables"][0]["filter"], "region == 'eu'");
    assert!(
        estimate["cost"].is_null() && estimate["costNote"].as_str().unwrap().contains("no price")
    );
    let sent = log.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        sent.body,
        json!({"catalog": "GZWarehousePROD", "query": "SELECT * FROM DemoDB.inventory WHERE region = 'eu'"})
    );
}

#[test]
fn without_the_route_the_estimate_is_every_table_whole() {
    let dir = temp_dir("bound");
    let (base, log) = stub(Lakehouse::Older);
    let out = gz(&dir, &base)
        .args([
            "sql",
            "estimate",
            BIG_QUERY,
            "--warehouse",
            "GZWarehousePROD",
        ])
        .output()
        .unwrap();
    let text = plain(&out.stdout);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert!(
        text.starts_with("Reads up to 9.0 GB · 351 files · 400,000,700 rows"),
        "{text}"
    );
    assert!(
        text.contains("DemoDB.inventory") && text.contains("DemoDB.sales"),
        "{text}"
    );
    let sized: Vec<String> = paths(&log)
        .into_iter()
        .filter(|p| p.ends_with("/versions"))
        .collect();
    // The CTE `s` is no table, so only the two tables are looked up.
    assert_eq!(
        sized,
        [
            "GET /lakehouse/tables/GZWarehousePROD/DemoDB/sales/versions",
            "GET /lakehouse/tables/GZWarehousePROD/DemoDB/inventory/versions",
        ]
    );
}

#[test]
fn max_scan_refuses_a_script_before_any_session_starts() {
    let dir = temp_dir("refuse");
    let file = dir.join("q.sql");
    std::fs::write(&file, format!("{BIG_QUERY};")).unwrap();
    let (base, log) = stub(Lakehouse::Older);
    let out = gz(&dir, &base)
        .args(["sql", "run-file"])
        .arg(&file)
        .args([
            "--warehouse",
            "GZWarehousePROD",
            "--compute",
            "mini",
            "--max-scan",
            "1GB",
        ])
        .output()
        .unwrap();
    let error = plain(&out.stderr);
    assert!(!out.status.success());
    assert!(
        error.contains("reads up to 9.0 GB, over the 1.0 GB limit (--max-scan)"),
        "{error}"
    );
    assert!(
        error.contains("--max-scan 10GB or --max-scan off"),
        "{error}"
    );
    let asked = paths(&log);
    assert!(
        !asked
            .iter()
            .any(|p| p.contains("startsession") || p.contains("submitquery")),
        "{asked:?}"
    );
}

#[test]
fn a_higher_max_scan_or_off_lets_it_run() {
    for limit in ["10GB", "off"] {
        let dir = temp_dir(&format!("override-{limit}"));
        let (base, log) = stub(Lakehouse::Older);
        let mut child = gz(&dir, &base)
            .args([
                "sql",
                "--warehouse",
                "GZWarehousePROD",
                "--compute",
                "mini",
                "--max-scan",
                limit,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{BIG_QUERY};").as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{limit}: {}", plain(&out.stderr));
        let asked = paths(&log);
        assert!(
            asked.contains(&"POST /lakehouse/submitquery".to_string()),
            "{limit}: {asked:?}"
        );
        let estimated = asked.iter().any(|p| p.ends_with("/versions"));
        assert_eq!(estimated, limit != "off", "{limit}: off skips the estimate");
    }
}

#[test]
fn the_profile_limit_is_saved_applies_to_submit_and_can_be_cleared() {
    let dir = temp_dir("profile");
    let (base, log) = stub(Lakehouse::Plans);
    let set = gz(&dir, &base)
        .args(["sql", "max-scan", "2GB"])
        .output()
        .unwrap();
    assert!(set.status.success(), "{}", plain(&set.stderr));
    let config = std::fs::read_to_string(dir.join("config/gz/config.toml")).unwrap();
    assert!(config.contains("sql_max_scan = \"2GB\""), "{config}");

    let submit = |log: &Arc<Mutex<Vec<Recorded>>>| {
        log.lock().unwrap().clear();
        gz(&dir, &base)
            .args([
                "sql",
                "submit",
                "--sql",
                "SELECT * FROM DemoDB.inventory",
                "--session",
                "s1",
            ])
            .args(["--warehouse", "GZWarehousePROD"])
            .output()
            .unwrap()
    };
    let refused = submit(&log);
    assert!(!refused.status.success());
    let error = plain(&refused.stderr);
    assert!(
        error.contains("over the 2.0 GB limit (sql_max_scan in this profile)"),
        "{error}"
    );
    assert!(!paths(&log).contains(&"POST /lakehouse/submitquery".to_string()));

    let cleared = gz(&dir, &base)
        .args(["sql", "max-scan", "off"])
        .output()
        .unwrap();
    assert!(cleared.status.success());
    let ran = submit(&log);
    assert!(ran.status.success(), "{}", plain(&ran.stderr));
    assert_eq!(
        paths(&log),
        ["POST /lakehouse/submitquery"],
        "no estimate without a limit"
    );
}
