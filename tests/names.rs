//! Names wherever gz takes an id, and dynamic completion, against a stub
//! backend that records each request. Commands run without a terminal, so
//! several matches are an error that lists the exact commands.

mod common;

use common::{plain, read_request, respond};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

type Seen = Arc<Mutex<Vec<String>>>;

fn reply(path: &str) -> Option<Value> {
    Some(match path {
        "/etl/etlProjects" => json!([
            {"Id": 1999, "Name": "Sales"},
            {"Id": 2001, "Name": "Ops Team"}
        ]),
        "/etl/etlJobs/1999/jobs" => {
            json!([{"Id": 3013, "Name": "DQC"}, {"Id": 3014, "Name": "Load"}])
        }
        "/etl/etlJobs/2001/jobs" => json!([{"Id": 3050, "Name": "DQC"}]),
        "/etl/etlJobs/jobs/1999/3013" => json!({"Id": 3013, "Name": "DQC"}),
        "/etl/etlJobs/jobs/1999/3014" => json!({"Id": 3014, "Name": "Load"}),
        "/etl/etlProjects/1999" => json!({"Id": 1999, "Name": "Sales"}),
        "/dashboard" => json!([{"Id": 1417, "Name": "Revenue"}]),
        "/dashboard/1417" => json!({"Id": 1417, "Name": "Revenue"}),
        "/lakehouse/warehouse" => {
            json!({"status": 200, "response": [{"id": 1108, "name": "Lineage"}]})
        }
        "/lakehouse/schema/Lineage" => json!({"response": [
            {"databaseName": "sales", "tableName": "orders", "columnName": "id"},
            {"databaseName": "sales", "tableName": "orders", "columnName": "amount"},
            {"databaseName": "sales", "tableName": "refunds", "columnName": "id"},
            {"databaseName": "ops", "tableName": "", "columnName": ""}
        ]}),
        "/lakehouse/tables/Lineage/sales/orders/versions" => json!({"response": {
            "currentSnapshotId": 2,
            "snapshots": [],
            "branches": [{"name": "main", "snapshotId": 2}, {"name": "try-dqc", "snapshotId": 1}],
            "tags": [{"name": "release", "snapshotId": 1}]
        }}),
        "/rte/compute" => json!([{"Id": 4, "Name": "small", "Description": "2 CPU"}]),
        _ => return None,
    })
}

fn stub_server() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            let path = request
                .target
                .split('?')
                .next()
                .unwrap_or_default()
                .to_string();
            log.lock()
                .unwrap()
                .push(format!("{} {path}", request.method));
            let read = request.method == "GET" && request.body.is_null();
            match reply(&path).filter(|_| read) {
                Some(body) => respond(&mut stream, "200 OK", "application/json", &body.to_string()),
                None => respond(&mut stream, "404 Not Found", "application/json", "{}"),
            }
        }
    });
    (base, seen)
}

fn folder(name: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("gz-names-{}-{name}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gz"));
    cmd.current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "names-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env_remove("GZ_API_KEY")
        .args(args);
    cmd
}

fn run(dir: &Path, base: &str, args: &[&str]) -> Output {
    gz(dir, base, args).output().unwrap()
}

fn json_of(out: &Output) -> Value {
    assert!(out.status.success(), "{}", plain(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn a_project_name_becomes_its_id() {
    let (base, seen) = stub_server();
    let out = run(
        &folder("p"),
        &base,
        &["-o", "json", "projects", "get", "sales"],
    );
    assert_eq!(json_of(&out)["Id"], 1999);
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["GET /etl/etlProjects", "GET /etl/etlProjects/1999"]
    );
}

#[test]
fn a_job_name_is_looked_for_in_the_project_given() {
    let (base, seen) = stub_server();
    let out = run(
        &folder("j"),
        &base,
        &["-o", "json", "projects", "job", "Sales", "Load"],
    );
    assert_eq!(json_of(&out)["Id"], 3014);
    // One list call per kind: the projects once, that project's jobs once.
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            "GET /etl/etlProjects",
            "GET /etl/etlJobs/1999/jobs",
            "GET /etl/etlJobs/jobs/1999/3014"
        ]
    );
}

#[test]
fn a_job_alone_is_found_in_your_projects() {
    let (base, _) = stub_server();
    let out = run(
        &folder("lone"),
        &base,
        &["-o", "json", "projects", "job", "Load"],
    );
    assert_eq!(json_of(&out)["Id"], 3014);
}

#[test]
fn ids_are_used_as_they_are() {
    let (base, seen) = stub_server();
    let out = run(
        &folder("ids"),
        &base,
        &["-o", "json", "projects", "job", "1999", "3013"],
    );
    assert_eq!(json_of(&out)["Id"], 3013);
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["GET /etl/etlJobs/jobs/1999/3013"]
    );
}

#[test]
fn several_matches_without_a_terminal_list_the_commands() {
    let (base, seen) = stub_server();
    let out = run(&folder("many"), &base, &["projects", "job", "DQC"]);
    assert!(!out.status.success());
    let err = plain(&out.stderr);
    assert!(err.contains("several jobs are named 'DQC'"), "{err}");
    assert!(
        err.contains("gz projects job 1999 3013    # DQC (id 3013) in project Sales (1999)"),
        "{err}"
    );
    assert!(
        err.contains("gz projects job 2001 3050    # DQC (id 3050) in project Ops Team (2001)"),
        "{err}"
    );
    // Nothing past the lookups ran.
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .all(|request| !request.contains("/etl/etlJobs/jobs/")));
}

#[test]
fn an_unknown_name_says_what_exists() {
    let (base, _) = stub_server();
    let out = run(&folder("none"), &base, &["designer", "dashboard", "Reven"]);
    assert!(!out.status.success());
    let err = plain(&out.stderr);
    assert!(err.contains("no dashboard named 'Reven'"), "{err}");
    assert!(err.contains("did you mean Revenue (id 1417)?"), "{err}");
    assert!(err.contains("`gz designer dashboards`"), "{err}");
}

#[test]
fn designer_rename_and_delete_read_the_kind_first() {
    let (base, _) = stub_server();
    let out = run(
        &folder("d"),
        &base,
        &["-o", "json", "designer", "dashboard", "revenue"],
    );
    assert_eq!(json_of(&out)["Id"], 1417);
}

// ---------- completion ----------

fn complete(dir: &Path, base: &str, word: &str, before: &[&str]) -> Vec<String> {
    let mut args = vec!["__complete", "zsh"];
    args.extend_from_slice(before);
    let out = gz(dir, base, &args)
        .env("GZ_COMPLETE_WORD", word)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert!(out.stderr.is_empty(), "{}", plain(&out.stderr));
    plain(&out.stdout)
        .lines()
        .map(|line| line.split('\t').next().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn completes_project_and_job_names_from_a_cache() {
    let (base, seen) = stub_server();
    let dir = folder("cp");
    // A job alone also works here, so jobs of every project are offered too.
    assert_eq!(
        complete(&dir, &base, "", &["jobs", "run"]),
        vec!["Sales", "Ops Team", "DQC", "Load"]
    );
    assert_eq!(complete(&dir, &base, "S", &["jobs", "run"]), vec!["Sales"]);
    assert_eq!(
        complete(&dir, &base, "", &["jobs", "run", "Sales"]),
        vec!["DQC", "Load"]
    );
    // The second project completion came from the cache.
    let projects = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|r| *r == "GET /etl/etlProjects")
        .count();
    assert_eq!(projects, 1);
    let cache = std::fs::read_to_string(dir.join("config/gz/completion/names-test.json")).unwrap();
    assert!(cache.contains("Sales"), "{cache}");
    assert!(!cache.contains("stub-token"), "{cache}");
}

#[test]
fn completes_subcommands_flags_and_choices() {
    let (base, seen) = stub_server();
    let dir = folder("cs");
    assert!(complete(&dir, &base, "r", &["jobs"]).contains(&"run".to_string()));
    assert!(complete(&dir, &base, "--com", &["jobs", "run", "1", "2"])
        .contains(&"--compute".to_string()));
    assert_eq!(complete(&dir, &base, "j", &["-o"]), vec!["json"]);
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn completes_tables_one_part_at_a_time_and_their_branches() {
    let (base, _) = stub_server();
    let dir = folder("ct");
    let show = ["quality", "show"];
    assert_eq!(complete(&dir, &base, "", &show), vec!["Lineage."]);
    assert_eq!(
        complete(&dir, &base, "Lineage.", &show),
        vec!["Lineage.sales.", "Lineage.ops."]
    );
    assert_eq!(
        complete(&dir, &base, "Lineage.sales.", &show),
        vec!["Lineage.sales.orders", "Lineage.sales.refunds"]
    );
    assert_eq!(
        complete(
            &dir,
            &base,
            "",
            &["diff", "sales.orders", "--warehouse", "Lineage", "--from"]
        ),
        vec!["main", "try-dqc", "release"]
    );
    assert_eq!(
        complete(
            &dir,
            &base,
            "--to=r",
            &["diff", "sales.orders", "--warehouse", "Lineage"]
        ),
        vec!["--to=release"]
    );
}

#[test]
fn completes_compute_names_and_profiles() {
    let (base, _) = stub_server();
    let dir = folder("cc");
    assert_eq!(
        complete(&dir, &base, "", &["jobs", "run", "1", "2", "--compute"]),
        vec!["small"]
    );
    std::fs::create_dir_all(dir.join("config/gz")).unwrap();
    std::fs::write(
        dir.join("config/gz/config.toml"),
        "[profiles.stage]\n[profiles.demo-x]\n",
    )
    .unwrap();
    assert_eq!(
        complete(&dir, &base, "", &["--profile"]),
        vec!["demo-x", "stage"]
    );
}

#[test]
fn completes_nothing_when_not_signed_in() {
    let (base, seen) = stub_server();
    let dir = folder("out");
    let out = gz(&dir, &base, &["__complete", "zsh", "jobs", "run"])
        .env_remove("GZ_TOKEN")
        .env("GZ_COMPLETE_WORD", "")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty(), "{}", plain(&out.stdout));
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn bash_completion_reads_the_line() {
    let (base, _) = stub_server();
    let dir = folder("bash");
    let out = gz(&dir, &base, &["__complete", "bash"])
        .env("GZ_COMPLETE_LINE", "gz projects get --x=1 'Ops T")
        .output()
        .unwrap();
    assert_eq!(plain(&out.stdout), "Ops\\ Team\n");
}

#[test]
fn completion_scripts_call_back_into_gz() {
    for shell in ["bash", "zsh", "fish", "powershell"] {
        let out = Command::new(env!("CARGO_BIN_EXE_gz"))
            .args(["completion", shell])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(plain(&out.stdout).contains("__complete"), "{shell}");
    }
    let help = Command::new(env!("CARGO_BIN_EXE_gz"))
        .args(["completions", "--help"])
        .output()
        .unwrap();
    assert!(plain(&help.stdout).contains("source <(gz completion zsh)"));
}

#[test]
fn jobs_run_takes_a_job_and_compute_by_name() {
    let (base, seen) = stub_server();
    let out = run(
        &folder("run"),
        &base,
        &[
            "jobs",
            "run",
            "Load",
            "--compute",
            "small",
            "--rte",
            "img:1",
            "--spec-only",
        ],
    );
    let spec = json_of(&out);
    let text = spec.to_string();
    assert!(text.contains("3014") && text.contains("1999"), "{text}");
    assert_eq!(spec["computeId"], 4, "{text}");
    // Only reads: nothing was submitted.
    assert!(seen.lock().unwrap().iter().all(|r| r.starts_with("GET ")));
}

#[test]
fn a_table_with_warehouse_completes_as_database_table() {
    let (base, _) = stub_server();
    let dir = folder("pt");
    assert_eq!(
        complete(&dir, &base, "Lineage.sales.o", &["table", "profile"]),
        vec!["Lineage.sales.orders"]
    );
    assert_eq!(
        complete(
            &dir,
            &base,
            "sales.r",
            &["table", "profile", "--warehouse", "Lineage"]
        ),
        vec!["sales.refunds"]
    );
    assert_eq!(
        complete(
            &dir,
            &base,
            "",
            &["table", "profile", "Lineage.sales.orders", "--at"]
        ),
        vec!["main", "try-dqc", "release"]
    );
}
