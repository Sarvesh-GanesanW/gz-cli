//! Script mode: `gz code "<prompt>" -o json|yaml` against a stub chat
//! server prints exactly one report on stdout, never waits for a person,
//! and exits with a code scripts can branch on.

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

const EXIT_NEEDED_APPROVAL: i32 = 4;

/// One SSE stream: each event gets an id, then `[DONE]`.
fn stream(events: &[Value]) -> String {
    let mut body = String::new();
    for (i, event) in events.iter().enumerate() {
        body.push_str(&format!("id: {}\ndata: {event}\n\n", i + 1));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// The run asks gz to run one client tool, then pauses for its result.
fn asks_for(tool: &str, input: Value) -> String {
    stream(&[
        json!({"type": "client_tool_request", "interruptId": "ct_1", "toolUseId": "tu_1", "toolName": tool, "input": input}),
        json!({"type": "run_status", "status": "interrupted", "reason": "client_tools"}),
    ])
}

/// The resumed run answers and completes.
fn answers() -> String {
    stream(&[
        json!({"type": "llm_chunk", "text": "All done."}),
        json!({"type": "response_metrics", "inputTokens": 100, "outputTokens": 20, "tokenUsageStatus": "reported", "costUsd": "0.004200"}),
        json!({"type": "run_status", "status": "completed"}),
    ])
}

fn capabilities(action_review: bool) -> String {
    json!({
        "groundzeroModes": [{"value": "sonnet55", "label": "Claude Sonnet 5.5", "reasoningEfforts": []}],
        "defaultGroundzeroMode": "sonnet55",
        "features": {"actionReview": action_review},
    })
    .to_string()
}

struct Stub {
    base: String,
    log: Arc<Mutex<Vec<Recorded>>>,
}

impl Stub {
    /// Serves `routes` (`"METHOD /path"` → response body) and records every
    /// request; the run starts as `r1` and its resume continues as `r2`.
    fn serve(routes: Vec<(&'static str, String)>) -> Self {
        let mut routes: HashMap<&'static str, String> = routes.into_iter().collect();
        routes.insert("POST /chat/runs", r#"{"runId":"r1"}"#.into());
        routes.insert(
            "POST /chat/runs/r1/client-tools/resume",
            r#"{"runId":"r2"}"#.into(),
        );
        routes.insert("GET /connection", "[]".into());
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

    fn requests(&self, method: &str, path: &str) -> Vec<Recorded> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == method && r.target.split('?').next() == Some(path))
            .cloned()
            .collect()
    }

    fn body(&self, method: &str, path: &str) -> Value {
        self.requests(method, path)
            .first()
            .unwrap_or_else(|| panic!("no {method} {path}"))
            .body
            .clone()
    }
}

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-code-script-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::write(dir.join("ws/hello.txt"), "hello from file\n").unwrap();
    dir
}

/// Runs `gz code` with its stdin left open and unread: a run that waited
/// for an answer would hang until the test's timeout.
fn gz_code(dir: &Path, stub: &Stub, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "script-test")
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

/// The whole of stdout must be one JSON object: nothing before or after.
fn report(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: Value = serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not one JSON object ({err}):\n{stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert!(value.is_object(), "{value}");
    value
}

#[test]
fn a_completed_run_prints_one_report_with_every_field() {
    let dir = workspace("completed");
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        (
            "GET /chat/runs/r1/events",
            asks_for("read_file", json!({"path": "hello.txt"})),
        ),
        ("GET /chat/runs/r2/events", answers()),
    ]);
    let out = gz_code(
        &dir,
        &stub,
        &["Summarize hello.txt", "--yes-reads", "-o", "json"],
    );
    let report = report(&out);
    assert_eq!(out.status.code(), Some(0), "{report}");

    let sent = stub.body("POST", "/chat/runs");
    assert_eq!(report["conversationId"], sent["conversationId"]);
    assert_eq!(report["model"], "sonnet55");
    assert_eq!(report["answer"], "All done.");
    assert_eq!(
        report["toolCalls"],
        json!([{"tool": "read_file", "summary": "read_file hello.txt", "decision": "preapproved", "isError": false}])
    );
    assert!(report.get("subagents").is_none(), "none ran");
    assert_eq!(
        report["usage"],
        json!({"inputTokens": 100, "outputTokens": 20, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0, "costUsd": 0.0042})
    );
    assert!(report["durationMs"].is_u64());
    assert_eq!(report["status"], "completed");
    assert!(report.get("error").is_none());
    let stderr = common::plain(&out.stderr);
    assert!(
        stderr.contains("read_file hello.txt"),
        "progress goes to stderr: {stderr}"
    );
}

#[test]
fn an_action_needing_approval_is_refused_without_waiting_and_exits_non_zero() {
    let dir = workspace("refused");
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        (
            "GET /chat/runs/r1/events",
            asks_for("read_file", json!({"path": "hello.txt"})),
        ),
        ("GET /chat/runs/r2/events", answers()),
    ]);
    let out = gz_code(&dir, &stub, &["Summarize hello.txt", "-o", "json", "-q"]);
    let report = report(&out);
    assert_eq!(out.status.code(), Some(EXIT_NEEDED_APPROVAL), "{report}");
    assert_eq!(report["status"], "completed");
    let call = &report["toolCalls"][0];
    assert_eq!(
        (&call["tool"], &call["decision"], &call["isError"]),
        (&json!("read_file"), &json!("blocked"), &json!(true))
    );
    assert!(
        call["reason"]
            .as_str()
            .unwrap()
            .contains("needs the user's approval"),
        "{call}"
    );
    let result =
        &stub.body("POST", "/chat/runs/r1/client-tools/resume")["toolResults"][0]["result"];
    assert_eq!(result["isError"], true);
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("no one is at a terminal"),
        "the model is told why: {result}"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "",
        "-q keeps progress off stderr"
    );
    let audit = std::fs::read_to_string(dir.join("config/gz/audit.log")).unwrap();
    assert!(audit.contains("\"decision\":\"blocked\""), "{audit}");
}

#[test]
fn a_failed_run_reports_failed_with_its_error() {
    let dir = workspace("failed");
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        (
            "GET /chat/runs/r1/events",
            stream(&[
                json!({"type": "llm_chunk", "text": "Starting"}),
                json!({"type": "run_status", "status": "failed", "error": "model unavailable"}),
            ]),
        ),
    ]);
    let out = gz_code(&dir, &stub, &["Summarize hello.txt", "-o", "json"]);
    let report = report(&out);
    assert_eq!(out.status.code(), Some(1), "{report}");
    assert_eq!(report["status"], "failed");
    assert_eq!(report["error"], "model unavailable");
}

#[test]
fn a_run_that_cannot_start_still_reports_failed() {
    let dir = workspace("unreachable");
    // Nothing listens on the discard port, so the run never starts.
    let out = Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "script-test")
        .env("GZ_HOST", "http://127.0.0.1:9")
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .args(["code", "hi", "-o", "yaml"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let report: Value = serde_yaml::from_slice(&out.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not one YAML report ({err}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(report["status"], "failed");
    assert!(report["error"].as_str().is_some_and(|e| !e.is_empty()));
    assert_eq!(report["model"], Value::Null, "the model list never loaded");
}

#[test]
fn a_follow_up_continues_the_conversation_it_names() {
    let dir = workspace("follow-up");
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        ("GET /chat/runs/r1/events", answers()),
    ]);
    let id = "5f0c6d1e-2b7a-4c39-9a1e-3d2f4b6a8c90";
    let out = gz_code(&dir, &stub, &["and then?", "-c", id, "-o", "json"]);
    let report = report(&out);
    assert_eq!(out.status.code(), Some(0), "{report}");
    assert_eq!(report["conversationId"], id);
    let sent = stub.body("POST", "/chat/runs");
    assert_eq!(sent["conversationId"], id);
    assert!(
        sent.get("conversationName").is_none(),
        "a follow-up isn't a new chat"
    );
}

#[test]
fn auto_mode_lets_the_review_run_a_command_with_no_terminal() {
    let dir = workspace("auto");
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(true)),
        (
            "GET /chat/runs/r1/events",
            asks_for("run_command", json!({"command": "echo reviewed"})),
        ),
        ("GET /chat/runs/r2/events", answers()),
        (
            "POST /chat/action-review",
            json!({"decision": "allow", "reason": "Asked for."}).to_string(),
        ),
    ]);
    let out = gz_code(&dir, &stub, &["run echo reviewed", "--auto", "-o", "json"]);
    let report = report(&out);
    assert_eq!(out.status.code(), Some(0), "{report}");
    assert_eq!(report["toolCalls"][0]["decision"], "auto");
    assert_eq!(report["toolCalls"][0]["isError"], false);
    let review = stub.body("POST", "/chat/action-review");
    assert_eq!(review["userMessages"], json!(["run echo reviewed"]));
    let result =
        &stub.body("POST", "/chat/runs/r1/client-tools/resume")["toolResults"][0]["result"];
    assert!(
        result["output"].as_str().unwrap().contains("reviewed"),
        "{result}"
    );
}

#[test]
fn auto_mode_refuses_to_start_on_a_server_without_the_review() {
    let dir = workspace("auto-unavailable");
    let stub = Stub::serve(vec![("GET /chat/capabilities", capabilities(false))]);
    let out = gz_code(&dir, &stub, &["run echo hi", "--auto", "-o", "json"]);
    let report = report(&out);
    assert_eq!(out.status.code(), Some(1), "{report}");
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("features.actionReview"),
        "{report}"
    );
    assert!(
        stub.requests("POST", "/chat/runs").is_empty(),
        "nothing is sent"
    );
}

/// Runs `gz code` with `input` piped in and its stdin then closed.
fn gz_code_piped(dir: &Path, stub: &Stub, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "script-test")
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
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).unwrap();
    drop(stdin);
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(30) {
            child.kill().ok();
            panic!("gz code did not finish");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn a_piped_prompt_runs_script_mode() {
    let dir = workspace("piped");
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        ("GET /chat/runs/r1/events", answers()),
    ]);
    // Several lines, one starting with `@`: all of it is the prompt, as is.
    let prompt = "@hello.txt is wrong\nfix the test\n";
    let out = gz_code_piped(&dir, &stub, &["-o", "json"], prompt);
    let report = report(&out);
    assert_eq!(out.status.code(), Some(0), "{report}");
    assert_eq!(report["answer"], "All done.");
    let runs = stub.requests("POST", "/chat/runs");
    assert_eq!(runs.len(), 1, "one run for the whole input");
    let sent = runs[0].body.to_string();
    assert!(
        sent.contains("@hello.txt is wrong\\nfix the test"),
        "the whole input is the prompt: {sent}"
    );
}

#[test]
fn an_empty_piped_prompt_says_how_to_pass_one() {
    let dir = workspace("piped-empty");
    let stub = Stub::serve(vec![("GET /chat/capabilities", capabilities(false))]);
    let out = gz_code_piped(&dir, &stub, &["-o", "json"], "  \n");
    assert_ne!(out.status.code(), Some(0));
    // With -o json the error is printed as JSON, like every gz error.
    let error = report(&out)["error"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        error.contains("standard input was empty") && error.contains("| gz code -o json"),
        "{error}"
    );
    assert!(stub.requests("POST", "/chat/runs").is_empty());
}
