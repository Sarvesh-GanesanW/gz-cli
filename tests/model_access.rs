//! `gz govern model-access` against a stub chat server: showing the policy
//! and changing one cell. Each gz run has its own config folder; nothing
//! leaves 127.0.0.1.

mod common;

use common::Recorded;
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

const PATH: &str = "/admin/model-read-policy";

fn policy() -> Value {
    json!({
        "groundzero": {"public": "allow", "internal": "allow", "confidential": "allow",
            "restricted": "mask", "unclassified": "allow", "ungoverned": "allow"},
        "customer": {"public": "allow", "internal": "allow", "confidential": "mask",
            "restricted": "deny", "unclassified": "mask", "ungoverned": "mask"}
    })
}

/// A chat server that holds one policy and records every request.
fn serve() -> (u16, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        let mut current = json!({"site": "acme", "isDefault": true, "updatedBy": null,
            "updatedAt": null, "policy": policy()});
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = common::read_request(&mut stream) else {
                continue;
            };
            log.lock().unwrap().push(request.clone());
            if request.target != PATH {
                common::respond(&mut stream, "404 Not Found", "application/json", "{}");
                continue;
            }
            if request.method == "PUT" {
                current["policy"] = request.body.clone();
                current["isDefault"] = json!(false);
                current["updatedBy"] = json!("root");
                current["updatedAt"] = json!("2026-11-11T10:00:00Z");
            }
            common::respond(
                &mut stream,
                "200 OK",
                "application/json",
                &current.to_string(),
            );
        }
    });
    (port, seen)
}

fn config_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-model-access-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(port: u16, name: &str, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gz"));
    for var in ["GZ_DOMAIN", "GZ_AUTH_HOST", "GZ_API_KEY"] {
        cmd.env_remove(var);
    }
    cmd.args(args)
        .env("XDG_CONFIG_HOME", config_dir(name))
        .env("GZ_PROFILE", "model-access-test")
        .env("GZ_HOST", format!("http://127.0.0.1:{port}"))
        .env("GZ_TOKEN", "stub-test-token")
        .env("GZ_SITE", "acme")
        .env("GZ_CLIENT", "dev")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    common::plain(&[out.stdout.as_slice(), out.stderr.as_slice()].concat())
}

#[test]
fn shows_each_level_for_both_kinds_of_model() {
    let (port, seen) = serve();

    let out = gz(port, "show", &["govern", "model-access"]);

    assert!(out.status.success(), "{}", text(&out));
    let shown = text(&out);
    assert!(shown.contains("the defaults"), "{shown}");
    let restricted = shown
        .lines()
        .find(|line| line.contains("restricted"))
        .unwrap_or_default();
    assert!(
        restricted.contains("mask") && restricted.contains("deny"),
        "{shown}"
    );
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        (requests[0].method.as_str(), requests[0].target.as_str()),
        ("GET", PATH)
    );
}

#[test]
fn set_sends_the_whole_policy_with_one_cell_changed() {
    let (port, seen) = serve();

    let out = gz(
        port,
        "set",
        &[
            "--json",
            "govern",
            "model-access",
            "set",
            "customer",
            "confidential",
            "deny",
            "--yes",
        ],
    );

    assert!(out.status.success(), "{}", text(&out));
    let requests = seen.lock().unwrap();
    let methods: Vec<&str> = requests.iter().map(|r| r.method.as_str()).collect();
    assert_eq!(methods, ["GET", "PUT"]);
    let mut expected = policy();
    expected["customer"]["confidential"] = json!("deny");
    assert_eq!(requests[1].body, expected);
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(reply["updatedBy"], "root");
}

#[test]
fn set_refuses_without_confirmation_when_it_cannot_ask() {
    let (port, seen) = serve();

    let out = gz(
        port,
        "refuse",
        &[
            "govern",
            "model-access",
            "set",
            "groundzero",
            "restricted",
            "allow",
        ],
    );

    assert!(!out.status.success());
    assert!(text(&out).contains("--yes"), "{}", text(&out));
    assert!(seen.lock().unwrap().iter().all(|r| r.method != "PUT"));
}

#[test]
fn rejects_a_level_the_platform_does_not_have() {
    let out = gz(
        1,
        "bad-level",
        &[
            "govern",
            "model-access",
            "set",
            "customer",
            "secret",
            "deny",
        ],
    );

    assert!(!out.status.success());
    assert!(text(&out).contains("confidential"), "{}", text(&out));
}
