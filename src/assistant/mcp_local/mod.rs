//! The user's own MCP servers for `gz code`: local commands over stdio
//! and remote servers over Streamable HTTP or HTTP+SSE (with OAuth), from
//! the user, project and local config scopes (see [`scopes`] and
//! [`config`]). The chat server can't learn their tools ahead of time, so
//! gz offers two generic client tools: `local_mcp_tools` lists the
//! servers' tools (read-only, no prompt) and `local_mcp_call` calls one
//! (asked like a command, see
//! [`crate::assistant::tools::Access::LocalTool`]).
//!
//! A server starts (or connects) on first use and stops when gz exits. If
//! it crashes, gz starts it again once, on the next use; a second crash
//! leaves it failed until `/mcp reconnect <name>`. A project server runs
//! only once the user approved it; a remote server that answers 401 waits
//! for `gz mcp login <name>`.

pub mod client;
pub mod config;
pub mod http;
pub mod oauth;
pub mod scopes;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::assistant::tools::{self, ToolOutput};
use client::{Connection, RpcError, PROTOCOL_VERSION};
use config::Target;
use scopes::{Approval, Entry, Places, Scope};

/// How long listing every server's tools waits for those still starting;
/// they go on starting, and can be listed by name later.
const LIST_ALL_WAIT: Duration = Duration::from_secs(20);
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
/// them) and at least one server may run (so they can do something; a
/// project server waiting for approval, or declined, can't).
pub fn offered(server_tools: Option<&[String]>, local: Option<&LocalMcp>) -> Vec<&'static str> {
    let (Some(server_tools), Some(local)) = (server_tools, local) else {
        return Vec::new();
    };
    if !local.has_usable_servers() {
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
    /// Connected, with its tools' names.
    Running {
        tools: Vec<String>,
    },
    /// It crashed; gz starts it again on the next use.
    Crashed(String),
    Failed(String),
    /// Starting, or answering a call; its tools aren't known until it's done.
    Busy,
    /// A remote server that wants the user to sign in.
    NeedsLogin(String),
    /// A project server the user hasn't approved yet.
    NeedsApproval,
    /// A project server the user declined.
    Declined,
}

impl Phase {
    pub fn label(&self) -> String {
        match self {
            Phase::NotStarted => "not started".to_string(),
            Phase::Running { tools } if tools.is_empty() => "connected · no tools".to_string(),
            Phase::Running { tools } => format!("connected · {}", tools.join(", ")),
            Phase::Crashed(why) => format!("crashed, restarts on next use: {why}"),
            Phase::Failed(why) => format!("failed: {why}"),
            Phase::Busy => "busy: starting, or answering a call".to_string(),
            Phase::NeedsLogin(_) => "needs login".to_string(),
            Phase::NeedsApproval => "needs approval".to_string(),
            Phase::Declined => "declined".to_string(),
        }
    }
}

/// One server for `/mcp`: its name, scope, masked target and state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub name: String,
    pub scope: Scope,
    pub transport: &'static str,
    pub command: String,
    pub phase: Phase,
}

/// The user's MCP servers; cheap to clone, shared by the main session,
/// its background subagents and the UI.
#[derive(Clone)]
pub struct LocalMcp {
    inner: Arc<Inner>,
}

struct Inner {
    places: Places,
    /// Files or entries that couldn't be used; gz runs without them.
    problems: Vec<String>,
    servers: BTreeMap<String, Server>,
}

struct Server {
    entry: Entry,
    /// The project approval, changed by `/mcp approve` / `decline`.
    approval: std::sync::Mutex<Approval>,
    log: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    running: Option<Running>,
    /// Why it isn't running when it should be: a crash, or a failure.
    stopped: Option<Stopped>,
    /// The automatic restart after a crash was used; reset by a reconnect.
    restarted: bool,
}

struct Running {
    connection: Connection,
    tools: Vec<ToolInfo>,
}

enum Stopped {
    Crashed(String),
    Failed(String),
    NeedsLogin(String),
}

impl LocalMcp {
    /// The servers for a session in `project`, from every scope. Bad files
    /// are reported by [`Self::problems`], never fatal.
    pub fn open(project: &Path) -> Self {
        match Places::for_project(Some(project)) {
            Ok(places) => Self::from_places(places),
            Err(err) => Self::unusable(format!("{err:#}")),
        }
    }

    /// [`Self::open`], warning on stderr about files that can't be used and
    /// project servers that wait for approval, so the user learns why
    /// servers are missing while gz carries on.
    pub fn open_reporting(project: &Path) -> Self {
        let servers = Self::open(project);
        for why in servers.problems() {
            crate::output::warn(&format!("MCP: {why}"));
        }
        let pending = servers.pending_approval();
        if !pending.is_empty() {
            crate::output::warn(&format!(
                "MCP: this project's server(s) {} wait for your approval; review with \
                 `gz mcp get <name>` or /mcp, then `gz mcp approve <name>` or /mcp approve <name>",
                pending.join(", ")
            ));
        }
        servers
    }

    /// Only the user scope, from `path`.
    #[cfg(test)]
    pub fn from_file(path: &Path) -> Self {
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut places = Places::at(dir, None);
        places.user_file = path.to_path_buf();
        Self::from_places(places)
    }

    pub fn from_places(places: Places) -> Self {
        let loaded = scopes::load(&places);
        let logs = places.logs();
        let servers = loaded
            .effective()
            .map(|entry| {
                let server = Server {
                    approval: std::sync::Mutex::new(entry.approval),
                    entry: entry.clone(),
                    log: logs.join(format!("{}.log", entry.name)),
                    state: Mutex::new(State::default()),
                };
                (entry.name.clone(), server)
            })
            .collect();
        Self {
            inner: Arc::new(Inner {
                places,
                problems: loaded.problems,
                servers,
            }),
        }
    }

    fn unusable(why: String) -> Self {
        Self {
            inner: Arc::new(Inner {
                places: Places::at(PathBuf::from("."), None),
                problems: vec![why],
                servers: BTreeMap::new(),
            }),
        }
    }

    /// The user-scope file.
    pub fn config_path(&self) -> &Path {
        &self.inner.places.user_file
    }

    pub fn problems(&self) -> &[String] {
        &self.inner.problems
    }

    #[cfg(test)]
    pub fn has_servers(&self) -> bool {
        !self.inner.servers.is_empty()
    }

    /// Whether any server may run: not a project server that waits for
    /// approval or was declined.
    pub fn has_usable_servers(&self) -> bool {
        self.inner
            .servers
            .values()
            .any(|server| matches!(server.approval(), Approval::NotNeeded | Approval::Approved))
    }

    /// Project servers waiting for the user's approval.
    pub fn pending_approval(&self) -> Vec<String> {
        self.inner
            .servers
            .iter()
            .filter(|(_, server)| server.approval() == Approval::Pending)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The server's full entry, for review before approving it.
    pub fn entry(&self, name: &str) -> Result<Entry> {
        Ok(self.server(name)?.entry.clone())
    }

    /// Every server's state, without starting any and without waiting: a
    /// server that is starting or answering a call shows as busy, since
    /// either can take minutes.
    pub fn statuses(&self) -> Vec<ServerStatus> {
        self.inner
            .servers
            .iter()
            .map(|(name, server)| ServerStatus {
                name: name.clone(),
                scope: server.entry.scope,
                transport: server.entry.spec.kind().as_str(),
                command: server.entry.spec.display(),
                phase: server.phase(),
            })
            .collect()
    }

    /// `/mcp approve` / `decline`: records the answer for the project
    /// server's current content; a declined server is disconnected.
    pub async fn approve(&self, name: &str, approved: bool) -> Result<()> {
        let server = self.server(name)?;
        scopes::approve(&self.inner.places, &server.entry, approved)?;
        if let Ok(mut approval) = server.approval.lock() {
            *approval = if approved {
                Approval::Approved
            } else {
                Approval::Declined
            };
        }
        if !approved {
            *server.state.lock().await = State::default();
        }
        Ok(())
    }

    /// Disconnects `name` if connected and connects afresh, with its
    /// automatic restart available again; returns how many tools it has.
    pub async fn restart(&self, name: &str) -> Result<usize> {
        let server = self.server(name)?;
        let mut state = server.state.lock().await;
        *state = State::default();
        server
            .ensure_running(name, &self.inner.places, &mut state)
            .await?;
        Ok(state.running.as_ref().map_or(0, |r| r.tools.len()))
    }

    /// The tools of one server, connecting if needed and listing them
    /// again when the server said they changed. A remote server that
    /// forgot gz's session meanwhile is reconnected once, as for a call.
    pub async fn tools(&self, name: &str) -> Result<Vec<ToolInfo>> {
        let server = self.server(name)?;
        let mut state = server.state.lock().await;
        let mut reconnected = false;
        loop {
            server
                .ensure_running(name, &self.inner.places, &mut state)
                .await?;
            match server.refresh_tools_if_changed(&mut state).await {
                Ok(()) => break,
                Err(RpcError::SessionExpired) if !reconnected => reconnected = true,
                Err(err) => anyhow::bail!("{name} tools/list: {err}"),
            }
        }
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
        self.list_tools_within(only, LIST_ALL_WAIT).await
    }

    /// [`Self::list_tools`]; listing every server waits at most `wait` for
    /// them, so one that hangs while starting can't hold up the others.
    /// One named server is waited for in full: that is what was asked.
    async fn list_tools_within(&self, only: Option<&str>, wait: Duration) -> ToolOutput {
        let entries = match only {
            Some(name) => {
                if let Err(err) = self.server(name) {
                    return ToolOutput::error(format!("{err:#}"));
                }
                match self.tools(name).await {
                    Ok(tools) => vec![tools_entry(name, &tools)],
                    Err(err) => return ToolOutput::error(format!("{err:#}")),
                }
            }
            None => self.every_servers_tools(wait).await,
        };
        let full = json!({"servers": entries}).to_string();
        if only.is_some() || tools::fits_one_result(&full) {
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

    /// Starts every server at once and lists each one's tools; a server
    /// not ready within `wait` is reported as busy and goes on starting.
    async fn every_servers_tools(&self, wait: Duration) -> Vec<Value> {
        let listing: Vec<_> = self
            .inner
            .servers
            .keys()
            .map(|name| {
                let (servers, server) = (self.clone(), name.clone());
                let task = tokio::spawn(async move { servers.tools(&server).await });
                (name.clone(), task)
            })
            .collect();
        let deadline = tokio::time::Instant::now() + wait;
        let mut entries = Vec::new();
        for (name, mut task) in listing {
            entries.push(match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(Ok(Ok(tools))) => tools_entry(&name, &tools),
                Ok(Ok(Err(err))) => json!({"server": name, "error": format!("{err:#}")}),
                Ok(Err(err)) => json!({"server": name, "error": format!("listing failed: {err}")}),
                Err(_) => json!({"server": name, "error": format!(
                    "still busy (starting, or answering a call) after {}s; call local_mcp_tools \
                     with server \"{name}\" to wait for it",
                    wait.as_secs()
                )}),
            });
        }
        entries
    }

    /// `local_mcp_call`, once allowed: the tool's text output, capped at
    /// the server's output limit. A remote server that forgot gz's session
    /// is reconnected and asked once more.
    pub async fn call(&self, name: &str, tool: &str, arguments: Value) -> ToolOutput {
        let server = match self.server(name) {
            Ok(server) => server,
            Err(err) => return ToolOutput::error(format!("{err:#}")),
        };
        let places = &self.inner.places;
        let mut state = server.state.lock().await;
        let params = json!({"name": tool, "arguments": arguments});
        let mut reconnected = false;
        loop {
            if let Err(err) = server.ensure_running(name, places, &mut state).await {
                return ToolOutput::error(format!("{err:#}"));
            }
            let Some(running) = state.running.as_mut() else {
                return ToolOutput::error(format!("{name} is not running; /mcp shows why"));
            };
            let reply = running
                .connection
                .request("tools/call", params.clone(), server.entry.spec.timeout())
                .await;
            match reply {
                Ok(result) => {
                    // A failed refresh shows on the next listing; the call
                    // itself succeeded.
                    let _ = server.refresh_tools_if_changed(&mut state).await;
                    let limit = server.entry.spec.output_limit();
                    return call_output(&result, limit);
                }
                Err(RpcError::SessionExpired) if !reconnected => {
                    reconnected = true;
                    state.running = None;
                }
                Err(err) => {
                    let fatal = err.is_fatal();
                    if fatal {
                        state.stop(&err);
                    }
                    return ToolOutput::error(call_failure(name, tool, &err, fatal));
                }
            }
        }
    }

    fn server(&self, name: &str) -> Result<&Server> {
        self.inner.servers.get(name).with_context(|| {
            let known: Vec<&str> = self.inner.servers.keys().map(String::as_str).collect();
            format!(
                "there is no MCP server '{name}'; configured: {} (see /mcp)",
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
    /// Drops the connection after a fatal error, keeping why.
    fn stop(&mut self, err: &RpcError) {
        self.running = None;
        self.stopped = Some(match err {
            RpcError::NeedsLogin(how) => Stopped::NeedsLogin(how.clone()),
            other => Stopped::Crashed(other.to_string()),
        });
    }
}

impl Server {
    fn approval(&self) -> Approval {
        self.approval
            .lock()
            .map(|a| *a)
            .unwrap_or(Approval::Pending)
    }

    fn phase(&self) -> Phase {
        match self.approval() {
            Approval::Pending => return Phase::NeedsApproval,
            Approval::Declined => return Phase::Declined,
            Approval::NotNeeded | Approval::Approved => {}
        }
        let Ok(state) = self.state.try_lock() else {
            return Phase::Busy;
        };
        match (&state.running, &state.stopped) {
            (Some(running), _) => Phase::Running {
                tools: running.tools.iter().map(|t| t.name.clone()).collect(),
            },
            (None, Some(Stopped::Crashed(why))) => Phase::Crashed(why.clone()),
            (None, Some(Stopped::Failed(why))) => Phase::Failed(why.clone()),
            (None, Some(Stopped::NeedsLogin(how))) => Phase::NeedsLogin(how.clone()),
            (None, None) => Phase::NotStarted,
        }
    }

    /// Connects when it isn't connected: on first use, once after a crash,
    /// and on every use while it needs a login (the user may have signed in
    /// since). A failure is kept, so later uses report it at once.
    async fn ensure_running(&self, name: &str, places: &Places, state: &mut State) -> Result<()> {
        match self.approval() {
            Approval::Pending => anyhow::bail!(
                "{name} is a project server (from {}) that waits for approval: review it with \
                 /mcp, then /mcp approve {name} (or `gz mcp approve {name}`)",
                self.entry.source.display()
            ),
            Approval::Declined => anyhow::bail!(
                "the user declined the project server {name}; /mcp approve {name} turns it on"
            ),
            Approval::NotNeeded | Approval::Approved => {}
        }
        if let Some(running) = state.running.as_mut() {
            if running.connection.is_alive() {
                return Ok(());
            }
            let why = running.connection.exit_reason().await;
            state.running = None;
            state.stopped = Some(Stopped::Crashed(why));
        }
        match &state.stopped {
            Some(Stopped::Failed(why)) => {
                anyhow::bail!("{name} failed: {why}. Fix it, then run /mcp reconnect {name}")
            }
            Some(Stopped::Crashed(why)) if state.restarted => {
                let why = format!("it crashed again after a restart ({why})");
                state.stopped = Some(Stopped::Failed(why.clone()));
                anyhow::bail!("{name} failed: {why}. Fix it, then run /mcp reconnect {name}")
            }
            Some(Stopped::Crashed(_)) => state.restarted = true,
            Some(Stopped::NeedsLogin(_)) | None => {}
        }
        match self.start(name, places).await {
            Ok(running) => {
                state.running = Some(running);
                state.stopped = None;
                Ok(())
            }
            Err(Start::NeedsLogin(how)) => {
                state.stopped = Some(Stopped::NeedsLogin(how.clone()));
                anyhow::bail!("{how}")
            }
            Err(Start::Failed(err)) => {
                let why = format!("{err:#}");
                state.stopped = Some(Stopped::Failed(why.clone()));
                anyhow::bail!(
                    "{name} failed to start: {why}. See /mcp; fix it, then run /mcp reconnect {name}"
                )
            }
        }
    }

    async fn start(&self, name: &str, places: &Places) -> Result<Running, Start> {
        let spec = &self.entry.spec;
        let target = spec
            .expand(&|var| std::env::var(var).ok())
            .map_err(Start::Failed)?;
        let mut connection = match target {
            Target::Stdio(launch) => {
                Connection::spawn(&launch, Some(self.log.clone())).map_err(Start::Failed)?
            }
            Target::Remote(remote) => {
                let auth = http::Auth {
                    store: places.auth(),
                    name: name.to_string(),
                };
                Connection::connect(&remote, Some(auth), spec.startup_timeout())
                    .await
                    .map_err(|err| Start::from_rpc("connect", err))?
            }
        };
        let init = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "gz", "version": env!("CARGO_PKG_VERSION")},
        });
        let reply = connection
            .request("initialize", init, spec.startup_timeout())
            .await
            .map_err(|err| Start::from_rpc("initialize", err))?;
        connection
            .notify("notifications/initialized", json!({}))
            .await
            .map_err(|err| Start::from_rpc("initialized", err))?;
        let tools = if reply["capabilities"].get("tools").is_some() {
            list_all_tools(&mut connection, spec.timeout())
                .await
                .map_err(|err| Start::from_rpc("tools/list", err))?
        } else {
            Vec::new()
        };
        // The fresh list already reflects any change announced meanwhile.
        let _ = connection.take_tools_changed().await;
        Ok(Running { connection, tools })
    }

    /// Lists the tools again when the server said they changed. A
    /// forgotten session drops the connection without counting as a
    /// crash: the next use reconnects at once.
    async fn refresh_tools_if_changed(&self, state: &mut State) -> Result<(), RpcError> {
        let Some(running) = state.running.as_mut() else {
            return Ok(());
        };
        if !running.connection.take_tools_changed().await {
            return Ok(());
        }
        match list_all_tools(&mut running.connection, self.entry.spec.timeout()).await {
            Ok(tools) => {
                running.tools = tools;
                Ok(())
            }
            Err(RpcError::SessionExpired) => {
                state.running = None;
                Err(RpcError::SessionExpired)
            }
            Err(err) => {
                if err.is_fatal() {
                    state.stop(&err);
                }
                Err(err)
            }
        }
    }
}

/// Why a server didn't start.
enum Start {
    NeedsLogin(String),
    Failed(anyhow::Error),
}

impl Start {
    fn from_rpc(step: &str, err: RpcError) -> Self {
        match err {
            RpcError::NeedsLogin(how) => Start::NeedsLogin(how),
            other => Start::Failed(anyhow::anyhow!("{step}: {other}")),
        }
    }
}

/// `tools/list`, following `nextCursor` through every page.
async fn list_all_tools(
    connection: &mut Connection,
    limit: Duration,
) -> Result<Vec<ToolInfo>, RpcError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_TOOL_PAGES {
        let params = match &cursor {
            Some(cursor) => json!({"cursor": cursor}),
            None => json!({}),
        };
        let page = connection.request("tools/list", params, limit).await?;
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
    Err(RpcError::Protocol(format!(
        "the server kept paging after {MAX_TOOL_PAGES} pages"
    )))
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

fn tools_entry(server: &str, tools: &[ToolInfo]) -> Value {
    json!({"server": server, "tools": tools.iter().map(tool_json).collect::<Vec<_>>()})
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
/// other kind of content as one line that says what it was; cut at
/// `limit` characters.
fn call_output(result: &Value, limit: Option<usize>) -> ToolOutput {
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
    let is_error = result["isError"].as_bool() == Some(true);
    let (text, truncated) = match limit {
        Some(limit) => cut_to(text, limit),
        None => (text, false),
    };
    let mut out = if is_error {
        ToolOutput::error(text)
    } else {
        ToolOutput::ok(text)
    };
    out.truncated |= truncated;
    out
}

/// Keeps the start of `text` within `limit` characters, saying how much
/// was left out.
fn cut_to(text: String, limit: usize) -> (String, bool) {
    let total = text.chars().count();
    if total <= limit {
        return (text, false);
    }
    let note = format!(
        "\n[cut: the tool returned {total} characters; this server's limit is {limit} \
         (max_output_chars). Ask the tool for less.]"
    );
    let keep = limit.saturating_sub(note.chars().count());
    let mut cut: String = text.chars().take(keep).collect();
    cut.push_str(&note);
    (cut, true)
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

#[cfg(test)]
mod http_tests;
