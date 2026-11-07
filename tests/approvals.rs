//! `gz approvals` and `gz chat approve --remember` against a stub chat
//! backend: server grants are listed and revoked with the backend's own
//! shapes, and the resume sends `remember` as a word, not a flag.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::json;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

fn stub_chat_server() -> (String, Arc<Mutex<Vec<Recorded>>>) {
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
            let body = match (request.method.as_str(), path) {
                ("GET", "/chat/approval-grants") => json!({
                    "conversation": [{"toolName": "run_query", "connectionName": null,
                                      "externalToolName": null, "grantedAt": "2026-11-07T10:00:00Z"}],
                    "always": [{"toolName": "save_dataset", "connectionName": null,
                                "externalToolName": null, "grantedAt": "2026-11-07T09:00:00Z"}],
                }),
                ("DELETE", "/chat/approval-grants") => json!({"revoked": true}),
                ("POST", "/chat/runs/r1/resume") => json!({"runId": "r2"}),
                ("GET", "/chat/runs/r2/events") => {
                    respond(
                        &mut stream,
                        "200 OK",
                        "text/event-stream",
                        "id: 1\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\ndata: [DONE]\n\n",
                    );
                    continue;
                }
                _ => {
                    respond(&mut stream, "404 Not Found", "application/json", "{}");
                    continue;
                }
            };
            respond(&mut stream, "200 OK", "application/json", &body.to_string());
        }
    });
    (base, log)
}

fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-approvals-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &PathBuf, base: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "approvals-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .args(args)
        .output()
        .unwrap()
}

fn find<'a>(log: &'a [Recorded], method: &str, path: &str) -> &'a Recorded {
    log.iter()
        .find(|r| r.method == method && r.target.starts_with(path))
        .unwrap_or_else(|| panic!("no {method} {path}"))
}

#[test]
fn lists_chat_grants_with_ids_and_revokes_by_id() {
    let (base, log) = stub_chat_server();
    let dir = folder("list");
    let out = gz(&dir, &base, &["approvals", "list", "--conversation", "c1"]);
    let text = plain(&out.stdout);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert!(text.contains("C1") && text.contains("run_query"), "{text}");
    assert!(
        text.contains("A1") && text.contains("save_dataset"),
        "{text}"
    );
    assert!(find(&log.lock().unwrap(), "GET", "/chat/approval-grants")
        .target
        .contains("conversationId=c1"));

    let out = gz(
        &dir,
        &base,
        &["approvals", "revoke", "c1", "--conversation", "c1"],
    );
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let log = log.lock().unwrap();
    let revoke = find(&log, "DELETE", "/chat/approval-grants");
    assert_eq!(
        revoke.body,
        json!({"scope": "conversation", "toolName": "run_query", "conversationId": "c1"})
    );
}

#[test]
fn approve_sends_remember_as_a_word() {
    let (base, log) = stub_chat_server();
    let dir = folder("approve");
    let out = gz(
        &dir,
        &base,
        &[
            "chat",
            "approve",
            "r1",
            "--interrupt",
            "v1:approval:9",
            "--remember",
            "chat",
        ],
    );
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let log = log.lock().unwrap();
    assert_eq!(
        find(&log, "POST", "/chat/runs/r1/resume").body,
        json!({"interruptId": "v1:approval:9", "approved": true, "remember": "conversation"})
    );
}
