//! `gz code` against a stub chat server that advertises
//! `features.localContext`: the run carries `clientContext` (folders,
//! instruction files, memories) and the extra client tools, and a
//! `change_directory` into an always-allowed folder moves the next tool
//! call there without a prompt.

use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

mod common;

use common::{read_request, respond, Recorded};

const FINISH: &str = "id: 3\ndata: {\"type\":\"llm_chunk\",\"text\":\"Done.\"}\n\nid: 4\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\ndata: [DONE]\n\n";

fn tool_request(id: &str, name: &str, input: Value) -> String {
    let event = json!({
        "type": "client_tool_request",
        "interruptId": id,
        "toolUseId": id,
        "toolName": name,
        "input": input,
    });
    format!("data: {event}\n\n")
}

fn stub_server(advertise: bool) -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    let pause = format!(
        "{}{}data: {{\"type\":\"run_status\",\"status\":\"interrupted\",\"reason\":\"client_tools\"}}\n\ndata: [DONE]\n\n",
        tool_request("ct_1", "change_directory", json!({"path": "~/elsewhere"})),
        tool_request("ct_2", "read_file", json!({"path": "notes.md"})),
    );
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            seen.lock().unwrap().push(request.clone());
            let path = request.target.split('?').next().unwrap().to_string();
            let (kind, body) = match (request.method.as_str(), path.as_str()) {
                ("GET", "/chat/capabilities") if advertise => (
                    "application/json",
                    r#"{"features":{"localContext":true},"localContextLimits":{"instructionChars":20000,"instructionFiles":8,"memoryEntries":200,"memoryChars":20000,"memoryEntryChars":500,"skills":200,"skillDescriptionChars":1024}}"#.to_string(),
                ),
                ("GET", "/chat/capabilities") => ("application/json", "{}".to_string()),
                ("POST", "/chat/runs") => ("application/json", r#"{"runId":"r1"}"#.to_string()),
                ("GET", "/chat/runs/r1/events") => ("text/event-stream", pause.clone()),
                ("POST", "/chat/runs/r1/client-tools/resume") => {
                    ("application/json", r#"{"runId":"r2"}"#.to_string())
                }
                ("GET", "/chat/runs/r2/events") => ("text/event-stream", FINISH.to_string()),
                _ => {
                    respond(&mut stream, "404 Not Found", "application/json", "{}");
                    continue;
                }
            };
            respond(&mut stream, "200 OK", kind, &body);
        }
    });
    (base, log)
}

/// A fake home with the repo `~/code/app` (where gz starts) and the
/// folder `~/elsewhere`, always allowed in the profile's config.
fn fake_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-local-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let home = dir.join("home");
    std::fs::create_dir_all(home.join("code/app")).unwrap();
    std::fs::create_dir_all(home.join("elsewhere")).unwrap();
    let home = home.canonicalize().unwrap();
    std::fs::write(home.join("code/app/AGENTS.md"), "Run make test first.").unwrap();
    std::fs::write(home.join("elsewhere/notes.md"), "notes from elsewhere\n").unwrap();
    let config = dir.join("config/gz");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        format!(
            "[profiles.local-test]\ncode_allowed_dirs = [{:?}]\n",
            home.join("elsewhere").display().to_string()
        ),
    )
    .unwrap();
    std::fs::write(
        config.join("memory.json"),
        r#"{"profiles":{"local-test":[{"id":"m_3f2a","text":"Uses snake_case for Python","created_at":1,"updated_at":1}]}}"#,
    )
    .unwrap();
    std::fs::write(config.join("instructions.md"), "Answer briefly.").unwrap();
    dir
}

fn gz_code(dir: &Path, base: &str) -> std::process::Output {
    let home = dir.join("home").canonicalize().unwrap();
    Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(home.join("code/app"))
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "local-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .stdin(std::process::Stdio::null())
        .args(["code", "Read my notes", "--yes-reads"])
        .output()
        .unwrap()
}

fn find<'a>(log: &'a [Recorded], method: &str, path: &str) -> &'a Recorded {
    log.iter()
        .find(|r| r.method == method && r.target.starts_with(path))
        .unwrap_or_else(|| panic!("no {method} {path} in {log:?}"))
}

#[test]
fn the_run_carries_local_context_and_moves_into_an_always_allowed_folder() {
    let dir = fake_home("advertised");
    let (base, log) = stub_server(true);
    let out = gz_code(&dir, &base);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");

    let log = log.lock().unwrap().clone();
    let start = &find(&log, "POST", "/chat/runs").body;
    let tools = start["clientTools"].as_array().unwrap();
    assert!(tools.contains(&json!("change_directory")) && tools.contains(&json!("memory")));
    let context = &start["clientContext"];
    assert_eq!(context["cwd"], "~/code/app");
    assert_eq!(context["roots"], json!(["~/code/app"]));
    let user_file = dir.join("config/gz/instructions.md");
    assert_eq!(
        context["instructions"],
        json!([
            {"source": "user", "path": user_file.display().to_string(), "text": "Answer briefly."},
            {"source": "project", "path": "~/code/app/AGENTS.md", "text": "Run make test first."},
        ])
    );
    assert_eq!(
        context["memories"],
        json!([{"id": "m_3f2a", "text": "Uses snake_case for Python"}])
    );

    let resume = &find(&log, "POST", "/chat/runs/r1/client-tools/resume").body;
    let results = resume["toolResults"].as_array().unwrap();
    assert_eq!(results[0]["result"]["output"], "Now in ~/elsewhere");
    assert_eq!(
        results[1]["result"]["output"], "     1\tnotes from elsewhere\n",
        "the relative read resolved in the new folder"
    );

    let audit = std::fs::read_to_string(dir.join("config/gz/audit.log")).unwrap();
    assert!(audit.contains("\"tool\":\"change_directory\"") && audit.contains("~/elsewhere"));
}

#[test]
fn an_older_server_gets_neither_the_context_nor_the_new_tools() {
    let dir = fake_home("older");
    let (base, log) = stub_server(false);
    let out = gz_code(&dir, &base);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = log.lock().unwrap().clone();
    let start = &find(&log, "POST", "/chat/runs").body;
    assert_eq!(start["clientTools"].as_array().unwrap().len(), 8);
    assert!(start.get("clientContext").is_none());
    let resume = &find(&log, "POST", "/chat/runs/r1/client-tools/resume").body;
    assert_eq!(resume["toolResults"][0]["result"]["isError"], true);
}
