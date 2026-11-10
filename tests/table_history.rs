//! `gz table versions`, `gz diff` and `gz bisect` against a stub lakehouse
//! provider: the versions route, the SQL each command sends, and the session
//! it starts and stops.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// `sales.orders`: four commits, where `amount < 0` rows appear in 103.
const FIRST_BAD: u64 = 103;

fn snapshot(id: u64, parent: Option<u64>, day: u32, total: u64) -> Value {
    json!({
        "snapshotId": id.to_string(),
        "parentSnapshotId": parent.map(|p| p.to_string()),
        "committedAt": format!("2026-09-{day:02}T09:00:00+00:00"),
        "operation": "append",
        "summary": {
            "added-records": "10",
            "total-records": total.to_string(),
            "app-name": "load_orders",
            "spark.app.id": format!("app-{id}"),
        },
    })
}

fn versions() -> Value {
    json!({
        "currentSnapshotId": "104",
        "snapshots": [
            snapshot(104, Some(103), 4, 40),
            snapshot(103, Some(102), 3, 30),
            snapshot(102, Some(101), 2, 20),
            snapshot(101, None, 1, 10),
        ],
        "branches": [{"name": "main", "snapshotId": "104"}],
        "tags": [],
    })
}

/// The check's count for the snapshot a query reads.
fn failing_rows(query: &str) -> u64 {
    let id: u64 = query
        .split("VERSION AS OF ")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|digits| digits.parse().ok())
        .expect("the query reads a snapshot");
    if id >= FIRST_BAD {
        3
    } else {
        0
    }
}

fn stub_lakehouse() -> (String, Arc<Mutex<Vec<Recorded>>>) {
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
                ("GET", "/lakehouse/warehouse") => json!({"status": 200, "response": [
                    {"id": 1114, "name": "Lineage"},
                ]}),
                ("GET", "/rte/compute") => json!([
                    {"Id": 1000, "Name": "mini", "Description": "Mini"},
                ]),
                ("GET", "/lakehouse/tables/Lineage/sales/orders/versions") => {
                    json!({"status": 200, "response": versions()})
                }
                ("POST", "/lakehouse/startsession") => {
                    json!({"status": 200, "response": {"sessionId": "s1"}})
                }
                ("POST", "/lakehouse/submitquery") => {
                    let failing = failing_rows(request.body["query"].as_str().unwrap_or_default());
                    json!({"status": 200, "queryExecutionId": "q1", "response": {
                        "schema": [{"name": "failing", "type": "long"}],
                        "data": [{"failing": failing}],
                        "pageNumber": 1, "hasMore": false, "totalRecords": 1,
                    }})
                }
                ("POST", "/lakehouse/stopsession") => json!({"status": 200, "response": {}}),
                _ => {
                    respond(&mut stream, "404 Not Found", "application/json", "{}");
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
    let dir = std::env::temp_dir().join(format!("gz-history-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gz"));
    cmd.env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "history-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme");
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

#[test]
fn bisect_reads_the_history_then_tests_snapshots_on_one_session() {
    let dir = temp_dir("bisect");
    let (base, log) = stub_lakehouse();
    let out = gz(&dir, &base)
        .args(["bisect", "sales.orders", "--where", "amount < 0"])
        .args(["--warehouse", "lineage", "--compute", "mini"])
        .output()
        .unwrap();
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    for part in [
        "First failing snapshot  103",
        "  committed   2026-09-03 09:00 UTC",
        "  written by  load_orders (Spark application app-103)",
        "  check       3 rows fail here · none in the snapshot before (102, 2026-09-02 09:00 UTC)",
        "Next: gz diff sales.orders --from 102 --to 103 --rows",
        "Tested 4 of 4 snapshots.",
    ] {
        assert!(stdout.contains(part), "missing {part:?} in\n{stdout}");
    }

    let queries: Vec<Value> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.target == "/lakehouse/submitquery")
        .map(|r| r.body.clone())
        .collect();
    let read: Vec<&str> = queries
        .iter()
        .map(|q| q["query"].as_str().unwrap())
        .collect();
    assert_eq!(
        read,
        [
            "SELECT count(*) AS failing FROM `sales`.`orders` VERSION AS OF 104 WHERE amount < 0",
            "SELECT count(*) AS failing FROM `sales`.`orders` VERSION AS OF 101 WHERE amount < 0",
            "SELECT count(*) AS failing FROM `sales`.`orders` VERSION AS OF 102 WHERE amount < 0",
            "SELECT count(*) AS failing FROM `sales`.`orders` VERSION AS OF 103 WHERE amount < 0",
        ],
        "newest, oldest, then the middle of what is left each time"
    );
    assert_eq!(queries[0]["catalog"], "Lineage");
    assert_eq!(queries[0]["sessionId"], "s1");

    let seen = requests(&log);
    assert_eq!(
        seen[..2],
        [
            "GET /lakehouse/warehouse",
            "GET /lakehouse/tables/Lineage/sales/orders/versions"
        ],
        "the history is read before any session starts"
    );
    assert_eq!(seen.last().unwrap(), "POST /lakehouse/stopsession");
}

#[test]
fn diff_without_rows_reads_only_the_history() {
    let dir = temp_dir("diff");
    let (base, log) = stub_lakehouse();
    let out = gz(&dir, &base)
        .args(["diff", "sales.orders", "--from", "2026-09-02T12:00"])
        .args(["--warehouse", "Lineage"])
        .output()
        .unwrap();
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    for part in [
        "  from  102 · 2026-09-02 09:00 UTC",
        "  to    104 · 2026-09-04 09:00 UTC · current, branch main",
        "Commits (2)",
        "Records  20 → 40 (+20)",
    ] {
        assert!(stdout.contains(part), "missing {part:?} in\n{stdout}");
    }
    assert_eq!(
        requests(&log),
        [
            "GET /lakehouse/warehouse",
            "GET /lakehouse/tables/Lineage/sales/orders/versions"
        ],
        "no session is started"
    );
}

#[test]
fn versions_as_json_is_the_servers_own_reply() {
    let dir = temp_dir("versions");
    let (base, _log) = stub_lakehouse();
    let out = gz(&dir, &base)
        .args(["-o", "json", "table", "versions", "sales.orders"])
        .args(["--warehouse", "Lineage"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(reply, versions());

    let unknown = gz(&dir, &base)
        .args([
            "diff",
            "sales.orders",
            "--from",
            "999",
            "--warehouse",
            "Lineage",
        ])
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    let stderr = plain(&unknown.stderr);
    assert!(
        stderr.contains("--from: the table has no snapshot 999"),
        "{stderr}"
    );
}
