//! MCP servers that run on the user's own machine, configured in
//! `<gz config folder>/mcp.toml` (see [`config`]). The chat server can't
//! learn their tools ahead of time, so gz offers two generic client tools:
//! `local_mcp_tools` lists the servers' tools (read-only, no prompt) and
//! `local_mcp_call` calls one (asked like a command, see
//! [`crate::assistant::tools::Access::LocalTool`]).
//!
//! A server starts on first use and stops when gz exits. If it crashes, gz
//! starts it again once, on the next use; a second crash leaves it failed
//! until `/mcp restart <name>`.

pub mod client;
pub mod config;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::assistant::tools::{ToolOutput, MAX_OUTPUT_CHARS};
use client::{Connection, RpcError, PROTOCOL_VERSION};
use config::ServerSpec;

const CONFIG_FILE: &str = "mcp.toml";
const LOG_FOLDER: &str = "mcp-logs";
/// Starting can mean `npx` downloading the server first.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
/// A guard against a server that pages forever.
const MAX_TOOL_PAGES: usize = 50;

/// The two client tools, in the chat server's names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalMcpTool {
    ListTools,
    Call,
}

impl LocalMcpTool {
    pub const ALL: [LocalMcpTool; 2] = [LocalMcpTool::ListTools, LocalMcpTool::Call];

    pub fn as_str(self) -> &'static str {
        match self {
            LocalMcpTool::ListTools => "local_mcp_tools",
            LocalMcpTool::Call => "local_mcp_call",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == name)
    }

    /// The tool behind a display name the chat server shows for it
    /// ("Local mcp call").
    pub fn from_display(name: &str) -> Option<Self> {
        Self::parse(&name.trim().to_ascii_lowercase().replace([' ', '-'], "_"))
    }
}

/// The tools to offer: only when the chat server lists them (so it knows
/// them) and at least one server is configured (so they can do something).
pub fn offered(server_tools: Option<&[String]>, local: Option<&LocalMcp>) -> Vec<&'static str> {
    let (Some(server_tools), Some(local)) = (server_tools, local) else {
        return Vec::new();
    };
    if !local.has_servers() {
        return Vec::new();
    }
    LocalMcpTool::ALL
        .into_iter()
        .map(LocalMcpTool::as_str)
        .filter(|name| server_tools.iter().any(|listed| listed == name))
        .collect()
}

/// One tool a server offers.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// Where a server stands, for `/mcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    NotStarted,
    Running {
        tools: usize,
    },
    /// It crashed; gz starts it again on the next use.
    Crashed(String),
    Failed(String),
}

impl Phase {
    pub fn label(&self) -> String {
        match self {
            Phase::NotStarted => "not started".to_string(),
            Phase::Running { tools: 1 } => "running · 1 tool".to_string(),
            Phase::Running { tools } => format!("running · {tools} tools"),
            Phase::Crashed(why) => format!("crashed, restarts on next use: {why}"),
            Phase::Failed(why) => format!("failed: {why}"),
        }
    }
}

/// One server for `/mcp`: its name, its masked command line and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub name: String,
    pub command: String,
    pub phase: Phase,
}

/// The user's local MCP servers; cheap to clone, shared by the main
/// session, its background subagents and the UI.
#[derive(Clone)]
pub struct LocalMcp {
    inner: Arc<Inner>,
}

struct Inner {
    config_path: PathBuf,
    /// Why `mcp.toml` couldn't be used; gz then runs without its servers.
    config_error: Option<String>,
    servers: BTreeMap<String, Server>,
}

struct Server {
    spec: ServerSpec,
    log: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    running: Option<Running>,
    /// Why it isn't running when it should be: a crash, or a failure.
    stopped: Option<Stopped>,
    /// The automatic restart after a crash was used; reset by `/mcp restart`.
    restarted: bool,
}

struct Running {
    connection: Connection,
    tools: Vec<ToolInfo>,
}

enum Stopped {
    Crashed(String),
    Failed(String),
}

impl LocalMcp {
    /// The servers in `<gz config folder>/mcp.toml`. A bad file is reported
    /// by [`Self::config_error`], never fatal.
    pub fn open() -> Self {
        match crate::config::config_path() {
            Ok(path) => Self::from_file(&path.with_file_name(CONFIG_FILE)),
            Err(err) => Self::unusable(PathBuf::from(CONFIG_FILE), format!("{err:#}")),
        }
    }

    /// [`Self::open`], warning on stderr when `mcp.toml` can't be used, so
    /// the user learns why their servers are missing while gz carries on.
    pub fn open_reporting() -> Self {
        let servers = Self::open();
        if let Some(why) = servers.config_error() {
            crate::output::warn(&format!("local MCP servers are off: {why}"));
        }
        servers
    }

    pub fn from_file(path: &Path) -> Self {
        let specs = match config::load(path) {
            Ok(specs) => specs,
            Err(err) => return Self::unusable(path.to_path_buf(), format!("{err:#}")),
        };
        let logs = path.with_file_name(LOG_FOLDER);
        let servers = specs
            .into_iter()
            .map(|(name, spec)| {
                let server = Server {
                    spec,
                    log: logs.join(format!("{name}.log")),
                    state: Mutex::new(State::default()),
                };
                (name, server)
            })
            .collect();
        Self::with(path.to_path_buf(), None, servers)
    }

    fn unusable(config_path: PathBuf, why: String) -> Self {
        Self::with(config_path, Some(why), BTreeMap::new())
    }

    fn with(
        config_path: PathBuf,
        config_error: Option<String>,
        servers: BTreeMap<String, Server>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                config_path,
                config_error,
                servers,
            }),
        }
    }

    pub fn config_path(&self) -> &Path {
        &self.inner.config_path
    }

    pub fn config_error(&self) -> Option<&str> {
        self.inner.config_error.as_deref()
    }

    pub fn has_servers(&self) -> bool {
        !self.inner.servers.is_empty()
    }

    /// Every server's state, without starting any.
    pub async fn statuses(&self) -> Vec<ServerStatus> {
        let mut out = Vec::new();
        for (name, server) in &self.inner.servers {
            let state = server.state.lock().await;
            let phase = match (&state.running, &state.stopped) {
                (Some(running), _) => Phase::Running {
                    tools: running.tools.len(),
                },
                (None, Some(Stopped::Crashed(why))) => Phase::Crashed(why.clone()),
                (None, Some(Stopped::Failed(why))) => Phase::Failed(why.clone()),
                (None, None) => Phase::NotStarted,
            };
            out.push(ServerStatus {
                name: name.clone(),
                command: server.spec.display(),
                phase,
            });
        }
        out
    }

    /// Stops `name` if it runs and starts it fresh, with its automatic
    /// restart available again; returns how many tools it has.
    pub async fn restart(&self, name: &str) -> Result<usize> {
        let server = self.server(name)?;
        let mut state = server.state.lock().await;
        *state = State::default();
        server.ensure_running(name, &mut state).await?;
        Ok(state.running.as_ref().map_or(0, |r| r.tools.len()))
    }

    /// The tools of one server, starting it if needed.
    pub async fn tools(&self, name: &str) -> Result<Vec<ToolInfo>> {
        let server = self.server(name)?;
        let mut state = server.state.lock().await;
        server.ensure_running(name, &mut state).await?;
        Ok(state
            .running
            .as_ref()
            .map(|r| r.tools.clone())
            .unwrap_or_default())
    }

    /// `local_mcp_tools`: each server's tools with their descriptions and
    /// input schemas, as JSON. One server's failure is reported in its
    /// entry and doesn't hide the others.
    pub async fn list_tools(&self, only: Option<&str>) -> ToolOutput {
        let names: Vec<String> = match only {
            Some(name) => match self.server(name) {
                Ok(_) => vec![name.to_string()],
                Err(err) => return ToolOutput::error(format!("{err:#}")),
            },
            None => self.inner.servers.keys().cloned().collect(),
        };
        let mut entries = Vec::new();
        for name in &names {
            entries.push(match self.tools(name).await {
                Ok(tools) => json!({"server": name, "tools": tools.iter().map(tool_json).collect::<Vec<_>>()}),
                Err(err) => json!({"server": name, "error": format!("{err:#}")}),
            });
        }
        if let (Some(_), Some(error)) = (only, entries[0]["error"].as_str()) {
            return ToolOutput::error(error.to_string());
        }
        let full = json!({"servers": entries}).to_string();
        if only.is_some() || full.chars().count() <= MAX_OUTPUT_CHARS {
            return ToolOutput::ok(full);
        }
        // Too much for one result: names only, and how to get the rest.
        let brief: Vec<Value> = entries
            .iter()
            .map(|entry| match entry["tools"].as_array() {
                Some(tools) => json!({
                    "server": entry["server"],
                    "tools": tools.iter().map(|t| t["name"].clone()).collect::<Vec<_>>(),
                }),
                None => entry.clone(),
            })
            .collect();
        ToolOutput::ok(format!(
            "{}\nSchemas left out to fit; call local_mcp_tools with a server for its tools' \
             descriptions and input schemas.",
            json!({"servers": brief})
        ))
    }

    /// `local_mcp_call`, once allowed: the tool's text output, capped.
    pub async fn call(&self, name: &str, tool: &str, arguments: Value) -> ToolOutput {
        let server = match self.server(name) {
            Ok(server) => server,
            Err(err) => return ToolOutput::error(format!("{err:#}")),
        };
        let mut state = server.state.lock().await;
        if let Err(err) = server.ensure_running(name, &mut state).await {
            return ToolOutput::error(format!("{err:#}"));
        }
        let Some(running) = state.running.as_mut() else {
            return ToolOutput::error(format!("{name} is not running; /mcp shows why"));
        };
        let params = json!({"name": tool, "arguments": arguments});
        match running
            .connection
            .request("tools/call", params, server.spec.timeout())
            .await
        {
            Ok(result) => call_output(&result),
            Err(err) => {
                let fatal = err.is_fatal();
                if fatal {
                    state.crashed(err.to_string());
                }
                ToolOutput::error(call_failure(name, tool, &err, fatal))
            }
        }
    }

    fn server(&self, name: &str) -> Result<&Server> {
        self.inner.servers.get(name).with_context(|| {
            let known: Vec<&str> = self.inner.servers.keys().map(String::as_str).collect();
            format!(
                "there is no local MCP server '{name}'; configured: {} (see /mcp)",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            )
        })
    }
}

impl State {
    fn crashed(&mut self, why: String) {
        self.running = None;
        self.stopped = Some(Stopped::Crashed(why));
    }
}

impl Server {
    /// Starts the server when it isn't running: on first use, or once after
    /// a crash. A failure is kept, so later uses report it at once.
    async fn ensure_running(&self, name: &str, state: &mut State) -> Result<()> {
        if let Some(running) = state.running.as_mut() {
            if running.connection.is_alive() {
                return Ok(());
            }
            let why = running.connection.exit_reason().await;
            state.crashed(why);
        }
        match &state.stopped {
            Some(Stopped::Failed(why)) => {
                anyhow::bail!("{name} failed: {why}. Fix it, then run /mcp restart {name}")
            }
            Some(Stopped::Crashed(why)) if state.restarted => {
                let why = format!("it crashed again after a restart ({why})");
                state.stopped = Some(Stopped::Failed(why.clone()));
                anyhow::bail!("{name} failed: {why}. Fix it, then run /mcp restart {name}")
            }
            Some(Stopped::Crashed(_)) => state.restarted = true,
            None => {}
        }
        match self.start().await {
            Ok(running) => {
                state.running = Some(running);
                state.stopped = None;
                Ok(())
            }
            Err(err) => {
                let why = format!("{err:#}");
                state.stopped = Some(Stopped::Failed(why.clone()));
                anyhow::bail!(
                    "{name} failed to start: {why}. See /mcp; fix it, then run /mcp restart {name}"
                )
            }
        }
    }

    async fn start(&self) -> Result<Running> {
        let launch = self.spec.expand(&|var| std::env::var(var).ok())?;
        let mut connection = Connection::spawn(&launch, Some(self.log.clone()))?;
        let init = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "gz", "version": env!("CARGO_PKG_VERSION")},
        });
        let reply = connection
            .request("initialize", init, STARTUP_TIMEOUT)
            .await
            .map_err(|err| anyhow::anyhow!("initialize: {err}"))?;
        connection
            .notify("notifications/initialized", json!({}))
            .await
            .map_err(|err| anyhow::anyhow!("initialized: {err}"))?;
        let tools = if reply["capabilities"].get("tools").is_some() {
            list_all_tools(&mut connection, self.spec.timeout()).await?
        } else {
            Vec::new()
        };
        Ok(Running { connection, tools })
    }
}

/// `tools/list`, following `nextCursor` through every page.
async fn list_all_tools(connection: &mut Connection, limit: Duration) -> Result<Vec<ToolInfo>> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_TOOL_PAGES {
        let params = match &cursor {
            Some(cursor) => json!({"cursor": cursor}),
            None => json!({}),
        };
        let page = connection
            .request("tools/list", params, limit)
            .await
            .map_err(|err| anyhow::anyhow!("tools/list: {err}"))?;
        tools.extend(
            page["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(tool_info),
        );
        cursor = page["nextCursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            return Ok(tools);
        }
    }
    anyhow::bail!("tools/list: the server kept paging after {MAX_TOOL_PAGES} pages")
}

fn tool_info(value: &Value) -> Option<ToolInfo> {
    Some(ToolInfo {
        name: value["name"].as_str()?.to_string(),
        description: value["description"].as_str().map(str::to_string),
        input_schema: value
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"})),
    })
}

fn tool_json(tool: &ToolInfo) -> Value {
    let mut entry = json!({"name": tool.name, "inputSchema": tool.input_schema});
    if let Some(description) = &tool.description {
        entry["description"] = json!(description);
    }
    entry
}

fn call_failure(server: &str, tool: &str, err: &RpcError, fatal: bool) -> String {
    let next = if fatal {
        " gz starts it again on the next call."
    } else {
        ""
    };
    format!("{server} {tool}: {err}.{next}")
}

/// A `tools/call` result as the model reads it: the text content; every
/// other kind of content as one line that says what it was.
fn call_output(result: &Value) -> ToolOutput {
    let mut parts: Vec<String> = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(content_text)
        .collect();
    if parts.is_empty() {
        if let Some(structured) = result.get("structuredContent") {
            parts.push(structured.to_string());
        }
    }
    let text = if parts.is_empty() {
        "(the tool returned no content)".to_string()
    } else {
        parts.join("\n")
    };
    if result["isError"].as_bool() == Some(true) {
        ToolOutput::error(text)
    } else {
        ToolOutput::ok(text)
    }
}

fn content_text(item: &Value) -> String {
    let kind = item["type"].as_str().unwrap_or("unknown");
    match kind {
        "text" => item["text"].as_str().unwrap_or_default().to_string(),
        "resource" if item["resource"]["text"].is_string() => {
            let resource = &item["resource"];
            format!(
                "[resource {}]\n{}",
                resource["uri"].as_str().unwrap_or("?"),
                resource["text"].as_str().unwrap_or_default()
            )
        }
        "resource" => described("resource", &item["resource"]),
        _ => described(kind, item),
    }
}

/// `[image: image/png, 12 KB]`: what non-text content was, not its bytes.
fn described(kind: &str, item: &Value) -> String {
    let mut facts: Vec<String> = Vec::new();
    for key in ["mimeType", "uri", "name"] {
        if let Some(value) = item[key].as_str() {
            facts.push(value.to_string());
        }
    }
    if let Some(data) = item["data"].as_str().or(item["blob"].as_str()) {
        // Base64: four characters carry three bytes.
        facts.push(format!("{} KB", data.len() * 3 / 4 / 1024));
    }
    if facts.is_empty() {
        format!("[{kind} content, not shown]")
    } else {
        format!("[{kind}: {}, not shown]", facts.join(", "))
    }
}

#[cfg(test)]
pub(crate) mod tests;
