//! Chat subagents against a stub chat server: the run streams
//! `subagent_*` events (ORCHESTRATION_CONTRACT.md, Phase 1) and both
//! `gz chat ask` and plain `gz code` show one line when each subagent
//! starts and one when it ends; `/subagents off` is sent as
//! `subagents: false`.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

mod common;

use common::{plain, read_request, respond, Recorded};

const RUN: &str = concat!(
    "id: 1\ndata: {\"type\":\"subagent_started\",\"subagentId\":\"sa_1\",\"name\":\"Check logs\",\"profile\":\"data_engineer\"}\n\n",
    "id: 2\ndata: {\"type\":\"subagent_started\",\"subagentId\":\"sa_2\",\"name\":\"Read schema\",\"profile\":\"analyst\"}\n\n",
    "id: 3\ndata: {\"type\":\"subagent_tool\",\"subagentId\":\"sa_1\",\"toolName\":\"get_job_logs\",\"status\":\"started\"}\n\n",
    "id: 4\ndata: {\"type\":\"subagent_text\",\"subagentId\":\"sa_1\",\"text\":\"scanning\"}\n\n",
    "id: 5\ndata: {\"type\":\"subagent_finished\",\"subagentId\":\"sa_2\",\"status\":\"failed\",\"summary\":\"no access\",\"durationMs\":3000}\n\n",
    "id: 6\ndata: {\"type\":\"subagent_finished\",\"subagentId\":\"sa_1\",\"status\":\"completed\",\"summary\":\"null key\",\"durationMs\":12000}\n\n",
    "id: 7\ndata: {\"type\":\"llm_chunk\",\"text\":\"Job 4 failed on a null key.\"}\n\n",
    "id: 8\ndata: {\"type\":\"response_metrics\",\"outputTokens\":9,\"subagents\":[{\"subagentId\":\"sa_1\",\"inputTokens\":900,\"outputTokens\":120}]}\n\n",
    "id: 9\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\n",
    "data: [DONE]\n\n",
);

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
            match request.target.split('?').next().unwrap() {
                "/chat/runs" => respond(
                    &mut stream,
                    "202 Accepted",
                    "application/json",
                    r#"{"runId":"r1"}"#,
                ),
                "/chat/runs/r1/events" => respond(&mut stream, "200 OK", "text/event-stream", RUN),
                _ => respond(&mut stream, "404 Not Found", "application/json", "{}"),
            }
        }
    });
    (base, log)
}

fn workspace(name: &str, config: Option<&str>) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-subagents-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("config/gz")).unwrap();
    if let Some(config) = config {
        std::fs::write(dir.join("config/gz/config.toml"), config).unwrap();
    }
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "subagents-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(args)
        .output()
        .unwrap()
}

fn start_body(log: &Arc<Mutex<Vec<Recorded>>>) -> serde_json::Value {
    log.lock()
        .unwrap()
        .iter()
        .find(|r| r.method == "POST" && r.target == "/chat/runs")
        .expect("the run was started")
        .body
        .clone()
}

#[test]
fn chat_ask_prints_a_line_per_subagent_start_and_finish() {
    let dir = workspace("ask", None);
    let (base, _log) = stub_chat_server();
    let out = gz(&dir, &base, &["chat", "ask", "Why did job 4 fail?"]);
    let stderr = plain(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("Job 4 failed on a null key."));
    for line in [
        "⚙ Task(Check logs · data_engineer) … started",
        "⚙ Task(Read schema · analyst) … started",
        "⚙ Task(Read schema · analyst) ✗ failed (3s)",
        "⚙ Task(Check logs · data_engineer) ✓ done (12s)",
    ] {
        assert!(stderr.contains(line), "missing {line:?} in:\n{stderr}");
    }
    assert!(
        !stderr.contains("scanning"),
        "subagent progress stays out of the stream"
    );
}

#[test]
fn gz_code_shows_subagents_and_sends_subagents_by_default() {
    let dir = workspace("code", None);
    let (base, log) = stub_chat_server();
    let out = gz(&dir, &base, &["code", "Why did job 4 fail?"]);
    let stderr = plain(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("⚙ Task(Check logs · data_engineer) ✓ done (12s)"),
        "{stderr}"
    );
    assert!(
        start_body(&log).get("subagents").is_none(),
        "on is the server default, so nothing is sent"
    );
}

#[test]
fn subagents_off_is_remembered_and_sent() {
    let config = "current_profile = \"subagents-test\"\n\n[profiles.subagents-test]\ncode_subagents = false\n";
    let dir = workspace("off", Some(config));
    let (base, log) = stub_chat_server();
    let out = gz(&dir, &base, &["code", "Why did job 4 fail?"]);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert_eq!(start_body(&log)["subagents"], serde_json::json!(false));
}
