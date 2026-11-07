//! `gz code` against a stub chat server: start → client-tool pause →
//! resume with the tool result → answer → done, per the client-tools
//! contract.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    target: String,
    body: Value,
}

fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while raw.len() < head_end + length {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let mut first = head.lines().next()?.split_whitespace();
    Some(Recorded {
        method: first.next()?.to_string(),
        target: first.next()?.to_string(),
        body: serde_json::from_slice(&raw[head_end..head_end + length]).unwrap_or(Value::Null),
    })
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(reply.as_bytes()).ok();
}

const PAUSE: &str = "id: 1\ndata: {\"type\":\"client_tool_request\",\"interruptId\":\"ct_1\",\"toolUseId\":\"tu_1\",\"toolName\":\"read_file\",\"input\":{\"path\":\"hello.txt\"}}\n\nid: 2\ndata: {\"type\":\"run_status\",\"status\":\"interrupted\",\"reason\":\"client_tools\"}\n\ndata: [DONE]\n\n";
const FINISH: &str = "id: 3\ndata: {\"type\":\"llm_chunk\",\"text\":\"All done.\"}\n\nid: 4\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\ndata: [DONE]\n\n";

/// Serves the scripted conversation and records every request.
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
            let events_calls = seen
                .lock()
                .unwrap()
                .iter()
                .filter(|r: &&Recorded| r.target.contains("/events"))
                .count();
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
                // Like the real backend, a resume starts a new durable run.
                ("POST", "/chat/runs/r1/client-tools/resume") => respond(
                    &mut stream,
                    "202 Accepted",
                    "application/json",
                    r#"{"runId":"r2"}"#,
                ),
                ("GET", "/chat/runs/r1/events") if events_calls == 0 => {
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

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-code-{}-{name}", std::process::id()));
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::write(dir.join("ws/hello.txt"), "hello from file\n").unwrap();
    dir
}

fn gz_code(dir: &Path, base: &str, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gz"));
    cmd.current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "assistant-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(["code", "Summarize hello.txt"])
        .args(extra);
    cmd.output().unwrap()
}

fn find<'a>(log: &'a [Recorded], method: &str, path: &str) -> &'a Recorded {
    log.iter()
        .find(|r| r.method == method && r.target.starts_with(path))
        .unwrap_or_else(|| panic!("no {method} {path} in {log:?}"))
}

#[test]
fn pauses_for_a_client_tool_resumes_with_its_result_and_finishes() {
    let dir = workspace("allow");
    let (base, log) = stub_chat_server();
    let out = gz_code(&dir, &base, &["--yes-reads"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("All done."), "{stdout}");
    assert!(stderr.contains("read_file hello.txt"), "{stderr}");

    let log = log.lock().unwrap().clone();
    let start = &find(&log, "POST", "/chat/runs").body;
    assert_eq!(start["clientTools"].as_array().unwrap().len(), 7);
    assert_eq!(start["clientWorkspace"]["name"], "ws");
    assert!(start["conversationId"].as_str().unwrap().len() == 36);
    assert!(["macos", "linux", "windows"]
        .contains(&start["clientWorkspace"]["platform"].as_str().unwrap()));

    let resume = &find(&log, "POST", "/chat/runs/r1/client-tools/resume").body;
    assert_eq!(
        resume["toolResults"][0],
        json!({
            "interruptId": "ct_1",
            "toolName": "read_file",
            "result": {"output": "     1\thello from file\n", "isError": false, "truncated": false},
        })
    );
    let events: Vec<&Recorded> = log
        .iter()
        .filter(|r| r.target.contains("/events"))
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[1].target, "/chat/runs/r2/events",
        "the resumed run is streamed from its start"
    );

    let audit = std::fs::read_to_string(dir.join("config/gz/audit.log")).unwrap();
    assert!(audit.contains("\"tool\":\"read_file\"") && audit.contains("\"decision\":\"allowed\""));
    assert!(
        !audit.contains("hello from file"),
        "audit must not hold file contents"
    );
}

#[test]
fn without_a_terminal_reads_are_denied_and_reported_as_errors() {
    let dir = workspace("deny");
    let (base, log) = stub_chat_server();
    let out = gz_code(&dir, &base, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = log.lock().unwrap().clone();
    let result =
        &find(&log, "POST", "/chat/runs/r1/client-tools/resume").body["toolResults"][0]["result"];
    assert_eq!(result["isError"], true);
    assert_eq!(result["output"], "The user denied this action.");
}
