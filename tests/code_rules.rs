//! `gz code` permission rules and modes end to end: `--allowedTools`,
//! `--disallowedTools`, `--permission-mode` and the user's rules file,
//! against a stub chat server.

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
    let dir = std::env::temp_dir().join(format!("gz-code-rules-{}-{name}", std::process::id()));
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

/// Writes the user's rules file in the run's config folder.
fn user_rules(dir: &Path, toml: &str) {
    std::fs::create_dir_all(dir.join("config/gz")).unwrap();
    std::fs::write(dir.join("config/gz/permissions.toml"), toml).unwrap();
}

fn tool_result(stub: &Stub) -> Value {
    stub.body("POST", "/chat/runs/r1/client-tools/resume")["toolResults"][0]["result"].clone()
}

fn command_run(line: &str) -> Stub {
    Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        (
            "GET /chat/runs/r1/events",
            asks_for("run_command", json!({"command": line})),
        ),
        ("GET /chat/runs/r2/events", answers()),
    ])
}

#[test]
fn a_command_line_deny_rule_refuses_a_chained_command_and_the_report_says_why() {
    let dir = workspace("flag-deny");
    let stub = command_run("ls && rm hello.txt");
    let out = gz_code(
        &dir,
        &stub,
        &[
            "tidy up",
            "--allowedTools",
            "run_command",
            "--disallowedTools",
            "run_command(rm:*)",
            "-o",
            "json",
        ],
    );
    let report = report(&out);
    assert!(dir.join("ws/hello.txt").exists(), "nothing was removed");
    let result = tool_result(&stub);
    assert_eq!(result["isError"], true);
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("the deny rule `run_command(rm:*)` from the command line forbids it"),
        "{result}"
    );
    let call = &report["toolCalls"][0];
    assert_eq!(call["decision"], "blocked", "{report}");
    assert!(
        call["reason"].as_str().unwrap().contains("a deny rule"),
        "{call}"
    );
}

#[test]
fn dont_ask_mode_runs_what_the_rules_allow_and_refuses_the_rest_without_waiting() {
    let dir = workspace("dont-ask");
    user_rules(&dir, "allow = [\"run_command(cat:*)\"]\n");
    let stub = command_run("cat hello.txt");
    let out = gz_code(
        &dir,
        &stub,
        &["show it", "--permission-mode", "dont-ask", "-o", "json"],
    );
    let summary = report(&out);
    assert_eq!(out.status.code(), Some(0), "{summary}");
    let result = tool_result(&stub);
    assert_eq!(result["isError"], false, "{result}");
    assert!(result["output"]
        .as_str()
        .unwrap()
        .contains("hello from file"));

    let dir = workspace("dont-ask-refuses");
    let stub = command_run("echo hi");
    let out = gz_code(
        &dir,
        &stub,
        &["say hi", "--permission-mode", "dontAsk", "-o", "json"],
    );
    let summary = report(&out);
    assert_eq!(out.status.code(), Some(4), "refused: {summary}");
    let result = tool_result(&stub);
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("don't-ask mode is on"),
        "{result}"
    );
}

#[test]
fn a_user_ask_rule_outranks_a_command_line_allow_and_a_bad_rule_is_reported() {
    let dir = workspace("ask-rule");
    user_rules(
        &dir,
        "ask = [\"run_command(echo:*)\"]\ndeny = [\"bash(rm)\"]\n",
    );
    let stub = command_run("echo hi");
    let out = gz_code(
        &dir,
        &stub,
        &["say hi", "--allowedTools", "run_command", "-o", "json"],
    );
    let report = report(&out);
    // No one is there to ask, so the ask rule's prompt is refused.
    assert_eq!(out.status.code(), Some(4), "{report}");
    let stderr = common::plain(&out.stderr);
    assert!(
        stderr.contains("user deny rule `bash(rm)` is ignored"),
        "{stderr}"
    );
}

#[test]
fn a_mode_that_isnt_one_is_refused_up_front() {
    let dir = workspace("bad-mode");
    let stub = command_run("ls");
    let out = gz_code(&dir, &stub, &["hi", "--permission-mode", "yolo"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = common::plain(&out.stderr);
    assert!(stderr.contains("dont-ask"), "{stderr}");
}

#[test]
fn a_deny_rule_sees_into_a_shell_string_even_with_a_broad_allow() {
    let dir = workspace("shell-string");
    user_rules(&dir, "deny = [\"run_command(rm:*)\"]\n");
    let stub = command_run("bash -c 'rm hello.txt'");
    let out = gz_code(
        &dir,
        &stub,
        &["tidy up", "--allowedTools", "run_command", "-o", "json"],
    );
    let report = report(&out);
    assert_eq!(out.status.code(), Some(4), "{report}");
    assert!(dir.join("ws/hello.txt").exists(), "nothing was removed");
    assert_eq!(tool_result(&stub)["isError"], true);
}

#[test]
fn a_glob_read_deny_rule_leaves_folder_listings_alone() {
    let dir = workspace("glob-listing");
    user_rules(&dir, "deny = [\"read(**/.env)\"]\n");
    let stub = Stub::serve(vec![
        ("GET /chat/capabilities", capabilities(false)),
        (
            "GET /chat/runs/r1/events",
            asks_for("list_directory", json!({"path": "."})),
        ),
        ("GET /chat/runs/r2/events", answers()),
    ]);
    let out = gz_code(
        &dir,
        &stub,
        &["look around", "--allowedTools", "read", "-o", "json"],
    );
    let report = report(&out);
    assert_eq!(out.status.code(), Some(0), "{report}");
    let result = tool_result(&stub);
    assert_eq!(result["isError"], false, "{result}");
    assert!(result["output"].as_str().unwrap().contains("hello.txt"));
}

#[test]
fn plan_mode_in_a_script_run_stays_read_only_and_waits_for_approval() {
    let dir = workspace("plan-allow");
    let stub = command_run("rm hello.txt");
    let out = gz_code(
        &dir,
        &stub,
        &[
            "tidy up",
            "--permission-mode",
            "plan",
            "--allowedTools",
            "run_command",
            "-o",
            "json",
        ],
    );
    let summary = report(&out);
    assert!(
        dir.join("ws/hello.txt").exists(),
        "an allow rule doesn't lift plan mode: {summary}"
    );
    assert!(
        tool_result(&stub)["output"]
            .as_str()
            .unwrap()
            .contains("plan mode"),
        "{summary}"
    );

    // Script runs don't offer exit_plan_mode, so the model is told to
    // answer with its plan.
    let user_input = stub.body("POST", "/chat/runs")["userInput"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        user_input.contains("Plan mode is on") && user_input.contains("answer with your plan"),
        "{user_input}"
    );
}
