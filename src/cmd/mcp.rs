//! `gz mcp setup`: connects AI tools (Claude Code, Codex, Cursor, VS Code) to
//! the site's Groundzero MCP server, mirroring the web app's MCP connect
//! guide (`ground-zero/src/core/mcpConnect/mcpConnection.js`). Every client
//! signs in with its own OAuth flow, so no token is written anywhere.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use crate::cli::{McpAction, McpClient};
use crate::cmd::Runtime;
use crate::config::{self, Resolved};
use crate::output::{self, OutputFormat};

/// The name clients register the server under, as in the web app.
pub const SERVER_NAME: &str = "groundzero";

/// What a config merge did.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    Added,
    Updated,
    Unchanged,
}

pub async fn run(rt: &Runtime, action: &McpAction) -> Result<()> {
    match action {
        McpAction::Setup { client } => setup(rt, *client),
        McpAction::Url => {
            let url = mcp_url(rt.http.resolved())?;
            if rt.format == OutputFormat::Table {
                println!("{url}");
                return Ok(());
            }
            rt.show(json!({"name": SERVER_NAME, "url": url})).await
        }
    }
}

/// The site's MCP endpoint: `{chat}/sites/{site}/mcp/`. A site named
/// `{client}-x` is addressed as `x`, and no site means `admin`, as the web
/// app does.
pub fn mcp_url(resolved: &Resolved) -> Result<String> {
    let chat = resolved.base_for("chat")?;
    let site = mcp_site_name(resolved.site().as_deref(), resolved.client().as_deref());
    Ok(format!(
        "{}/sites/{site}/mcp/",
        config::trim_trailing_slash(&chat)
    ))
}

fn mcp_site_name(site: Option<&str>, client: Option<&str>) -> String {
    let Some(site) = site.filter(|s| !s.is_empty()) else {
        return "admin".to_string();
    };
    let stripped = client
        .and_then(|c| site.strip_prefix(&format!("{c}-")))
        .unwrap_or(site);
    if stripped.is_empty() {
        "admin".to_string()
    } else {
        stripped.to_string()
    }
}

fn setup(rt: &Runtime, client: McpClient) -> Result<()> {
    let url = mcp_url(rt.http.resolved())?;
    match client {
        McpClient::Print => {
            print_snippets(&url);
            Ok(())
        }
        McpClient::Claude => setup_claude(&url),
        McpClient::Codex => setup_codex(&url),
        McpClient::Cursor => {
            let home = home_dir()?;
            write_json_config(
                &home.join(".cursor/mcp.json"),
                "mcpServers",
                json!({"url": url}),
            )?;
            output::info("In Cursor's MCP settings, enable groundzero and sign in when prompted.");
            Ok(())
        }
        McpClient::Vscode => {
            let file = std::env::current_dir()?.join(".vscode/mcp.json");
            write_json_config(&file, "servers", json!({"type": "http", "url": url}))?;
            output::info(
                "Start groundzero from VS Code's MCP servers view and sign in when it asks.",
            );
            Ok(())
        }
    }
}

/// Uses `claude mcp add` (user scope) when Claude Code is installed; else
/// writes a project `.mcp.json` it will pick up.
fn setup_claude(url: &str) -> Result<()> {
    let added = run_client_cli(
        "claude",
        &[
            "mcp",
            "add",
            "--scope",
            "user",
            "--transport",
            "http",
            SERVER_NAME,
            url,
        ],
    )?;
    if !added {
        let file = std::env::current_dir()?.join(".mcp.json");
        write_json_config(&file, "mcpServers", json!({"type": "http", "url": url}))?;
    }
    output::info("Open Claude Code, run /mcp, pick groundzero, and finish the browser sign-in.");
    Ok(())
}

/// Uses `codex mcp add` when Codex is installed; else merges
/// `~/.codex/config.toml`.
fn setup_codex(url: &str) -> Result<()> {
    if !run_client_cli("codex", &["mcp", "add", SERVER_NAME, "--url", url])? {
        let file = home_dir()?.join(".codex/config.toml");
        write_toml_config(&file, url)?;
    }
    output::info("Run `codex mcp login groundzero` and approve access in your browser.");
    Ok(())
}

/// Runs a client's own CLI; `Ok(false)` when it isn't installed.
fn run_client_cli(program: &str, args: &[&str]) -> Result<bool> {
    let output = match Process::new(program).args(args).output() {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("cannot run `{program}`")),
    };
    if output.status.success() {
        output::success(&format!("added {SERVER_NAME} with `{program} mcp add`"));
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.to_lowercase().contains("already exists") {
        output::warn(&format!(
            "{program} already has a server named {SERVER_NAME}; remove it with `{program} mcp remove {SERVER_NAME}` to re-add"
        ));
        return Ok(true);
    }
    anyhow::bail!("`{program} mcp add` failed: {}", stderr.trim())
}

fn home_dir() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .context("cannot find your home directory")
}

/// Adds or updates `{container}.groundzero` in a JSON config, keeping every
/// other server. Pure, so merging is testable without touching disk.
pub fn merge_json(
    existing: Option<&str>,
    container: &str,
    entry: Value,
) -> Result<(String, Change)> {
    let mut root: Value = match existing.map(str::trim).filter(|t| !t.is_empty()) {
        Some(text) => {
            serde_json::from_str(text).context("the existing config is not valid JSON")?
        }
        None => json!({}),
    };
    let servers = root
        .as_object_mut()
        .context("the existing config is not a JSON object")?
        .entry(container)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(|| format!("`{container}` in the existing config is not an object"))?;
    let change = match servers.get(SERVER_NAME) {
        Some(current) if *current == entry => Change::Unchanged,
        Some(_) => Change::Updated,
        None => Change::Added,
    };
    servers.insert(SERVER_NAME.to_string(), entry);
    Ok((
        format!("{}\n", serde_json::to_string_pretty(&root)?),
        change,
    ))
}

/// Adds or updates `[mcp_servers.groundzero]` in a Codex `config.toml`.
pub fn merge_toml(existing: Option<&str>, url: &str) -> Result<(String, Change)> {
    let mut root: toml::Table = match existing.filter(|t| !t.trim().is_empty()) {
        Some(text) => text
            .parse()
            .context("the existing config is not valid TOML")?,
        None => toml::Table::new(),
    };
    let servers = root
        .entry("mcp_servers")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .context("`mcp_servers` in the existing config is not a table")?;
    let mut entry = toml::Table::new();
    entry.insert("url".to_string(), toml::Value::String(url.to_string()));
    let entry = toml::Value::Table(entry);
    let change = match servers.get(SERVER_NAME) {
        Some(current) if *current == entry => Change::Unchanged,
        Some(_) => Change::Updated,
        None => Change::Added,
    };
    servers.insert(SERVER_NAME.to_string(), entry);
    Ok((toml::to_string_pretty(&root)?, change))
}

fn write_json_config(file: &Path, container: &str, entry: Value) -> Result<()> {
    let existing = read_optional(file)?;
    let (text, change) = merge_json(existing.as_deref(), container, entry)?;
    write_merged(file, existing.as_deref(), &text, change)
}

fn write_toml_config(file: &Path, url: &str) -> Result<()> {
    let existing = read_optional(file)?;
    let (text, change) = merge_toml(existing.as_deref(), url)?;
    write_merged(file, existing.as_deref(), &text, change)
}

fn read_optional(file: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", file.display())),
    }
}

/// Writes the merged config, first copying any existing file to
/// `<file>.bak-<unix time>` so nothing is lost.
fn write_merged(file: &Path, existing: Option<&str>, text: &str, change: Change) -> Result<()> {
    if change == Change::Unchanged {
        output::success(&format!("{} already has {SERVER_NAME}", file.display()));
        return Ok(());
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    if let Some(original) = existing {
        let backup = backup_path(file, config::now_unix());
        std::fs::write(&backup, original)
            .with_context(|| format!("cannot back up {}", file.display()))?;
        output::info(&format!("backed up the old config to {}", backup.display()));
    }
    std::fs::write(file, text).with_context(|| format!("cannot write {}", file.display()))?;
    let verb = if change == Change::Added {
        "added"
    } else {
        "updated"
    };
    output::success(&format!("{verb} {SERVER_NAME} in {}", file.display()));
    Ok(())
}

pub fn backup_path(file: &Path, unix: i64) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(format!(".bak-{unix}"));
    PathBuf::from(name)
}

fn print_snippets(url: &str) {
    println!("Groundzero MCP server: {url}");
    println!("Transport: Streamable HTTP · Auth: OAuth (your client opens the sign-in)\n");
    println!("Claude Code:\n  claude mcp add --transport http {SERVER_NAME} \"{url}\"\n  then run /mcp in Claude Code to sign in\n");
    println!(
        "Codex:\n  codex mcp add {SERVER_NAME} --url \"{url}\"\n  codex mcp login {SERVER_NAME}\n"
    );
    let cursor = json!({"mcpServers": {SERVER_NAME: {"url": url}}});
    let vscode = json!({"servers": {SERVER_NAME: {"type": "http", "url": url}}});
    println!(
        "Cursor (~/.cursor/mcp.json):\n{}\n",
        serde_json::to_string_pretty(&cursor).unwrap_or_default()
    );
    println!(
        "VS Code (.vscode/mcp.json):\n{}",
        serde_json::to_string_pretty(&vscode).unwrap_or_default()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_sites_like_the_web_app() {
        assert_eq!(mcp_site_name(Some("preview"), Some("stage")), "preview");
        assert_eq!(mcp_site_name(Some("stage-sales"), Some("stage")), "sales");
        assert_eq!(mcp_site_name(None, Some("stage")), "admin");
        assert_eq!(mcp_site_name(Some("stage-"), Some("stage")), "admin");
    }

    #[test]
    fn builds_the_site_endpoint() {
        let mut config = crate::config::Config::default();
        let profile = config.profile_mut("p");
        profile.client = Some("stage".into());
        profile.site = Some("preview".into());
        profile.domain = Some("groundzerodev.cloud".into());
        let resolved = crate::config::resolve(&config, Some("p"), None, None);
        assert_eq!(
            mcp_url(&resolved).unwrap(),
            "https://stage-admin-gz-chat.stage.api.groundzerodev.cloud/sites/preview/mcp/"
        );
    }

    #[test]
    fn json_merge_keeps_other_servers() {
        let existing = r#"{"mcpServers": {"sentry": {"url": "https://s"}}, "other": 1}"#;
        let (text, change) =
            merge_json(Some(existing), "mcpServers", json!({"url": "https://gz"})).unwrap();
        let merged: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(change, Change::Added);
        assert_eq!(merged["mcpServers"]["sentry"]["url"], "https://s");
        assert_eq!(merged["mcpServers"]["groundzero"]["url"], "https://gz");
        assert_eq!(merged["other"], 1);
        let (_, again) =
            merge_json(Some(&text), "mcpServers", json!({"url": "https://gz"})).unwrap();
        assert_eq!(again, Change::Unchanged);
        let (_, moved) =
            merge_json(Some(&text), "mcpServers", json!({"url": "https://new"})).unwrap();
        assert_eq!(moved, Change::Updated);
    }

    #[test]
    fn json_merge_creates_and_rejects_bad_files() {
        let (text, change) = merge_json(None, "servers", json!({"type": "http"})).unwrap();
        assert_eq!(change, Change::Added);
        assert!(text.contains("\"groundzero\""));
        assert!(merge_json(Some("[1]"), "servers", json!({})).is_err());
        assert!(merge_json(Some("{not json"), "servers", json!({})).is_err());
        assert!(merge_json(Some(r#"{"servers": 3}"#), "servers", json!({})).is_err());
    }

    #[test]
    fn toml_merge_keeps_other_settings() {
        let existing = "model = \"o3\"\n\n[mcp_servers.docs]\ncommand = \"docs-mcp\"\n";
        let (text, change) = merge_toml(Some(existing), "https://gz").unwrap();
        assert_eq!(change, Change::Added);
        let merged: toml::Table = text.parse().unwrap();
        assert_eq!(merged["model"].as_str(), Some("o3"));
        assert_eq!(
            merged["mcp_servers"]["docs"]["command"].as_str(),
            Some("docs-mcp")
        );
        assert_eq!(
            merged["mcp_servers"]["groundzero"]["url"].as_str(),
            Some("https://gz")
        );
        assert_eq!(
            merge_toml(Some(&text), "https://gz").unwrap().1,
            Change::Unchanged
        );
    }

    #[test]
    fn writes_with_a_backup_of_the_old_file() {
        let dir = std::env::temp_dir().join(format!("gz-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("mcp.json");
        std::fs::write(&file, r#"{"mcpServers": {"sentry": {"url": "https://s"}}}"#).unwrap();
        write_json_config(&file, "mcpServers", json!({"url": "https://gz"})).unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["sentry"]["url"], "https://s");
        let backups: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("mcp.json.bak-"))
            .collect();
        assert_eq!(backups.len(), 1);
        let original = std::fs::read_to_string(backups[0].path()).unwrap();
        assert!(!original.contains("groundzero"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backup_names_carry_the_time() {
        assert_eq!(
            backup_path(Path::new("/x/mcp.json"), 42),
            PathBuf::from("/x/mcp.json.bak-42")
        );
    }
}
