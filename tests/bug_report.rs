//! `gz bug-report` and the error log it reads, through the real binary with
//! an isolated config folder.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const ID_TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJzbW9rZSJ9.c2lnbmF0dXJlLXZhbHVl";
const REFRESH_TOKEN: &str = "refresh-token-value-not-for-reports";
const QUERY_SECRET: &str = "query-secret-value";

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-bugreport-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("gz")).unwrap();
    dir
}

fn gz(config_home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gz"));
    cmd.env("XDG_CONFIG_HOME", config_home)
        .env("GZ_PROFILE", "bug-report-test");
    for var in [
        "GZ_TOKEN",
        "GZ_HOST",
        "GZ_CLIENT",
        "GZ_SITE",
        "GZ_DOMAIN",
        "GZ_AUTH_HOST",
        "GZ_API_KEY",
        "GZ_WEB_URL",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn write_profile(config_home: &Path, host: &str) {
    std::fs::write(
        config_home.join("gz/config.toml"),
        format!(
            "[profiles.bug-report-test]\nhost = \"{host}\"\nclient = \"acme\"\nsite = \"main\"\ntoken = \"{ID_TOKEN}\"\nrefresh_token = \"{REFRESH_TOKEN}\"\nusername = \"ana@example.com\"\n\n[profiles.bug-report-test.future_feature]\nwebhook_secret = \"nested-secret-value\"\nregion = \"eu-west-1\"\n"
        ),
    )
    .unwrap();
}

/// Answers one request with `status`, a request id header and `body`.
fn serve_once(status: &'static str, body: &'static str) -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut raw = Vec::new();
        let mut chunk = [0u8; 1024];
        while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = socket.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
        }
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nx-amzn-RequestId: req-abc-123\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(reply.as_bytes()).unwrap();
    });
    (port, handle)
}

fn report(config_home: &Path) -> String {
    let out = gz(config_home)
        .args(["bug-report", "--stdout"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn assert_no_secrets(text: &str) {
    for secret in [
        ID_TOKEN,
        REFRESH_TOKEN,
        QUERY_SECRET,
        "nested-secret-value",
        "ana@example.com",
        "sensitive-server-message",
    ] {
        assert!(!text.contains(secret), "{secret} leaked:\n{text}");
    }
}

#[test]
fn a_failed_request_lands_in_the_report_without_its_query_or_body() {
    let dir = temp_dir("http");
    let (port, server) = serve_once(
        "503 Service Unavailable",
        r#"{"message":"sensitive-server-message"}"#,
    );
    write_profile(&dir, &format!("http://127.0.0.1:{port}"));
    let failed = gz(&dir)
        .args(["request", "etl", "GET", "/etl/jobs/status"])
        .args(["--query", &format!("token={QUERY_SECRET}")])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(!failed.status.success());

    let log = std::fs::read_to_string(dir.join("gz/errors.jsonl")).unwrap();
    assert_no_secrets(&log);

    let text = report(&dir);
    assert_no_secrets(&text);
    let row = format!(
        "| gz request | GET | http://127.0.0.1:{port}/etl/jobs/status | 503 | req-abc-123 |"
    );
    assert!(text.contains(&row), "missing {row} in\n{text}");
    assert!(text.contains("## Recent errors (1)"), "{text}");
    assert!(
        text.contains(&format!("| etl | 127.0.0.1:{port} |")),
        "{text}"
    );
    assert!(text.contains("token = \"[redacted]\""), "{text}");
    assert!(
        text.contains("username = \"[redacted: personal]\""),
        "{text}"
    );
    assert!(text.contains("region = \"eu-west-1\""), "{text}");
    assert!(
        text.contains(&format!("- version: {}", env!("CARGO_PKG_VERSION"))),
        "{text}"
    );
}

#[test]
fn an_error_inside_a_200_reply_is_recorded_too() {
    let dir = temp_dir("envelope");
    let (port, server) = serve_once(
        "200 OK",
        r#"{"success":false,"message":"sensitive-server-message"}"#,
    );
    write_profile(&dir, &format!("http://127.0.0.1:{port}"));
    let failed = gz(&dir)
        .args(["request", "etl", "POST", "/etl/submit"])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(!failed.status.success());
    let text = report(&dir);
    assert_no_secrets(&text);
    assert!(
        text.contains("| POST | ") && text.contains("| 200 (error in body) | req-abc-123 |"),
        "{text}"
    );
}

#[test]
fn an_unreachable_host_is_recorded_with_no_status() {
    let dir = temp_dir("connect");
    let closed_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    write_profile(&dir, &format!("http://127.0.0.1:{closed_port}"));
    let failed = gz(&dir)
        .args(["request", "etl", "GET", "/etl/health"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let text = report(&dir);
    assert!(text.contains("| no response (connect) | — |"), "{text}");
}

#[test]
fn with_no_failures_and_no_profile_the_report_still_writes() {
    let dir = temp_dir("empty");
    let text = report(&dir);
    assert!(text.contains("No request failures recorded."), "{text}");
    assert!(
        text.contains("# no settings saved for this profile"),
        "{text}"
    );
}

#[test]
fn the_report_goes_to_a_file_and_gz_prints_its_path() {
    let dir = temp_dir("file");
    write_profile(&dir, "https://example.com");
    let target = dir.join("report.md");
    let out = gz(&dir)
        .args(["bug-report", "--out"])
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        target.display().to_string()
    );
    let text = std::fs::read_to_string(&target).unwrap();
    assert!(text.starts_with("# gz bug report"));
    assert_no_secrets(&text);

    let default_named = gz(&dir)
        .arg("bug-report")
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(default_named.status.success());
    let printed = String::from_utf8_lossy(&default_named.stdout)
        .trim()
        .to_string();
    assert!(
        printed.starts_with("gz-bug-report-") && printed.ends_with(".md"),
        "{printed}"
    );
    assert!(dir.join(&printed).exists());
}

#[test]
fn secret_environment_overrides_are_redacted() {
    let dir = temp_dir("env");
    let out = gz(&dir)
        .env("GZ_TOKEN", ID_TOKEN)
        .env("GZ_API_KEY", "sk_ci_ana_k1.env-secret")
        .env("GZ_SITE", "main")
        .args(["bug-report", "--stdout"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains(ID_TOKEN) && !text.contains("env-secret"),
        "{text}"
    );
    assert!(text.contains("- env GZ_TOKEN: [redacted]"), "{text}");
    assert!(text.contains("- env GZ_API_KEY: [redacted]"), "{text}");
    assert!(text.contains("- env GZ_SITE: main"), "{text}");
}
