//! `gz code` hooks end to end against a stub chat server: session start
//! and message hooks add context to the message, `pre_tool_use` refuses a
//! call before any permission check, and `session_end` runs on exit; a
//! one-shot slash command sends what it sends in the full-screen view.

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

mod common;

use common::{read_request, respond, Recorded};

const PAUSE: &str = "id: 1\ndata: {\"type\":\"client_tool_request\",\"interruptId\":\"ct_1\",\"toolUseId\":\"tu_1\",\"toolName\":\"read_file\",\"input\":{\"path\":\"hello.txt\"}}\n\nid: 2\ndata: {\"type\":\"run_status\",\"status\":\"interrupted\",\"reason\":\"client_tools\"}\n\ndata: [DONE]\n\n";
const FINISH: &str = "id: 3\ndata: {\"type\":\"llm_chunk\",\"text\":\"All done.\"}\n\nid: 4\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\ndata: [DONE]\n\n";

/// A run that asks for `read_file`, then finishes once resumed.
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
            match (
                request.method.as_str(),
                request.target.split('?').next().unwrap(),
            ) {
                ("POST", "/chat/runs") => respond(
                    &mut stream,
                    "202 Accepted",
                    "application/json",
                    r#"{"runId":"r1"}"#,
                ),
                ("POST", "/chat/runs/r1/client-tools/resume") => respond(
                    &mut stream,
                    "202 Accepted",
                    "application/json",
                    r#"{"runId":"r2"}"#,
                ),
                ("GET", "/chat/runs/r1/events") => {
                    respond(&mut stream, "200 OK", "text/event-stream", PAUSE)
                }
                ("GET", "/chat/runs/r2/events") => {
                    respond(&mut stream, "200 OK", "text/event-stream", FINISH)
                }
                _ => {
                    stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .ok();
                }
            }
        }
    });
    (base, log)
}

fn workspace(name: &str, hooks_toml: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-hooks-{}-{name}", std::process::id()));
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("config/gz")).unwrap();
    std::fs::write(dir.join("ws/hello.txt"), "hello from file\n").unwrap();
    std::fs::write(dir.join("config/gz/hooks.toml"), hooks_toml).unwrap();
    dir
}

fn gz_code(dir: &Path, base: &str) -> std::process::Output {
    gz_code_saying(dir, base, "Summarize hello.txt")
}

fn gz_code_saying(dir: &Path, base: &str, prompt: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "hooks-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(["code", prompt, "--yes-reads"])
        .output()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn hooks_shape_the_message_refuse_a_call_and_see_the_session_end() {
    let ended = "ended.json";
    let hooks = format!(
        r#"
[[session_start]]
command = "echo 'branch: main'"

[[user_prompt_submit]]
command = "echo '{{\"additionalContext\":\"tests: make test\"}}'"

[[pre_tool_use]]
matcher = "read_file"
command = "echo 'reads go through the index' >&2; exit 2"

[[session_end]]
command = "cat > {ended}"
"#
    );
    let dir = workspace("e2e", &hooks);
    let (base, log) = stub_chat_server();
    let out = gz_code(&dir, &base);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("All done."), "{stdout}");

    let log = log.lock().unwrap().clone();
    let start = log
        .iter()
        .find(|r| r.method == "POST" && r.target == "/chat/runs")
        .expect("the run started");
    let input = start.body["userInput"].as_str().unwrap();
    assert!(input.starts_with("Summarize hello.txt"), "{input}");
    assert!(
        input.contains("<hook_context>\nbranch: main\n\ntests: make test\n</hook_context>"),
        "{input}"
    );

    let resume = log
        .iter()
        .find(|r| r.target == "/chat/runs/r1/client-tools/resume")
        .expect("gz resumed the run");
    let result = &resume.body["toolResults"][0]["result"];
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["output"],
        "A pre_tool_use hook denied this call: reads go through the index"
    );

    let end: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("ws").join(ended)).unwrap())
            .unwrap();
    assert_eq!(end["event"], "session_end");
    assert_eq!(end["reason"], "exit");
    assert_eq!(end["conversationId"], start.body["conversationId"]);
}

#[cfg(unix)]
#[test]
fn a_one_shot_command_file_and_review_send_their_expanded_text() {
    let dir = workspace("one-shot-command", "");
    std::fs::create_dir_all(dir.join("ws/.gz/commands")).unwrap();
    std::fs::write(
        dir.join("ws/.gz/commands/explain.md"),
        "---\ndescription: Explain a file\n---\nExplain $1 in one line",
    )
    .unwrap();
    let (base, log) = stub_chat_server();
    let out = gz_code_saying(&dir, &base, "/explain hello.txt");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    let input = log
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.method == "POST" && r.target == "/chat/runs")
        .map(|r| r.body["userInput"].as_str().unwrap().to_string())
        .expect("the run started");
    assert!(
        input.starts_with("Explain hello.txt in one line"),
        "{input}"
    );

    let refused = gz_code_saying(&dir, &base, "/review x;y");
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("not a branch name"), "{stderr}");
}
