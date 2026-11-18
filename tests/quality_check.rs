//! `gz quality check` against stub platform services: the connection of the
//! table's warehouse, a temporary job given that connection, a run whose
//! log is read only once it is final, the report it printed, the exit code
//! of the gate, and the job deleted again.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const JOB: &str = "/etl/etlJobs/jobs/1999/4242";

#[derive(Default)]
struct Platform {
    requests: Vec<Recorded>,
    code: String,
    config: Value,
    /// Checks the run's report says failed the gate.
    gate_failures: Vec<&'static str>,
    /// The run prints no report and fails.
    crashes: bool,
}

/// The marker the generated script prints its report lines with.
fn marker(code: &str) -> String {
    let line = code
        .lines()
        .find(|l| l.starts_with("PARAMS = json.loads("))
        .expect("the script loads its PARAMS");
    let literal = &line["PARAMS = json.loads(".len()..line.len() - 1];
    let params: Value =
        serde_json::from_str(&serde_json::from_str::<String>(literal).unwrap()).unwrap();
    params["marker"].as_str().unwrap().to_string()
}

fn report() -> Value {
    json!({
        "dataset": {"row_count": 1200, "source": "gzlakehouse__prod.sales.orders",
                    "source_version": "8234019231840123", "source_ref": null},
        "results": [
            {"name": "outliers", "severity": "warn", "status": "completed", "error": null,
             "payload": {"per_column": [{"column": "units", "lower_bound": 0.0, "upper_bound": 9.0, "outlier_count": 3}],
                         "quantile_provenance": {"method": "exact"}}},
            {"name": "missing_values", "severity": "ok", "status": "completed", "error": null,
             "payload": {"total_null_count": 0, "worst_column_pct": 0.0, "per_column": []}},
        ],
    })
}

fn log_events(platform: &Platform) -> Value {
    if platform.crashes {
        return json!([
            {"timestamp": "t", "message": "Traceback (most recent call last):\n"},
            {"timestamp": "t", "message": "ValueError: Table sales.orders not found\n"},
        ]);
    }
    let text = json!({"report": report(), "gateFailures": platform.gate_failures}).to_string();
    let (first, second) = text.split_at(text.len() / 2);
    let marker = marker(&platform.code);
    json!([
        {"timestamp": "t", "message": "Spark session ready\n"},
        {"timestamp": "t", "message": format!("{marker} 1/2 {first}\n")},
        {"timestamp": "t", "message": format!("{marker} 2/2 {second}\n")},
    ])
}

fn reply(platform: &mut Platform, request: &Recorded) -> Option<Value> {
    let reply = match (request.method.as_str(), request.target.as_str()) {
        ("GET", "/connection") => json!([
            {"Id": 5, "Name": "snow", "type": "SNOWFLAKE"},
            {"Id": 1366, "Name": "Real", "type": "GZLAKEHOUSE"},
            {"Id": 1321, "Name": "GZLakehouseProd", "type": "GZLAKEHOUSE"},
        ]),
        ("GET", "/connection/1366") => {
            json!([{"config": {"warehouseName": "Other"}, "type": "GZLAKEHOUSE"}])
        }
        ("GET", "/connection/1321") => {
            json!([{"config": {"warehouseName": "Lineage", "databaseName": "sales"}}])
        }
        ("GET", "/rte/compute") => json!([{"Id": 1006, "Name": "small", "Description": "Small"}]),
        ("GET", "/etl/etlProjects") => json!([{"Id": 1999, "Name": "DataQuality"}]),
        ("POST", "/etl/etlJobs/1999") => {
            platform.code = request.body["fileContent"].as_str()?.to_string();
            json!({"Id": 4242})
        }
        ("GET", JOB) => json!({"config": platform.config, "fileContent": platform.code}),
        ("PUT", JOB) => {
            platform.config = request.body["config"].clone();
            json!({"status": 200})
        }
        ("POST", "/etl/job") => json!({"jobIdentifier": "run77", "sessionId": "s1"}),
        ("POST", "/etl/jobStatus") => {
            let status = if platform.crashes {
                "Failed"
            } else {
                "Success"
            };
            json!({"data": [{"jobstatus": status}]})
        }
        ("POST", "/etl/logs") => {
            json!({"events": log_events(platform), "hasMoreLogs": false, "cursor": "c1"})
        }
        ("DELETE", "/etl/etlJobs/jobs/4242") => json!({"status": 200}),
        _ => return None,
    };
    Some(reply)
}

fn stub(gate_failures: Vec<&'static str>, crashes: bool) -> (String, Arc<Mutex<Platform>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let platform = Arc::new(Mutex::new(Platform {
        gate_failures,
        crashes,
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
    let dir = std::env::temp_dir().join(format!("gz-quality-check-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "quality-check-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
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

/// Cells of the first table row starting with `first`.
fn row(stdout: &str, first: &str) -> String {
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
}

#[test]
fn a_passing_check_runs_a_temporary_job_and_prints_each_check() {
    let dir = temp_dir("pass");
    let (base, platform) = stub(vec![], false);

    let out = gz(
        &dir,
        &base,
        &[
            "quality",
            "check",
            "Lineage.sales.orders",
            "--project",
            "1999",
        ],
    );

    assert!(out.status.success(), "{}", plain(&out.stderr));
    let stdout = plain(&out.stdout);
    assert!(
        stdout.contains("Lineage.sales.orders: 1200 rows at snapshot 8234019231840123"),
        "{stdout}"
    );
    assert_eq!(row(&stdout, "check"), "check | severity | status | details");
    assert_eq!(
        row(&stdout, "outliers"),
        "outliers | warn | completed | units: 3 outliers"
    );
    assert_eq!(
        row(&stdout, "missing_values"),
        "missing_values | ok | completed | no nulls"
    );
    assert!(stdout.contains("passed at --fail-on error"), "{stdout}");

    let calls = calls(&platform);
    assert_eq!(
        calls,
        [
            "GET /connection",
            "GET /connection/1366",
            "GET /connection/1321",
            "GET /rte/compute",
            "POST /etl/etlJobs/1999",
            &format!("GET {JOB}"),
            &format!("PUT {JOB}"),
            &format!("GET {JOB}"),
            "GET /etl/etlProjects",
            "POST /etl/job",
            "POST /etl/jobStatus",
            "POST /etl/logs",
            "DELETE /etl/etlJobs/jobs/4242",
        ]
    );
    let platform = platform.lock().unwrap();
    assert_eq!(
        platform.config["connectionNames"],
        json!(["GZLakehouseProd"])
    );
    assert_eq!(platform.config["connectionIds"], json!([1321]));
    let created = &platform.requests[4].body;
    assert!(created["Name"]
        .as_str()
        .unwrap()
        .starts_with("gz-quality-check-"));
    let run = platform
        .requests
        .iter()
        .find(|r| r.target == "/etl/job")
        .unwrap();
    assert_eq!(run.body["computeId"], json!(1006));
    assert_eq!(run.body["rteName"], json!("gz-rte-spark-sklearn:1.0.0"));
    assert_eq!(run.body["ConnectionNames"], json!(["GZLakehouseProd"]));
    assert!(platform
        .code
        .contains("session.runTableDataQualityChecks(**options)"));
}

#[test]
fn a_failed_gate_exits_2_and_json_has_the_whole_report() {
    let dir = temp_dir("fail");
    let (base, _platform) = stub(vec!["outliers"], false);

    let out = gz(
        &dir,
        &base,
        &[
            "-o",
            "json",
            "quality",
            "check",
            "Lineage.sales.orders",
            "--project",
            "1999",
            "--severity",
            "outliers=error",
        ],
    );

    assert_eq!(out.status.code(), Some(2), "{}", plain(&out.stderr));
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["passed"], json!(false));
    assert_eq!(shown["gateFailures"], json!(["outliers"]));
    assert_eq!(shown["run"], json!("run77"));
    assert_eq!(shown["report"], report());
    assert!(plain(&out.stderr).contains("failed at --fail-on error: outliers"));
}

#[test]
fn a_run_that_prints_no_report_shows_its_log_and_still_deletes_the_job() {
    let dir = temp_dir("crash");
    let (base, platform) = stub(vec![], true);

    let out = gz(
        &dir,
        &base,
        &[
            "quality",
            "check",
            "Lineage.sales.orders",
            "--project",
            "1999",
        ],
    );

    assert_eq!(out.status.code(), Some(1));
    let said = plain(&out.stderr);
    assert!(
        said.contains("run run77 ended Failed without printing a report"),
        "{said}"
    );
    assert!(
        said.contains("ValueError: Table sales.orders not found"),
        "{said}"
    );
    assert_eq!(
        calls(&platform).last().unwrap(),
        "DELETE /etl/etlJobs/jobs/4242"
    );
}

#[test]
fn bad_flags_are_refused_before_anything_runs() {
    let dir = temp_dir("refused");
    let (base, platform) = stub(vec![], false);

    for (args, said) in [
        (&["--severity", "outlier=error"][..], "no check 'outlier'"),
        (
            &[
                "--rte",
                "gz-rte-duckdb-sklearn:1.0.0",
                "--results",
                "Lineage.dq.results",
            ][..],
            "--results needs a Spark runtime",
        ),
        (
            &["--rte", "gz-rte-pandas:1.0.0"][..],
            "neither a Spark nor a DuckDB runtime",
        ),
    ] {
        let mut full = vec![
            "quality",
            "check",
            "Lineage.sales.orders",
            "--project",
            "1999",
        ];
        full.extend_from_slice(args);
        let out = gz(&dir, &base, &full);
        assert!(!out.status.success());
        assert!(plain(&out.stderr).contains(said), "{}", plain(&out.stderr));
    }
    let out = gz(
        &dir,
        &base,
        &[
            "quality",
            "check",
            "Elsewhere.sales.orders",
            "--project",
            "1999",
        ],
    );
    assert!(plain(&out.stderr).contains("no lakehouse connection points at warehouse Elsewhere"));
    assert!(!calls(&platform).iter().any(|c| c.starts_with("POST /etl")));
}
