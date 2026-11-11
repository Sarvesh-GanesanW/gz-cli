//! A minimal MCP client over stdio: JSON-RPC 2.0, one JSON message per
//! line, one request at a time. Enough for `initialize`, `tools/list` and
//! `tools/call`; the server's own requests get `ping` answered and
//! everything else refused, since gz offers no client capabilities.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

use super::config::Launch;

/// The MCP revision gz speaks; servers may answer with an older one, whose
/// tool calls work the same way.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// The biggest message gz reads from a server; anything larger means the
/// server is broken or hostile, not that gz should buffer it.
const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
/// How much of a server's stderr is kept on disk per server.
const MAX_LOG_BYTES: u64 = 1024 * 1024;
/// How much of the last stderr line is kept to explain a failure.
const MAX_TAIL_CHARS: usize = 300;
/// How long a closed server gets to report its exit status.
const EXIT_GRACE: Duration = Duration::from_secs(1);
const METHOD_NOT_FOUND: i64 = -32601;

/// Why a request got no result.
#[derive(Debug)]
pub enum RpcError {
    /// No answer in time; the server still runs.
    TimedOut(Duration),
    /// The server exited or closed its pipes (a crash).
    Exited(String),
    /// The server answered with a JSON-RPC error.
    Server { code: i64, message: String },
    /// The server broke the protocol; it can't be trusted to answer again.
    Protocol(String),
}

impl RpcError {
    /// Whether the server must be restarted before it can be used again.
    pub fn is_fatal(&self) -> bool {
        matches!(self, RpcError::Exited(_) | RpcError::Protocol(_))
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::TimedOut(limit) => write!(
                f,
                "no answer within {}s (raise timeout_seconds in mcp.toml if it needs longer)",
                limit.as_secs()
            ),
            RpcError::Exited(why) => write!(f, "{why}"),
            RpcError::Server { code, message } => write!(f, "{message} (error {code})"),
            RpcError::Protocol(why) => write!(f, "{why}"),
        }
    }
}

/// One running server process and its pipes.
pub struct Connection {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Bytes of a line not finished yet; kept here so a request that times
    /// out mid-line doesn't lose them and misread the next message.
    partial: Vec<u8>,
    next_id: u64,
    stderr_tail: Arc<Mutex<String>>,
}

impl Connection {
    /// Starts the server; its stderr goes to `log` (capped), never the screen.
    pub fn spawn(launch: &Launch, log: Option<PathBuf>) -> Result<Self> {
        let mut command = tokio::process::Command::new(&launch.program);
        command
            .args(&launch.args)
            .envs(launch.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own group, so stopping it also stops what it started (npx
        // runs the real server as a child).
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().with_context(|| {
            format!(
                "cannot start `{}`; check the command in mcp.toml is installed and on PATH",
                launch.program
            )
        })?;
        let stdin = child.stdin.take().context("the server has no stdin")?;
        let stdout = child.stdout.take().context("the server has no stdout")?;
        let stderr = child.stderr.take().context("the server has no stderr")?;
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        tokio::spawn(keep_stderr(stderr, log, stderr_tail.clone()));
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            partial: Vec::new(),
            next_id: 1,
            stderr_tail,
        })
    }

    /// Whether the process is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Sends `method` and waits up to `limit` for its result. A request
    /// that times out is cancelled, as the protocol asks.
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, RpcError> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(&message).await?;
        match tokio::time::timeout(limit, self.response_to(id)).await {
            Ok(reply) => reply,
            Err(_) => {
                let cancel = json!({"requestId": id, "reason": "gz stopped waiting"});
                // Best effort: the caller already reports the timeout.
                let _ = self.notify("notifications/cancelled", cancel).await;
                Err(RpcError::TimedOut(limit))
            }
        }
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<(), RpcError> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Why the server stopped answering, with its last stderr line.
    pub async fn exit_reason(&mut self) -> String {
        let status = tokio::time::timeout(EXIT_GRACE, self.child.wait()).await;
        let mut why = match status {
            Ok(Ok(status)) => format!("the server exited ({status})"),
            _ => "the server closed its output".to_string(),
        };
        let tail = self
            .stderr_tail
            .lock()
            .map(|t| t.clone())
            .unwrap_or_default();
        if !tail.is_empty() {
            why.push_str(&format!("; last stderr: {tail}"));
        }
        why
    }

    async fn send(&mut self, message: &Value) -> Result<(), RpcError> {
        let mut line = message.to_string();
        line.push('\n');
        let written = async {
            self.stdin.write_all(line.as_bytes()).await?;
            self.stdin.flush().await
        }
        .await;
        match written {
            Ok(()) => Ok(()),
            Err(_) => Err(RpcError::Exited(self.exit_reason().await)),
        }
    }

    async fn response_to(&mut self, id: u64) -> Result<Value, RpcError> {
        loop {
            let message = self.read_message().await?;
            if let Some(method) = message["method"].as_str() {
                if let Some(request_id) = message.get("id") {
                    let answer = server_request_answer(method, request_id.clone());
                    self.send(&answer).await?;
                }
                continue;
            }
            // An answer to a request gz stopped waiting for.
            if message["id"] != json!(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(RpcError::Server {
                    code: error["code"].as_i64().unwrap_or_default(),
                    message: error["message"]
                        .as_str()
                        .unwrap_or("the server reported an error")
                        .to_string(),
                });
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// The next JSON object the server sends. Lines that aren't JSON
    /// objects are skipped: some servers log to stdout by mistake.
    async fn read_message(&mut self) -> Result<Value, RpcError> {
        loop {
            let line = self.read_line().await?;
            if let Ok(message @ Value::Object(_)) = serde_json::from_slice(&line) {
                return Ok(message);
            }
        }
    }

    async fn read_line(&mut self) -> Result<Vec<u8>, RpcError> {
        loop {
            let available = match self.stdout.fill_buf().await {
                Ok(bytes) => bytes,
                Err(err) => return Err(RpcError::Exited(format!("cannot read the server: {err}"))),
            };
            if available.is_empty() {
                return Err(RpcError::Exited(self.exit_reason().await));
            }
            let (taken, complete) = match available.iter().position(|b| *b == b'\n') {
                Some(at) => (at + 1, true),
                None => (available.len(), false),
            };
            self.partial.extend_from_slice(&available[..taken]);
            self.stdout.consume(taken);
            if self.partial.len() > MAX_MESSAGE_BYTES {
                return Err(RpcError::Protocol(format!(
                    "the server sent a message over {} MiB",
                    MAX_MESSAGE_BYTES / (1024 * 1024)
                )));
            }
            if complete {
                return Ok(std::mem::take(&mut self.partial));
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // SAFETY: killpg only sends a signal; the group was created for
            // this server (process_group(0)), so no unrelated process is hit.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
}

/// gz's answer to a request from the server: `ping` succeeds, the rest
/// are capabilities gz never offered.
fn server_request_answer(method: &str, id: Value) -> Value {
    if method == "ping" {
        return json!({"jsonrpc": "2.0", "id": id, "result": {}});
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": METHOD_NOT_FOUND, "message": format!("gz does not support {method}")},
    })
}

/// Copies the server's stderr into its log until the cap, and keeps the
/// last line to explain a crash.
async fn keep_stderr(stderr: ChildStderr, log: Option<PathBuf>, tail: Arc<Mutex<String>>) {
    let mut log = log.and_then(|path| StderrLog::open(path).ok());
    let mut lines = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        match lines.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            if let Ok(mut last) = tail.lock() {
                *last = trimmed.chars().take(MAX_TAIL_CHARS).collect();
            }
        }
        if let Some(file) = log.as_mut() {
            file.append(&line);
        }
    }
}

/// A server's stderr log: appended across restarts, never past the cap.
struct StderrLog {
    file: std::fs::File,
    size: u64,
}

impl StderrLog {
    fn open(path: PathBuf) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        // A full log starts over, so the newest run is the one on disk.
        let fresh = size >= MAX_LOG_BYTES;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(!fresh)
            .write(true)
            .truncate(fresh)
            .open(&path)?;
        Ok(Self {
            file,
            size: if fresh { 0 } else { size },
        })
    }

    fn append(&mut self, bytes: &[u8]) {
        use std::io::Write;
        if self.size >= MAX_LOG_BYTES {
            return;
        }
        let room = (MAX_LOG_BYTES - self.size) as usize;
        let kept = &bytes[..bytes.len().min(room)];
        if self.file.write_all(kept).is_ok() {
            self.size += kept.len() as u64;
        }
    }
}
