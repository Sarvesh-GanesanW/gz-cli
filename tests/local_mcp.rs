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
        stderr.contains("MCP: ")
            && stderr.contains(&named)
            && stderr.contains("server 'files' needs a command"),
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

/// `gz mcp ...` in `ws`, with its own config folder; no sign-in needed.
/// Returns success, stdout, and everything it said (stdout and stderr).
fn gz_mcp(dir: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_gz"))
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "local-mcp-cli-test")
        .env_remove("GZ_TOKEN")
        .stdin(std::process::Stdio::null())
        .arg("mcp")
        .args(args)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let said = format!(
        "{}{}",
        common::plain(&out.stdout),
        common::plain(&out.stderr)
    );
    (out.status.success(), stdout, said)
}

#[test]
fn gz_mcp_adds_lists_approves_and_removes_servers_in_each_scope() {
    let dir = std::env::temp_dir().join(format!("gz-mcp-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();

    let (ok, _, err) = gz_mcp(
        &dir,
        &[
            "add",
            "files",
            "--scope",
            "user",
            "--",
            "gz-test-missing-server",
            "-y",
        ],
    );
    assert!(ok, "{err}");
    let (ok, _, err) = gz_mcp(
        &dir,
        &[
            "add",
            "remote",
            "-H",
            "X-Api-Key: literal-key",
            "http://127.0.0.1:9/mcp",
        ],
    );
    assert!(ok && err.contains("local scope"), "{err}");
    let (ok, _, err) = gz_mcp(
        &dir,
        &[
            "add",
            "shared",
            "--scope",
            "project",
            "--",
            "gz-test-missing-server",
        ],
    );
    assert!(ok && err.contains("gz mcp approve shared"), "{err}");
    assert!(dir.join("ws/.mcp.json").exists());

    let (ok, out, err) = gz_mcp(&dir, &["list", "-o", "json"]);
    assert!(ok, "{err}");
    let rows: serde_json::Value = serde_json::from_str(&out).unwrap();
    let row = |name: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("{name} in {rows}"))
            .clone()
    };
    assert_eq!(row("files")["scope"], "user");
    assert!(
        row("files")["status"]
            .as_str()
            .unwrap()
            .contains("cannot start `gz-test-missing-server`"),
        "{rows}"
    );
    assert_eq!(row("remote")["scope"], "local");
    assert!(
        row("remote")["login"].is_null(),
        "no sign-in, none needed: {rows}"
    );
    assert!(row("remote")["status"]
        .as_str()
        .unwrap()
        .contains("cannot reach"));
    assert!(row("shared")["status"]
        .as_str()
        .unwrap()
        .contains("needs approval"));
    assert!(
        !out.contains("literal-key"),
        "header values stay masked: {out}"
    );

    let (ok, out, _) = gz_mcp(&dir, &["get", "remote", "-o", "json"]);
    assert!(ok);
    let got: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(got["settings"]["headers"]["X-Api-Key"], "****");

    let (ok, _, err) = gz_mcp(&dir, &["approve", "shared"]);
    assert!(ok && err.contains("Approved shared"), "{err}");
    let (_, out, _) = gz_mcp(&dir, &["list", "-o", "json"]);
    assert!(!out.contains("needs approval"), "{out}");

    let (ok, _, err) = gz_mcp(&dir, &["login", "files"]);
    assert!(!ok && err.contains("no sign-in"), "{err}");
    let (ok, _, err) = gz_mcp(&dir, &["logout", "remote"]);
    assert!(ok && err.contains("had no sign-in"), "{err}");

    let (ok, _, err) = gz_mcp(&dir, &["remove", "files"]);
    assert!(ok && err.contains("user scope"), "{err}");
    let (ok, _, err) = gz_mcp(&dir, &["remove", "files"]);
    assert!(!ok && err.contains("gz mcp list"), "{err}");
    let user = std::fs::read_to_string(dir.join("config/gz/mcp.toml")).unwrap();
    assert!(!user.contains("files"), "{user}");
}

#[test]
fn gz_code_says_which_project_servers_wait_for_approval() {
    let dir = std::env::temp_dir().join(format!("gz-mcp-pending-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("config/gz")).unwrap();
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::write(
        dir.join("ws/.mcp.json"),
        r#"{"mcpServers": {"team-tools": {"command": "team-mcp"}}}"#,
    )
    .unwrap();
    let (base, _log) = stub_chat_server();
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
    let stderr = common::plain(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("team-tools wait for your approval") && stderr.contains("/mcp approve"),
        "{stderr}"
    );
}

#[test]
fn gz_mcp_login_never_contacts_a_project_server_before_its_approval() {
    let dir = std::env::temp_dir().join(format!("gz-mcp-login-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    std::fs::write(
        dir.join("ws/.mcp.json"),
        format!(
            r#"{{"mcpServers": {{"team": {{"url": "{url}", "headers": {{"X-Leak": "${{GZ_TEST_LOGIN_SECRET}}"}}}}}}}}"#
        ),
    )
    .unwrap();
    let login = |dir: &std::path::Path| {
        let out = Command::new(env!("CARGO_BIN_EXE_gz"))
            .current_dir(dir.join("ws"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("GZ_PROFILE", "local-mcp-cli-test")
            .env("GZ_TEST_LOGIN_SECRET", "top-secret-value")
            .env_remove("GZ_TOKEN")
            .stdin(std::process::Stdio::null())
            .args(["mcp", "login", "team", "--no-browser"])
            .output()
            .unwrap();
        (out.status.success(), common::plain(&out.stderr))
    };

    let (ok, err) = login(&dir);
    assert!(
        !ok && err.contains("haven't approved") && err.contains("gz mcp approve team"),
        "{err}"
    );
    let (ok, _, err) = gz_mcp(&dir, &["approve", "team", "--decline"]);
    assert!(ok, "{err}");
    let (ok, err) = login(&dir);
    assert!(!ok && err.contains("declined"), "{err}");
    assert!(
        listener.accept().is_err(),
        "nothing reached the project's URL"
    );
}
