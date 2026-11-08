//! The auto-compact threshold against a stub chat server: the saved
//! `code_auto_compact` and `gz code --auto-compact` go out as
//! `autoCompactTokens` on every run; `--auto-compact` is remembered.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

mod common;

use common::{plain, read_request, respond, Recorded};

const RUN: &str = concat!(
    "id: 1\ndata: {\"type\":\"llm_chunk\",\"text\":\"Done.\"}\n\n",
    "id: 2\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\n",
    "data: [DONE]\n\n",
);
const PROFILE: &str = "compaction-test";

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

fn workspace(name: &str, saved: Option<&str>) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-compaction-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("config/gz")).unwrap();
    if let Some(setting) = saved {
        let config =
            format!("current_profile = \"{PROFILE}\"\n\n[profiles.{PROFILE}]\n{setting}\n");
        std::fs::write(dir.join("config/gz/config.toml"), config).unwrap();
    }
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", PROFILE)
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(args)
        .output()
        .unwrap()
}

fn start_body(log: &Arc<Mutex<Vec<Recorded>>>) -> Option<serde_json::Value> {
    log.lock()
        .unwrap()
        .iter()
        .find(|r| r.method == "POST" && r.target == "/chat/runs")
        .map(|r| r.body.clone())
}

fn saved_config(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("config/gz/config.toml")).unwrap_or_default()
}

#[test]
fn without_a_setting_the_server_default_applies() {
    let dir = workspace("unset", None);
    let (base, log) = stub_chat_server();
    let out = gz(&dir, &base, &["code", "hello"]);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert!(start_body(&log).unwrap().get("autoCompactTokens").is_none());
}

#[test]
fn the_saved_threshold_is_sent() {
    let dir = workspace("saved", Some("code_auto_compact = 300000"));
    let (base, log) = stub_chat_server();
    let out = gz(&dir, &base, &["code", "hello"]);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert_eq!(
        start_body(&log).unwrap()["autoCompactTokens"],
        serde_json::json!(300_000)
    );
}

#[test]
fn the_flag_is_sent_and_remembered() {
    let dir = workspace("flag", Some("code_auto_compact = 300000"));
    let (base, log) = stub_chat_server();
    let out = gz(&dir, &base, &["code", "--auto-compact", "200k", "hello"]);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert_eq!(
        start_body(&log).unwrap()["autoCompactTokens"],
        serde_json::json!(200_000)
    );
    assert!(saved_config(&dir).contains("code_auto_compact = 200000"));

    let (base, log) = stub_chat_server();
    let out = gz(&dir, &base, &["code", "--auto-compact", "default", "hello"]);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert!(start_body(&log).unwrap().get("autoCompactTokens").is_none());
    assert!(!saved_config(&dir).contains("code_auto_compact"));
}

#[test]
fn a_threshold_off_the_100k_steps_is_refused_before_any_call() {
    let dir = workspace("bad", None);
    let (base, log) = stub_chat_server();
    let out = gz(&dir, &base, &["code", "--auto-compact", "150k", "hello"]);
    assert!(!out.status.success());
    assert!(
        plain(&out.stderr).contains("100k steps"),
        "{}",
        plain(&out.stderr)
    );
    assert!(start_body(&log).is_none());
}
