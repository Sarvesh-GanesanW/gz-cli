//! `run_script`: programmatic tool calling. The model writes one short
//! Python or Node program that calls gz's file tools as functions; gz runs
//! it in the workspace and returns only what it prints.
//!
//! The script is arbitrary code running as the user, so it is approved like
//! a command (the user reads it first). Its helper calls reach gz over a
//! loopback "bridge" — 127.0.0.1, random port, random per-run token,
//! newline-delimited JSON — and go through the same sandbox, secret
//! blocking, permission prompts, and audit log as single tool calls.

use anyhow::{Context as _, Result};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use crate::assistant::audit::{AuditLog, Entry};
use crate::assistant::checkpoints::{self, EditHistory};
use crate::assistant::hooks::{self, Hooks};
use crate::assistant::local_context::LocalContext;
use crate::assistant::os_sandbox::{Confined, Confinement};
use crate::assistant::output_spill::OutputSpill;
use crate::assistant::permissions::{grant_folder, Approver, Decision, Policy, ScriptEdits};
use crate::assistant::sandbox::Sandbox;
use crate::assistant::session::AssistantEvent;
use crate::assistant::tools::{self, Access, Language, Planned, ScriptSpec, ToolName, ToolOutput};
use crate::cmd::new_uuid;

/// Contract cap on helper calls per script.
pub const MAX_HELPER_CALLS: usize = 500;
/// Largest single bridge request (a `write_file` carries the whole file).
const MAX_REQUEST_BYTES: u64 = 32 * 1024 * 1024;
/// Output kept from each of stdout and stderr: the first and last half of
/// it, with the middle drained unread and marked. What doesn't fit one
/// result is saved for the model to page through.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

const HELPER_PY: &str = include_str!("script_helpers/gz.py");
const HELPER_JS: &str = include_str!("script_helpers/gz.js");
const CALLER_JS: &str = include_str!("script_helpers/gz_call.js");

pub type HelperFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

/// Answers one helper call from a running script.
pub trait HelperHandler: Send {
    fn handle<'a>(&'a mut self, tool: &'a str, input: &'a Value) -> HelperFuture<'a>;

    /// Where output too long for one result is saved whole; `None` cuts it.
    fn output_spill(&self) -> Option<OutputSpill> {
        None
    }
}

/// Runs an approved script, under `confinement` when given, and returns
/// its output for the model.
pub async fn execute(
    spec: &ScriptSpec,
    root: &Path,
    confinement: Option<&Confinement>,
    handler: &mut dyn HelperHandler,
) -> ToolOutput {
    match run(spec, root, confinement, handler).await {
        Ok(out) => out,
        Err(err) => ToolOutput::error(format!("{err:#}")),
    }
}

async fn run(
    spec: &ScriptSpec,
    root: &Path,
    confinement: Option<&Confinement>,
    handler: &mut dyn HelperHandler,
) -> Result<ToolOutput> {
    let workdir = ScratchDir::create()?;
    let script_path = workdir.write_helpers(spec)?;
    let mut bridge = Bridge::bind(MAX_HELPER_CALLS).await?;
    let confinement = confinement.map(|c| c.with_bridge_port(bridge.port()));
    let (mut command, confined) = match interpreter(spec.language, confinement.as_ref()) {
        Ok(found) => found,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ToolOutput::error(missing_interpreter(spec.language)));
        }
        Err(err) => return Err(err).context("cannot start the script"),
    };
    set_up(
        &mut command,
        spec.language,
        &script_path,
        &workdir.path,
        root,
        &bridge,
    );
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ToolOutput::error(missing_interpreter(spec.language)));
        }
        Err(err) => return Err(err).context("cannot start the script"),
    };
    let _group = ProcessGroup::of(&child);
    let stdout = tokio::spawn(capture(child.stdout.take()));
    let stderr = tokio::spawn(capture(child.stderr.take()));

    // Time spent answering helper calls (including the user reading a diff)
    // doesn't count against the script's timeout.
    let mut deadline = Instant::now() + spec.timeout;
    let status = loop {
        tokio::select! {
            status = child.wait() => break Some(status.context("cannot wait for the script")?),
            accepted = bridge.listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let started = Instant::now();
                bridge.serve(stream, handler).await;
                deadline += started.elapsed();
            }
            _ = tokio::time::sleep_until(deadline) => break None,
        }
    };
    let ending = match status {
        Some(status) => Ending::Exited(status.code()),
        None => {
            child.kill().await.ok();
            Ending::TimedOut(spec.timeout)
        }
    };
    let stdout = stdout.await.unwrap_or_default();
    let stderr = stderr.await.unwrap_or_default();
    let spill = handler.output_spill();
    let mut out = report(&stdout, &stderr, ending, bridge.over_limit, spill.as_ref());
    if let Some(confined) = &confined {
        let reverts = confined.git.restore_risky_changes();
        if !reverts.is_empty() {
            out.output.push_str(&format!("\n{}", reverts.model_note()));
            if let Some(confinement) = &confinement {
                confinement.tell_user(&reverts);
            }
        }
    }
    // A read-only run gets its own note (`read_only::after_read_only_script`):
    // the /sandbox commands this one offers would not help it.
    if confinement.as_ref().is_some_and(Confinement::is_read_only) {
        return Ok(out.refit());
    }
    Ok(with_blocked_note(out, &spec.code, confined).refit())
}

/// Adds the sandbox's note when a confined script failed on a denial.
fn with_blocked_note(mut out: ToolOutput, code: &str, confined: Option<Confined>) -> ToolOutput {
    if !out.is_error {
        return out;
    }
    if let Some(note) = confined.and_then(|c| c.blocked_note(code, &out.output)) {
        out.output.push_str(&format!("\n{note}"));
    }
    out
}

/// How the script process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// Exit code, or `None` when a signal stopped it.
    Exited(Option<i32>),
    TimedOut(Duration),
}

fn interpreter_name(language: Language) -> &'static str {
    match language {
        Language::Python if cfg!(windows) => "python",
        Language::Python => "python3",
        Language::Node => "node",
    }
}

fn missing_interpreter(language: Language) -> String {
    let other = match language {
        Language::Python => "node",
        Language::Node => "python",
    };
    format!(
        "{} not found on this machine; install it or write the script in {other}",
        interpreter_name(language)
    )
}

/// The interpreter, under the OS sandbox when given one.
fn interpreter(
    language: Language,
    confinement: Option<&Confinement>,
) -> std::io::Result<(tokio::process::Command, Option<Confined>)> {
    let name = interpreter_name(language);
    match confinement {
        Some(confinement) => confinement.command(name),
        None => Ok((tokio::process::Command::new(name), None)),
    }
}

fn set_up(
    command: &mut tokio::process::Command,
    language: Language,
    script: &Path,
    helpers: &Path,
    root: &Path,
    bridge: &Bridge,
) {
    command
        .arg(script)
        .current_dir(root)
        .env("GZ_BRIDGE", bridge.address())
        .env("GZ_BRIDGE_TOKEN", &bridge.token)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match language {
        Language::Python => command
            .env("PYTHONPATH", helpers)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("PYTHONIOENCODING", "utf-8"),
        Language::Node => command.env("NODE_PATH", helpers),
    };
    #[cfg(unix)]
    command.process_group(0);
}

/// The model-facing result: stdout, then stderr, then why it stopped.
fn report(
    stdout: &Captured,
    stderr: &Captured,
    ending: Ending,
    over_limit: bool,
    spill: Option<&OutputSpill>,
) -> ToolOutput {
    let mut text = stdout.text();
    let errors = stderr.text();
    if !errors.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&errors);
    }
    let mut failed = over_limit;
    let mut note = |line: String| {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&line);
    };
    if over_limit {
        note(format!(
            "[gz stopped answering helper calls after {MAX_HELPER_CALLS}]"
        ));
    }
    match ending {
        Ending::Exited(Some(0)) => {}
        Ending::Exited(Some(code)) => {
            failed = true;
            note(format!("[exit code {code}]"));
        }
        Ending::Exited(None) => {
            failed = true;
            note("[the script was stopped by a signal]".to_string());
        }
        Ending::TimedOut(limit) => {
            failed = true;
            note(format!(
                "[timed out after {}s; gz stopped the script]",
                limit.as_secs()
            ));
        }
    }
    match spill {
        Some(spill) => spill.fit(text, failed, stdout.dropped + stderr.dropped),
        None if failed => ToolOutput::error(text),
        None => ToolOutput::ok(text),
    }
}

/// What one stream of a script printed: all of it, or its first and last
/// `MAX_CAPTURE_BYTES / 2` bytes when it printed more. The start holds what
/// the script set out to do, the end its traceback or totals.
#[derive(Debug, Default)]
struct Captured {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    /// Bytes from the middle that were not kept.
    dropped: usize,
}

impl Captured {
    const HALF: usize = MAX_CAPTURE_BYTES / 2;

    #[cfg(test)]
    fn of(bytes: &[u8]) -> Self {
        let mut captured = Self::default();
        captured.push(bytes);
        captured
    }

    fn push(&mut self, bytes: &[u8]) {
        let room = Self::HALF.saturating_sub(self.head.len()).min(bytes.len());
        let (start, rest) = bytes.split_at(room);
        self.head.extend_from_slice(start);
        self.tail.extend(rest);
        if self.tail.len() > Self::HALF {
            let cut = self.tail.len() - Self::HALF;
            self.tail.drain(..cut);
            self.dropped += cut;
        }
    }

    /// The kept text, with a line marking the dropped middle. A character
    /// split by the cut is left out rather than shown as U+FFFD.
    fn text(&self) -> String {
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        if self.dropped == 0 {
            let mut all = self.head.clone();
            all.extend_from_slice(&tail);
            return String::from_utf8_lossy(&all).into_owned();
        }
        let head = &self.head[..char_start_at_or_before(&self.head, self.head.len())];
        let partial = tail.iter().take_while(|b| is_continuation(**b)).count();
        format!(
            "{}\n[gz dropped {} bytes from the middle of this stream]\n{}",
            String::from_utf8_lossy(head),
            self.dropped,
            String::from_utf8_lossy(&tail[partial.min(3)..])
        )
    }
}

fn is_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

/// Where `bytes[..end]` ends without a character cut in two: `end`, or the
/// start of a last multi-byte character that is not complete.
fn char_start_at_or_before(bytes: &[u8], end: usize) -> usize {
    let lead = (end.saturating_sub(4)..end)
        .rev()
        .find(|&i| !is_continuation(bytes[i]));
    let Some(lead) = lead else { return end };
    let width = match bytes[lead] {
        b if b < 0x80 => 1,
        b if b >= 0xF0 => 4,
        b if b >= 0xE0 => 3,
        _ => 2,
    };
    match lead + width > end {
        true => lead,
        false => end,
    }
}

/// Reads a pipe to the end, keeping its start and end (see [`Captured`])
/// so a chatty script can't exhaust memory or block on a full pipe.
async fn capture(pipe: Option<impl AsyncRead + Unpin>) -> Captured {
    let mut captured = Captured::default();
    let Some(mut pipe) = pipe else {
        return captured;
    };
    let mut buf = [0u8; 8192];
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            break;
        }
        captured.push(&buf[..n]);
    }
    captured
}

/// Per-run folder holding the script and the `gz` helper modules; removed
/// when the run ends.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn create() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("gz-script-{}", new_uuid()));
        std::fs::create_dir_all(&path).context("cannot create a folder for the script")?;
        Ok(Self { path })
    }

    /// Writes the helpers for the script's language and the script itself;
    /// returns the script's path.
    fn write_helpers(&self, spec: &ScriptSpec) -> Result<PathBuf> {
        let files: &[(&str, &str)] = match spec.language {
            Language::Python => &[("gz.py", HELPER_PY)],
            Language::Node => &[("gz.js", HELPER_JS), ("gz_call.js", CALLER_JS)],
        };
        for (name, text) in files {
            std::fs::write(self.path.join(name), text)
                .with_context(|| format!("cannot write the {name} helper"))?;
        }
        let script = self.path.join(match spec.language {
            Language::Python => "script.py",
            Language::Node => "script.js",
        });
        std::fs::write(&script, &spec.code).context("cannot write the script")?;
        Ok(script)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).ok();
    }
}

/// Kills the script's whole process group when the run ends, so background
/// children can't outlive a timeout or a cancelled turn.
struct ProcessGroup(Option<u32>);

impl ProcessGroup {
    fn of(child: &tokio::process::Child) -> Self {
        Self(child.id())
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            // SAFETY: killpg only sends a signal; the group was created for
            // this script (process_group(0)), so no unrelated process is hit.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

/// The loopback endpoint scripts call.
struct Bridge {
    listener: TcpListener,
    token: String,
    max_calls: usize,
    calls: usize,
    over_limit: bool,
}

impl Bridge {
    async fn bind(max_calls: usize) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("cannot open the local script bridge")?;
        Ok(Self {
            listener,
            token: new_uuid(),
            max_calls,
            calls: 0,
            over_limit: false,
        })
    }

    fn port(&self) -> u16 {
        self.listener.local_addr().map_or(0, |a| a.port())
    }

    fn address(&self) -> String {
        self.listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default()
    }

    /// Answers every request on one connection, one JSON line each. A wrong
    /// token ends the connection.
    async fn serve(&mut self, stream: TcpStream, handler: &mut dyn HelperHandler) {
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read.take(MAX_REQUEST_BYTES));
        let mut line = String::new();
        while matches!(reader.read_line(&mut line).await, Ok(n) if n > 0) {
            let (reply, keep_open) = self.answer(&line, handler).await;
            line.clear();
            let mut text = reply.to_string();
            text.push('\n');
            if write.write_all(text.as_bytes()).await.is_err() || !keep_open {
                return;
            }
        }
    }

    async fn answer(&mut self, line: &str, handler: &mut dyn HelperHandler) -> (Value, bool) {
        let Ok(request) = serde_json::from_str::<Value>(line) else {
            return (failure("gz could not read this request"), false);
        };
        if request["token"].as_str() != Some(self.token.as_str()) {
            return (
                failure("gz rejected this request: wrong bridge token"),
                false,
            );
        }
        if self.calls >= self.max_calls {
            self.over_limit = true;
            return (
                failure(&format!(
                    "this script reached gz's limit of {} helper calls",
                    self.max_calls
                )),
                true,
            );
        }
        self.calls += 1;
        let tool = request["tool"].as_str().unwrap_or_default();
        match handler.handle(tool, &request["input"]).await {
            Ok(result) => (json!({"ok": true, "result": result}), true),
            Err(error) => (failure(&error), true),
        }
    }
}

fn failure(error: &str) -> Value {
    json!({"ok": false, "error": error})
}

/// Helper calls from an approved script, checked like single tool calls:
/// reads inside the workspace are covered by approving the script; each
/// edit or write shows its diff and asks unless edits were allowed for this
/// chat (by any edit's prompt, a script's or a direct one) or always.
pub struct ScriptContext<'a> {
    pub sandbox: &'a Sandbox,
    pub policy: &'a Policy,
    pub approver: &'a dyn Approver,
    pub audit: Option<&'a AuditLog>,
    pub conversation_id: &'a str,
    pub run_id: &'a str,
    pub on_event: &'a mut (dyn FnMut(AssistantEvent) + Send),
    /// Set when the user answered an edit with "allow always": the session
    /// saves the rule once the script ends.
    pub edits_always: Option<Planned>,
    /// Where each helper write keeps what it replaced (`/rewind`).
    edit_history: EditHistory,
    /// The session's hooks, run around each helper call like any tool call.
    pub hooks: Option<Hooks>,
    /// What `after_tool` hooks sent back for the model (exit code 2); the
    /// session adds it to the script's result.
    pub hook_feedback: Vec<String>,
    /// The session's local context: files a helper reads or edits pick up
    /// their folder's instruction files, as the same direct tool call would.
    pub local_context: Option<&'a LocalContext>,
    /// The script runs without anyone having approved it, so its helper
    /// writes are refused (see [`crate::assistant::read_only`]).
    pub read_only: bool,
    /// Set when a helper write was refused for that.
    pub write_refused: bool,
    /// Checks the files helpers write; the session reports them with the
    /// script's result.
    pub diagnostics: crate::assistant::diagnostics::Batch,
}

impl<'a> ScriptContext<'a> {
    pub fn new(
        sandbox: &'a Sandbox,
        policy: &'a Policy,
        approver: &'a dyn Approver,
        audit: Option<&'a AuditLog>,
        ids: (&'a str, &'a str),
        on_event: &'a mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Self {
        Self {
            sandbox,
            policy,
            approver,
            audit,
            conversation_id: ids.0,
            run_id: ids.1,
            on_event,
            edits_always: None,
            edit_history: EditHistory::disabled(),
            hooks: None,
            hook_feedback: Vec::new(),
            local_context: None,
            read_only: false,
            write_refused: false,
            diagnostics: Default::default(),
        }
    }

    /// Keeps what helper writes replace in `history`.
    pub fn with_edit_history(mut self, history: EditHistory) -> Self {
        self.edit_history = history;
        self
    }

    async fn call(&mut self, tool_name: &str, input: &Value) -> Result<Value, String> {
        let tool = ToolName::parse(tool_name)
            .filter(|t| {
                // Scripts get the folder helpers only (the contract's list).
                !matches!(
                    t,
                    ToolName::RunCommand
                        | ToolName::RunScript
                        | ToolName::NotebookEdit
                        | ToolName::WebFetch
                )
            })
            .ok_or_else(|| format!("'{tool_name}' is not available inside scripts"))?;
        let planned = tools::plan(self.sandbox, tool, input).map_err(|err| {
            self.record(tool_name, &input.to_string(), ("rejected", None), true);
            format!("{err:#}")
        })?;
        let mut decided = "allowed";
        if planned.access == Access::Write && self.read_only {
            self.write_refused = true;
            self.record(tool_name, &planned.summary, ("read-only", None), true);
            return Err(crate::assistant::read_only::HELPER_WRITE_REFUSED.to_string());
        }
        if planned.access == Access::Write && !self.policy.edits_allowed(&planned) {
            let folder = grant_folder(self.sandbox, &planned);
            let request = ScriptEdits::request_for(&planned, &self.sandbox.display_folder(&folder));
            let decision = self.approver.decide(&request).await;
            match decision {
                Decision::Deny => {
                    let refusal = self.approver.take_refusal();
                    let reason = refusal.audit_reason();
                    self.record(
                        tool_name,
                        &planned.summary,
                        (refusal.audit_label(), reason.as_deref()),
                        true,
                    );
                    return Err(refusal.message());
                }
                Decision::AllowChat => self.policy.remember(&planned, decision, &folder),
                Decision::AllowAlways => {
                    self.policy.remember(&planned, decision, &folder);
                    self.edits_always = Some(planned.clone());
                }
                Decision::AllowOnce => {}
            }
            decided = self.approver.take_decided_by().audit_label(decision);
        }
        let hook_call = hooks::ToolCall::new(self.sandbox, tool_name, &planned.summary, input);
        if let Some(blocked) = self.before_hooks(&hook_call).await {
            return Err(blocked.output);
        }
        (self.on_event)(AssistantEvent::ToolStarted {
            name: tool_name.to_string(),
            summary: format!("script · {}", planned.summary),
            local: true,
        });
        let sandbox = self.sandbox;
        (self.diagnostics)
            .before(
                &planned,
                &sandbox.cwd(),
                |f| sandbox.display(f),
                self.on_event,
            )
            .await;
        let result =
            checkpoints::run_helper(&self.edit_history, self.sandbox, &planned, self.on_event)
                .map_err(|err| format!("{err:#}"));
        self.after_hooks(&hook_call, &result).await;
        if let (Ok(_), Some(context)) = (&result, self.local_context) {
            context.note_tool_call(self.sandbox, tool_name, input);
        }
        self.record(
            tool_name,
            &planned.summary,
            (decided, None),
            result.is_err(),
        );
        result
    }

    async fn before_hooks(&mut self, call: &hooks::ToolCall<'_>) -> Option<ToolOutput> {
        let hooks = self.hooks.clone()?;
        let log = self.hook_log();
        hooks.before_tool(call, &log, self.on_event).await
    }

    async fn after_hooks(&mut self, call: &hooks::ToolCall<'_>, result: &Result<Value, String>) {
        let Some(hooks) = self.hooks.clone() else {
            return;
        };
        let output = match result {
            Ok(value) => value.to_string(),
            Err(err) => err.clone(),
        };
        let log = self.hook_log();
        let feedback = hooks
            .after_tool_feedback(call, (result.is_err(), &output), &log, self.on_event)
            .await;
        self.hook_feedback.extend(feedback);
    }

    fn hook_log(&self) -> hooks::Log<'a> {
        hooks::Log {
            audit: self.audit,
            conversation_id: self.conversation_id,
            run_id: self.run_id,
        }
    }

    /// Logs one helper call; `decision` is the decision and, for a block, why.
    fn record(&self, tool: &str, target: &str, decision: (&str, Option<&str>), is_error: bool) {
        let Some(log) = self.audit else {
            return;
        };
        let (decision, reason) = decision;
        let entry = Entry {
            conversation_id: self.conversation_id,
            run_id: self.run_id,
            tool: &format!("run_script/{tool}"),
            target,
            decision,
            reason,
            is_error,
        };
        if let Err(err) = log.record(&entry) {
            crate::output::warn(&format!("cannot write the audit log: {err:#}"));
        }
    }
}

impl HelperHandler for ScriptContext<'_> {
    fn handle<'b>(&'b mut self, tool: &'b str, input: &'b Value) -> HelperFuture<'b> {
        Box::pin(self.call(tool, input))
    }

    fn output_spill(&self) -> Option<OutputSpill> {
        Some(self.sandbox.spill().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::permissions::{DecisionFuture, PermissionRequest};
    use crate::assistant::sandbox::tests::temp_workspace;
    use std::sync::Mutex;

    /// Answers every prompt with `answer` and remembers what was asked.
    struct Scripted {
        answer: Decision,
        asked: Mutex<Vec<PermissionRequest>>,
    }

    impl Scripted {
        fn new(answer: Decision) -> Self {
            Self {
                answer,
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl Approver for Scripted {
        fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
            self.asked.lock().unwrap().push(request.clone());
            Box::pin(async move { self.answer })
        }
    }

    fn has(program: &str) -> bool {
        std::process::Command::new(program)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn spec(language: Language, code: &str, secs: u64) -> ScriptSpec {
        ScriptSpec {
            language,
            code: code.to_string(),
            timeout: Duration::from_secs(secs),
        }
    }

    async fn run_in(
        sandbox: &Sandbox,
        approver: &Scripted,
        script: &ScriptSpec,
    ) -> (ToolOutput, Vec<String>) {
        let policy = Policy::default();
        let mut events = Vec::new();
        let mut on_event = |e: AssistantEvent| {
            if let AssistantEvent::ToolStarted { summary, .. } = e {
                events.push(summary);
            }
        };
        let mut ctx =
            ScriptContext::new(sandbox, &policy, approver, None, ("c", "r"), &mut on_event);
        let out = execute(script, &sandbox.cwd(), None, &mut ctx).await;
        (out, events)
    }

    /// Echoes the tool name back, counting calls.
    struct Echo(usize);

    impl HelperHandler for Echo {
        fn handle<'a>(&'a mut self, tool: &'a str, _: &'a Value) -> HelperFuture<'a> {
            self.0 += 1;
            Box::pin(async move { Ok(json!(tool)) })
        }
    }

    async fn ask(bridge: &mut Bridge, handler: &mut Echo, line: String) -> Value {
        let address = bridge.address();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(line.as_bytes()).await.unwrap();
            let mut reply = String::new();
            BufReader::new(stream).read_line(&mut reply).await.unwrap();
            reply
        });
        let (stream, _) = bridge.listener.accept().await.unwrap();
        bridge.serve(stream, handler).await;
        serde_json::from_str(&client.await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn bridge_rejects_wrong_tokens() {
        let mut bridge = Bridge::bind(10).await.unwrap();
        let mut echo = Echo(0);
        let reply = ask(
            &mut bridge,
            &mut echo,
            "{\"token\":\"nope\",\"tool\":\"read_file\"}\n".into(),
        )
        .await;
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("token"));
        assert_eq!(echo.0, 0, "a rejected request never reaches a tool");
    }

    #[tokio::test]
    async fn bridge_caps_helper_calls() {
        let mut bridge = Bridge::bind(2).await.unwrap();
        let mut echo = Echo(0);
        let token = bridge.token.clone();
        let line =
            format!("{{\"token\":\"{token}\",\"tool\":\"list_directory\",\"input\":{{}}}}\n");
        for _ in 0..2 {
            let reply = ask(&mut bridge, &mut echo, line.clone()).await;
            assert_eq!(reply["result"], "list_directory");
        }
        let reply = ask(&mut bridge, &mut echo, line).await;
        assert!(reply["error"].as_str().unwrap().contains("limit of 2"));
        assert!(bridge.over_limit);
        assert_eq!(echo.0, 2);
    }

    #[test]
    fn reports_exit_codes_timeouts_and_limits() {
        let ok = report(
            &Captured::of(b"hi\n"),
            &Captured::default(),
            Ending::Exited(Some(0)),
            false,
            None,
        );
        assert_eq!((ok.output.as_str(), ok.is_error), ("hi\n", false));
        let failed = report(
            &Captured::of(b"x"),
            &Captured::of(b"Traceback\n"),
            Ending::Exited(Some(1)),
            false,
            None,
        );
        assert!(failed.is_error);
        assert!(failed.output.ends_with("Traceback\n[exit code 1]"));
        let timed_out = report(
            &Captured::default(),
            &Captured::default(),
            Ending::TimedOut(Duration::from_secs(3)),
            false,
            None,
        );
        assert!(timed_out.output.contains("timed out after 3s"));
        assert!(
            report(
                &Captured::default(),
                &Captured::default(),
                Ending::Exited(Some(0)),
                true,
                None
            )
            .is_error
        );
    }

    #[test]
    fn long_streams_keep_their_start_and_end() {
        let mut captured = Captured::default();
        captured.push("é".repeat(MAX_CAPTURE_BYTES / 4 + 1).as_bytes());
        for i in 0..200_000 {
            captured.push(format!("row {i}\n").as_bytes());
        }
        captured.push(b"Traceback: boom\n");
        let text = captured.text();
        assert!(captured.dropped > 0);
        assert!(text.starts_with("éé"));
        assert!(
            text.ends_with("row 199999\nTraceback: boom\n"),
            "the end is kept"
        );
        assert!(text.contains(&format!(
            "\n[gz dropped {} bytes from the middle of this stream]\n",
            captured.dropped
        )));
        assert!(
            !text.contains('\u{FFFD}'),
            "a character cut in two is left out"
        );

        let short = Captured::of("one ✓\n".as_bytes());
        assert_eq!((short.text().as_str(), short.dropped), ("one ✓\n", 0));
    }

    #[test]
    fn a_character_split_at_the_cut_is_left_out() {
        let bytes = "aé".as_bytes();
        assert_eq!(char_start_at_or_before(bytes, 2), 1);
        assert_eq!(char_start_at_or_before(bytes, 3), 3);
        assert_eq!(char_start_at_or_before("a€".as_bytes(), 3), 1);
    }

    /// A script nobody approved: under the read-only profile it reads the
    /// open folder, but writes nothing, directly or through a helper, and
    /// reads nothing else under home.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_read_only_script_reads_but_cannot_write_or_leave_the_folder() {
        use crate::assistant::os_sandbox::read_only::ReadOnlyKind;
        use crate::assistant::os_sandbox::{OsSandbox, SandboxSettings};
        if !has("python3") {
            eprintln!("skipped: python3 not installed");
            return;
        }
        let home = crate::assistant::sandbox::tests::temp_dir("ro-script-home");
        std::fs::create_dir_all(home.join("work")).unwrap();
        std::fs::write(home.join("work/a.txt"), "inside").unwrap();
        std::fs::write(home.join("private.txt"), "outside").unwrap();
        let sandbox = Sandbox::with_home(&home.join("work"), Some(&home)).unwrap();
        let confinement = Confinement::new(
            sandbox.clone(),
            OsSandbox::unsaved(SandboxSettings::default()),
        )
        .read_only(ReadOnlyKind::Script);
        // A local server the script must not reach (its bridge to gz it can).
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let code = format!(
            r#"
import gz
print(open("a.txt").read())
for attempt in (lambda: open("{home}/private.txt").read(), lambda: open("b.txt", "w").write("x"),
                lambda: gz.write_file("c.txt", "x"),
                lambda: __import__("socket").create_connection(("127.0.0.1", {port}), 3)):
    try:
        attempt()
        print("allowed")
    except Exception as err:
        print("refused:", type(err).__name__)
"#,
            home = home.display()
        );
        let policy = Policy::default();
        let approver = Scripted::new(Decision::AllowOnce);
        let mut on_event = |_: AssistantEvent| {};
        let mut ctx = ScriptContext::new(
            &sandbox,
            &policy,
            &approver,
            None,
            ("c", "r"),
            &mut on_event,
        );
        ctx.read_only = true;
        let script = spec(Language::Python, &code, 30);
        let out = execute(&script, &sandbox.cwd(), Some(&confinement), &mut ctx).await;
        assert!(out.output.contains("inside"), "{}", out.output);
        assert!(!out.output.contains("allowed"), "{}", out.output);
        assert_eq!(out.output.matches("refused:").count(), 4, "{}", out.output);
        assert!(ctx.write_refused, "the helper write was refused");
        assert!(
            approver.asked.lock().unwrap().is_empty(),
            "nobody was asked"
        );
        assert!(!home.join("work/b.txt").exists() && !home.join("work/c.txt").exists());
    }

    #[tokio::test]
    async fn python_scripts_call_tools_and_print_summaries() {
        if !has("python3") {
            eprintln!("skipped: python3 not installed");
            return;
        }
        let (dir, sandbox) = temp_workspace("script-py");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(dir.join("src/b.rs"), "fn c() {}\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        let code = r#"
import gz
total = 0
for path in gz.find_files("**/*.rs"):
    total += len(gz.read_file(path).splitlines())
print(f"{len(gz.find_files('**/*.rs'))} files, {total} lines")
hits = gz.search_files("fn c")
print(hits[0]["path"], hits[0]["line"])
try:
    gz.read_file(".env")
except gz.ToolError as err:
    print("blocked:", err)
"#;
        let approver = Scripted::new(Decision::Deny);
        let (out, events) = run_in(&sandbox, &approver, &spec(Language::Python, code, 30)).await;
        assert!(!out.is_error, "{}", out.output);
        assert!(out.output.contains("2 files, 3 lines"), "{}", out.output);
        assert!(out.output.contains("src/b.rs 1"), "{}", out.output);
        assert!(out.output.contains("blocked:"), "{}", out.output);
        assert!(
            approver.asked.lock().unwrap().is_empty(),
            "reads never prompt"
        );
        assert!(
            events.iter().any(|e| e == "script · read_file src/a.rs"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn node_scripts_call_tools_synchronously() {
        if !has("node") {
            eprintln!("skipped: node not installed");
            return;
        }
        let (dir, sandbox) = temp_workspace("script-node");
        std::fs::write(dir.join("notes.md"), "one\ntwo\n").unwrap();
        let code = r#"
const gz = require("gz");
const files = gz.list_directory(".");
console.log(files.join(","), gz.read_file("notes.md").split("\n").length);
"#;
        let approver = Scripted::new(Decision::Deny);
        let (out, _) = run_in(&sandbox, &approver, &spec(Language::Node, code, 30)).await;
        assert!(!out.is_error, "{}", out.output);
        assert!(out.output.contains("notes.md 3"), "{}", out.output);
    }

    #[tokio::test]
    async fn edits_inside_scripts_ask_with_a_diff_and_denials_raise() {
        if !has("python3") {
            eprintln!("skipped: python3 not installed");
            return;
        }
        let (dir, sandbox) = temp_workspace("script-edit");
        std::fs::write(dir.join("a.txt"), "old\n").unwrap();
        crate::assistant::sandbox::tests::seen_by_model(&sandbox, &dir.join("a.txt"));
        let code = r#"
import gz
try:
    gz.edit_file("a.txt", "old", "new")
except gz.ToolError as err:
    print("refused:", err)
"#;
        let approver = Scripted::new(Decision::Deny);
        let (out, _) = run_in(&sandbox, &approver, &spec(Language::Python, code, 30)).await;
        assert!(
            out.output.contains("refused: The user denied this action."),
            "{}",
            out.output
        );
        let asked = approver.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].chat_scope, "all edits in this chat");
        assert_eq!(asked[0].summary, "edit_file a.txt");
        assert!(asked[0].preview.as_deref().unwrap().contains("+new"));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "old\n");
    }

    #[tokio::test]
    async fn allowing_edits_for_the_chat_covers_the_scripts_later_edits() {
        if !has("python3") {
            eprintln!("skipped: python3 not installed");
            return;
        }
        let (dir, sandbox) = temp_workspace("script-edit-all");
        std::fs::write(dir.join("a.txt"), "x\n").unwrap();
        std::fs::write(dir.join("b.txt"), "x\n").unwrap();
        for name in ["a.txt", "b.txt"] {
            crate::assistant::sandbox::tests::seen_by_model(&sandbox, &dir.join(name));
        }
        let code =
            "import gz\nfor f in ['a.txt', 'b.txt']:\n    print(gz.edit_file(f, 'x', 'y'))\n";
        let approver = Scripted::new(Decision::AllowChat);
        let (out, _) = run_in(&sandbox, &approver, &spec(Language::Python, code, 30)).await;
        assert!(!out.is_error, "{}", out.output);
        assert_eq!(approver.asked.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "y\n");
    }

    #[tokio::test]
    async fn long_script_output_is_saved_whole_for_the_model() {
        if !has("python3") {
            eprintln!("skipped: python3 not installed");
            return;
        }
        let (_dir, sandbox) = temp_workspace("script-spill");
        let code = "for i in range(30000):\n    print(f'row {i}')\n";
        let approver = Scripted::new(Decision::AllowOnce);
        let (out, _) = run_in(&sandbox, &approver, &spec(Language::Python, code, 30)).await;
        assert!(out.truncated && !out.is_error, "{}", out.output);
        assert!(out
            .output
            .starts_with("[The whole output (30000 lines) is saved in "));
        assert!(out.output.contains("]\nrow 0\n") && out.output.ends_with("row 29999\n"));
        let saved = out.output.split("is saved in ").nth(1).unwrap();
        let path = std::path::PathBuf::from(saved.split(':').next().unwrap());
        let whole = std::fs::read_to_string(&path).unwrap();
        assert_eq!(whole.lines().count(), 30_000);
        assert!(sandbox.spill().contains(&path));
    }

    #[tokio::test]
    async fn timeouts_stop_the_script() {
        if !has("python3") {
            eprintln!("skipped: python3 not installed");
            return;
        }
        let (_dir, sandbox) = temp_workspace("script-timeout");
        let approver = Scripted::new(Decision::Deny);
        let started = std::time::Instant::now();
        let code = "import time\nprint('start', flush=True)\ntime.sleep(30)\n";
        let (out, _) = run_in(&sandbox, &approver, &spec(Language::Python, code, 1)).await;
        assert!(out.is_error);
        assert!(out.output.contains("timed out after 1s"), "{}", out.output);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn helper_reads_pick_up_the_folders_instruction_files() {
        use crate::assistant::local_context::{tests::recording_folders, Limits};
        let (dir, sandbox) = temp_workspace("script-instructions");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/GZ.md"), "sub rules").unwrap();
        std::fs::write(dir.join("sub/x.txt"), "x").unwrap();
        let (allowed, _) = recording_folders(Vec::new());
        let context = LocalContext::new(None, None, allowed);
        let picked_up = || {
            context
                .instructions(&sandbox, &Limits::default())
                .iter()
                .any(|entry| entry.text == "sub rules")
        };
        assert!(!picked_up());
        let approver = Scripted::new(Decision::AllowOnce);
        let policy = Policy::default();
        let mut on_event = |_: AssistantEvent| {};
        let mut ctx = ScriptContext::new(
            &sandbox,
            &policy,
            &approver,
            None,
            ("c", "r"),
            &mut on_event,
        );
        ctx.local_context = Some(&context);
        ctx.call("read_file", &json!({"path": "sub/missing.txt"}))
            .await
            .unwrap_err();
        assert!(!picked_up(), "a failed read picks up nothing");
        ctx.call("read_file", &json!({"path": "sub/x.txt"}))
            .await
            .unwrap();
        assert!(picked_up());
    }

    #[tokio::test]
    async fn commands_are_not_offered_to_scripts() {
        let (_dir, sandbox) = temp_workspace("script-nocmd");
        let approver = Scripted::new(Decision::AllowOnce);
        let policy = Policy::default();
        let mut on_event = |_: AssistantEvent| {};
        let mut ctx = ScriptContext::new(
            &sandbox,
            &policy,
            &approver,
            None,
            ("c", "r"),
            &mut on_event,
        );
        let err = ctx
            .call("run_command", &json!({"command": "ls"}))
            .await
            .unwrap_err();
        assert!(err.contains("not available inside scripts"));
    }

    #[test]
    fn missing_interpreters_suggest_the_other_language() {
        assert!(missing_interpreter(Language::Node).contains("python"));
        assert!(missing_interpreter(Language::Python).contains("node"));
    }
}
