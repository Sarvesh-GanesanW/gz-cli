//! `gz open --print` against a stub backend: each kind is looked up with
//! the backend's own shapes and becomes the web app's route. Only --print
//! and -o json are run here, so no test ever starts a browser.

mod common;

use common::{plain, read_request, respond};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

const WEB: &str = "https://acme.example.test";

fn reply(path: &str) -> Option<Value> {
    Some(match path {
        "/etl/etlProjects" => {
            json!([{"Id": 1510, "Name": "Invoice Extractor"}, {"Id": 2098, "Name": "RCM"}])
        }
        "/etl/etlJobs/1510/jobs" => json!([{"Id": 2125, "Name": "ChallanReceiptBulk"}]),
        "/etl/etlJobs/2098/jobs" => json!([]),
        "/search" => json!({"results": [{"kind": "agent", "id": 696, "name": "rcm-bot",
            "parentId": 2098, "context": "RCM", "routeKey": "1a37e97d-893f-47ea-af39-ef5ba85b6128"}]}),
        "/etl/consumption-metrics/run-1" => json!({"identity": {"executionId": "run-1",
            "jobId": 2125, "projectId": 1510, "workloadType": "job"}}),
        "/lakehouse/warehouse" => {
            json!({"status": 200, "response": [{"id": 1108, "name": "Lineage"}]})
        }
        "/dashboard" => json!([{"Id": 1417, "Name": "persistency"}]),
        "/jupyter/1510/notebook" => json!({"success": true, "notebooks": []}),
        "/jupyter/2098/notebook" => {
            json!({"success": true, "notebooks": [{"id": 1177, "name": "Denial Risk"}]})
        }
        "/chat/conversations" => json!({"conversations": [
            {"conversationId": "ab02311f-1e17-43e4-a830-899363aad9a6", "conversationName": "Orders cleanup"}
        ]}),
        _ => return None,
    })
}

fn stub_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            let path = request.target.split('?').next().unwrap_or_default();
            // Every lookup is a read; a body would mean gz sent a change.
            let read = request.method == "GET" && request.body.is_null();
            match reply(path).filter(|_| read) {
                Some(body) => respond(&mut stream, "200 OK", "application/json", &body.to_string()),
                None => respond(&mut stream, "404 Not Found", "application/json", "{}"),
            }
        }
    });
    base
}

fn folder() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-open-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "open-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .env("GZ_WEB_URL", WEB)
        .args(args)
        .output()
        .unwrap()
}

fn printed(base: &str, args: &[&str]) -> String {
    let mut all = vec!["open"];
    all.extend_from_slice(args);
    all.push("--print");
    let out = gz(&folder(), base, &all);
    assert!(out.status.success(), "{args:?}: {}", plain(&out.stderr));
    plain(&out.stdout).trim().to_string()
}

#[test]
fn every_kind_prints_its_web_app_route() {
    let base = stub_server();
    let cases: &[(&[&str], &str)] = &[
        (&[], "/"),
        (
            &["project", "Invoice Extractor"],
            "/etl/projectId/1510/projectName/Invoice%20Extractor",
        ),
        (
            &["job", "ChallanReceiptBulk"],
            "/etl/code-editor/2125?projectId=1510&jobId=2125",
        ),
        (
            &["agent", "rcm-bot"],
            "/etl/projectId/2098/projectName/RCM/agents/1a37e97d-893f-47ea-af39-ef5ba85b6128",
        ),
        (
            &["run", "run-1"],
            "/etl/code-editor/2125?projectId=1510&jobId=2125",
        ),
        (
            &["table", "Lineage.sales.orders"],
            "/lakehouse/code-editor/TGluZWFnZSYxMTA4",
        ),
        (&["dashboard", "1417"], "/designer/dashboards/1417"),
        (
            &["notebook", "denial risk"],
            "/etl/notebook/Denial%20Risk?projectId=2098&notebookId=1177",
        ),
        (
            &["chat", "Orders cleanup"],
            "/conversation/ab02311f-1e17-43e4-a830-899363aad9a6",
        ),
    ];
    for (args, path) in cases {
        assert_eq!(printed(&base, args), format!("{WEB}{path}"), "{args:?}");
    }
}

#[test]
fn says_when_the_page_is_not_exactly_the_thing() {
    let base = stub_server();
    let out = gz(&folder(), &base, &["open", "run", "run-1", "--print"]);
    assert!(
        plain(&out.stderr).contains("no page for a single run"),
        "{}",
        plain(&out.stderr)
    );
    let out = gz(
        &folder(),
        &base,
        &["open", "table", "Lineage.sales.orders", "-o", "json"],
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value["note"]
        .as_str()
        .unwrap()
        .contains("no page for one table"));
}

#[test]
fn refuses_what_does_not_exist() {
    let base = stub_server();
    let out = gz(&folder(), &base, &["open", "dashboard", "99", "--print"]);
    assert!(!out.status.success());
    assert!(plain(&out.stderr).contains("no dashboard named or numbered '99'"));
    let out = gz(&folder(), &base, &["open", "job", "--print"]);
    assert!(!out.status.success());
}
