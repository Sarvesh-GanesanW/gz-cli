//! `gz sql` against a stub lakehouse provider: pick warehouse and compute,
//! start a session, submit (running → polled → results page), stop the
//! session; plus the failed and cancelled query paths.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    target: String,
    body: Value,
}

fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while raw.len() < head_end + length {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let mut first = head.lines().next()?.split_whitespace();
    Some(Recorded {
        method: first.next()?.to_string(),
        target: first.next()?.to_string(),
        body: serde_json::from_slice(&raw[head_end..head_end + length]).unwrap_or(Value::Null),
    })
}

fn respond(stream: &mut TcpStream, status: &str, body: &Value) {
    let text = body.to_string();
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    stream.write_all(reply.as_bytes()).ok();
}

/// How the stub's query ends once polled.
#[derive(Clone, Copy)]
enum Ending {
    Succeeds,
    /// Finishes inline; the session keeps the result as a pointer.
    Stored,
    Fails,
    Cancelled,
}

fn stub_lakehouse(ending: Ending) -> (String, Arc<Mutex<Vec<Recorded>>>) {
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
            let path = request
                .target
                .split('?')
                .next()
                .unwrap_or_default()
                .to_string();
            let reply = match (request.method.as_str(), path.as_str()) {
                ("GET", "/lakehouse/warehouse") => json!({"status": 200, "response": [
                    {"id": 1108, "name": "GZWarehousePROD"},
                    {"id": 1114, "name": "Lineage"},
                ]}),
                ("GET", "/rte/compute") => json!([
                    {"Id": 1000, "Name": "mini", "Description": "Mini"},
                    {"Id": 1006, "Name": "small", "Description": "Small"},
                ]),
                ("POST", "/lakehouse/startsession") => {
                    json!({"status": 200, "response": {"sessionId": "s1"}})
                }
                ("POST", "/lakehouse/submitquery") => match ending {
                    Ending::Stored => json!({
                        "status": 200,
                        "response": {"status": "SUCCEEDED", "queryExecutionId": "q1", "resultFormat": "arrow", "cachedRecords": 2},
                        "queryExecutionId": "q1",
                    }),
                    _ => json!({
                        "status": 200,
                        "response": {"status": "RUNNING", "queryExecutionId": "q1"},
                        "queryExecutionId": "q1",
                    }),
                },
                ("GET", "/lakehouse/querystatus/s1/q1") => {
                    let status = match ending {
                        Ending::Succeeds | Ending::Stored => {
                            json!({"status": "SUCCEEDED", "totalRecords": 2})
                        }
                        Ending::Fails => {
                            json!({"status": "FAILED", "error": "Table or view not found: nope"})
                        }
                        Ending::Cancelled => json!({"status": "CANCELLED"}),
                    };
                    json!({"status": 200, "response": status})
                }
                ("GET", "/lakehouse/queryresults/s1/q1") => json!({"status": 200, "response": {
                    "schema": [{"name": "name", "type": "string"}, {"name": "amount", "type": "decimal"}],
                    "data": [{"name": "a,b", "amount": "1.500000"}, {"name": "c", "amount": null}],
                    "pageNumber": 1, "hasMore": false, "totalRecords": 2,
                }}),
                ("POST", "/lakehouse/stopsession") => json!({"status": 200, "response": {}}),
                _ => {
                    respond(
                        &mut stream,
                        "404 Not Found",
                        &json!({"message": "no route"}),
                    );
                    continue;
                }
            };
            respond(&mut stream, "200 OK", &reply);
        }
    });
    (base, log)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-sql-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gz"));
    cmd.env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "sql-test")
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
fn runs_a_file_through_the_whole_session_lifecycle() {
    let dir = temp_dir("lifecycle");
    let file = dir.join("q.sql");
    std::fs::write(
        &file,
        "-- totals; per name\nselect name, amount from sales;",
    )
    .unwrap();
    let (base, log) = stub_lakehouse(Ending::Succeeds);
    let out = gz(&dir, &base)
        .args(["-o", "csv", "sql", "run-file"])
        .arg(&file)
        .args(["--warehouse", "gzwarehouseprod", "--compute", "mini"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout, "name,amount\n\"a,b\",1.500000\nc,\n",
        "CSV keeps raw values"
    );

    assert_eq!(
        requests(&log),
        vec![
            "GET /lakehouse/warehouse",
            "GET /rte/compute",
            "POST /lakehouse/startsession",
            "POST /lakehouse/submitquery",
            "GET /lakehouse/querystatus/s1/q1",
            "GET /lakehouse/queryresults/s1/q1",
            "POST /lakehouse/stopsession",
        ]
    );
    let log = log.lock().unwrap();
    assert_eq!(log[2].body, json!({"computeId": 1000, "warehouseId": 1108}));
    let submit = &log[3].body;
    assert_eq!(submit["catalog"], "GZWarehousePROD");
    assert_eq!(submit["sessionId"], "s1");
    assert_eq!(submit["asyncQuery"], true);
    assert_eq!(
        submit["resultFormat"], "arrow",
        "keeps the result for paging"
    );
    assert!(submit["query"]
        .as_str()
        .unwrap()
        .starts_with("-- totals; per name"));
    assert!(log[5].target.contains("catalog=GZWarehousePROD"));
    assert_eq!(log[6].body, json!({"sessionId": "s1"}));
}

#[test]
fn piped_sql_prints_a_table_and_remembers_the_choice() {
    let dir = temp_dir("piped");
    let (base, _log) = stub_lakehouse(Ending::Succeeds);
    let mut child = gz(&dir, &base)
        .args(["sql", "--warehouse", "1108", "--compute", "small"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"select 1;").unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("name │ amount"), "{stdout}");
    assert!(stdout.contains("1.5"), "table tidies decimals: {stdout}");
    let config = std::fs::read_to_string(dir.join("config/gz/config.toml")).unwrap();
    assert!(
        config.contains("sql_warehouse = \"GZWarehousePROD\""),
        "{config}"
    );
    assert!(config.contains("sql_compute = \"small\""), "{config}");
    assert!(
        !config.contains("sql_session ="),
        "a stopped session is forgotten: {config}"
    );
}

#[test]
fn a_stored_result_is_read_from_queryresults_without_polling() {
    let dir = temp_dir("stored");
    let (base, log) = stub_lakehouse(Ending::Stored);
    let mut child = gz(&dir, &base)
        .args([
            "-o",
            "json",
            "sql",
            "--warehouse",
            "Lineage",
            "--compute",
            "mini",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"select name from t")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        value[0]["statement"], "select name from t",
        "a final statement needs no ;"
    );
    assert_eq!(value[0]["rows"][0]["name"], "a,b");
    let calls = requests(&log);
    assert!(
        !calls.iter().any(|c| c.contains("querystatus")),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"GET /lakehouse/queryresults/s1/q1".to_string()),
        "{calls:?}"
    );
}

#[test]
fn a_failed_query_exits_non_zero_and_still_stops_the_session() {
    let dir = temp_dir("failed");
    let (base, log) = stub_lakehouse(Ending::Fails);
    let mut child = gz(&dir, &base)
        .args(["sql", "--warehouse", "Lineage", "--compute", "mini"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"select * from nope;")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("Table or view not found: nope"), "{stderr}");
    assert!(stderr.contains("in: select * from nope"), "{stderr}");
    assert_eq!(
        requests(&log).last().unwrap(),
        "POST /lakehouse/stopsession"
    );
}

#[test]
fn a_cancelled_query_is_reported_as_cancelled() {
    let dir = temp_dir("cancelled");
    let (base, _log) = stub_lakehouse(Ending::Cancelled);
    let mut child = gz(&dir, &base)
        .args([
            "sql",
            "--warehouse",
            "Lineage",
            "--compute",
            "mini",
            "--keep-session",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"select 1;").unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("Query cancelled"), "{stderr}");
    assert!(stderr.contains("session kept running (s1)"), "{stderr}");
}

#[test]
fn needs_a_warehouse_when_nothing_is_remembered() {
    let dir = temp_dir("nowarehouse");
    let (base, _log) = stub_lakehouse(Ending::Succeeds);
    let mut child = gz(&dir, &base)
        .args(["sql"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"select 1;").unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("pass --warehouse NAME (have: GZWarehousePROD, Lineage)"),
        "{stderr}"
    );
}
