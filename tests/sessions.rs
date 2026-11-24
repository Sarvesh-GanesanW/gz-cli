//! `gz code --fork-session` against a stub chat server: the conversation is
//! copied through `POST /chat/conversations/{id}/fork` before the turn, and
//! the turn goes to the copy.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;

use common::{read_request, respond, Recorded};

fn answers() -> String {
    let events = [
        json!({"type": "llm_chunk", "text": "Carried on."}),
        json!({"type": "run_status", "status": "completed"}),
    ];
    let mut body = String::new();
    for (i, event) in events.iter().enumerate() {
        body.push_str(&format!("id: {}\ndata: {event}\n\n", i + 1));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

struct Stub {
    base: String,
    log: Arc<Mutex<Vec<Recorded>>>,
}

impl Stub {
    fn serve(routes: Vec<(&'static str, String)>) -> Self {
        let routes: HashMap<&'static str, String> = routes.into_iter().collect();
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
                let path = request.target.split('?').next().unwrap();
                let key = format!("{} {path}", request.method);
                match routes.get(key.as_str()) {
                    Some(body) if path.ends_with("/events") => {
                        respond(&mut stream, "200 OK", "text/event-stream", body)
                    }
                    Some(body) => respond(&mut stream, "200 OK", "application/json", body),
                    None => {
                        stream
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .ok();
                    }
                }
            }
        });
        Self { base, log }
    }

    fn bodies(&self, method: &str, path: &str) -> Vec<Value> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == method && r.target.split('?').next() == Some(path))
            .map(|r| r.body.clone())
            .collect()
    }
}

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-sessions-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    dir
}

fn gz_code(dir: &Path, stub: &Stub, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "sessions-test")
        .env("GZ_HOST", &stub.base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .arg("code")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _open_stdin = child.stdin.take();
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(30) {
            child.kill().ok();
            panic!("gz code waited for input instead of finishing");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn fork_session_copies_the_conversation_and_sends_the_turn_to_the_copy() {
    let dir = workspace("fork");
    let stub = Stub::serve(vec![
        (
            "GET /chat/capabilities",
            json!({"groundzeroModes": [{"value": "sonnet55", "label": "Sonnet", "reasoningEfforts": []}],
                   "defaultGroundzeroMode": "sonnet55", "features": {}})
            .to_string(),
        ),
        ("GET /connection", "[]".into()),
        (
            "POST /chat/conversations/c1/fork",
            json!({"conversationId": "f1", "conversationName": "Parser (branch)",
                   "assistantMode": "chat"})
            .to_string(),
        ),
        (
            "GET /chat/conversations/f1",
            json!({"conversationId": "f1", "conversationName": "Parser (branch)", "messages": [
                {"speaker": "Human", "message": "fix the parser", "timestamp": "2026-11-24T10:00:00+00:00"},
                {"speaker": "AI", "message": "Fixed.", "timestamp": "2026-11-24T10:00:05+00:00"},
            ]})
            .to_string(),
        ),
        ("POST /chat/runs", r#"{"runId":"r1"}"#.into()),
        ("GET /chat/runs/r1/events", answers()),
    ]);
    let out = gz_code(
        &dir,
        &stub,
        &["-c", "c1", "--fork-session", "carry on", "-o", "json"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}\n{}",
        common::plain(&out.stderr)
    );
    assert_eq!(
        stub.bodies("POST", "/chat/conversations/c1/fork"),
        [json!({})],
        "the whole conversation is copied, named by the server"
    );
    let run = &stub.bodies("POST", "/chat/runs")[0];
    assert_eq!(run["conversationId"], "f1");
    assert!(
        run.get("conversationName").is_none(),
        "the fork already exists"
    );
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["conversationId"], "f1");

    let index = std::fs::read_to_string(dir.join("config/gz/code_sessions.json")).unwrap();
    let entries: Value = serde_json::from_str(&index).unwrap();
    let fork = entries
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "f1")
        .expect("the fork is listed for /resume");
    assert_eq!(fork["title"], "Parser (branch)");
}

#[test]
fn fork_session_needs_a_conversation_to_fork() {
    let dir = workspace("alone");
    let stub = Stub::serve(vec![]);
    let out = gz_code(&dir, &stub, &["--fork-session", "hi", "-o", "json"]);
    assert_ne!(out.status.code(), Some(0));
    let all = format!(
        "{}{}",
        common::plain(&out.stdout),
        common::plain(&out.stderr)
    );
    assert!(all.contains("add --continue, --resume or -c <id>"), "{all}");
    assert!(stub.bodies("POST", "/chat/runs").is_empty());
}

#[test]
fn a_failed_fork_stops_before_any_turn() {
    let dir = workspace("failed");
    let stub = Stub::serve(vec![("GET /connection", "[]".into())]);
    let out = gz_code(
        &dir,
        &stub,
        &["-c", "gone", "--fork-session", "hi", "-o", "json"],
    );
    assert_ne!(out.status.code(), Some(0));
    let all = format!(
        "{}{}",
        common::plain(&out.stdout),
        common::plain(&out.stderr)
    );
    assert!(all.contains("cannot fork"), "{all}");
    assert!(stub.bodies("POST", "/chat/runs").is_empty());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("script mode prints its report ({e}): {all}"));
    assert!(report.to_string().contains("cannot fork"), "{report}");
}
