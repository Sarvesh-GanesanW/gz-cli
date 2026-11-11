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
use crate::assistant::permissions::{Approver, Decision, Policy, ScriptEdits};
use crate::assistant::sandbox::Sandbox;
use crate::assistant::session::AssistantEvent;
use crate::assistant::tools::{self, Access, Language, Planned, ScriptSpec, ToolName, ToolOutput};
use crate::cmd::new_uuid;

/// Contract cap on helper calls per script.
pub const MAX_HELPER_CALLS: usize = 500;
/// Largest single bridge request (a `write_file` carries the whole file).
const MAX_REQUEST_BYTES: u64 = 32 * 1024 * 1024;
/// Output kept from each of stdout and stderr; the rest is drained unread.
const MAX_CAPTURE_BYTES: usize = 4 * tools::MAX_OUTPUT_CHARS;

const HELPER_PY: &str = include_str!("script_helpers/gz.py");
const HELPER_JS: &str = include_str!("script_helpers/gz.js");
const CALLER_JS: &str = include_str!("script_helpers/gz_call.js");

pub type HelperFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

/// Answers one helper call from a running script.
pub trait HelperHandler: Send {
    fn handle<'a>(&'a mut self, tool: &'a str, input: &'a Value) -> HelperFuture<'a>;
}

/// Runs an approved script and returns its output for the model.
pub async fn execute(
    spec: &ScriptSpec,
    root: &Path,
    handler: &mut dyn HelperHandler,
) -> ToolOutput {
    match run(spec, root, handler).await {
        Ok(out) => out,
        Err(err) => ToolOutput::error(format!("{err:#}")),
    }
}

async fn run(
    spec: &ScriptSpec,
    root: &Path,
    handler: &mut dyn HelperHandler,
) -> Result<ToolOutput> {
    let workdir = ScratchDir::create()?;
    let script_path = workdir.write_helpers(spec)?;
    let mut bridge = Bridge::bind(MAX_HELPER_CALLS).await?;
    let mut command = interpreter(spec.language, &script_path, &workdir.path, root, &bridge);
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
    Ok(report(&stdout, &stderr, ending, bridge.over_limit))
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

fn interpreter(
    language: Language,
    script: &Path,
    helpers: &Path,
    root: &Path,
    bridge: &Bridge,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(interpreter_name(language));
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
    command
}

/// The model-facing result: stdout, then stderr, then why it stopped.
fn report(stdout: &[u8], stderr: &[u8], ending: Ending, over_limit: bool) -> ToolOutput {
    let mut text = String::from_utf8_lossy(stdout).into_owned();
    let errors = String::from_utf8_lossy(stderr);
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
    if failed {
        ToolOutput::error(text)
    } else {
        ToolOutput::ok(text)
    }
}

/// Reads a pipe to the end, keeping only the first `MAX_CAPTURE_BYTES` so a
/// chatty script can't exhaust memory or block on a full pipe.
async fn capture(pipe: Option<impl AsyncRead + Unpin>) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let room = MAX_CAPTURE_BYTES.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..n.min(room)]);
    }
    kept
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
/// edit or write shows its diff and asks unless edits were allowed for the
/// session or for the rest of this script.
pub struct ScriptContext<'a> {
    pub sandbox: &'a Sandbox,
    pub policy: &'a Policy,
    pub approver: &'a dyn Approver,
    pub audit: Option<&'a AuditLog>,
    pub conversation_id: &'a str,
    pub run_id: &'a str,
    pub on_event: &'a mut (dyn FnMut(AssistantEvent) + Send),
    edits: ScriptEdits,
    /// Set when the user answered an edit with "allow always": the session
    /// saves the rule once the script ends.
    pub edits_always: Option<Planned>,
    /// Where each helper write keeps what it replaced (`/rewind`).
    edit_history: EditHistory,
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
            edits: ScriptEdits::default(),
            edits_always: None,
            edit_history: EditHistory::disabled(),
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
        if planned.access == Access::Write && !self.edits.is_allowed(self.policy, &planned) {
            let request = ScriptEdits::request_for(&planned);
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
                Decision::AllowChat => self.edits.allow_rest_of_script(),
                Decision::AllowAlways => {
                    self.edits.allow_rest_of_script();
                    self.edits_always = Some(planned.clone());
                }
                Decision::AllowOnce => {}
            }
            decided = self.approver.take_decided_by().audit_label(decision);
        }
        (self.on_event)(AssistantEvent::ToolStarted {
            name: tool_name.to_string(),
            summary: format!("script · {}", planned.summary),
            local: true,
        });
        let result =
            checkpoints::run_helper(&self.edit_history, self.sandbox, &planned, self.on_event)
                .map_err(|err| format!("{err:#}"));
        self.record(
            tool_name,
            &planned.summary,
            (decided, None),
            result.is_err(),
        );
        result
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
        let out = execute(script, &sandbox.cwd(), &mut ctx).await;
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
        let ok = report(b"hi\n", b"", Ending::Exited(Some(0)), false);
        assert_eq!((ok.output.as_str(), ok.is_error), ("hi\n", false));
        let failed = report(b"x", b"Traceback\n", Ending::Exited(Some(1)), false);
        assert!(failed.is_error);
        assert!(failed.output.ends_with("Traceback\n[exit code 1]"));
        let timed_out = report(b"", b"", Ending::TimedOut(Duration::from_secs(3)), false);
        assert!(timed_out.output.contains("timed out after 3s"));
        assert!(report(b"", b"", Ending::Exited(Some(0)), true).is_error);
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
        assert_eq!(asked[0].chat_scope, "all edits from this script");
        assert!(asked[0].preview.as_deref().unwrap().contains("+new"));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "old\n");
    }

    #[tokio::test]
    async fn approving_rest_of_script_covers_later_edits() {
        if !has("python3") {
            eprintln!("skipped: python3 not installed");
            return;
        }
        let (dir, sandbox) = temp_workspace("script-edit-all");
        std::fs::write(dir.join("a.txt"), "x\n").unwrap();
        std::fs::write(dir.join("b.txt"), "x\n").unwrap();
        let code =
            "import gz\nfor f in ['a.txt', 'b.txt']:\n    print(gz.edit_file(f, 'x', 'y'))\n";
        let approver = Scripted::new(Decision::AllowChat);
        let (out, _) = run_in(&sandbox, &approver, &spec(Language::Python, code, 30)).await;
        assert!(!out.is_error, "{}", out.output);
        assert_eq!(approver.asked.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "y\n");
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
