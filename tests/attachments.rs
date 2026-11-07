//! `gz code "… @file @image.png"` against a stub chat server: text files
//! travel inline, images are uploaded through chat's document flow, and the
//! run carries their ids in `docIds` with `useRag`, as the web app sends.

use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

mod common;

use common::{read_request, respond, Recorded};

const FINISH: &str = "id: 1\ndata: {\"type\":\"llm_chunk\",\"text\":\"Seen.\"}\n\nid: 2\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\ndata: [DONE]\n\n";

fn stub_server() -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    let upload_url = format!("{base}/s3/upload");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            seen.lock().unwrap().push(request.clone());
            let path = request
                .target
                .split('?')
                .next()
                .unwrap_or_default()
                .to_string();
            let reply = match (request.method.as_str(), path.as_str()) {
                ("POST", "/chat/upload/presigned-url") => format!(
                    r#"{{"uploadUrl":"{upload_url}","s3Key":"site/u/1/chart.png","bucket":"b","contentType":"image/png"}}"#
                ),
                ("PUT", "/s3/upload") => String::new(),
                ("POST", "/chat/upload") => {
                    r#"{"documentId":"doc-1","status":"success","fileType":"png"}"#.to_string()
                }
                ("POST", "/chat/runs") => r#"{"runId":"r1"}"#.to_string(),
                ("GET", "/chat/runs/r1/events") => {
                    respond(&mut stream, "200 OK", "text/event-stream", FINISH);
                    continue;
                }
                _ => {
                    respond(&mut stream, "404 Not Found", "application/json", "{}");
                    continue;
                }
            };
            respond(&mut stream, "200 OK", "application/json", &reply);
        }
    });
    (base, log)
}

#[test]
fn mentions_inline_text_and_upload_images_as_documents() {
    let dir = std::env::temp_dir().join(format!("gz-attach-e2e-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::write(dir.join("ws/notes.md"), "first line\nsecond line\n").unwrap();
    std::fs::write(dir.join("ws/chart.png"), [0x89, b'P', b'N', b'G']).unwrap();
    let (base, log) = stub_server();

    let out = Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "attach-test")
        .env("GZ_HOST", &base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(["code", "Compare @notes.md with @chart.png"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("Read notes.md (2 lines)"), "{stderr}");
    assert!(stderr.contains("Attached chart.png"), "{stderr}");

    let log = log.lock().unwrap().clone();
    let order: Vec<String> = log
        .iter()
        .map(|r| format!("{} {}", r.method, r.target.split('?').next().unwrap()))
        .collect();
    assert_eq!(
        &order[..4],
        &[
            "POST /chat/upload/presigned-url",
            "PUT /s3/upload",
            "POST /chat/upload",
            "POST /chat/runs"
        ],
        "uploads finish before the run starts"
    );
    assert!(log[0].target.contains("filename=chart.png"));
    assert_eq!(
        log[2].body,
        serde_json::json!({"s3Key": "site/u/1/chart.png", "bucket": "b"})
    );
    let run = &log[3].body;
    assert_eq!(run["docIds"], serde_json::json!(["doc-1"]));
    assert_eq!(run["useRag"], true);
    let prompt = run["userInput"].as_str().unwrap();
    assert!(
        prompt.starts_with("Compare @notes.md with @chart.png\n\n"),
        "{prompt}"
    );
    assert!(
        prompt.contains("<file path=\"notes.md\">\nfirst line\nsecond line\n</file>"),
        "{prompt}"
    );
    assert!(!prompt.contains("PNG"), "images are never inlined");
}

#[test]
fn an_unknown_mention_fails_before_anything_is_sent() {
    let dir = std::env::temp_dir().join(format!("gz-attach-missing-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    let (base, log) = stub_server();
    let out = Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "attach-test")
        .env("GZ_HOST", &base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(["code", "Explain @nowhere.rs"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("doesn't exist"), "{stderr}");
    assert!(log.lock().unwrap().is_empty(), "nothing reached the server");
}
