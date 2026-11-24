//! `gz code` against a stub chat server: a write_file the model asked for
//! is checked by the user's configured checker, and the new problem goes
//! back to the model in the tool result.

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

mod common;

use common::{read_request, respond, Recorded};

const PAUSE: &str = "id: 1\ndata: {\"type\":\"client_tool_request\",\"interruptId\":\"ct_1\",\"toolUseId\":\"tu_1\",\"toolName\":\"write_file\",\"input\":{\"path\":\"app.py\",\"content\":\"x = 1\\nBAD = 2\\n\"}}\n\nid: 2\ndata: {\"type\":\"run_status\",\"status\":\"interrupted\",\"reason\":\"client_tools\"}\n\ndata: [DONE]\n\n";
const FINISH: &str = "id: 3\ndata: {\"type\":\"llm_chunk\",\"text\":\"Fixed.\"}\n\nid: 4\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\ndata: [DONE]\n\n";

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

/// A workspace whose edits are allowed, with a user checker that reports
/// every line holding `BAD`.
fn workspace(name: &str, diagnostics_toml: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-diag-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("config/gz")).unwrap();
    let ws = dir.join("ws").canonicalize().unwrap();
    std::fs::write(
        dir.join("config/gz/permissions.toml"),
        format!(
            "[workspaces.{:?}]\nedits = true\n",
            ws.display().to_string()
        ),
    )
    .unwrap();
    std::fs::write(dir.join("config/gz/diagnostics.toml"), diagnostics_toml).unwrap();
    dir
}

const CHECKER: &str = r#"auto = false

[[checkers]]
name = "badcheck"
extensions = ["py"]
command = "grep -n BAD {files} | sed 's/^\\([0-9]*\\):.*/app.py:\\1:1: E9 BAD is not allowed/'"
"#;

fn gz_code(dir: &Path, base: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "diagnostics-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(["code", "Add BAD to app.py"])
        .output()
        .unwrap()
}

fn resume_output(log: &[Recorded]) -> String {
    let resume = log
        .iter()
        .find(|r| r.target.starts_with("/chat/runs/r1/client-tools/resume"))
        .unwrap_or_else(|| panic!("no resume in {log:?}"));
    resume.body["toolResults"][0]["result"]["output"]
        .as_str()
        .unwrap()
        .to_string()
}

#[cfg(unix)]
#[test]
fn a_written_files_new_problems_reach_the_model() {
    let dir = workspace("on", CHECKER);
    let (base, log) = stub_chat_server();
    let out = gz_code(&dir, &base);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(dir.join("ws/app.py")).unwrap(),
        "x = 1\nBAD = 2\n"
    );
    let output = resume_output(&log.lock().unwrap());
    assert!(
        output.ends_with(
            "Diagnostics after this edit: badcheck found 1 problem in app.py:\napp.py:2:1: E9 BAD is not allowed\n"
        ),
        "{output}"
    );
    assert!(
        stderr.contains("badcheck found 1 problem in app.py"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn the_off_switch_leaves_results_alone() {
    let dir = workspace("off", &format!("enabled = false\n{CHECKER}"));
    let (base, log) = stub_chat_server();
    let out = gz_code(&dir, &base);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let output = resume_output(&log.lock().unwrap());
    assert!(!output.contains("Diagnostics"), "{output}");
}
