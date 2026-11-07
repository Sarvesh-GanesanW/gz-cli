use std::path::PathBuf;
use std::process::Command;

fn gz() -> Command {
    let bin = env!("CARGO_BIN_EXE_gz");
    let mut cmd = Command::new(bin);
    cmd.env("GZ_PROFILE", "smoke-test-profile");
    // Never touch the developer's real config, even in tests that forget temp_config.
    cmd.env(
        "XDG_CONFIG_HOME",
        std::env::temp_dir().join("gz-smoke-isolated"),
    );
    for var in [
        "GZ_TOKEN",
        "GZ_HOST",
        "GZ_CLIENT",
        "GZ_SITE",
        "GZ_DOMAIN",
        "GZ_AUTH_HOST",
        "GZ_API_KEY",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn temp_config(cmd: &mut Command, name: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("gz-smoke-{}-{name}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    cmd.env("XDG_CONFIG_HOME", &dir);
    dir
}

#[test]
fn help_lists_all_groups() {
    let out = gz().arg("--help").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for group in [
        "warehouse",
        "sql",
        "table",
        "jobs",
        "notebooks",
        "models",
        "agents",
        "auth",
        "request",
        "doctor",
        "projects",
        "designer",
        "workspaces",
        "schedules",
        "permissions",
        "connections",
        "update",
    ] {
        assert!(text.contains(group), "missing group {group}");
    }
}

#[test]
fn doctor_json_is_machine_readable_without_auth() {
    let mut cmd = gz();
    temp_config(&mut cmd, "t");
    let out = cmd
        .args(["--json", "doctor", "--offline"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["username"], serde_json::Value::Null);
    assert_eq!(value["tokenSource"], "missing");
    assert!(value["version"].is_string());
}

#[test]
fn error_envelope_is_json() {
    let mut cmd = gz();
    temp_config(&mut cmd, "t");
    let out = cmd
        .args(["--json", "request", "bogus", "GET", "/x"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value["error"].as_str().unwrap().contains("unknown service"));
}

#[test]
fn gateway_commands_explain_how_to_load_urls() {
    let mut cmd = gz();
    temp_config(&mut cmd, "t");
    let out = cmd.args(["--json", "rte", "list"]).output().unwrap();
    assert!(!out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value["error"].as_str().unwrap().contains("gz auth login"));
}

#[test]
fn configure_and_profile_roundtrip() {
    let mut cmd = gz();
    let dir = temp_config(&mut cmd, "t");
    let out = cmd
        .args(["configure", "--client", "dev", "--site", "acme"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut show = gz();
    show.env("XDG_CONFIG_HOME", &dir);
    let out = show.args(["--json", "profile", "show"]).output().unwrap();
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["client"], "dev");
    assert_eq!(value["site"], "acme");
    let mut doctor = gz();
    doctor.env("XDG_CONFIG_HOME", &dir);
    doctor.env("GZ_AUTH_HOST", "http://127.0.0.1:9");
    let out = doctor
        .args(["--json", "doctor", "--offline"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let etl = value["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["service"] == "etl")
        .unwrap();
    assert_eq!(
        etl["baseUrl"],
        "https://dev-admin-etlprovider.dev.api.groundzero.cloud"
    );
    assert_eq!(etl["source"], "derived");
}

#[test]
fn completion_emits_script() {
    let out = gz().args(["completion", "bash"]).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("_gz()"));
}

#[cfg(unix)]
#[test]
fn config_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let mut cmd = gz();
    let dir = temp_config(&mut cmd, "perms");
    let out = cmd
        .args(["configure", "--host", "https://example.test"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let file = dir.join("gz/config.toml");
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "config.toml must be 0600");
    let dir_mode = std::fs::metadata(dir.join("gz"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700, "config dir must be 0700");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut again = gz();
    again.env("XDG_CONFIG_HOME", &dir);
    let out = again.args(["configure", "--site", "x"]).output().unwrap();
    assert!(out.status.success());
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "pre-existing config must be tightened to 0600");
}

#[test]
fn stdin_body_and_raw_token_reach_server() {
    use std::io::{Read, Write};
    use std::process::Stdio;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut raw = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = socket.read(&mut chunk).unwrap();
            raw.extend_from_slice(&chunk[..n]);
            if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&raw);
        assert!(
            head.starts_with("POST /echo "),
            "unexpected request: {head}"
        );
        assert!(
            head.to_lowercase()
                .contains("authorization: stub-test-token\r\n"),
            "token must be sent raw, like the web app: {head}"
        );
        assert!(
            head.to_lowercase().contains("gz-site: acme"),
            "missing gz-site: {head}"
        );
        let len: usize = head
            .lines()
            .find_map(|l| {
                l.strip_prefix("Content-Length: ")
                    .or_else(|| l.strip_prefix("content-length: "))
            })
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        while raw.len() < header_end + len {
            let n = socket.read(&mut chunk).unwrap();
            raw.extend_from_slice(&chunk[..n]);
        }
        let body = &raw[header_end..header_end + len];
        assert_eq!(body, br#"{"secret":"from-stdin"}"#);
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"ok\":true,\"n\":1}";
        socket.write_all(response).unwrap();
    });
    let mut cmd = gz();
    temp_config(&mut cmd, "stdin");
    cmd.env("GZ_HOST", format!("http://127.0.0.1:{port}"))
        .env("GZ_TOKEN", "stub-test-token")
        .env("GZ_SITE", "acme")
        .args(["--json", "request", "data", "POST", "/echo", "--data", "@-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"secret":"from-stdin"}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value, serde_json::json!({"ok": true, "n": 1}));
    server.join().unwrap();
}

/// Serves one request on a local port and hands back its request line,
/// headers, and body, answering with `reply` (a JSON body).
fn serve_once(reply: &'static str) -> (u16, std::thread::JoinHandle<(String, String)>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let n = socket.read(&mut chunk).unwrap();
            raw.extend_from_slice(&chunk[..n]);
            if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
        let len: usize = head
            .lines()
            .find_map(|l| {
                l.to_lowercase()
                    .strip_prefix("content-length: ")
                    .map(str::to_string)
            })
            .map(|v| v.trim().parse().unwrap())
            .unwrap_or(0);
        while raw.len() < header_end + len {
            let n = socket.read(&mut chunk).unwrap();
            raw.extend_from_slice(&chunk[..n]);
        }
        let body = String::from_utf8_lossy(&raw[header_end..header_end + len]).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        socket.write_all(response.as_bytes()).unwrap();
        (head, body)
    });
    (port, handle)
}

fn gz_against(port: u16, name: &str) -> Command {
    let mut cmd = gz();
    temp_config(&mut cmd, name);
    cmd.env("GZ_HOST", format!("http://127.0.0.1:{port}"))
        .env("GZ_TOKEN", "stub-test-token");
    cmd
}

#[test]
fn designer_rename_patches_the_name() {
    let (port, server) = serve_once(r#"{"ok":true}"#);
    let out = gz_against(port, "rename")
        .args([
            "--json", "designer", "rename", "dataset", "1888", "new-name",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (head, body) = server.join().unwrap();
    assert!(head.starts_with("PATCH /dataset/1888 "), "{head}");
    assert_eq!(body, r#"{"Name":"new-name"}"#);
}

#[test]
fn log_export_runs_use_the_gateway_query_and_unwrap_runs() {
    let (port, server) = serve_once(r#"{"runs":[{"runId":"r1"}],"total":1}"#);
    let out = gz_against(port, "logruns")
        .args([
            "--json",
            "logs",
            "runs",
            "--trigger",
            "manual",
            "--page-size",
            "5",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (head, _) = server.join().unwrap();
    assert!(
        head.starts_with("GET /log-export/runs?triggerType=manual&limit=5 "),
        "{head}"
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value, serde_json::json!([{"runId": "r1"}]));
}

/// Serves one canned JSON reply and hands back the request head it received.
fn stub_server(reply: &'static str) -> (u16, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut raw = Vec::new();
        let mut chunk = [0u8; 1024];
        while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = socket.read(&mut chunk).unwrap();
            raw.extend_from_slice(&chunk[..n]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        socket.write_all(response.as_bytes()).unwrap();
        String::from_utf8_lossy(&raw).to_lowercase()
    });
    (port, handle)
}

/// A signed-in profile pointing every service at the stub.
fn signed_in_profile(name: &str, port: u16) -> Command {
    let mut cmd = gz();
    let dir = temp_config(&mut cmd, name);
    std::fs::create_dir_all(dir.join("gz")).unwrap();
    std::fs::write(
        dir.join("gz/config.toml"),
        format!(
            "current_profile = \"smoke-test-profile\"\n\n[profiles.smoke-test-profile]\nhost = \"http://127.0.0.1:{port}\"\nsite = \"acme\"\ntoken = \"id-token\"\naccess_token = \"access-token\"\n"
        ),
    )
    .unwrap();
    cmd
}

#[test]
fn user_admin_sends_the_access_token() {
    let (port, server) = stub_server(r#"{"Users":[{"Username":"ana","Enabled":true}]}"#);
    let out = signed_in_profile("admin", port)
        .args(["--json", "admin", "users", "list", "--limit", "5"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let head = server.join().unwrap();
    assert!(head.starts_with("get /listusers?limit=5 "), "{head}");
    assert!(head.contains("authorization: access-token\r\n"), "{head}");
}

#[test]
fn column_tags_are_written_with_if_match() {
    let (port, server) = stub_server(r#"{"version":4,"tags":{"pii":"yes"}}"#);
    let out = signed_in_profile("coltags", port)
        .args([
            "--json",
            "tags",
            "column-set",
            "wh",
            "ns",
            "orders",
            "7",
            "--tag",
            "pii=yes",
            "--version",
            "3",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let head = server.join().unwrap();
    assert!(
        head.starts_with("put /lakehouse/tags/wh/tables/ns/orders/columns/7/tags "),
        "{head}"
    );
    assert!(head.contains("if-match: \"3\"\r\n"), "{head}");
    assert!(head.contains("x-access-type: web\r\n"), "{head}");
    assert!(head.contains("authorization: id-token\r\n"), "{head}");
}

const TEST_KEY: &str = "sk_ci_ana_k1.s3cret";

/// A profile pointing every service at `host`, with an API key and,
/// optionally, a signed-in user's token.
fn key_profile(name: &str, host: &str, with_user: bool) -> Command {
    let mut cmd = gz();
    let dir = temp_config(&mut cmd, name);
    std::fs::create_dir_all(dir.join("gz")).unwrap();
    let user = if with_user {
        "token = \"id-token\"\n"
    } else {
        ""
    };
    std::fs::write(
        dir.join("gz/config.toml"),
        format!(
            "current_profile = \"smoke-test-profile\"\n\n[profiles.smoke-test-profile]\nhost = \"{host}\"\nsite = \"acme\"\napi_key = \"{TEST_KEY}\"\n{user}"
        ),
    )
    .unwrap();
    cmd
}

#[test]
fn api_key_goes_to_the_etl_provider_as_bearer() {
    let (port, server) = stub_server(r#"{"items":[]}"#);
    let out = key_profile("key-etl", &format!("http://127.0.0.1:{port}"), false)
        .args(["--json", "jobs", "metrics"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let head = server.join().unwrap();
    assert!(head.starts_with("get /etl/consumption-metrics "), "{head}");
    assert!(
        head.contains(&format!(
            "authorization: bearer {}\r\n",
            TEST_KEY.to_lowercase()
        )),
        "{head}"
    );
}

#[test]
fn key_only_callers_are_refused_where_keys_do_not_work() {
    let out = key_profile("key-refuse", "http://127.0.0.1:9", false)
        .args(["warehouse", "list"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("gz auth login"), "{stderr}");
    assert!(stderr.contains("lakehouse"), "{stderr}");
}

#[test]
fn a_signed_in_user_wins_over_a_stored_key() {
    let (port, server) = stub_server(r#"{"items":[]}"#);
    let out = key_profile("key-user", &format!("http://127.0.0.1:{port}"), true)
        .args(["--json", "jobs", "metrics"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let head = server.join().unwrap();
    assert!(head.contains("authorization: id-token\r\n"), "{head}");
    assert!(!head.contains("sk_"), "{head}");
}

#[test]
fn an_explicit_env_key_wins_for_jobs_but_not_for_the_lakehouse() {
    let (port, server) = stub_server(r#"{"items":[]}"#);
    let out = key_profile("key-env", &format!("http://127.0.0.1:{port}"), true)
        .env("GZ_API_KEY", "sk_env_bob_k2.other")
        .args(["--json", "jobs", "metrics"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(server
        .join()
        .unwrap()
        .contains("authorization: bearer sk_env_bob_k2.other\r\n"));

    let (port, server) = stub_server(r#"{"response":[]}"#);
    let out = key_profile("key-env-lh", &format!("http://127.0.0.1:{port}"), true)
        .env("GZ_API_KEY", "sk_env_bob_k2.other")
        .args(["--json", "warehouse", "list"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let head = server.join().unwrap();
    assert!(head.contains("authorization: id-token\r\n"), "{head}");
}

#[test]
fn api_keys_never_travel_over_plain_http_to_other_hosts() {
    let out = key_profile("key-http", "http://example.test", false)
        .args(["jobs", "metrics"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("plain http"), "{stderr}");
}

#[test]
fn api_key_login_rejects_things_that_are_not_keys() {
    use std::io::Write;
    let mut cmd = gz();
    temp_config(&mut cmd, "key-login");
    let mut child = cmd
        .env("GZ_CLIENT", "stage")
        .env("GZ_SITE", "preview")
        .args(["auth", "login", "--api-key"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"eyJ.not.a-key\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("doesn't look like a Groundzero API key"),
        "{stderr}"
    );
}
