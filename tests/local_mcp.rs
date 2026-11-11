//! `gz code` with a broken `mcp.toml` in its config folder: it says which
//! file is wrong and why, and still runs the turn.

use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

mod common;

use common::{read_request, respond, Recorded};

const FINISH: &str = "id: 1\ndata: {\"type\":\"llm_chunk\",\"text\":\"Hello.\"}\n\nid: 2\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\ndata: [DONE]\n\n";

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
            let path = request.target.split('?').next().unwrap().to_string();
            match (request.method.as_str(), path.as_str()) {
                ("POST", "/chat/runs") => respond(
                    &mut stream,
                    "202 Accepted",
                    "application/json",
                    r#"{"runId":"r1"}"#,
                ),
                ("GET", "/chat/runs/r1/events") => {
                    respond(&mut stream, "200 OK", "text/event-stream", FINISH)
                }
                _ => respond(&mut stream, "404 Not Found", "application/json", "{}"),
            }
        }
    });
    (base, log)
}

#[test]
fn a_bad_mcp_toml_is_named_with_its_problem_and_gz_still_runs() {
    let dir = std::env::temp_dir().join(format!("gz-local-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = dir.join("config/gz");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::write(
        config.join("mcp.toml"),
        "[servers.files]\nargs = [\"-y\"]\n",
    )
    .unwrap();
    let (base, log) = stub_chat_server();

    let out = Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "local-mcp-test")
        .env("GZ_HOST", &base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(["code", "hi"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = common::plain(&out.stderr);
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Hello."), "{stdout}");
    let named = config.join("mcp.toml").display().to_string();
    assert!(
        stderr.contains("local MCP servers are off")
            && stderr.contains(&named)
            && stderr.contains("missing field `command`"),
        "{stderr}"
    );
    let log = log.lock().unwrap();
    let start = log
        .iter()
        .find(|r| r.method == "POST" && r.target == "/chat/runs")
        .expect("the turn was sent");
    let tools = start.body["clientTools"].as_array().unwrap();
    assert!(
        !tools
            .iter()
            .any(|t| t.as_str().unwrap().starts_with("local_mcp")),
        "no servers, no local MCP tools: {tools:?}"
    );
}
