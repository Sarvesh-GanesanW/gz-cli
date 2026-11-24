//! `gz code` against a stub chat server streaming reasoning and a server
//! tool's `tool_use`/`tool_result` (shapes from gz-chat-eks
//! agent/agent_factory.py and agent/reasoning_stream.py): plain output
//! shows the tool's input and result summary, and never the reasoning.

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

use common::{read_request, respond};

const RUN: &str = concat!(
    "id: 1\ndata: {\"type\":\"reasoning_delta\",\"source\":\"agent\",\"text\":\"secret plan about orders\"}\n\n",
    "id: 2\ndata: {\"type\":\"tool_use\",\"source\":\"agent\",\"tool_name\":\"Query lakehouse\",\"toolName\":\"Query lakehouse\",\"name\":\"Query lakehouse\",\"tool_use_id\":\"t1\",\"toolUseId\":\"t1\",\"turn_count\":1,\"input\":{\"sql\":\"select count(*)\\n from sales.orders\"}}\n\n",
    "id: 3\ndata: {\"type\":\"tool_result\",\"source\":\"agent\",\"tool_use_id\":\"t1\",\"toolUseId\":\"t1\",\"content\":[{\"text\":\"count = 12\"}],\"redacted\":true,\"status\":\"success\"}\n\n",
    "id: 4\ndata: {\"type\":\"llm_chunk\",\"text\":\"There are 12 orders.\"}\n\n",
    "id: 5\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\n",
    "data: [DONE]\n\n",
);

/// A server tool announced before a client-tool pause, with its result
/// streamed by the run the resume starts.
const LEG_ONE: &str = concat!(
    "id: 1\ndata: {\"type\":\"tool_use\",\"source\":\"agent\",\"tool_name\":\"Search documents\",\"name\":\"Search documents\",\"tool_use_id\":\"s1\",\"toolUseId\":\"s1\",\"turn_count\":1,\"input\":{\"query\":\"refund policy\"}}\n\n",
    "id: 2\ndata: {\"type\":\"client_tool_request\",\"interruptId\":\"ct_1\",\"toolUseId\":\"tu_1\",\"toolName\":\"list_directory\",\"input\":{\"path\":\".\"}}\n\n",
    "id: 3\ndata: {\"type\":\"run_status\",\"status\":\"interrupted\",\"reason\":\"client_tools\"}\n\n",
    "data: [DONE]\n\n",
);
const LEG_TWO: &str = concat!(
    "id: 1\ndata: {\"type\":\"tool_result\",\"source\":\"agent\",\"tool_use_id\":\"s1\",\"toolUseId\":\"s1\",\"content\":[{\"text\":\"Refunds within 30 days\"}],\"redacted\":true,\"status\":\"success\"}\n\n",
    "id: 2\ndata: {\"type\":\"llm_chunk\",\"text\":\"Thirty days.\"}\n\n",
    "id: 3\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\n",
    "data: [DONE]\n\n",
);

fn stub_chat_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            match (
                request.method.as_str(),
                request.target.split('?').next().unwrap(),
            ) {
                ("POST", "/chat/runs") if request.body["userInput"] != "Two legs" => respond(
                    &mut stream,
                    "202 Accepted",
                    "application/json",
                    r#"{"runId":"r1"}"#,
                ),
                ("POST", "/chat/runs") if request.body["userInput"] == "Two legs" => respond(
                    &mut stream,
                    "202 Accepted",
                    "application/json",
                    r#"{"runId":"leg1"}"#,
                ),
                ("GET", "/chat/runs/r1/events") => {
                    respond(&mut stream, "200 OK", "text/event-stream", RUN)
                }
                ("GET", "/chat/runs/leg1/events") => {
                    respond(&mut stream, "200 OK", "text/event-stream", LEG_ONE)
                }
                // A resume starts a new run, as the real server does.
                ("POST", "/chat/runs/leg1/client-tools/resume") => respond(
                    &mut stream,
                    "202 Accepted",
                    "application/json",
                    r#"{"runId":"leg2"}"#,
                ),
                ("GET", "/chat/runs/leg2/events") => {
                    respond(&mut stream, "200 OK", "text/event-stream", LEG_TWO)
                }
                _ => {
                    stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .ok();
                }
            }
        }
    });
    base
}

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-code-{}-{name}", std::process::id()));
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    dir
}

fn gz_code(dir: &Path, base: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "rich-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .arg("code")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn server_tool_results_pair_across_a_client_tool_resume() {
    let dir = workspace("rich-legs");
    let base = stub_chat_server();
    let out = gz_code(&dir, &base, &["Two legs", "--yes-reads"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Thirty days."), "{stdout}");
    assert!(
        stderr.contains("Refunds within 30 days"),
        "the result from the second leg is shown: {stderr}"
    );
}

#[test]
fn server_tools_show_input_and_result_and_reasoning_stays_out() {
    let dir = workspace("rich");
    let base = stub_chat_server();
    let out = gz_code(&dir, &base, &["How many orders?"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("There are 12 orders."), "{stdout}");
    assert!(
        stderr.contains("Query lakehouse select count(*) from sales.orders"),
        "the tool's input, on one line: {stderr}"
    );
    assert!(stderr.contains("10 B · count = 12"), "{stderr}");
    assert!(
        !stdout.contains("secret plan") && !stderr.contains("secret plan"),
        "reasoning is not printed: {stdout}{stderr}"
    );
}
