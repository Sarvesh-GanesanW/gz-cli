//! Drives one assistant conversation: starts chat runs that declare gz's
//! client tools, streams the answer, and whenever the run pauses for client
//! tools (or a server-side approval) asks the user, runs the approved tools
//! locally, and resumes the run — until the turn is done.
//!
//! Front ends (the `gz code` REPL, later the TUI) use only [`Session::send`],
//! an [`Approver`], and the [`AssistantEvent`] callback:
//!
//! ```ignore
//! let mut session = Session::new(http, &std::env::current_dir()?)?;
//! let end = session.send("explain src/main.rs", &approver, &mut |event| match event {
//!     AssistantEvent::Text(t) => print!("{t}"),
//!     AssistantEvent::ToolStarted { summary, .. } => eprintln!("⚙ {summary}"),
//!     _ => {}
//! }).await?;
//! ```
//! Cancel a running turn from elsewhere with [`Session::canceller`].

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::assistant::audit::{AuditLog, Entry};
use crate::assistant::models::Selection;
use crate::assistant::permissions::{
    denial_message, Approver, Decision, PermissionRequest, Policy,
};
use crate::assistant::sandbox::Sandbox;
use crate::assistant::script::{self, ScriptContext};
use crate::assistant::tools::{self, Access, ToolName, ToolOutput};
use crate::cmd::stream::{self, Approval, RunEvent};
use crate::cmd::{new_uuid, seg};
use crate::http::ApiClient;
use crate::sse::Flow;

const CHAT: &str = "chat";
/// Contract cap on one resume body; leave headroom for the JSON around it.
const RESUME_OUTPUT_BUDGET: usize = 1_000_000;
const TITLE_CHARS: usize = 60;

/// What a front end shows while a turn runs.
#[derive(Debug, Clone, PartialEq)]
pub enum AssistantEvent {
    /// A piece of the answer, to append as it arrives.
    Text(String),
    /// A tool began. `local` is true for tools gz runs on this machine.
    ToolStarted {
        name: String,
        summary: String,
        local: bool,
    },
    /// A local tool finished; `detail` is a one-line result summary and
    /// `diff` the unified diff an edit or write applied.
    ToolFinished {
        name: String,
        summary: String,
        is_error: bool,
        detail: String,
        diff: Option<String>,
    },
    /// Something worth a dim note (warnings, refusals, server tool errors).
    Notice(String),
    Done(TurnEnd),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    Completed,
    Cancelled,
    Failed(String),
}

/// Stops the turn in progress (e.g. on Ctrl+C); cheap to clone.
#[derive(Clone)]
pub struct Canceller {
    http: ApiClient,
    active_run: Arc<Mutex<Option<String>>>,
}

impl Canceller {
    /// Asks the server to cancel the active run; false if none was running.
    pub async fn cancel(&self) -> Result<bool> {
        let run = self.active_run.lock().ok().and_then(|r| r.clone());
        let Some(run) = run else {
            return Ok(false);
        };
        self.http
            .request_json(
                CHAT,
                Method::POST,
                &format!("/chat/runs/{}/cancel", seg(&run)),
                &[],
                None,
            )
            .await?;
        Ok(true)
    }
}

/// Everything collected while streaming one leg of a run (up to a pause or
/// the end).
#[derive(Default)]
struct Leg {
    client_calls: Vec<Value>,
    approval: Option<Value>,
    status: Option<String>,
    error: Option<String>,
    cancelled: bool,
}

pub struct Session {
    http: ApiClient,
    sandbox: Sandbox,
    policy: Policy,
    audit: Option<AuditLog>,
    conversation_id: String,
    is_new: bool,
    active_run: Arc<Mutex<Option<String>>>,
    /// `None` lets the server pick its default model.
    model: Option<Selection>,
}

impl Session {
    /// A new conversation working in `root` (normally the current folder).
    pub fn new(http: ApiClient, root: &Path) -> Result<Self> {
        Ok(Self {
            http,
            sandbox: Sandbox::new(root)?,
            policy: Policy::default(),
            audit: AuditLog::open().ok(),
            conversation_id: new_uuid(),
            is_new: true,
            active_run: Arc::new(Mutex::new(None)),
            model: None,
        })
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn workspace(&self) -> &Path {
        self.sandbox.root()
    }

    pub fn workspace_name(&self) -> String {
        self.sandbox.name()
    }

    /// Starts over with a fresh conversation; session permissions are kept.
    pub fn new_conversation(&mut self) {
        self.conversation_id = new_uuid();
        self.is_new = true;
    }

    pub fn continue_conversation(&mut self, conversation_id: &str) {
        self.conversation_id = conversation_id.to_string();
        self.is_new = false;
    }

    /// Pre-approves every read inside the workspace (`--yes-reads`).
    pub fn allow_all_reads(&mut self) {
        let root = self.sandbox.root().to_path_buf();
        self.policy.allow_reads_under(&root);
    }

    /// The model for the next turns (`None`: the server default).
    pub fn set_model(&mut self, model: Option<Selection>) {
        self.model = model;
    }

    pub fn canceller(&self) -> Canceller {
        Canceller {
            http: self.http.clone(),
            active_run: self.active_run.clone(),
        }
    }

    /// Sends one message and runs the turn to its end, asking `approver`
    /// before any local tool runs. `on_event` sees text and tool activity
    /// as it happens and finally [`AssistantEvent::Done`].
    pub async fn send(
        &mut self,
        prompt: &str,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Result<TurnEnd> {
        let end = self.run_turn(prompt, approver, on_event).await;
        self.set_active(None);
        let end = end?;
        on_event(AssistantEvent::Done(end.clone()));
        Ok(end)
    }

    async fn run_turn(
        &mut self,
        prompt: &str,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Result<TurnEnd> {
        let started = self
            .post_run("/chat/runs".to_string(), self.start_body(prompt))
            .await?;
        self.is_new = false;
        let mut run_id = run_id_of(&started, None)?;
        let mut last_id: Option<String> = None;
        loop {
            self.set_active(Some(run_id.clone()));
            let leg = self.stream_leg(&run_id, &mut last_id, on_event).await?;
            if leg.cancelled {
                return Ok(TurnEnd::Cancelled);
            }
            if let Some(error) = leg.error {
                return Ok(TurnEnd::Failed(error));
            }
            let resumed = if !leg.client_calls.is_empty() {
                let results = self
                    .answer_client_calls(&run_id, leg.client_calls, approver, on_event)
                    .await;
                let path = format!("/chat/runs/{}/client-tools/resume", seg(&run_id));
                self.post_run(path, json!({"toolResults": results})).await?
            } else if let Some(interrupt) = leg.approval {
                let body = self.decide_server_action(&interrupt, approver).await;
                let path = format!("/chat/runs/{}/resume", seg(&run_id));
                self.post_run(path, body).await?
            } else {
                return Ok(match leg.status.as_deref() {
                    Some(status) if stream::failure_reason(status, None).is_some() => {
                        TurnEnd::Failed(stream::failure_reason(status, None).unwrap_or_default())
                    }
                    Some("cancelled") => TurnEnd::Cancelled,
                    _ => TurnEnd::Completed,
                });
            };
            let next = run_id_of(&resumed, Some(&run_id))?;
            if next != run_id {
                run_id = next;
                last_id = None;
            }
        }
    }

    fn start_body(&self, prompt: &str) -> Value {
        let mut body = json!({
            "userInput": prompt,
            "conversationId": self.conversation_id,
            "assistantMode": "chat",
            "useRag": false,
            "supportsActionConfirmations": true,
            "clientTools": ToolName::ALL.map(ToolName::as_str),
            "clientWorkspace": {"name": self.sandbox.name(), "platform": platform()},
        });
        if self.is_new {
            body["conversationName"] = json!(default_title(prompt));
        }
        if let Some(model) = &self.model {
            model.apply_to(&mut body);
        }
        body
    }

    async fn post_run(&self, path: String, body: Value) -> Result<Value> {
        let headers = stream::run_headers(&self.http).await;
        self.http
            .request_json_with(CHAT, Method::POST, &path, &[], Some(body), &headers)
            .await
    }

    async fn stream_leg(
        &self,
        run_id: &str,
        last_id: &mut Option<String>,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Result<Leg> {
        let mut leg = Leg::default();
        let path = format!("/chat/runs/{}/events", seg(run_id));
        stream::read_run(&self.http, CHAT, &path, last_id, |_, event| {
            match event {
                RunEvent::Text(text) if !text.is_empty() => on_event(AssistantEvent::Text(text)),
                // The server announces client tools too ("Read file") just
                // before asking gz to run them; gz shows its own line instead.
                RunEvent::ServerTool(name) if ToolName::from_display(&name).is_some() => {}
                RunEvent::ServerTool(name) => on_event(AssistantEvent::ToolStarted {
                    summary: name.clone(),
                    name,
                    local: false,
                }),
                RunEvent::ToolError(why) => {
                    on_event(AssistantEvent::Notice(format!("tool failed: {why}")))
                }
                RunEvent::Warning(why) => on_event(AssistantEvent::Notice(why)),
                RunEvent::ClientToolRequest(call) => leg.client_calls.push(call),
                RunEvent::Approval(interrupt) => leg.approval = Some(interrupt),
                RunEvent::Error(why) => leg.error = Some(why),
                RunEvent::Cancelled => leg.cancelled = true,
                RunEvent::Status(status) => {
                    let stop = RunEvent::is_terminal_status(&status) && status != "completed";
                    leg.status = Some(status);
                    if stop {
                        return Ok(Flow::Stop);
                    }
                }
                _ => {}
            }
            Ok(Flow::Continue)
        })
        .await?;
        Ok(leg)
    }

    /// Plans, asks, runs, and audits each requested call; every call gets a
    /// result (refusals and failures included) so the run can continue.
    async fn answer_client_calls(
        &mut self,
        run_id: &str,
        calls: Vec<Value>,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Vec<Value> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let interrupt_id = call["interruptId"].as_str().unwrap_or_default().to_string();
            let tool_name = call["toolName"].as_str().unwrap_or_default().to_string();
            let output = self
                .answer_one(run_id, &tool_name, &call["input"], approver, on_event)
                .await;
            results.push((interrupt_id, tool_name, output));
        }
        fit_to_budget(&mut results);
        results
            .into_iter()
            .map(|(interrupt_id, tool_name, out)| {
                json!({
                    "interruptId": interrupt_id,
                    "toolName": tool_name,
                    "result": {"output": out.output, "isError": out.is_error, "truncated": out.truncated},
                })
            })
            .collect()
    }

    async fn answer_one(
        &mut self,
        run_id: &str,
        tool_name: &str,
        input: &Value,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> ToolOutput {
        let Some(tool) = ToolName::parse(tool_name) else {
            return ToolOutput::error(format!("gz cannot run the tool '{tool_name}'"));
        };
        let planned = match tools::plan(&self.sandbox, tool, input) {
            Ok(planned) => planned,
            Err(err) => {
                let out = ToolOutput::error(format!("{err:#}"));
                self.audit(run_id, tool_name, &input.to_string(), "rejected", true);
                on_event(AssistantEvent::Notice(format!("{tool_name}: {err:#}")));
                return out;
            }
        };
        let decision = if self.policy.is_preapproved(&planned) {
            Decision::AllowOnce
        } else {
            let label = self.sandbox.display(&planned.scope);
            approver
                .decide(&Policy::request_for(&planned, &label))
                .await
        };
        if decision == Decision::Deny {
            self.audit(run_id, tool_name, &planned.summary, "denied", true);
            on_event(AssistantEvent::Notice(format!(
                "denied: {}",
                planned.summary
            )));
            return ToolOutput::error(denial_message(approver.take_denial_note()));
        }
        if decision == Decision::AllowSession {
            self.policy.remember(&planned);
        }
        on_event(AssistantEvent::ToolStarted {
            name: tool_name.to_string(),
            summary: planned.summary.clone(),
            local: true,
        });
        let out = match planned.script() {
            Some(spec) => {
                let mut ctx = ScriptContext::new(
                    &self.sandbox,
                    &self.policy,
                    approver,
                    self.audit.as_ref(),
                    (&self.conversation_id, run_id),
                    &mut *on_event,
                );
                script::execute(spec, self.sandbox.root(), &mut ctx).await
            }
            None => tools::run(&self.sandbox, &planned).await,
        };
        self.audit(run_id, tool_name, &planned.summary, "allowed", out.is_error);
        let diff = (planned.access == Access::Write && !out.is_error)
            .then(|| planned.preview.clone())
            .flatten();
        on_event(AssistantEvent::ToolFinished {
            name: tool_name.to_string(),
            summary: planned.summary,
            is_error: out.is_error,
            detail: result_detail(&out),
            diff,
        });
        out
    }

    /// The chat server's own approval interrupt (e.g. "may I run this
    /// action?"), answered through the same approver.
    async fn decide_server_action(&self, interrupt: &Value, approver: &dyn Approver) -> Value {
        let approval = Approval::from_event(interrupt);
        let request = PermissionRequest {
            summary: approval
                .summary
                .clone()
                .unwrap_or_else(|| format!("Groundzero wants to run {}", approval.tool)),
            preview: approval.preview.clone(),
            session_scope: "this action in this conversation".to_string(),
            access: Access::ServerAction,
        };
        let decision = approver.decide(&request).await;
        json!({
            "interruptId": approval.interrupt_id,
            "approved": decision != Decision::Deny,
            "remember": decision == Decision::AllowSession,
        })
    }

    fn audit(&self, run_id: &str, tool: &str, target: &str, decision: &str, is_error: bool) {
        let Some(log) = &self.audit else {
            return;
        };
        let entry = Entry {
            conversation_id: &self.conversation_id,
            run_id,
            tool,
            target,
            decision,
            is_error,
        };
        if let Err(err) = log.record(&entry) {
            crate::output::warn(&format!("cannot write the audit log: {err:#}"));
        }
    }

    fn set_active(&self, run: Option<String>) {
        if let Ok(mut active) = self.active_run.lock() {
            *active = run;
        }
    }
}

fn run_id_of(reply: &Value, fallback: Option<&str>) -> Result<String> {
    reply["runId"]
        .as_str()
        .or(fallback)
        .map(str::to_string)
        .context("chat did not return a run id")
}

fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

fn default_title(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or(prompt).trim();
    if line.chars().count() <= TITLE_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(TITLE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

fn result_detail(out: &ToolOutput) -> String {
    let lines = out.output.lines().count();
    if out.is_error {
        out.output.lines().next().unwrap_or("failed").to_string()
    } else if lines <= 1 {
        out.output.trim().to_string()
    } else {
        format!("{lines} lines")
    }
}

/// Keeps one resume body under the contract's 1 MiB by trimming the later
/// outputs once the budget is spent.
fn fit_to_budget(results: &mut [(String, String, ToolOutput)]) {
    let mut remaining = RESUME_OUTPUT_BUDGET;
    for (_, _, out) in results.iter_mut() {
        let size = out.output.chars().count();
        if size <= remaining {
            remaining -= size;
            continue;
        }
        let mut kept: String = out.output.chars().take(remaining).collect();
        kept.push_str("\n… output truncated by gz");
        out.output = kept;
        out.truncated = true;
        remaining = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::permissions::DecisionFuture;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    struct ApproveAll;

    impl Approver for ApproveAll {
        fn decide<'a>(&'a self, _: &'a PermissionRequest) -> DecisionFuture<'a> {
            Box::pin(async { Decision::AllowOnce })
        }
    }

    /// One HTTP request off the socket: request line and JSON body.
    fn read_request(stream: &mut std::net::TcpStream) -> (String, Value) {
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        let head_end = loop {
            let n = stream.read(&mut buf).unwrap();
            raw.extend_from_slice(&buf[..n]);
            if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
        let length: usize = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0);
        while raw.len() < head_end + length {
            let n = stream.read(&mut buf).unwrap();
            raw.extend_from_slice(&buf[..n]);
        }
        let line = head.lines().next().unwrap_or_default().to_string();
        let body = serde_json::from_slice(&raw[head_end..]).unwrap_or(Value::Null);
        (line, body)
    }

    fn reply(stream: &mut std::net::TcpStream, content_type: &str, body: &str) {
        let text = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(text.as_bytes()).ok();
    }

    /// A chat server whose run asks for one `run_script`, then finishes on
    /// the resumed run. Returns its address and the resume body it received.
    fn script_chat_server(code: &str) -> (String, Arc<Mutex<Option<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let resume = Arc::new(Mutex::new(None));
        let seen = resume.clone();
        let request = json!({
            "type": "client_tool_request",
            "interruptId": "v1:tool_call:tu_1:x",
            "toolUseId": "tu_1",
            "toolName": "run_script",
            "input": {"language": "python", "code": code},
        });
        let pause = format!(
            "id: 1\ndata: {request}\n\nid: 2\ndata: {{\"type\":\"run_status\",\"status\":\"interrupted\",\"reason\":\"client_tools\"}}\n\ndata: [DONE]\n\n"
        );
        let finish = "id: 1\ndata: {\"type\":\"llm_chunk\",\"text\":\"Counted.\"}\n\nid: 2\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\ndata: [DONE]\n\n";
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (line, body) = read_request(&mut stream);
                let path = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                match path.split('?').next().unwrap_or_default() {
                    "/chat/runs" => reply(&mut stream, "application/json", r#"{"runId":"r1"}"#),
                    "/chat/runs/r1/events" => reply(&mut stream, "text/event-stream", &pause),
                    "/chat/runs/r1/client-tools/resume" => {
                        *seen.lock().unwrap() = Some(body);
                        reply(&mut stream, "application/json", r#"{"runId":"r2"}"#)
                    }
                    "/chat/runs/r2/events" => reply(&mut stream, "text/event-stream", finish),
                    _ => reply(&mut stream, "application/json", "{}"),
                }
            }
        });
        (base, resume)
    }

    #[tokio::test]
    async fn runs_requested_scripts_and_resumes_with_their_output() {
        if !std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            eprintln!("skipped: python3 not installed");
            return;
        }
        let (base, resume) = script_chat_server(
            "import gz\nnames = gz.list_directory('.')\nprint(f'{len(names)} entries')\n",
        );
        let dir = std::env::temp_dir().join(format!("gz-session-script-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        let resolved = crate::config::resolve(
            &crate::config::Config::default(),
            Some("session-test"),
            Some(base),
            Some("stub-token".to_string()),
        );
        let http =
            ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 30).unwrap();
        let mut session = Session::new(http, &dir).unwrap();
        session.audit = None;
        let mut text = String::new();
        let end = session
            .send("count files", &ApproveAll, &mut |event| {
                if let AssistantEvent::Text(t) = event {
                    text.push_str(&t);
                }
            })
            .await
            .unwrap();
        assert_eq!(end, TurnEnd::Completed);
        assert_eq!(text, "Counted.");
        let body = resume.lock().unwrap().clone().expect("gz resumed the run");
        let result = &body["toolResults"][0];
        assert_eq!(result["toolName"], "run_script");
        assert_eq!(result["result"]["isError"], false);
        assert_eq!(result["result"]["output"], "2 entries\n");
    }

    #[test]
    fn budget_trims_only_what_overflows() {
        let big = |n: usize| {
            (
                "i".to_string(),
                "read_file".to_string(),
                ToolOutput::ok("x".repeat(n)),
            )
        };
        let mut results = vec![big(260_000), big(260_000), big(260_000), big(260_000)];
        fit_to_budget(&mut results);
        assert!(results[..3].iter().all(|r| !r.2.truncated));
        assert!(results[3].2.truncated);
        assert!(results[3].2.output.chars().count() < 220_100);
    }

    #[test]
    fn summarises_results_in_one_line() {
        assert_eq!(result_detail(&ToolOutput::ok("a\nb\nc".into())), "3 lines");
        assert_eq!(
            result_detail(&ToolOutput::ok("wrote a.rs".into())),
            "wrote a.rs"
        );
        assert_eq!(
            result_detail(&ToolOutput::error("exit code 1\nboom")),
            "exit code 1"
        );
    }

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(default_title("fix the build\nplease"), "fix the build");
    }
}
