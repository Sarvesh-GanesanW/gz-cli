//! `gz impact` against a stub lineage: tables found through the etl
//! provider's catalog, the downstream walk (depth and a cycle), datasets,
//! visualizations, dashboards and agents on top, and `--git` in a real git
//! repository with a `gz sync` folder, including the exit code CI gates on.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// raw (0) → sales.orders (1) by job load-orders (50); orders → sales.daily
/// (2) by build-daily (60); daily → sales.weekly (3) and weekly → daily (a
/// cycle; load-orders also reads orders); daily → dataset Revenue (900) on dashboard Exec (300), whose
/// visualization 400 charts Revenue and 401 something else. Ontology model
/// 7 is grounded on weekly; agent Analyst uses it.
fn asset(id: i64, kind: &str, name: &str) -> Value {
    json!({"assetId": id, "assetType": kind, "displayName": name, "qualifiedName": name, "owner": "ana"})
}

fn view(center: i64) -> Value {
    let versions = json!([
        {"jobVersionId": 500, "jobId": 60, "jobName": "build-daily", "owner": "bo"},
        {"jobVersionId": 501, "datasetId": 900, "jobName": "Revenue", "owner": "cy"},
        {"jobVersionId": 502, "jobId": 70, "jobName": "build-weekly", "owner": "bo"},
        {"jobVersionId": 503, "jobId": 50, "jobName": "load-orders", "owner": "bo"},
    ]);
    let graph = match center {
        1 => json!({
            "assets": [asset(1, "TABLE", "sales.orders"), asset(2, "TABLE", "sales.daily")],
            "relations": [
                {"sourceAssetId": 1, "targetAssetId": 2, "sourceColumn": "amount", "targetColumn": "total", "jobVersionId": 500},
                {"sourceAssetId": 1, "targetAssetId": 2, "sourceColumn": "region", "targetColumn": "region", "jobVersionId": 500},
            ],
            // load-orders merges into orders, so it reads it too.
            "assetWorkloads": [{"assetId": 1, "jobVersionId": 503, "role": "SOURCE"}],
        }),
        2 => json!({
            "assets": [
                asset(2, "TABLE", "sales.daily"),
                asset(3, "TABLE", "sales.weekly"),
                {"assetId": 20, "assetType": "DATASET", "displayName": "Revenue", "qualifiedName": "dataset:900", "owner": "cy"},
            ],
            "relations": [
                {"sourceAssetId": 2, "targetAssetId": 3, "jobVersionId": 502},
                {"sourceAssetId": 2, "targetAssetId": 20, "jobVersionId": 501},
            ],
            "dashboards": [{"dashboardId": 300, "name": "Exec", "owner": "dee",
                            "datasetIds": [900], "visualizationIds": [400, 401]}],
        }),
        3 => json!({
            "assets": [asset(3, "TABLE", "sales.weekly"), asset(2, "TABLE", "sales.daily")],
            "relations": [{"sourceAssetId": 3, "targetAssetId": 2, "jobVersionId": 502}],
        }),
        _ => return json!({"message": "Lineage asset not found"}),
    };
    let mut graph = graph;
    graph["centerAssetId"] = json!(center);
    graph["jobVersions"] = versions;
    json!({"graph": graph})
}

fn reply_for(request: &Recorded) -> Option<Value> {
    let path = request.target.split('?').next().unwrap_or_default();
    let query = request.target.split_once('?').map(|(_, q)| q).unwrap_or("");
    Some(match (request.method.as_str(), path) {
        ("GET", "/etl/lineage/connections") => json!({"connections": [
            {"connectionId": 5, "assetTypes": [{"assetType": "TABLE", "count": 3}]},
            {"connectionId": 6, "assetTypes": [{"assetType": "OBJECT", "count": 1}]},
        ]}),
        ("GET", "/etl/lineage/assets") => {
            assert!(
                query.contains("connectionId=5"),
                "only table connections: {query}"
            );
            json!({"assets": [
                {"assetId": 1, "qualifiedName": "\"sales\".\"orders\""},
                {"assetId": 99, "qualifiedName": "sales.orders_archive"},
            ]})
        }
        ("GET", "/etl/lineage/assets/1/impact") => json!({"definitions": [
            {"modelId": 7, "modelName": "Sales model", "groundedAssetId": 3},
        ]}),
        ("GET", "/etl/lineage/jobs/50") => json!({"graph": {
            "assets": [asset(0, "OBJECT", "raw.csv"), asset(1, "TABLE", "sales.orders")],
            "relations": [{"sourceAssetId": 0, "targetAssetId": 1}],
        }}),
        ("GET", "/etl/etlJobs/7/jobs") => json!([
            {"Id": 50, "Name": "load-orders"},
            {"Id": 60, "Name": "build-daily"},
        ]),
        ("GET", "/visualization") => {
            json!([{"Id": 400, "Name": "Revenue by day", "created_by": "eve"}])
        }
        ("GET", "/visualization/400") => {
            json!({"Id": 400, "Name": "Revenue by day", "config": {"chartConfig": {"datasetName": 900}}})
        }
        ("GET", "/visualization/401") => {
            json!({"Id": 401, "Name": "Costs", "config": {"chartConfig": {"datasetName": "12"}}})
        }
        ("GET", "/v1/agents") => json!({"items": [
            {"id": "ag-1", "name": "Analyst", "createdBy": "fay",
             "metadata": {"groundzeroRuntime": {"ontologyModelIds": [7]}}},
            {"id": "ag-2", "name": "Other", "metadata": {}},
        ], "limit": 100, "offset": 0}),
        ("GET", p) if p.starts_with("/etl/lineage/assets/") => {
            let id: i64 = p.trim_start_matches("/etl/lineage/assets/").parse().ok()?;
            view(id)
        }
        _ => return None,
    })
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
            match reply_for(&request) {
                Some(reply) => respond(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    &reply.to_string(),
                ),
                None => respond(&mut stream, "404 Not Found", "application/json", "{}"),
            }
        }
    });
    (base, log)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-impact-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gz"));
    cmd.current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join(".config"))
        .env("GZ_PROFILE", "impact-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .env_remove("GZ_API_KEY");
    cmd
}

fn json_of(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}\n{}", plain(&out.stdout), plain(&out.stderr)))
}

fn kinds_and_names(report: &Value) -> Vec<(String, String)> {
    report["dependents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["kind"].as_str().unwrap().to_string(),
                d["name"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn a_table_lists_everything_downstream_through_a_cycle() {
    let dir = temp_dir("table");
    let (base, log) = stub();
    let out = gz(&dir, &base)
        .args(["impact", "sales.orders", "-o", "json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let report = json_of(&out);
    assert_eq!(
        report["targets"][0]["assetIds"],
        json!([1]),
        "quoted names match, look-alikes don't"
    );
    assert_eq!(
        kinds_and_names(&report),
        vec![
            ("table".into(), "sales.daily".into()),
            ("table".into(), "sales.weekly".into()),
            ("job".into(), "build-daily".into()),
            ("job".into(), "load-orders".into()),
            ("job".into(), "build-weekly".into()),
            ("dataset".into(), "Revenue".into()),
            ("visualization".into(), "Revenue by day".into()),
            ("dashboard".into(), "Exec".into()),
            ("agent".into(), "Analyst".into()),
        ]
    );
    let deps = report["dependents"].as_array().unwrap();
    assert_eq!(deps[0]["how"], "direct read");
    assert_eq!(deps[0]["writtenBy"], json!(["build-daily"]));
    assert_eq!(deps[1]["how"], "via table sales.daily");
    assert_eq!(deps[1]["depth"], 2);
    assert_eq!(deps[6]["owner"], "eve");
    assert_eq!(deps[7]["how"], "via dataset Revenue");
    assert_eq!(deps[8]["how"], "via ontology model Sales model");
    assert_eq!(
        deps[8]["depth"], 2,
        "as deep as weekly, where the model is grounded"
    );
    assert_eq!(report["counts"]["tables"], 2);

    assert!(
        log.lock()
            .unwrap()
            .iter()
            .all(|r| r.method == "GET" && r.body.is_null()),
        "impact only reads"
    );
    let views: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.target.clone())
        .filter(|t| t.starts_with("/etl/lineage/assets/") && !t.contains("/impact"))
        .collect();
    assert_eq!(
        views.len(),
        3,
        "each table read once, the cycle back to daily never: {views:?}"
    );
    assert!(views[0].contains("upstreamDepth=0") && views[0].contains("downstreamDepth=1"));
}

#[test]
fn depth_one_stops_at_direct_readers() {
    let dir = temp_dir("depth");
    let (base, _log) = stub();
    let out = gz(&dir, &base)
        .args(["impact", "sales.orders", "--depth", "1", "-o", "json"])
        .output()
        .unwrap();
    let report = json_of(&out);
    let names: Vec<String> = kinds_and_names(&report)
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert_eq!(names, vec!["sales.daily", "build-daily", "load-orders"]);
}

#[test]
fn a_column_without_column_lineage_downstream_says_so() {
    let dir = temp_dir("column");
    let (base, _log) = stub();
    let out = gz(&dir, &base)
        .args([
            "impact",
            "sales.orders.amount",
            "--depth",
            "2",
            "-o",
            "json",
        ])
        .output()
        .unwrap();
    let report = json_of(&out);
    let deps = report["dependents"].as_array().unwrap();
    assert_eq!(deps[0]["name"], "sales.daily");
    assert_eq!(deps[0]["columnLevel"], "known");
    assert_eq!(deps[0]["columns"], json!(["total"]));
    assert_eq!(deps[1]["name"], "sales.weekly");
    assert_eq!(
        deps[1]["columnLevel"], "unknown",
        "daily → weekly has table-level lineage only"
    );

    let text = gz(&dir, &base)
        .args(["impact", "sales.orders.amount", "--depth", "2"])
        .output()
        .unwrap();
    let shown = plain(&text.stdout);
    assert!(
        shown.contains("not known (table-level lineage only)"),
        "{shown}"
    );
}

#[test]
fn a_table_lineage_never_saw_is_reported_not_failed() {
    let dir = temp_dir("missing");
    let (base, _log) = stub();
    let out = gz(&dir, &base)
        .args(["impact", "sales.returns", "--fail-on", "dashboards"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        plain(&out.stderr).contains("sales.returns: not in lineage"),
        "{}",
        plain(&out.stderr)
    );
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

/// A repo whose `jobs/` folder syncs with project 7 and fails on
/// dashboards; the branch changes the load-orders job and the README.
fn repo_with_a_changed_job(name: &str) -> PathBuf {
    let dir = temp_dir(name);
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(dir.join("jobs/etl")).unwrap();
    std::fs::write(
        dir.join("jobs/gz-sync.toml"),
        "[jobs]\n\"etl/load_orders.py\" = \"load-orders\"\n\n[impact]\nproject = 7\nfail_on = [\"dashboards\"]\n",
    )
    .unwrap();
    std::fs::write(dir.join("jobs/etl/load_orders.py"), "print(1)\n").unwrap();
    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    git(&dir, &["checkout", "-q", "-b", "change"]);
    std::fs::write(dir.join("jobs/etl/load_orders.py"), "print(2)\n").unwrap();
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(&dir, &["commit", "-qam", "change"]);
    dir
}

#[test]
fn git_mode_fails_a_change_that_reaches_a_protected_dashboard() {
    let dir = repo_with_a_changed_job("git");
    let (base, log) = stub();
    let out = gz(&dir, &base)
        .args(["impact", "--git", "main", "--dir", "jobs"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", plain(&out.stderr));
    let stderr = plain(&out.stderr);
    assert!(
        stderr.contains("affects protected dependents ([impact] fail_on): 1 dashboard (Exec)"),
        "{stderr}"
    );
    let stdout = plain(&out.stdout);
    assert!(
        stdout.contains("load_orders.py → job load-orders writes sales.orders"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Unmapped (no job):") && stdout.contains("README.md"),
        "{stdout}"
    );
    let targets: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.target.clone())
        .collect();
    assert!(
        targets.iter().any(|t| t == "/etl/etlJobs/7/jobs"),
        "{targets:?}"
    );
    assert!(
        targets
            .iter()
            .any(|t| t.starts_with("/etl/lineage/jobs/50?")),
        "{targets:?}"
    );
    assert!(
        !targets
            .iter()
            .any(|t| t.starts_with("/etl/lineage/connections")),
        "a job's tables come as lineage assets; no search"
    );

    // The flag replaces the folder's setting. The changed job reads the
    // table it writes, but it is the change, not a dependent.
    let json = gz(&dir, &base)
        .args([
            "impact",
            "--git",
            "main",
            "--dir",
            "jobs",
            "--fail-on",
            "jobs",
            "-o",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(json.status.code(), Some(2));
    let report = json_of(&json);
    assert_eq!(report["files"][0]["job"], "load-orders");
    assert_eq!(report["files"][0]["jobId"], 50);
    assert_eq!(report["files"][0]["writes"], json!(["sales.orders"]));
    assert_eq!(report["unmapped"].as_array().unwrap().len(), 1);
    let jobs: Vec<&str> = report["dependents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["kind"] == "job")
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert!(!jobs.contains(&"load-orders"), "{jobs:?}");
}

#[test]
fn nothing_protected_affected_exits_zero_and_unmapped_files_are_not_errors() {
    let dir = repo_with_a_changed_job("pass");
    let (base, _log) = stub();
    let out = gz(&dir, &base)
        .args(["impact", "--changed", "README.md", "--dir", "jobs"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", plain(&out.stderr));
    let stdout = plain(&out.stdout);
    assert!(stdout.contains("Unmapped (no job): README.md"), "{stdout}");
    assert!(stdout.contains("No changed file maps to a job"), "{stdout}");

    let unknown = gz(&dir, &base)
        .args(["impact", "--changed", "README.md", "--fail-on", "charts"])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(1));
    assert!(plain(&unknown.stderr).contains("unknown kind 'charts'"));
}
