use std::path::PathBuf;
use std::process::Command;

fn gz() -> Command {
    let bin = env!("CARGO_BIN_EXE_gz");
    let mut cmd = Command::new(bin);
    cmd.env("GZ_PROFILE", "smoke-test-profile");
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
    ] {
        assert!(text.contains(group), "missing group {group}");
    }
}

#[test]
fn doctor_json_is_machine_readable_without_auth() {
    let mut cmd = gz();
    temp_config(&mut cmd, "t");
    let out = cmd.args(["--json", "doctor"]).output().unwrap();
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["authenticated"], false);
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
fn rte_list_offline() {
    let mut cmd = gz();
    temp_config(&mut cmd, "t");
    let out = cmd.args(["--json", "rte", "list"]).output().unwrap();
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value.as_array().unwrap().len(), 9);
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
    let out = doctor.args(["--json", "doctor"]).output().unwrap();
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
fn stdin_body_and_bearer_header_reach_server() {
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
                .contains("authorization: bearer stub-test-token"),
            "missing bearer: {head}"
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
