//! `gz review` against stub ETL, connection and lakehouse providers: the
//! code it tries, the verdict and exit code, the branches it cleans up, and
//! the SQL sessions it stops (only its own).

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const TABLE: &str = "/lakehouse/tables/Manufacturing/mfg_ops/oee_daily";
const SAVED: &str = "print('saved')\n";
const LOCAL: &str = "print('changed')\n";

#[derive(Clone, Copy, Default)]
struct Stub {
    /// The try run ends with status Failed.
    run_fails: bool,
    /// The job's lineage shows no lakehouse table it writes.
    no_outputs: bool,
    /// The branch's `amount` column is a string; main's is an int.
    retyped: bool,
    /// The lakehouse refuses to make the branch.
    refuse_branch: bool,
}

/// What the stub remembers between requests.
#[derive(Default)]
struct State {
    branch: Option<String>,
}

fn snapshot(id: u64, parent: Option<u64>, minute: u32, total: u64) -> Value {
    json!({
        "snapshotId": id.to_string(),
        "parentSnapshotId": parent.map(|p| p.to_string()),
        "committedAt": format!("2026-11-11T09:{minute:02}:00+00:00"),
        "operation": "append",
        "summary": {"added-records": "5", "total-records": total.to_string()},
    })
}

fn versions(state: &State) -> Value {
    let mut branches = vec![json!({"name": "main", "snapshotId": "11"})];
    let mut snapshots = vec![snapshot(11, None, 1, 10)];
    if let Some(branch) = &state.branch {
        branches.push(json!({"name": branch, "snapshotId": "13"}));
        snapshots.insert(0, snapshot(13, Some(11), 3, 15));
    }
    json!({"currentSnapshotId": "11", "snapshots": snapshots, "branches": branches, "tags": []})
}

fn lineage(stub: Stub) -> Value {
    let sink = if stub.no_outputs { "source" } else { "sink" };
    json!({"graph": {
        "assets": [
            {"assetId": 900, "assetType": "TABLE", "qualifiedName": "mfg_ops.oee_daily"},
        ],
        "relations": [{"sourceAssetId": 901, "targetAssetId": 900}],
        "compilerGraph": {"assets": [
            {"platform": "gzlakehouse", "directions": [sink], "connection": {"id": "1360"},
             "attributes": {"qualifiedName": "MFG_OPS.OEE_DAILY"}},
        ]},
    }})
}

/// `SELECT * … LIMIT 0` gives the columns of the snapshot it reads; the
/// count query gives five added rows; samples give one row.
fn query_reply(stub: Stub, query: &str) -> Value {
    let page = |schema: Value, data: Value| {
        json!({"status": 200, "queryExecutionId": "q1", "response": {
            "schema": schema, "data": data, "pageNumber": 1, "hasMore": false, "totalRecords": 1,
        }})
    };
    if query.ends_with("LIMIT 0") {
        let branch = query.contains("VERSION AS OF 13");
        let amount = if branch && stub.retyped {
            "string"
        } else {
            "int"
        };
        return page(
            json!([{"name": "id", "type": "long"}, {"name": "amount", "type": amount}]),
            json!([]),
        );
    }
    if query.contains("AS added") {
        return page(
            json!([{"name": "added", "type": "long"}, {"name": "removed", "type": "long"}]),
            json!([{"added": 5, "removed": 0}]),
        );
    }
    page(
        json!([{"name": "id", "type": "long"}, {"name": "amount", "type": "int"}]),
        json!([{"id": 1, "amount": 2}]),
    )
}

fn reply(stub: Stub, state: &mut State, request: &Recorded) -> Option<Value> {
    let path = request.target.split('?').next().unwrap_or_default();
    let branch_path = |what: &str| state.branch.as_ref().map(|b| format!("{TABLE}/{what}/{b}"));
    let run_status = if stub.run_fails { "Failed" } else { "Success" };
    Some(match (request.method.as_str(), path) {
        ("GET", "/etl/etlProjects") => json!([{"Id": 1138, "Name": "Manufacturing"}]),
        ("GET", "/etl/etlJobs/1138/jobs") => json!([{"Id": 1324, "Name": "build-oee"}]),
        ("GET", "/etl/lineage/jobs/1324") => lineage(stub),
        ("GET", "/connection/1360") => json!([{"name": "manufacturingops", "type": "GZLAKEHOUSE",
            "config": {"warehouseName": "Manufacturing", "databaseName": "mfg_ops"}}]),
        ("GET", "/etl/job/options") => json!({"writeBranch": {"runtimes": ["spark"]}}),
        ("GET", "/etl/etlJobs/jobs/1138/1324") => json!({
            "config": {"connectionNames": ["manufacturingops"], "minimumWorkers": 2},
            "fileContent": SAVED,
        }),
        ("PUT", p) if p.starts_with(&format!("{TABLE}/write-branches/")) && stub.refuse_branch => {
            return None;
        }
        ("PUT", p) if p.starts_with(&format!("{TABLE}/write-branches/")) => {
            state.branch = p.rsplit('/').next().map(str::to_string);
            json!({"status": 201, "response": {"writable": true}})
        }
        ("POST", "/etl/job") => json!({"jobIdentifier": "run-1", "writeBranch": state.branch}),
        ("POST", "/etl/jobStatus") => json!({"success": true, "data": [{"jobstatus": run_status}]}),
        ("GET", p) if p == format!("{TABLE}/versions") => {
            json!({"status": 200, "response": versions(state)})
        }
        ("DELETE", p) if Some(p.to_string()) == branch_path("branches") => {
            state.branch = None;
            json!({"status": 200, "response": {"deleted": true}})
        }
        ("GET", "/lakehouse/warehouse") => {
            json!({"status": 200, "response": [{"id": 1114, "name": "Manufacturing"}]})
        }
        ("GET", "/rte/compute") => json!([{"Id": 1000, "Name": "mini", "Description": "Mini"}]),
        ("GET", "/lakehouse/sessionstate/kept-1") => {
            json!({"status": 200, "response": {"sessionStatus": "Running"}})
        }
        ("POST", "/lakehouse/startsession") => {
            json!({"status": 200, "response": {"sessionId": "s-new"}})
        }
        ("POST", "/lakehouse/submitquery") => {
            query_reply(stub, request.body["query"].as_str().unwrap_or_default())
        }
        ("POST", "/lakehouse/stopsession") => json!({"status": 200, "response": {}}),
        _ => return None,
    })
}

fn stub_providers(stub: Stub) -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    let state = Arc::new(Mutex::new(State::default()));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            seen.lock().unwrap().push(request.clone());
            match reply(stub, &mut state.lock().unwrap(), &request) {
                Some(body) => respond(&mut stream, "200 OK", "application/json", &body.to_string()),
                None => respond(&mut stream, "404 Not Found", "application/json", "{}"),
            }
        }
    });
    (base, log)
}

/// A synced folder with build-oee's file edited.
fn folder(name: &str, code: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-review-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("jobs")).unwrap();
    std::fs::write(
        dir.join("jobs/gz-sync.toml"),
        "[jobs]\n\"build_oee.py\" = \"build-oee\"\n\n[impact]\nproject = 1138\n",
    )
    .unwrap();
    std::fs::write(dir.join("jobs/build_oee.py"), code).unwrap();
    std::fs::write(dir.join("jobs/README.md"), "notes").unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "review-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .args(args)
        .output()
        .unwrap()
}

const TRY_FLAGS: &[&str] = &["--compute", "1003", "--rte", "gz-rte-spark-pytorch:1.0.0"];

fn review(dir: &Path, base: &str, extra: &[&str]) -> Output {
    let mut args = vec![
        "-o",
        "json",
        "review",
        "--dir",
        "jobs",
        "--sql-compute",
        "mini",
    ];
    args.extend_from_slice(TRY_FLAGS);
    args.extend_from_slice(extra);
    gz(dir, base, &args)
}

fn shown(out: &Output) -> Value {
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::Deserializer::from_str(&text)
        .into_iter::<Value>()
        .next()
        .unwrap_or_else(|| panic!("no JSON: {}{}", text, plain(&out.stderr)))
        .unwrap()
}

fn requests(log: &Arc<Mutex<Vec<Recorded>>>, method: &str) -> Vec<Recorded> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|r| r.method == method)
        .cloned()
        .collect()
}

fn said(out: &Output) -> String {
    format!("{}{}", plain(&out.stdout), plain(&out.stderr))
}

#[test]
fn a_changed_job_is_tried_compared_and_cleaned_up_and_passes() {
    let (base, log) = stub_providers(Stub::default());
    let dir = folder("pass", LOCAL);
    let out = review(&dir, &base, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", said(&out));
    let value = shown(&out);
    assert_eq!(value["verdict"], json!("pass"), "{value:#}");
    let job = &value["jobs"][0];
    assert_eq!(job["job"], json!("build-oee"));
    assert!(job["code"]["diff"]
        .as_str()
        .unwrap()
        .contains("+print('changed')"));
    assert_eq!(job["impact"]["writes"], json!(["mfg_ops.oee_daily"]));
    let run = requests(&log, "POST")
        .into_iter()
        .find(|r| r.target == "/etl/job")
        .expect("a try run");
    assert_eq!(run.body["code"], json!(LOCAL), "the local code runs");
    let branch = run.body["writeBranch"].as_str().unwrap().to_string();
    assert!(branch.starts_with("review-1324-"), "{branch}");
    assert_eq!(job["rows"][0]["status"], json!("done"), "{job:#}");
    assert_eq!(job["rows"][0]["rows"]["added"], json!(5));
    let deletes: Vec<String> = requests(&log, "DELETE")
        .into_iter()
        .map(|r| r.target)
        .collect();
    assert_eq!(deletes, vec![format!("{TABLE}/branches/{branch}")]);
    assert_eq!(job["cleanup"]["status"], json!("done"));
    assert_eq!(
        requests(&log, "POST")
            .iter()
            .filter(|r| r.target == "/lakehouse/stopsession")
            .count(),
        1,
        "the session review started is stopped"
    );
}

#[test]
fn a_failed_run_fails_with_exit_2_and_its_branch_is_still_dropped() {
    let (base, log) = stub_providers(Stub {
        run_fails: true,
        ..Stub::default()
    });
    let dir = folder("fail", LOCAL);
    let out = review(&dir, &base, &[]);
    assert_eq!(out.status.code(), Some(2), "{}", said(&out));
    let value = shown(&out);
    assert_eq!(value["verdict"], json!("fail"));
    assert!(value["jobs"][0]["reasons"][0]["text"]
        .as_str()
        .unwrap()
        .contains("ended with status Failed"));
    assert_eq!(
        requests(&log, "DELETE").len(),
        1,
        "cleanup runs after a failed run"
    );
    assert!(
        !requests(&log, "POST")
            .iter()
            .any(|r| r.target == "/lakehouse/submitquery"),
        "rows of a failed run are not compared"
    );
}

#[test]
fn keep_branches_leaves_the_branch() {
    let (base, log) = stub_providers(Stub::default());
    let dir = folder("keep", LOCAL);
    let out = review(&dir, &base, &["--keep-branches", "--no-rows"]);
    assert_eq!(out.status.code(), Some(0), "{}", said(&out));
    assert!(requests(&log, "DELETE").is_empty());
    let cleanup = &shown(&out)["jobs"][0]["cleanup"];
    assert!(
        cleanup["reason"]
            .as_str()
            .unwrap()
            .contains("gz jobs discard 1324"),
        "{cleanup}"
    );
}

#[test]
fn a_retyped_column_warns_and_strict_exits_3() {
    let (base, _) = stub_providers(Stub {
        retyped: true,
        ..Stub::default()
    });
    let dir = folder("strict", LOCAL);
    let out = review(&dir, &base, &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "warn passes without --strict: {}",
        said(&out)
    );
    assert_eq!(shown(&out)["verdict"], json!("warn"));
    let (base, _) = stub_providers(Stub {
        retyped: true,
        ..Stub::default()
    });
    let out = review(&dir, &base, &["--strict"]);
    assert_eq!(out.status.code(), Some(3), "{}", said(&out));
    let reasons = shown(&out)["jobs"][0]["reasons"].to_string();
    assert!(reasons.contains("amount: int → string"), "{reasons}");
}

#[test]
fn a_job_without_lakehouse_output_is_reported_and_its_other_steps_run() {
    let (base, log) = stub_providers(Stub {
        no_outputs: true,
        ..Stub::default()
    });
    let dir = folder("no-output", LOCAL);
    let out = review(&dir, &base, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", said(&out));
    let job = &shown(&out)["jobs"][0];
    assert_eq!(job["verdict"], json!("warn"));
    assert_eq!(job["try"]["status"], json!("skipped"));
    assert!(job["try"]["reason"]
        .as_str()
        .unwrap()
        .contains("no lakehouse table"));
    assert_eq!(job["impact"]["status"], json!("done"), "impact still runs");
    assert!(job["code"]["diff"].is_string(), "the diff still shows");
    assert!(requests(&log, "PUT").is_empty() && requests(&log, "DELETE").is_empty());
}

#[test]
fn without_compute_jobs_are_not_tried_and_the_verdict_warns() {
    let (base, log) = stub_providers(Stub::default());
    let dir = folder("no-compute", LOCAL);
    let out = gz(&dir, &base, &["-o", "json", "review", "--dir", "jobs"]);
    assert_eq!(out.status.code(), Some(0), "{}", said(&out));
    let job = &shown(&out)["jobs"][0];
    assert_eq!(job["verdict"], json!("warn"));
    assert!(job["try"]["reason"]
        .as_str()
        .unwrap()
        .contains("--compute and --rte"));
    assert!(!requests(&log, "POST")
        .iter()
        .any(|r| r.target == "/etl/job"));
}

#[test]
fn a_running_session_gz_sql_remembers_is_reused_and_left_running() {
    let (base, log) = stub_providers(Stub::default());
    let dir = folder("session", LOCAL);
    let config = dir.join("config/gz");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        "current_profile = \"review-test\"\n\n[profiles.review-test]\nsql_session = \"kept-1\"\nsql_session_warehouse = \"Manufacturing\"\nsql_compute = \"mini\"\n",
    )
    .unwrap();
    let out = review(&dir, &base, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", said(&out));
    let posts = requests(&log, "POST");
    assert!(
        posts.iter().any(|r| r.target == "/lakehouse/submitquery"),
        "rows were compared"
    );
    assert!(!posts.iter().any(|r| r.target == "/lakehouse/startsession"));
    assert!(
        !posts.iter().any(|r| r.target == "/lakehouse/stopsession"),
        "not review's to stop"
    );
}

#[test]
fn only_changed_jobs_are_reviewed_and_unchanged_given_files_say_so() {
    let (base, log) = stub_providers(Stub::default());
    let dir = folder("unchanged", SAVED);
    let out = gz(&dir, &base, &["review", "--dir", "jobs"]);
    assert_eq!(out.status.code(), Some(0), "{}", said(&out));
    assert!(
        said(&out).contains("No changed job to review"),
        "{}",
        said(&out)
    );
    let out = gz(
        &dir,
        &base,
        &[
            "-o",
            "json",
            "review",
            "--dir",
            "jobs",
            "jobs/build_oee.py",
            "jobs/README.md",
        ],
    );
    let value = shown(&out);
    assert_eq!(value["jobs"][0]["code"]["state"], json!("unchanged"));
    assert_eq!(value["unmapped"], json!(["jobs/README.md"]));
    assert!(requests(&log, "PUT").is_empty());
}

#[test]
fn markdown_is_for_a_pull_request_comment() {
    let (base, _) = stub_providers(Stub::default());
    let dir = folder("markdown", LOCAL);
    let mut args = vec![
        "review",
        "--dir",
        "jobs",
        "--no-rows",
        "--format",
        "markdown",
    ];
    args.extend_from_slice(TRY_FLAGS);
    let out = gz(&dir, &base, &args);
    assert_eq!(out.status.code(), Some(0), "{}", said(&out));
    let md = String::from_utf8_lossy(&out.stdout);
    assert!(
        md.starts_with("<!-- gz-review -->\n## gz review: **PASS** for project 1138: 1 job"),
        "{md}"
    );
    assert!(md.contains("```diff"), "{md}");
    let both = gz(
        &dir,
        &base,
        &[
            "-o", "json", "review", "--dir", "jobs", "--format", "markdown",
        ],
    );
    assert_eq!(both.status.code(), Some(1));
}

#[test]
fn a_branch_the_lakehouse_refuses_is_a_gz_error_with_nothing_to_clean_up() {
    let (base, log) = stub_providers(Stub {
        refuse_branch: true,
        ..Stub::default()
    });
    let dir = folder("refused-branch", LOCAL);
    let out = review(&dir, &base, &[]);
    assert_eq!(out.status.code(), Some(1), "{}", said(&out));
    let job = &shown(&out)["jobs"][0];
    assert_eq!(job["try"]["status"], json!("error"));
    assert!(job["try"]["reason"]
        .as_str()
        .unwrap()
        .contains("cannot make branch"));
    assert_eq!(job["cleanup"]["status"], json!("notRun"), "{job:#}");
    assert!(requests(&log, "DELETE").is_empty());
}
