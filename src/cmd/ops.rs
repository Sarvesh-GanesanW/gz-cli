use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::{ComputeAction, ConfigureArgs, LogsAction, ProfileAction, RequestArgs, RteAction};
use crate::cmd::{merge_body, parse_query, prompt_line, read_data_arg, seg, Runtime};
use crate::config::{self, load_config, redact_token, save_config};
use crate::http::parse_method;
use crate::output::{self, OutputFormat};

const SERVICES: &[(&str, &str)] = &[
    ("auth", "Sign-in host for MCP tokens"),
    ("lakehouse", "Lakehouse provider"),
    ("etl", "ETL provider"),
    ("files", "File manager"),
    ("agents", "Agent builder"),
    ("chat", "Chat"),
    ("data", "Data provider gateway"),
    ("mlops", "MLOps gateway"),
    ("etlprojects", "DE/ML projects gateway"),
    ("datasets", "Designer datasets gateway"),
    ("dashboards", "Designer dashboards gateway"),
    ("visualizations", "Designer visualizations gateway"),
    ("filters", "Designer filters gateway"),
    ("workspaces", "Workspaces gateway"),
    ("schedules", "Schedules gateway"),
    ("rtes", "Runtimes gateway"),
    ("permissions", "Permissions gateway"),
    ("admin", "Admin gateway"),
    ("logexports", "Log exports gateway"),
    ("connections", "Connections gateway"),
];

/// A cheap path to probe: a health route where the service has one, else
/// the root (any HTTP answer there still proves the host is reachable).
fn probe_path(service: &str) -> &'static str {
    match service {
        "lakehouse" | "agents" | "chat" | "auth" => "/health",
        _ => "/",
    }
}

pub async fn doctor(rt: &Runtime, offline: bool) -> Result<()> {
    let resolved = rt.http.resolved();
    let token = resolved.token();
    let mut hints: Vec<String> = Vec::new();
    if resolved.web_url().is_none() && resolved.host().is_none() {
        hints.push("run `gz configure` to set client, site, and domain".to_string());
    }
    if token.is_none() {
        hints.push("run `gz auth login` (or set GZ_TOKEN for one-off calls)".to_string());
    } else if resolved.token_expired() && resolved.profile.refresh_token.is_none() {
        hints.push("your session expired; run `gz auth login` again".to_string());
    }
    if resolved.profile.gateways.is_empty() {
        hints.push("gateway URLs not loaded yet; run `gz auth login` once".to_string());
    }
    hints.extend(insecure_hints(
        resolved.host().as_deref(),
        resolved.auth_host().as_deref(),
    ));

    let probes = probe_services(rt, offline).await;
    let auth_check = match &token {
        _ if offline => "not checked (--offline)".to_string(),
        Some(_) => rt
            .http
            .request_json(
                "lakehouse",
                Method::GET,
                "/lakehouse/getClientName",
                &[],
                None,
            )
            .await
            .map(|_| "accepted".to_string())
            .unwrap_or_else(|err| {
                format!(
                    "rejected: {}",
                    err.to_string().lines().next().unwrap_or_default()
                )
            }),
        None => "not signed in".to_string(),
    };

    if rt.format != OutputFormat::Table {
        let report = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "profile": resolved.profile_name,
            "configFile": config::config_path()?.display().to_string(),
            "site": resolved.web_url(),
            "username": resolved.profile.username,
            "tokenSource": resolved.token_source,
            "token": token.as_deref().map(redact_token),
            "auth": auth_check,
            "services": probes.iter().map(Probe::to_json).collect::<Vec<_>>(),
            "hints": hints,
        });
        return rt.show(report).await;
    }

    println!(
        "{}",
        output::dim(&format!("gz {} doctor", env!("CARGO_PKG_VERSION")))
    );
    let rows = [
        ("profile", resolved.profile_name.clone()),
        (
            "site",
            resolved
                .web_url()
                .unwrap_or_else(|| "(not configured)".to_string()),
        ),
        (
            "user",
            resolved
                .profile
                .username
                .clone()
                .unwrap_or_else(|| "(not signed in)".to_string()),
        ),
        ("auth", auth_check),
    ];
    for (key, val) in rows {
        println!("  {key:<10} {val}");
    }
    println!();
    for probe in &probes {
        println!("  {}", probe.line());
    }
    if !hints.is_empty() {
        println!();
        for hint in &hints {
            output::warn(hint);
        }
    }
    Ok(())
}

/// Reachability of one service.
struct Probe {
    service: &'static str,
    base_url: Option<String>,
    /// Where the URL came from: override, host, derived, or discovered.
    source: &'static str,
    /// HTTP status and round-trip time, or why the host couldn't be reached.
    outcome: std::result::Result<(u16, u128), String>,
}

impl Probe {
    /// Any HTTP answer below 500 means the host is up (auth is checked separately).
    fn is_up(&self) -> bool {
        matches!(self.outcome, Ok((status, _)) if status < 500)
    }

    fn line(&self) -> String {
        use owo_colors::OwoColorize;
        let name = format!("{:<15}", self.service);
        match (&self.base_url, &self.outcome) {
            (None, _) => format!("{} {name} {}", "–".dimmed(), "not configured".dimmed()),
            (Some(url), Ok((_, ms))) if self.is_up() => {
                format!(
                    "{} {name} {} {}",
                    "✓".green(),
                    format!("{ms}ms").dimmed(),
                    output::dim(url)
                )
            }
            (Some(url), Ok((status, _))) => {
                format!("{} {name} HTTP {status} {}", "✗".red(), output::dim(url))
            }
            (Some(url), Err(err)) if err == NOT_PROBED => {
                format!("{} {name} {}", "·".dimmed(), output::dim(url))
            }
            (Some(url), Err(err)) => format!("{} {name} {err} {}", "✗".red(), output::dim(url)),
        }
    }

    fn to_json(&self) -> Value {
        let (status, latency, error) = match &self.outcome {
            Ok((status, ms)) => (Some(*status), Some(*ms), None),
            Err(err) => (None, None, Some(err.clone())),
        };
        json!({
            "service": self.service,
            "baseUrl": self.base_url,
            "source": self.source,
            "up": self.base_url.is_some() && self.is_up(),
            "status": status,
            "latencyMs": latency,
            "error": error,
        })
    }
}

const NOT_PROBED: &str = "not probed";

fn url_source(resolved: &config::Resolved, service: &str) -> &'static str {
    if resolved.profile.services.contains_key(service) {
        "override"
    } else if resolved.host().is_some() {
        "host"
    } else if resolved.profile.gateways.contains_key(service) {
        "discovered"
    } else {
        "derived"
    }
}

/// Probes every configured service in parallel (or just lists them offline).
async fn probe_services(rt: &Runtime, offline: bool) -> Vec<Probe> {
    let resolved = rt.http.resolved();
    let mut tasks = Vec::new();
    for (service, _) in SERVICES {
        let base_url = resolved.base_for(service).ok();
        let source = url_source(resolved, service);
        let http = rt.http.clone();
        tasks.push(tokio::spawn(async move {
            let outcome = match &base_url {
                Some(_) if offline => Err(NOT_PROBED.to_string()),
                Some(_) => http
                    .probe(service, probe_path(service))
                    .await
                    .map(|(status, elapsed)| (status, elapsed.as_millis()))
                    .map_err(|err| short_network_error(&err)),
                None => Err("not configured".to_string()),
            };
            Probe {
                service,
                base_url,
                source,
                outcome,
            }
        }));
    }
    let mut probes = Vec::with_capacity(tasks.len());
    for task in tasks {
        if let Ok(probe) = task.await {
            probes.push(probe);
        }
    }
    probes
}

fn short_network_error(err: &anyhow::Error) -> String {
    let text = format!("{err:#}").to_lowercase();
    if text.contains("dns") {
        "host not found".to_string()
    } else if text.contains("timed out") {
        "timed out".to_string()
    } else {
        "unreachable".to_string()
    }
}

fn prompt_profile(updated: &mut config::Profile) -> Result<()> {
    println!("Configuring profile (Enter keeps current value in [brackets]):");
    let client = prompt_line(&format!(
        "Client [{}]: ",
        updated.client.as_deref().unwrap_or("dev")
    ))?;
    if !client.is_empty() {
        updated.client = Some(client);
    } else if updated.client.is_none() {
        updated.client = Some("dev".to_string());
    }
    let site = prompt_line(&format!(
        "Site [{}]: ",
        updated.site.as_deref().unwrap_or("dev")
    ))?;
    if !site.is_empty() {
        updated.site = Some(site);
    } else if updated.site.is_none() {
        updated.site = Some("dev".to_string());
    }
    let domain = prompt_line(&format!(
        "Domain [{}]: ",
        updated.domain.as_deref().unwrap_or(config::DEFAULT_DOMAIN)
    ))?;
    if !domain.is_empty() {
        updated.domain = Some(domain.trim().trim_end_matches('/').to_string());
    }
    Ok(())
}

fn insecure_hints(host: Option<&str>, auth_host: Option<&str>) -> Vec<String> {
    let mut hints = Vec::new();
    if let Some(url) = host {
        if !config::is_secure_base(url) {
            hints.push(format!("host {url} is plain http; switch to https"));
        }
    }
    if let Some(url) = auth_host {
        if Some(url) != host && !config::is_secure_base(url) {
            hints.push(format!("auth host {url} is plain http; switch to https"));
        }
    }
    hints
}

pub async fn configure(args: &ConfigureArgs) -> Result<()> {
    let mut config = load_config()?;
    let name = config.active_name(None);
    let entry = config.profile_mut(&name).clone();
    let mut updated = entry.clone();
    if args.client.is_none()
        && args.domain.is_none()
        && args.host.is_none()
        && args.auth_host.is_none()
        && args.site.is_none()
        && args.service.is_empty()
        && !args.clear_services
        && !args.dry_run
    {
        prompt_profile(&mut updated)?;
    }
    if let Some(client) = &args.client {
        updated.client = Some(client.clone());
    }
    if let Some(domain) = &args.domain {
        updated.domain = Some(domain.trim().trim_end_matches('/').to_string());
    }
    if let Some(host) = &args.host {
        updated.host = Some(host.trim_end_matches('/').to_string());
    }
    if let Some(auth_host) = &args.auth_host {
        updated.auth_host = Some(auth_host.trim_end_matches('/').to_string());
    }
    if let Some(site) = &args.site {
        updated.site = Some(site.clone());
    }
    if args.clear_services {
        updated.services.clear();
    }
    for item in &args.service {
        let (key, url) = item
            .split_once('=')
            .with_context(|| format!("--service must be SERVICE=URL, got '{item}'"))?;
        if !SERVICES.iter().any(|(name, _)| *name == key) {
            anyhow::bail!("unknown service '{key}' (see `gz doctor` for the list)");
        }
        updated
            .services
            .insert(key.to_string(), url.trim_end_matches('/').to_string());
    }
    if args.dry_run {
        let value = json!({"profile": name, "before": entry, "after": updated});
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    *config.profile_mut(&name) = updated;
    if config.current_profile.is_none() {
        config.current_profile = Some(name.clone());
    }
    let path = save_config(&config)?;
    output::success(&format!("profile '{name}' saved to {}", path.display()));
    Ok(())
}

pub async fn profile(action: &ProfileAction, rt: &Runtime) -> Result<()> {
    let mut config = load_config()?;
    match action {
        ProfileAction::List => {
            let current = config.active_name(None);
            let rows: Vec<Value> = {
                let mut names: Vec<&String> = config.profiles.keys().collect();
                names.sort();
                names
                    .into_iter()
                    .map(|name| {
                        let entry = &config.profiles[name];
                        json!({
                            "name": name,
                            "current": *name == current,
                            "client": entry.client,
                            "site": entry.site,
                            "authenticated": entry.token.is_some(),
                        })
                    })
                    .collect()
            };
            rt.show(Value::Array(rows)).await
        }
        ProfileAction::Show => {
            let name = config.active_name(None);
            let entry = config.profile(&name);
            let value = json!({
                "name": name,
                "client": entry.client,
                "domain": entry.domain,
                "hostOverride": entry.host,
                "authHost": entry.auth_host,
                "site": entry.site,
                "services": entry.services,
                "username": entry.username,
                "token": entry.token.as_deref().map(redact_token),
                "expiresAt": entry.expires_at,
            });
            rt.show(value).await
        }
        ProfileAction::Use { name } => {
            if !config.profiles.contains_key(name) {
                anyhow::bail!("no such profile '{name}' (run `gz profile list`)");
            }
            config.current_profile = Some(name.clone());
            save_config(&config)?;
            output::success(&format!("now using profile '{name}'"));
            Ok(())
        }
        ProfileAction::Delete { name } => {
            if config.profiles.remove(name).is_none() {
                anyhow::bail!("no such profile '{name}'");
            }
            if config.current_profile.as_deref() == Some(name) {
                config.current_profile = None;
            }
            save_config(&config)?;
            output::success(&format!("deleted profile '{name}'"));
            Ok(())
        }
    }
}

pub async fn raw_request(rt: &Runtime, args: &RequestArgs) -> Result<()> {
    if !SERVICES.iter().any(|(name, _)| *name == args.service) {
        anyhow::bail!(
            "unknown service '{}' (see `gz doctor` for the list)",
            args.service
        );
    }
    let method = parse_method(&args.method)?;
    let payload = merge_body(args.body.data.as_deref(), None)?;
    let query = parse_query(&args.body.query)?;
    if !matches!(method, Method::GET | Method::HEAD)
        && rt.format == OutputFormat::Table
        && !rt.quiet
    {
        output::info(&format!("{} {}", method, args.path));
    }
    let value = rt
        .http
        .request_json(&args.service, method, &args.path, &query, payload)
        .await?;
    rt.show(value).await
}

pub async fn logs(rt: &Runtime, action: &LogsAction) -> Result<()> {
    const SVC: &str = "logexports";
    let run = |id: &str| format!("/log-export/runs/{}", seg(id));
    let schedule = |name: &str| format!("/log-export/schedules/{}", seg(name));
    match action {
        LogsAction::Export { body } => {
            rt.call(SVC, Method::POST, "/log-export/runs", body, None)
                .await
        }
        LogsAction::Runs {
            schedule,
            trigger,
            page_size,
            offset,
        } => {
            let mut query = Vec::new();
            for (key, value) in [
                ("scheduleName", schedule.clone()),
                ("triggerType", trigger.clone()),
                ("limit", page_size.map(|n| n.to_string())),
                ("offset", offset.map(|n| n.to_string())),
            ] {
                if let Some(value) = value {
                    query.push((key.to_string(), value));
                }
            }
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/log-export/runs", &query, None)
                .await?;
            rt.show_columns(
                &value["runs"],
                &[
                    "exportName",
                    "status",
                    "triggerType",
                    "scheduleName",
                    "createdAt",
                    "exportRunId",
                ],
            )
        }
        LogsAction::Run { id } => rt.get(SVC, &run(id), &[]).await,
        LogsAction::DeleteRun { id } => rt.send_empty(SVC, Method::DELETE, &run(id)).await,
        LogsAction::Schedules => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/log-export/schedules", &[], None)
                .await?;
            rt.show_columns(
                &value["schedules"],
                &[
                    "scheduleName",
                    "status",
                    "cronExpression",
                    "timezone",
                    "createdBy",
                    "modifiedAt",
                ],
            )
        }
        LogsAction::GetSchedule { name } => rt.get(SVC, &schedule(name), &[]).await,
        LogsAction::CreateSchedule { body } => {
            rt.call(SVC, Method::POST, "/log-export/schedules", body, None)
                .await
        }
        LogsAction::UpdateSchedule { name, body } => {
            rt.call(SVC, Method::PATCH, &schedule(name), body, None)
                .await
        }
        LogsAction::StopSchedule { name } => {
            rt.send_empty(SVC, Method::POST, &format!("{}/stop", schedule(name)))
                .await
        }
        LogsAction::DeleteSchedule { name } => {
            rt.send_empty(SVC, Method::DELETE, &schedule(name)).await
        }
    }
}

pub async fn rte(rt: &Runtime, action: &RteAction) -> Result<()> {
    const SVC: &str = "rtes";
    match action {
        RteAction::List => rt.get(SVC, "/rte/image", &[]).await,
        RteAction::Register { body } => rt.call(SVC, Method::POST, "/rte/image", body, None).await,
        RteAction::Images => rt.get(SVC, "/rte/image/images", &[]).await,
        RteAction::All => rt.get(SVC, "/rte/image/all", &[]).await,
        RteAction::Compute { action } => compute(rt, action.as_ref()).await,
    }
}

async fn compute(rt: &Runtime, action: Option<&ComputeAction>) -> Result<()> {
    const SVC: &str = "rtes";
    let one = |id: i64| format!("/rte/compute/{id}");
    let Some(action) = action else {
        let value = rt
            .http
            .request_json(SVC, Method::GET, "/rte/compute", &[], None)
            .await?;
        return rt.show_columns(&value, &["Name", "Id", "Description", "Config"]);
    };
    match action {
        ComputeAction::Get { id } => rt.get(SVC, &one(*id), &[]).await,
        ComputeAction::Create { name } => {
            rt.send(SVC, Method::POST, "/rte/compute", json!({"Name": name}))
                .await
        }
        ComputeAction::Update { id, config } => {
            let config = read_data_arg(Some(config))?.context("--config is empty")?;
            rt.send(SVC, Method::PUT, &one(*id), json!({"config": config}))
                .await
        }
        ComputeAction::Rename { id, name } => {
            rt.send(SVC, Method::PATCH, &one(*id), json!({"Name": name}))
                .await
        }
        ComputeAction::Delete { id } => rt.send_empty(SVC, Method::DELETE, &one(*id)).await,
    }
}

pub fn completion(shell: &str) -> Result<()> {
    use clap::CommandFactory;
    use clap_complete::generate;
    use std::io::stdout;
    let generator = match shell.to_lowercase().as_str() {
        "bash" => clap_complete::Shell::Bash,
        "zsh" => clap_complete::Shell::Zsh,
        "fish" => clap_complete::Shell::Fish,
        "powershell" | "pwsh" => clap_complete::Shell::PowerShell,
        "elvish" => clap_complete::Shell::Elvish,
        other => anyhow::bail!("unsupported shell '{other}' (bash, zsh, fish, powershell, elvish)"),
    };
    let mut cmd = crate::cli::Cli::command();
    generate(generator, &mut cmd, "gz", &mut stdout());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_plaintext_hosts_once() {
        assert!(insecure_hints(Some("https://a.test"), Some("https://a.test")).is_empty());
        assert!(insecure_hints(Some("http://127.0.0.1:9"), None).is_empty());
        let hints = insecure_hints(Some("http://a.test"), Some("http://a.test"));
        assert_eq!(hints.len(), 1);
        let hints = insecure_hints(Some("http://a.test"), Some("http://b.test"));
        assert_eq!(hints.len(), 2);
    }
}
