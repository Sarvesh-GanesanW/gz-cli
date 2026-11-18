//! `gz bug-report`: one Markdown file to attach to an issue — gz's version
//! and platform, how it was installed, the active profile's settings with
//! credentials redacted, the hosts in use, and the last request failures.

mod redact;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use crate::cmd::ops::SERVICES;
use crate::config::Resolved;
use crate::error_log::{self, ErrorEntry};
use crate::install_method;

const DEFAULT_ERRORS: u64 = 20;
/// Environment variables that change which settings gz uses.
const ENV_OVERRIDES: &[&str] = &[
    "GZ_PROFILE",
    "GZ_HOST",
    "GZ_AUTH_HOST",
    "GZ_WEB_URL",
    "GZ_CLIENT",
    "GZ_SITE",
    "GZ_DOMAIN",
    "GZ_TOKEN",
    "GZ_API_KEY",
    "GZ_UPDATE_REPO",
    "XDG_CONFIG_HOME",
];

#[derive(Args, Debug)]
pub struct BugReportArgs {
    // `--output` is gz's global output-format flag, so the file flag is `--out`.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "stdout",
        help = "Write the report here (default: gz-bug-report-<time>.md in this folder)"
    )]
    pub out: Option<PathBuf>,
    #[arg(long, help = "Print the report instead of writing a file")]
    pub stdout: bool,
    #[arg(
        long,
        value_name = "N",
        default_value_t = DEFAULT_ERRORS,
        value_parser = clap::value_parser!(u64).range(0..=error_log::MAX_ENTRIES as u64),
        help = "How many recent request failures to include"
    )]
    pub errors: u64,
}

/// Everything the report shows, gathered before rendering so rendering
/// stays a pure function.
struct Facts {
    generated: String,
    version: &'static str,
    os: &'static str,
    arch: &'static str,
    os_release: Option<String>,
    exe: Option<String>,
    install_method: &'static str,
    config_file: Option<String>,
    profile_name: String,
    token_source: &'static str,
    signed_in: bool,
    settings: String,
    env_overrides: Vec<(String, String)>,
    endpoints: BTreeMap<String, String>,
    errors: Vec<ErrorEntry>,
    error_log: Option<String>,
}

pub fn run(resolved: &Resolved, args: &BugReportArgs) -> Result<()> {
    let now = crate::config::now_unix();
    let facts = gather(resolved, args.errors as usize, now)?;
    let report = render(&facts);
    if args.stdout {
        print!("{report}");
        return Ok(());
    }
    let path = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(default_file_name(now)));
    std::fs::write(&path, &report).with_context(|| {
        format!(
            "cannot write {}; pick another place with --out, or use --stdout",
            path.display()
        )
    })?;
    println!("{}", path.display());
    crate::output::info(
        "Bug report written. Tokens, keys and your username are redacted; read it before sharing.",
    );
    Ok(())
}

fn default_file_name(now_unix: i64) -> String {
    let stamp: String = error_log::utc_timestamp(now_unix)
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    format!("gz-bug-report-{}-{}.md", &stamp[..8], &stamp[8..])
}

fn gather(resolved: &Resolved, error_count: usize, now_unix: i64) -> Result<Facts> {
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    let home = home.as_deref();
    let shown = |path: &Path| redact::redact_text(&path.to_string_lossy(), home);
    let log_path = error_log::log_path().ok();
    let errors = match &log_path {
        Some(path) => error_log::recent(path, error_count)?,
        None => Vec::new(),
    };
    Ok(Facts {
        generated: error_log::utc_timestamp(now_unix),
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        os_release: os_release(),
        exe: install_method::current_exe().map(|exe| shown(&exe)),
        install_method: install_method::current().label(),
        config_file: crate::config::config_path().ok().map(|path| shown(&path)),
        profile_name: resolved.profile_name.clone(),
        token_source: resolved.token_source,
        signed_in: resolved.is_signed_in(),
        settings: redacted_settings(&resolved.profile, home)?,
        env_overrides: env_overrides(home),
        endpoints: endpoint_hosts(resolved),
        errors,
        error_log: log_path.map(|path| shown(&path)),
    })
}

fn redacted_settings(profile: &crate::config::Profile, home: Option<&Path>) -> Result<String> {
    let table = toml::Table::try_from(profile).context("cannot read the profile's settings")?;
    let redacted = redact::redact_table(&table, home);
    toml::to_string_pretty(&redacted).context("cannot encode the profile's settings")
}

fn env_overrides(home: Option<&Path>) -> Vec<(String, String)> {
    ENV_OVERRIDES
        .iter()
        .filter_map(|name| {
            let value = std::env::var(name).ok().filter(|v| !v.trim().is_empty())?;
            let shown = redact::redact_value(name, &toml::Value::String(value), home);
            Some((
                name.to_string(),
                shown.as_str().unwrap_or_default().to_string(),
            ))
        })
        .collect()
}

/// Every service gz knows plus any discovered or overridden one, mapped to
/// the host (and port) it calls. Paths are left out: hosts are enough to
/// tell environments apart.
fn endpoint_hosts(resolved: &Resolved) -> BTreeMap<String, String> {
    let names = SERVICES
        .iter()
        .map(|(name, _)| name.to_string())
        .chain(resolved.profile.gateways.keys().cloned())
        .chain(resolved.profile.services.keys().cloned());
    names
        .map(|name| {
            let host = match resolved.base_for(&name) {
                Ok(base) => host_of(&base),
                Err(_) => "not configured".to_string(),
            };
            (name, host)
        })
        .collect()
}

fn host_of(base: &str) -> String {
    let Ok(url) = url::Url::parse(base) else {
        return "unreadable URL".to_string();
    };
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        (None, _) => "unreadable URL".to_string(),
    }
}

fn render(facts: &Facts) -> String {
    let mut out = String::new();
    out.push_str("# gz bug report\n\n");
    out.push_str(&format!("Generated {} (UTC).\n", facts.generated));
    out.push_str(
        "Tokens, keys, passwords and the username are redacted. Read it before sharing.\n\n",
    );

    out.push_str("## gz\n\n");
    push_row(&mut out, "version", facts.version);
    push_row(&mut out, "os", &format!("{} ({})", facts.os, facts.arch));
    if let Some(release) = &facts.os_release {
        push_row(&mut out, "os release", release);
    }
    push_row(
        &mut out,
        "install path",
        facts.exe.as_deref().unwrap_or("unknown"),
    );
    push_row(&mut out, "install method", facts.install_method);
    push_row(
        &mut out,
        "config file",
        facts.config_file.as_deref().unwrap_or("unknown"),
    );

    out.push_str(&format!("\n## Profile `{}`\n\n", facts.profile_name));
    push_row(&mut out, "credential source", facts.token_source);
    push_row(
        &mut out,
        "signed in",
        if facts.signed_in { "yes" } else { "no" },
    );
    if facts.env_overrides.is_empty() {
        push_row(&mut out, "environment overrides", "none");
    } else {
        for (name, value) in &facts.env_overrides {
            push_row(&mut out, &format!("env {name}"), value);
        }
    }
    out.push_str("\n```toml\n");
    if facts.settings.trim().is_empty() {
        out.push_str("# no settings saved for this profile\n");
    } else {
        out.push_str(&facts.settings);
    }
    out.push_str("```\n");

    out.push_str("\n## Endpoints\n\n| service | host |\n|---|---|\n");
    for (service, host) in &facts.endpoints {
        out.push_str(&format!("| {service} | {host} |\n"));
    }

    out.push_str(&format!("\n## Recent errors ({})\n\n", facts.errors.len()));
    if facts.errors.is_empty() {
        out.push_str("No request failures recorded.\n");
    } else {
        out.push_str(
            "| time (UTC) | command | method | url | status | request id |\n|---|---|---|---|---|---|\n",
        );
        for entry in facts.errors.iter().rev() {
            out.push_str(&error_row(entry));
        }
    }
    if let Some(log) = &facts.error_log {
        out.push_str(&format!(
            "\nRecorded in {log} (last {} kept).\n",
            error_log::MAX_ENTRIES
        ));
    }
    out
}

fn push_row(out: &mut String, label: &str, value: &str) {
    out.push_str(&format!("- {label}: {value}\n"));
}

fn error_row(entry: &ErrorEntry) -> String {
    let status = match (entry.status, entry.kind.as_str()) {
        (Some(code), "envelope") => format!("{code} (error in body)"),
        (Some(code), _) => code.to_string(),
        (None, kind) => format!("no response ({kind})"),
    };
    format!(
        "| {} | {} | {} | {} | {} | {} |\n",
        entry.time,
        entry.command,
        entry.method,
        entry.url.replace('|', "%7C"),
        status,
        entry.request_id.as_deref().unwrap_or("—"),
    )
}

/// The OS name and version as the system reports them.
fn os_release() -> Option<String> {
    let kernel = kernel_release();
    let product = product_release();
    match (product, kernel) {
        (Some(product), Some(kernel)) => Some(format!("{product} (kernel {kernel})")),
        (product, kernel) => product.or(kernel),
    }
}

#[cfg(unix)]
fn kernel_release() -> Option<String> {
    use std::ffi::CStr;
    // SAFETY: `uname` fills the zeroed struct with NUL-terminated strings.
    let mut info: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut info) } != 0 {
        return None;
    }
    let field = |raw: &[libc::c_char]| {
        unsafe { CStr::from_ptr(raw.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    Some(format!("{} {}", field(&info.sysname), field(&info.release)))
}

#[cfg(not(unix))]
fn kernel_release() -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn product_release() -> Option<String> {
    let version = command_output("sw_vers", &["-productVersion"])?;
    Some(format!("macOS {version}"))
}

#[cfg(target_os = "linux")]
fn product_release() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|name| name.trim_matches('"').to_string())
}

#[cfg(windows)]
fn product_release() -> Option<String> {
    command_output("cmd", &["/C", "ver"])
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn product_release() -> Option<String> {
    None
}

#[cfg(any(target_os = "macos", windows))]
fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            generated: "2026-11-18T10:00:00Z".into(),
            version: "0.10.0",
            os: "macos",
            arch: "aarch64",
            os_release: Some("macOS 26.1".into()),
            exe: Some("/opt/homebrew/Cellar/gz/0.10.0/bin/gz".into()),
            install_method: "Homebrew",
            config_file: Some("~/.config/gz/config.toml".into()),
            profile_name: "prod".into(),
            token_source: "config",
            signed_in: true,
            settings: "client = \"acme\"\ntoken = \"[redacted]\"\n".into(),
            env_overrides: vec![("GZ_TOKEN".into(), "[redacted]".into())],
            endpoints: BTreeMap::from([
                (
                    "etl".to_string(),
                    "acme-admin-etlprovider.acme.api.groundzero.cloud".to_string(),
                ),
                ("mlops".to_string(), "not configured".to_string()),
            ]),
            errors: vec![
                ErrorEntry {
                    time: "2026-11-18T09:00:00Z".into(),
                    command: "gz jobs list".into(),
                    method: "GET".into(),
                    url: "https://acme-admin-etlprovider.acme.api.groundzero.cloud/etl/jobs".into(),
                    status: Some(502),
                    request_id: Some("req-1".into()),
                    kind: "http".into(),
                },
                ErrorEntry {
                    time: "2026-11-18T09:30:00Z".into(),
                    command: "gz sql query".into(),
                    method: "POST".into(),
                    url: "https://x.example.com/lakehouse/run".into(),
                    status: None,
                    request_id: None,
                    kind: "timeout".into(),
                },
            ],
            error_log: Some("~/.config/gz/errors.jsonl".into()),
        }
    }

    #[test]
    fn the_report_shows_every_section_newest_error_first() {
        let report = render(&facts());
        for expected in [
            "- version: 0.10.0",
            "- os: macos (aarch64)",
            "- install method: Homebrew",
            "## Profile `prod`",
            "token = \"[redacted]\"",
            "- env GZ_TOKEN: [redacted]",
            "| etl | acme-admin-etlprovider.acme.api.groundzero.cloud |",
            "| mlops | not configured |",
            "## Recent errors (2)",
            "| 2026-11-18T09:00:00Z | gz jobs list | GET | https://acme-admin-etlprovider.acme.api.groundzero.cloud/etl/jobs | 502 | req-1 |",
            "| no response (timeout) | — |",
        ] {
            assert!(report.contains(expected), "missing {expected:?} in\n{report}");
        }
        let newest = report.find("09:30:00").unwrap();
        let oldest = report.find("09:00:00Z | gz jobs").unwrap();
        assert!(newest < oldest, "newest error first");
    }

    #[test]
    fn an_empty_history_says_so() {
        let mut facts = facts();
        facts.errors.clear();
        facts.settings.clear();
        let report = render(&facts);
        assert!(report.contains("No request failures recorded."));
        assert!(report.contains("# no settings saved for this profile"));
    }

    #[test]
    fn profile_settings_come_out_redacted() {
        let mut profile = crate::config::Profile {
            client: Some("acme".into()),
            token: Some("eyJraWQiOiJ4In0.eyJzdWIiOiJhbmEifQ.sig".into()),
            refresh_token: Some("refresh-value".into()),
            api_key: Some("sk_ci_ana_k1.secretpart".into()),
            username: Some("ana@example.com".into()),
            ..Default::default()
        };
        profile
            .unknown
            .insert("webhook_secret".into(), toml::Value::String("w".into()));
        let text = redacted_settings(&profile, None).unwrap();
        for secret in [
            "eyJraWQ",
            "refresh-value",
            "sk_ci_ana",
            "ana@example.com",
            "\"w\"",
        ] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert!(text.contains("client = \"acme\""), "{text}");
    }

    #[test]
    fn endpoints_show_hosts_and_ports_only() {
        assert_eq!(
            host_of("https://acme-admin-etlprovider.acme.api.groundzero.cloud/v1"),
            "acme-admin-etlprovider.acme.api.groundzero.cloud"
        );
        assert_eq!(host_of("http://127.0.0.1:8080/x?y=1"), "127.0.0.1:8080");
        assert_eq!(host_of("nonsense"), "unreadable URL");
    }

    #[test]
    fn the_default_file_name_is_timestamped() {
        assert_eq!(default_file_name(0), "gz-bug-report-19700101-000000.md");
    }
}
