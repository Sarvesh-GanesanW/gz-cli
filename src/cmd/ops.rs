use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::{ConfigureArgs, LogsAction, ProfileAction, RequestArgs, RteAction};
use crate::cmd::{merge_body, parse_query, Runtime};
use crate::config::{self, load_config, redact_token, save_config};
use crate::http::parse_method;
use crate::oauth;
use crate::output::{self, OutputFormat};

const SERVICES: &[(&str, &str)] = &[
    ("auth", "OAuth + chat API host"),
    ("lakehouse", "Iceberg provider API"),
    ("iceberg", "Iceberg engine API"),
    ("etl", "ETL provider API"),
    ("data", "Data provider API"),
    ("files", "File manager API"),
    ("mlops", "MLOps provider API"),
    ("mlopsadmin", "MLOps admin API"),
    ("agents", "Agent builder API"),
    ("chat", "Chat API"),
    ("logs", "Log provider API"),
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
    ("connections", "Connections gateway"),
];

pub async fn doctor(rt: &Runtime) -> Result<()> {
    let resolved = rt.http.resolved();
    let token = resolved.token();
    let host = resolved.host();
    let auth_host = resolved.auth_host();

    let mut oauth_ok = false;
    let mut oauth_error: Option<String> = None;
    if let Some(auth) = auth_host.clone() {
        match oauth::discover(rt.http.raw(), &auth).await {
            Some(_) => oauth_ok = true,
            None => oauth_error = Some("authorization-server metadata unreachable".to_string()),
        }
    } else {
        oauth_error = Some("no host configured".to_string());
    }

    let missing: Vec<String> = Vec::new();
    let mut hints: Vec<String> = Vec::new();
    if resolved.client().is_none() && host.is_none() {
        hints.push(
            "run `gz configure --client <name> --site <site>` (URLs derive automatically)"
                .to_string(),
        );
    }
    if resolved.site().is_none() {
        hints.push(
            "no site set; run `gz configure --site <site>` (sent as gz-site header)".to_string(),
        );
    }
    if token.is_none() {
        hints.push("run `gz auth login` (or set GZ_TOKEN for one-off calls)".to_string());
    } else if resolved.token_expired() {
        hints.push("stored token is expired; run `gz auth login` again".to_string());
    }
    hints.extend(insecure_hints(host.as_deref(), auth_host.as_deref()));

    let report = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "profile": resolved.profile_name,
        "configFile": config::config_path()?.display().to_string(),
        "client": resolved.client(),
        "domain": resolved.domain(),
        "hostOverride": host,
        "authHost": auth_host,
        "site": resolved.site(),
        "authenticated": token.is_some(),
        "tokenSource": resolved.token_source,
        "token": token.as_deref().map(redact_token),
        "tokenExpired": token.is_some() && resolved.token_expired(),
        "oauthDiscovery": {"ok": oauth_ok, "error": oauth_error},
        "services": service_table(resolved),
        "missing": missing,
        "hints": hints,
    });

    if rt.format == OutputFormat::Table {
        println!("{}", output::dim("gz doctor"));
        let rows = [
            ("version", env!("CARGO_PKG_VERSION").to_string()),
            ("profile", resolved.profile_name.clone()),
            (
                "client",
                resolved.client().unwrap_or_else(|| "(missing)".to_string()),
            ),
            (
                "site",
                resolved.site().unwrap_or_else(|| "(missing)".to_string()),
            ),
            ("domain", resolved.domain()),
            (
                "authHost",
                auth_host.clone().unwrap_or_else(|| "(missing)".to_string()),
            ),
            (
                "auth",
                match &token {
                    Some(t) => format!("ok ({} {})", resolved.token_source, redact_token(t)),
                    None => "missing".to_string(),
                },
            ),
            (
                "oauth discovery",
                if oauth_ok {
                    "ok".to_string()
                } else {
                    "unreachable".to_string()
                },
            ),
        ];
        for (key, val) in rows {
            println!("  {key:<16} {val}");
        }
        if !hints.is_empty() {
            println!();
            for hint in &hints {
                output::warn(hint);
            }
        }
        return Ok(());
    }
    rt.show(report).await
}

fn prompt_line(label: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    print!("{label}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("cannot read input")?;
    Ok(line.trim().to_string())
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

fn service_table(resolved: &config::Resolved) -> Value {
    let rows: Vec<Value> = SERVICES
        .iter()
        .map(|(key, desc)| {
            let (url, source) = match resolved.base_for(key) {
                Ok(url) => {
                    let source = if resolved.profile.services.contains_key(*key) {
                        "override"
                    } else if resolved.host().is_some() {
                        "host"
                    } else {
                        "derived"
                    };
                    (url, source)
                }
                Err(_) => ("(unconfigured)".to_string(), "missing"),
            };
            json!({"service": key, "description": desc, "baseUrl": url, "source": source})
        })
        .collect();
    Value::Array(rows)
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
                "token": entry.token.as_deref().map(redact_token),
                "tokenType": entry.token_type,
                "scope": entry.scope,
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
    match action {
        LogsAction::Validate { body } => {
            rt.call("logs", Method::POST, "/log-exports/validate", body, None)
                .await
        }
        LogsAction::Export { body } => {
            rt.call("logs", Method::POST, "/log-exports", body, None)
                .await
        }
        LogsAction::Schedules => {
            let value = rt
                .http
                .request_json("logs", Method::GET, "/log-export-schedules", &[], None)
                .await?;
            rt.show(value).await
        }
        LogsAction::GetSchedule { name } => {
            let value = rt
                .http
                .request_json(
                    "logs",
                    Method::GET,
                    &format!("/log-export-schedules/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        LogsAction::CreateSchedule { body } => {
            rt.call("logs", Method::POST, "/log-export-schedules", body, None)
                .await
        }
        LogsAction::DeleteSchedule { name } => {
            let value = rt
                .http
                .request_json(
                    "logs",
                    Method::DELETE,
                    &format!("/log-export-schedules/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        LogsAction::PauseSchedule { name } => {
            let value = rt
                .http
                .request_json(
                    "logs",
                    Method::POST,
                    &format!("/log-export-schedules/{name}/pause"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        LogsAction::ResumeSchedule { name } => {
            let value = rt
                .http
                .request_json(
                    "logs",
                    Method::POST,
                    &format!("/log-export-schedules/{name}/resume"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
    }
}

pub async fn rte(rt: &Runtime, action: &RteAction) -> Result<()> {
    match action {
        RteAction::List => {
            let rows = vec![
                json!({"engine": "duckdb", "flavor": "base", "repo": "gz-base-rte-duckdb"}),
                json!({"engine": "duckdb", "flavor": "pytorch", "repo": "gz-rte-duckdb-pytorch"}),
                json!({"engine": "duckdb", "flavor": "sklearn", "repo": "gz-rte-duckdb-sklearn"}),
                json!({"engine": "duckdb", "flavor": "tensorflow", "repo": "gz-rte-duckdb-tensorflow"}),
                json!({"engine": "spark", "flavor": "base", "repo": "gz-base-rte-spark"}),
                json!({"engine": "spark", "flavor": "pytorch", "repo": "gz-rte-spark-pytorch"}),
                json!({"engine": "spark", "flavor": "sklearn", "repo": "gz-rte-spark-sklearn"}),
                json!({"engine": "spark", "flavor": "tensorflow", "repo": "gz-rte-spark-tensorflow"}),
                json!({"engine": "custom", "flavor": "custom", "repo": "gz-custom-rte"}),
            ];
            rt.show(Value::Array(rows)).await
        }
        RteAction::Image { body } => rt.call("rtes", Method::GET, "/rte/image", body, None).await,
        RteAction::Register { body } => {
            rt.call("rtes", Method::POST, "/rte/image", body, None)
                .await
        }
        RteAction::Images => {
            let value = rt
                .http
                .request_json("rtes", Method::GET, "/rte/image/images", &[], None)
                .await?;
            rt.show(value).await
        }
        RteAction::All => {
            let value = rt
                .http
                .request_json("rtes", Method::GET, "/rte/image/all", &[], None)
                .await?;
            rt.show(value).await
        }
        RteAction::Compute => {
            let value = rt
                .http
                .request_json("rtes", Method::GET, "/rte/compute", &[], None)
                .await?;
            rt.show(value).await
        }
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
