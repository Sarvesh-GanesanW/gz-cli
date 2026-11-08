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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::assistant::attachments::{self, PreparedTurn};
use crate::assistant::audit::{AuditLog, Entry};
use crate::assistant::history;
use crate::assistant::local_context::{ContextTool, Limits, LocalContext};
use crate::assistant::memory;
use crate::assistant::models::Selection;
use crate::assistant::permissions::{Approver, Decision, PermissionRequest, Policy, Refusal};
use crate::assistant::sandbox::{FolderCheck, Sandbox};
use crate::assistant::saved_rules;
use crate::assistant::script::{self, ScriptContext};
use crate::assistant::steering::{SentNote, Steering};
use crate::assistant::subagent_settings::SubagentSettings;
use crate::assistant::tools::{self, Access, Planned, ToolName, ToolOutput};
use crate::cmd::stream::{self, Approval, RunEvent};
use crate::cmd::{new_uuid, seg};
use crate::http::ApiClient;
use crate::sse::Flow;
use crate::subagents::{self, SubagentEvent, SubagentReport, SubagentRequest};

const CHAT: &str = "chat";
/// Contract cap on one resume body; leave headroom for the JSON around it.
/// Total tool output one resume may carry: under the backend's 64 MiB body
/// limit with room for JSON overhead and multi-byte characters.
const RESUME_OUTPUT_BUDGET: usize = 48_000_000;
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
    /// A local tool finished; `detail` is a one-line result summary,
    /// `output` the full result the model received, and `diff` the unified
    /// diff an edit or write applied.
    ToolFinished {
        name: String,
        summary: String,
        is_error: bool,
        detail: String,
        output: String,
        diff: Option<String>,
    },
    /// Token and cost figures for the answer just finished.
    Usage(stream::Usage),
    /// The chat asks gz to run a subagent in the background.
    SubagentRequested(SubagentRequest),
    /// A background subagent finished; its report goes to the main agent.
    SubagentReport(SubagentReport),
    /// The run took a note the user sent while it worked.
    SteerSent(SentNote),
    /// The model read that note (by its steer id).
    SteerApplied(String),
    /// The run ended before reading these notes.
    SteerUnapplied(Vec<String>),
    /// How full the model's context window was when the run ended.
    Context(stream::ContextUsage),
    /// A subagent the run started (the chat's `task` tool) moved on.
    Subagent(SubagentEvent),
    /// Something worth a dim note (warnings, refusals, server tool errors).
    Notice(String),
    /// What a folder command (`/cd`, `/add-dir`, `/dirs remove`) did, or
    /// why it couldn't.
    Folders(Result<String, String>),
    /// A tool event (or note) from work a subagent asked for, so front ends
    /// can show it on that subagent's page instead of the main one.
    ForSubagent {
        id: String,
        event: Box<AssistantEvent>,
    },
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
    /// Subagent ids → names seen this leg, to attribute later requests.
    subagents: HashMap<String, String>,
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
    /// Where "allow always" answers are saved; `None` if the config folder
    /// can't be resolved (always-answers then last for the session only).
    saved: Option<saved_rules::Store>,
    audit: Option<AuditLog>,
    /// Which conversations were started in which folder, for `/resume`.
    history: Option<history::Index>,
    conversation_id: String,
    is_new: bool,
    active_run: Arc<Mutex<Option<String>>>,
    steering: Steering,
    /// `None` lets the server pick its default model.
    model: Option<Selection>,
    /// Documents attached in this conversation; resent every turn, as the
    /// web app does, so the model keeps seeing them.
    documents: Vec<String>,
    /// Whether the chat may start subagents (`/subagents off` sends
    /// `subagents: false`).
    subagents_enabled: bool,
    /// Conversation size, in tokens, at which the chat summarizes older
    /// messages; `None` keeps the server's default.
    auto_compact: Option<u64>,
    /// Names of this turn's subagents, for permission prompts they raise.
    subagent_names: HashMap<String, String>,
    /// Ask the chat to hand subagents to gz instead of running them inline
    /// (only when the server advertises `features.backgroundSubagents`).
    background_subagents: bool,
    /// Set when this session runs a background subagent's task: its profile
    /// and the name its conversation gets.
    subagent: Option<(String, String)>,
    /// The user's `/subagents` settings, sent as `subagentSettings` only
    /// when the server takes them (`features.subagentSettings`).
    subagent_settings: SubagentSettings,
    subagent_settings_supported: bool,
    /// Instructions, memory and allowed folders, shared with background
    /// subagents; sent only when the server takes them.
    context: LocalContext,
    /// `--yes-reads`: reads in every folder opened later are pre-approved too.
    yes_reads: bool,
}

impl Session {
    /// The signed-in client, for front ends that make their own calls
    /// (e.g. account usage) next to the session.
    pub fn http(&self) -> ApiClient {
        self.http.clone()
    }

    /// A new conversation working in `root` (normally the current folder),
    /// with the signed-in profile's memory and allowed folders.
    pub fn new(http: ApiClient, root: &Path) -> Result<Self> {
        let sandbox = Sandbox::new(root)?;
        let resolved = http.resolved();
        let context = LocalContext::for_profile(&resolved.profile_name, &resolved.profile);
        Ok(Self::with_sandbox(http, sandbox, context))
    }

    /// A new conversation in `sandbox`'s folders (a background subagent
    /// gets a fork of the main session's).
    pub fn with_sandbox(http: ApiClient, sandbox: Sandbox, context: LocalContext) -> Self {
        let active_run = Arc::new(Mutex::new(None));
        let steering = Steering::new(http.clone(), active_run.clone());
        let saved = saved_rules::Store::open().ok();
        let always = saved
            .as_ref()
            .and_then(|store| store.all().ok())
            .unwrap_or_default();
        Self {
            http,
            sandbox,
            policy: Policy::with_saved(always),
            saved,
            audit: AuditLog::open().ok(),
            history: history::Index::open().ok(),
            conversation_id: new_uuid(),
            is_new: true,
            active_run,
            steering,
            model: None,
            documents: Vec::new(),
            subagents_enabled: true,
            auto_compact: None,
            subagent_names: HashMap::new(),
            background_subagents: false,
            subagent: None,
            subagent_settings: SubagentSettings::default(),
            subagent_settings_supported: false,
            context,
            yes_reads: false,
        }
    }

    /// The local-context settings, to share with background subagents.
    pub fn local_context(&self) -> LocalContext {
        self.context.clone()
    }

    /// Sends instructions, memories and folders, and offers
    /// `change_directory` and `memory`, when the server takes them
    /// (`features.localContext`; `None` turns them off).
    pub fn set_local_context(&mut self, limits: Option<Limits>) {
        self.context.set_limits(limits);
    }

    /// `/memory on|off`: whether the model gets the memory tool and the
    /// saved memories.
    pub fn set_memory(&mut self, on: bool) {
        self.context.set_memory(on);
    }

    /// The grants this session's background subagents share with it.
    pub fn shared_policy(&self) -> Policy {
        self.policy.shared()
    }

    /// Turns a fresh session into a background subagent's: its runs carry
    /// the profile, it delegates nothing, it stays out of `/resume`, and
    /// it uses (and adds to) the main session's grants in `policy`.
    pub fn into_subagent(mut self, profile: &str, name: &str, policy: Policy) -> Self {
        self.subagent = Some((profile.to_string(), name.to_string()));
        self.policy = policy;
        self.subagents_enabled = false;
        self.history = None;
        self
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    /// The folder gz started in.
    pub fn workspace(&self) -> PathBuf {
        self.sandbox.start()
    }

    /// The workspace sandbox, for resolving `@` mentions before a turn.
    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// Resolves mentions and pasted images in `text` (see
    /// [`attachments::prepare`]); nothing is uploaded yet.
    pub fn prepare(&self, text: &str, images: &[PathBuf]) -> Result<PreparedTurn> {
        attachments::prepare(&self.sandbox, text, images)
    }

    /// The start folder's name, for the banner.
    pub fn workspace_name(&self) -> String {
        self.sandbox
            .start()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".to_string())
    }

    /// Starts over with a fresh conversation. "For this chat" approvals end
    /// here; saved "always" rules are kept.
    pub fn new_conversation(&mut self) {
        self.conversation_id = new_uuid();
        self.is_new = true;
        self.documents.clear();
        self.policy.clear_chat();
    }

    pub fn continue_conversation(&mut self, conversation_id: &str) {
        if conversation_id != self.conversation_id {
            self.policy.clear_chat();
        }
        self.conversation_id = conversation_id.to_string();
        self.is_new = false;
    }

    /// Re-reads the saved "always" rules (after `/permissions` revoked one).
    pub fn reload_saved_rules(&mut self) {
        if let Some(rules) = self.saved.as_ref().and_then(|store| store.all().ok()) {
            self.policy.set_saved(rules);
        }
    }

    /// Pre-approves every read inside the open folders (`--yes-reads`),
    /// including folders opened later.
    pub fn allow_all_reads(&mut self) {
        self.yes_reads = true;
        for root in self.sandbox.roots() {
            self.policy.allow_reads_under(&root);
        }
    }

    /// Lets the chat start subagents on the next turns, or not.
    pub fn set_subagents(&mut self, enabled: bool) {
        self.subagents_enabled = enabled;
    }

    /// Lets the chat hand subagents to gz to run in the background.
    pub fn set_background_subagents(&mut self, on: bool) {
        self.background_subagents = on;
    }

    pub fn subagents_enabled(&self) -> bool {
        self.subagents_enabled
    }

    /// The `/subagents` settings for the next turns.
    pub fn set_subagent_settings(&mut self, settings: SubagentSettings) {
        self.subagent_settings = settings;
    }

    pub fn subagent_settings(&self) -> &SubagentSettings {
        &self.subagent_settings
    }

    /// Whether the server takes `subagentSettings` (from its capabilities).
    pub fn set_subagent_settings_supported(&mut self, supported: bool) {
        self.subagent_settings_supported = supported;
    }

    /// Where the next turns auto-compact (`None`: the server default).
    pub fn set_auto_compact(&mut self, tokens: Option<u64>) {
        self.auto_compact = tokens;
    }

    pub fn auto_compact(&self) -> Option<u64> {
        self.auto_compact
    }

    /// The model for the next turns (`None`: the server default).
    pub fn set_model(&mut self, model: Option<Selection>) {
        self.model = model;
    }

    /// The notes the user sends while gz works, shared with the UI.
    pub fn steering(&self) -> Steering {
        self.steering.clone()
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

    /// Uploads the turn's attachments, records what was read, then sends it
    /// like [`Self::send`]. A failed upload fails the turn before anything
    /// reaches the model.
    pub async fn send_prepared(
        &mut self,
        turn: &PreparedTurn,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Result<TurnEnd> {
        for path in &turn.uploads {
            let reply = attachments::upload(&self.http, path)
                .await
                .with_context(|| format!("cannot attach {}", path.display()))?;
            self.documents.push(attachments::document_id(&reply)?);
        }
        for read in &turn.reads {
            self.audit("", "mention", read, ("allowed", None), false);
        }
        self.send(&turn.prompt, approver, on_event).await
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
        if let Some(index) = &self.history {
            // Best effort: losing this note only hides the chat from /resume.
            let _ = index.record(
                &self.sandbox.start(),
                &self.conversation_id,
                &default_title(prompt),
            );
        }
        self.subagent_names.clear();
        let mut run_id = run_id_of(&started, None)?;
        let mut last_id: Option<String> = None;
        loop {
            self.set_active(Some(run_id.clone()));
            // Notes typed while the run waited on gz reach it before its next
            // step, right after the tool results.
            for note in self.steering.deliver().await {
                on_event(AssistantEvent::SteerSent(note));
            }
            let mut leg = self.stream_leg(&run_id, &mut last_id, on_event).await?;
            self.subagent_names.extend(leg.subagents.drain());
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
                let body = self
                    .decide_server_action(&interrupt, approver, on_event)
                    .await;
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
            "useRag": !self.documents.is_empty(),
            "supportsActionConfirmations": true,
            "clientTools": self.client_tools(),
            "clientWorkspace": {"name": self.sandbox.name(), "platform": platform()},
        });
        if let Some(context) = self.context.request_field(&self.sandbox) {
            body["clientContext"] = context;
        }
        if self.is_new {
            body["conversationName"] = json!(match &self.subagent {
                Some((_, name)) => format!("Subagent · {name}"),
                None => default_title(prompt),
            });
        }
        if let Some(model) = &self.model {
            model.apply_to(&mut body);
        }
        if let Some((profile, _)) = &self.subagent {
            body["subagentProfile"] = json!(profile);
        } else if !self.subagents_enabled {
            body["subagents"] = json!(false);
        } else {
            if self.background_subagents {
                body["backgroundSubagents"] = json!(true);
            }
            if let Some(settings) = self.subagent_settings_body() {
                body["subagentSettings"] = settings;
            }
        }
        if let Some(tokens) = self.auto_compact {
            body["autoCompactTokens"] = json!(tokens);
        }
        if !self.documents.is_empty() {
            body["docIds"] = json!(self.documents);
        }
        body
    }

    /// Every client tool gz offers: the file and shell tools, plus the
    /// local-context ones when the server takes them.
    fn client_tools(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = ToolName::ALL.map(ToolName::as_str).to_vec();
        names.extend(self.context.tool_names());
        names
    }

    fn subagent_settings_body(&self) -> Option<Value> {
        if !self.subagent_settings_supported {
            return None;
        }
        self.subagent_settings.request_body(self.model.as_ref())
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
                RunEvent::ServerTool(name)
                    if ToolName::from_display(&name).is_some()
                        || ContextTool::from_display(&name).is_some() => {}
                RunEvent::ServerTool(name) => on_event(AssistantEvent::ToolStarted {
                    summary: name.clone(),
                    name,
                    local: false,
                }),
                RunEvent::ToolError(why) => {
                    on_event(AssistantEvent::Notice(format!("tool failed: {why}")))
                }
                RunEvent::Warning(why) => on_event(AssistantEvent::Notice(why)),
                RunEvent::Usage(usage) => on_event(AssistantEvent::Usage(usage)),
                RunEvent::Context(context) => on_event(AssistantEvent::Context(context)),
                RunEvent::SteerApplied(id) => on_event(AssistantEvent::SteerApplied(id)),
                RunEvent::SubagentRequested(request) => {
                    on_event(AssistantEvent::SubagentRequested(request))
                }
                RunEvent::SteerUnapplied(notes) => on_event(AssistantEvent::SteerUnapplied(notes)),
                RunEvent::Subagent(event) => {
                    if let SubagentEvent::Started { id, name, .. } = &event {
                        leg.subagents.insert(id.clone(), name.clone());
                    }
                    on_event(AssistantEvent::Subagent(event));
                }
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
            let asked_by = self.subagent_name(&call);
            let output = match subagents::asked_by(&call) {
                Some(id) => {
                    let mut routed = |event: AssistantEvent| {
                        on_event(AssistantEvent::ForSubagent {
                            id: id.clone(),
                            event: Box::new(event),
                        })
                    };
                    self.answer_one(
                        run_id,
                        &tool_name,
                        &call["input"],
                        asked_by,
                        approver,
                        &mut routed,
                    )
                    .await
                }
                None => {
                    self.answer_one(
                        run_id,
                        &tool_name,
                        &call["input"],
                        asked_by,
                        approver,
                        on_event,
                    )
                    .await
                }
            };
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
        asked_by: Option<String>,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> ToolOutput {
        // Only tools gz offered this run are run.
        let offered = self.context.tool_names().contains(&tool_name);
        match ContextTool::parse(tool_name).filter(|_| offered) {
            Some(ContextTool::ChangeDirectory) => {
                return self
                    .change_directory(run_id, input, asked_by, approver, on_event)
                    .await
            }
            Some(ContextTool::Memory) => return self.memory_call(run_id, input, on_event),
            None => {}
        }
        let Some(tool) = ToolName::parse(tool_name) else {
            return ToolOutput::error(format!("gz cannot run the tool '{tool_name}'"));
        };
        let planned = match tools::plan(&self.sandbox, tool, input) {
            Ok(planned) => planned,
            Err(err) => {
                let out = ToolOutput::error(format!("{err:#}"));
                self.audit(
                    run_id,
                    tool_name,
                    &input.to_string(),
                    ("rejected", None),
                    true,
                );
                on_event(AssistantEvent::Notice(format!("{tool_name}: {err:#}")));
                return out;
            }
        };
        let preapproved = self.policy.is_preapproved(&planned);
        let decision = if preapproved {
            Decision::AllowOnce
        } else {
            let label = self.sandbox.display(&planned.scope);
            let folder = self.sandbox.display_folder(&self.grant_folder(&planned));
            let mut request = Policy::request_for(&planned, &label, &folder);
            request.asked_by = asked_by;
            approver.decide(&request).await
        };
        if decision == Decision::Deny {
            let refusal = approver.take_refusal();
            let entry = (refusal.audit_label(), refusal.audit_reason());
            self.audit(run_id, tool_name, &planned.summary, entry, true);
            on_event(AssistantEvent::Notice(refusal.notice(&planned.summary)));
            return ToolOutput::error(refusal.message());
        }
        let decided = if preapproved {
            "preapproved"
        } else {
            approver.take_decided_by().audit_label(decision)
        };
        let folder = self.grant_folder(&planned);
        self.policy.remember(&planned, decision, &folder);
        if decision == Decision::AllowAlways {
            self.save_always(&planned, on_event);
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
                let out = script::execute(spec, &self.sandbox.cwd(), &mut ctx).await;
                let edits_always = ctx.edits_always;
                if let Some(edit) = edits_always {
                    let folder = self.grant_folder(&edit);
                    self.policy.remember(&edit, Decision::AllowAlways, &folder);
                    self.save_always(&edit, on_event);
                }
                out
            }
            None => tools::run(&self.sandbox, &planned).await,
        };
        self.audit(
            run_id,
            tool_name,
            &planned.summary,
            (decided, None),
            out.is_error,
        );
        let diff = (planned.access == Access::Write && !out.is_error)
            .then(|| planned.preview.clone())
            .flatten();
        on_event(AssistantEvent::ToolFinished {
            name: tool_name.to_string(),
            summary: planned.summary,
            is_error: out.is_error,
            detail: result_detail(&out),
            output: out.output.clone(),
            diff,
        });
        out
    }

    /// The folder an "always" answer for `planned` is saved under: the
    /// current folder when the call works inside it, else the open folder
    /// that holds what it touches.
    fn grant_folder(&self, planned: &Planned) -> PathBuf {
        let cwd = self.sandbox.cwd();
        if planned.scope.starts_with(&cwd) {
            return cwd;
        }
        self.sandbox
            .roots()
            .into_iter()
            .filter(|root| planned.scope.starts_with(root))
            .max_by_key(|root| root.components().count())
            .unwrap_or(cwd)
    }

    /// Writes an "always" answer to the saved rules; a failure only warns
    /// (the answer still holds for this session).
    fn save_always(&self, planned: &Planned, on_event: &mut (dyn FnMut(AssistantEvent) + Send)) {
        let folder = self.grant_folder(planned);
        let saved = self
            .saved
            .as_ref()
            .map(|store| store.grant(&folder, planned));
        if let Some(Err(err)) = saved {
            on_event(AssistantEvent::Notice(format!(
                "couldn't save \"always\" ({err:#}); it applies for this session only"
            )));
        }
    }

    /// `change_directory`: inside an open folder gz switches at once;
    /// outside, the user decides (unless they always allowed it), and an
    /// allowed folder is opened for the rest of the session.
    async fn change_directory(
        &mut self,
        run_id: &str,
        input: &Value,
        asked_by: Option<String>,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> ToolOutput {
        let tool = ContextTool::ChangeDirectory.as_str();
        let requested = input["path"].as_str().unwrap_or_default();
        let check = match self.sandbox.check_folder(requested) {
            Ok(check) => check,
            Err(err) => {
                self.audit(run_id, tool, requested, ("rejected", None), true);
                on_event(AssistantEvent::Notice(format!("{tool}: {err:#}")));
                return ToolOutput::error(format!("{err:#}"));
            }
        };
        let (folder, decided) = match check {
            FolderCheck::Inside(folder) => (folder, "inside"),
            FolderCheck::Outside(folder) if self.context.allowed.allows(&folder) => {
                (folder, "preapproved")
            }
            FolderCheck::Outside(folder) => {
                let request = self.folder_request(&folder, asked_by);
                let decision = approver.decide(&request).await;
                if decision == Decision::Deny {
                    let refusal = approver.take_refusal();
                    let entry = (refusal.audit_label(), refusal.audit_reason());
                    self.audit(run_id, tool, &request.summary, entry, true);
                    on_event(AssistantEvent::Notice(refusal.notice(&request.summary)));
                    return ToolOutput::error(refusal.message());
                }
                if decision == Decision::AllowAlways {
                    if let Err(err) = self.context.allowed.remember(&folder) {
                        on_event(AssistantEvent::Notice(format!(
                            "couldn't save \"always\" ({err:#}); it applies for this session only"
                        )));
                    }
                }
                (folder, approver.take_decided_by().audit_label(decision))
            }
        };
        let shown = self.sandbox.display_folder(&folder);
        let summary = format!("{tool} {shown}");
        let out = match self.enter(&folder) {
            Ok(()) => ToolOutput::ok(format!("Now in {shown}")),
            Err(err) => ToolOutput::error(format!("{err:#}")),
        };
        self.audit(run_id, tool, &shown, (decided, None), out.is_error);
        on_event(AssistantEvent::ToolStarted {
            name: tool.to_string(),
            summary: summary.clone(),
            local: true,
        });
        on_event(AssistantEvent::ToolFinished {
            name: tool.to_string(),
            summary,
            is_error: out.is_error,
            detail: result_detail(&out),
            output: out.output.clone(),
            diff: None,
        });
        out
    }

    fn folder_request(&self, folder: &Path, asked_by: Option<String>) -> PermissionRequest {
        let shown = self.sandbox.display_folder(folder);
        PermissionRequest {
            access: Access::Folder,
            summary: format!("{} {shown}", ContextTool::ChangeDirectory.as_str()),
            preview: None,
            chat_scope: format!("work in {shown} for this session"),
            always_scope: Some(format!("always allow {shown}")),
            action: None,
            asked_by,
            target: Some(shown),
            note: None,
        }
    }

    /// Opens `folder` if it isn't already and makes it the current folder.
    fn enter(&mut self, folder: &Path) -> Result<()> {
        if self.sandbox.add_root(folder)? && self.yes_reads {
            self.policy.allow_reads_under(folder);
        }
        self.sandbox.set_cwd(folder)
    }

    /// `memory`: the assistant's own notes, so no prompt, but every change
    /// is shown and audited (by id; the text stays out of the log).
    fn memory_call(
        &mut self,
        run_id: &str,
        input: &Value,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> ToolOutput {
        let tool = ContextTool::Memory.as_str();
        let action = input["action"].as_str().unwrap_or_default().to_string();
        let (Some(store), Some(limits)) = (self.context.active_memory(), self.context.limits())
        else {
            return ToolOutput::error("memory is off; the user can turn it on with /memory on");
        };
        let summary = format!("{tool} {action}");
        let (out, detail) = match memory::run_tool(store, input, &limits) {
            Ok(change) => {
                if let Some(word) = change.audit_action() {
                    let target = change.audit_target();
                    self.audit(
                        run_id,
                        &format!("{tool}/{word}"),
                        &target,
                        ("allowed", None),
                        false,
                    );
                }
                (ToolOutput::ok(change.output()), change.notice())
            }
            Err(err) => {
                self.audit(
                    run_id,
                    &format!("{tool}/{action}"),
                    "",
                    ("rejected", None),
                    true,
                );
                let text = format!("{err:#}");
                (ToolOutput::error(text.clone()), text)
            }
        };
        on_event(AssistantEvent::ToolStarted {
            name: tool.to_string(),
            summary: summary.clone(),
            local: true,
        });
        on_event(AssistantEvent::ToolFinished {
            name: tool.to_string(),
            summary,
            is_error: out.is_error,
            detail,
            output: out.output.clone(),
            diff: None,
        });
        out
    }

    /// `/cd <path>`: the user's own move, so any existing folder (outside
    /// the open ones too, except the disk root and secrets) is fine.
    pub fn change_dir(&mut self, requested: &str) -> Result<String> {
        let folder = match self.sandbox.check_folder(requested)? {
            FolderCheck::Inside(folder) | FolderCheck::Outside(folder) => folder,
        };
        self.enter(&folder)?;
        let shown = self.sandbox.display_folder(&folder);
        self.audit("", "user/cd", &shown, ("user", None), false);
        Ok(format!("Now in {shown}"))
    }

    /// `/add-dir <path>`: opens another folder without moving.
    pub fn add_dir(&mut self, requested: &str) -> Result<String> {
        let folder = match self.sandbox.check_folder(requested)? {
            FolderCheck::Inside(folder) => {
                let shown = self.sandbox.display_folder(&folder);
                return Ok(format!("{shown} is already open"));
            }
            FolderCheck::Outside(folder) => folder,
        };
        if self.sandbox.add_root(&folder)? && self.yes_reads {
            self.policy.allow_reads_under(&folder);
        }
        let shown = self.sandbox.display_folder(&folder);
        self.audit("", "user/add-dir", &shown, ("user", None), false);
        Ok(format!("Added {shown}"))
    }

    /// `/dirs remove <path>`: closes an added folder.
    pub fn remove_dir(&mut self, requested: &str) -> Result<String> {
        let folder = match self.sandbox.check_folder(requested)? {
            FolderCheck::Inside(folder) | FolderCheck::Outside(folder) => folder,
        };
        self.sandbox.remove_root(&folder)?;
        let shown = self.sandbox.display_folder(&folder);
        self.audit("", "user/remove-dir", &shown, ("user", None), false);
        Ok(format!(
            "Removed {shown}; now in {}",
            self.sandbox.display_folder(&self.sandbox.cwd())
        ))
    }

    /// The chat server's own approval interrupt (e.g. "may I run this
    /// action?"), answered through the same approver.
    async fn decide_server_action(
        &mut self,
        interrupt: &Value,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Value {
        let approval = Approval::from_event(interrupt);
        let request = PermissionRequest {
            summary: approval
                .summary
                .clone()
                .unwrap_or_else(|| format!("Groundzero wants to run {}", approval.tool)),
            preview: approval.preview.clone(),
            chat_scope: "this action in this chat".to_string(),
            // The server never remembers destructive actions forever.
            always_scope: (!approval.destructive).then(|| "this action in every chat".to_string()),
            access: Access::ServerAction,
            action: Some(approval.tool.clone()),
            asked_by: self.subagent_name(interrupt),
            target: None,
            note: None,
        };
        // Allowed for this chat from another of its sessions (the main
        // agent's or a background subagent's): the server keeps this
        // conversation's grants apart, so gz answers and it remembers.
        if self.policy.allows_action(&approval.tool) {
            return server_answer(&approval.interrupt_id, Decision::AllowChat);
        }
        let mut decision = approver.decide(&request).await;
        self.policy.remember_action(&approval.tool, decision);
        if decision == Decision::Deny {
            // The server's resume takes no reason, so only the user sees why.
            let refusal = approver.take_refusal();
            if matches!(refusal, Refusal::Blocked { .. }) {
                on_event(AssistantEvent::Notice(refusal.notice(&request.summary)));
            }
        }
        if decision == Decision::AllowAlways && approval.destructive {
            decision = Decision::AllowChat;
        }
        server_answer(&approval.interrupt_id, decision)
    }

    /// The name of the subagent behind an interrupt or client-tool request.
    /// Who asks for `data`: the run's subagent that raised it or, in a
    /// background subagent's own session, that subagent.
    fn subagent_name(&self, data: &Value) -> Option<String> {
        let Some(id) = subagents::asked_by(data) else {
            return self.subagent.as_ref().map(|(_, name)| name.clone());
        };
        Some(self.subagent_names.get(&id).cloned().unwrap_or(id))
    }

    /// Logs one call; `decision` is the decision and, for a block, why.
    fn audit(
        &self,
        run_id: &str,
        tool: &str,
        target: &str,
        decision: (&str, Option<String>),
        is_error: bool,
    ) {
        let Some(log) = &self.audit else {
            return;
        };
        let (decision, reason) = decision;
        let entry = Entry {
            conversation_id: &self.conversation_id,
            run_id,
            tool,
            target,
            decision,
            reason: reason.as_deref(),
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

pub(crate) fn platform() -> &'static str {
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

/// Keeps one resume body under the backend's size limit by trimming the
/// later outputs once the budget is spent.
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

/// The resume body for a server approval: the backend's `remember` is a
/// word (`once` / `conversation` / `always`), not a flag.
pub fn server_answer(interrupt_id: &str, decision: Decision) -> Value {
    json!({
        "interruptId": interrupt_id,
        "approved": decision != Decision::Deny,
        "remember": decision.server_remember(),
    })
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
        session.history = None;
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

    fn offline_session(name: &str) -> Session {
        let dir = std::env::temp_dir().join(format!("gz-session-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let resolved = crate::config::resolve(
            &crate::config::Config::default(),
            Some("session-test"),
            Some("http://127.0.0.1:9".into()),
            Some("stub-token".to_string()),
        );
        let http = ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5).unwrap();
        let mut session = Session::new(http, &dir).unwrap();
        session.audit = None;
        session.history = None;
        session
    }

    #[test]
    fn background_subagents_are_asked_for_only_when_on() {
        let mut session = offline_session("background");
        assert!(session
            .start_body("hi")
            .get("backgroundSubagents")
            .is_none());
        session.set_background_subagents(true);
        assert_eq!(session.start_body("hi")["backgroundSubagents"], json!(true));
        session.set_subagents(false);
        let body = session.start_body("hi");
        assert_eq!(body["subagents"], json!(false));
        assert!(body.get("backgroundSubagents").is_none());
    }

    #[test]
    fn subagent_settings_go_out_only_when_the_server_takes_them() {
        use crate::assistant::models::{LlmConnection, Model};
        let mut session = offline_session("settings");
        session.set_subagent_settings(SubagentSettings {
            model: Some("haiku55".into()),
            max_concurrent: Some(3),
            ..SubagentSettings::default()
        });
        assert!(
            session.start_body("hi").get("subagentSettings").is_none(),
            "not advertised"
        );
        session.set_subagent_settings_supported(true);
        assert_eq!(
            session.start_body("hi")["subagentSettings"],
            json!({"model": "haiku55", "maxConcurrent": 3})
        );
        session.set_model(Some(Selection {
            model: Model::Connection(LlmConnection {
                provider: "OPENAI".into(),
                name: "my-gpt".into(),
                model_id: "gpt-5".into(),
            }),
            effort: None,
        }));
        assert_eq!(
            session.start_body("hi")["subagentSettings"],
            json!({"maxConcurrent": 3}),
            "a connection chat sends no model"
        );
        session.set_subagents(false);
        assert!(session.start_body("hi").get("subagentSettings").is_none());
        session.set_subagents(true);
        session.set_subagent_settings(SubagentSettings::default());
        assert!(
            session.start_body("hi").get("subagentSettings").is_none(),
            "nothing set"
        );
        let mut child = offline_session("settings-child").into_subagent(
            "analyst",
            "Look",
            session.shared_policy(),
        );
        child.set_subagent_settings_supported(true);
        child.set_subagent_settings(SubagentSettings {
            max_subagents: Some(2),
            ..SubagentSettings::default()
        });
        assert!(
            child.start_body("x").get("subagentSettings").is_none(),
            "a subagent starts no subagents"
        );
    }

    #[test]
    fn a_subagent_session_runs_its_profile_and_asks_in_its_name() {
        let main = offline_session("main");
        let session = offline_session("child").into_subagent(
            "data_engineer",
            "Check runs",
            main.shared_policy(),
        );
        let body = session.start_body("Why do the runs fail?");
        assert_eq!(body["subagentProfile"], json!("data_engineer"));
        assert_eq!(body["conversationName"], json!("Subagent · Check runs"));
        assert!(body.get("subagents").is_none() && body.get("backgroundSubagents").is_none());
        assert!(session.history.is_none(), "kept out of /resume");
        assert_eq!(
            session.subagent_name(&json!({})).as_deref(),
            Some("Check runs")
        );
    }

    /// Answers every prompt with `decision` and keeps what it was asked.
    struct Answering {
        decision: Decision,
        asked: Mutex<Vec<PermissionRequest>>,
    }

    impl Answering {
        fn new(decision: Decision) -> Self {
            Self {
                decision,
                asked: Mutex::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<PermissionRequest> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Approver for Answering {
        fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
            self.asked.lock().unwrap().push(request.clone());
            Box::pin(async move { self.decision })
        }
    }

    /// A session opened at `~/code/app` of a fake home that also holds
    /// `~/code/other` and `~/elsewhere`; "always" answers land in the
    /// returned list instead of the user's config.
    fn folder_session(name: &str) -> (PathBuf, Session, Arc<Mutex<Vec<PathBuf>>>) {
        use crate::assistant::local_context::tests::recording_folders;
        let home = crate::assistant::sandbox::tests::temp_dir(name);
        for dir in ["code/app", "code/other", "elsewhere"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        let mut session = offline_session(name);
        session.sandbox = Sandbox::with_home(&home.join("code/app"), Some(&home)).unwrap();
        let (allowed, saved) = recording_folders(Vec::new());
        session.context = LocalContext::new(None, None, allowed);
        session.set_local_context(Some(Limits::default()));
        (home, session, saved)
    }

    async fn cd(session: &mut Session, path: &str, approver: &dyn Approver) -> ToolOutput {
        session
            .answer_one(
                "r",
                "change_directory",
                &json!({"path": path}),
                None,
                approver,
                &mut |_| {},
            )
            .await
    }

    #[tokio::test]
    async fn change_directory_inside_the_open_folders_needs_no_prompt() {
        let (home, mut session, _) = folder_session("cd-inside");
        std::fs::create_dir_all(home.join("code/app/src")).unwrap();
        let approver = Answering::new(Decision::Deny);
        let out = cd(&mut session, "src", &approver).await;
        assert_eq!(out.output, "Now in ~/code/app/src");
        assert!(approver.asked().is_empty());
        assert_eq!(session.sandbox.cwd(), home.join("code/app/src"));
        assert!(
            cd(&mut session, "/definitely/not/here", &approver)
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn change_directory_outside_asks_and_a_denial_keeps_gz_where_it_was() {
        let (home, mut session, saved) = folder_session("cd-deny");
        let approver = Answering::new(Decision::Deny);
        let out = cd(&mut session, "~/code/other", &approver).await;
        assert!(
            out.is_error && out.output.contains("denied"),
            "{}",
            out.output
        );
        let asked = approver.asked();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].access, Access::Folder);
        assert_eq!(asked[0].summary, "change_directory ~/code/other");
        assert_eq!(session.sandbox.cwd(), home.join("code/app"));
        assert_eq!(session.sandbox.roots().len(), 1);
        assert!(saved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_folder_allowed_for_the_session_opens_without_being_saved() {
        let (_home, mut session, saved) = folder_session("cd-chat");
        let approver = Answering::new(Decision::AllowChat);
        assert_eq!(
            cd(&mut session, "../other", &approver).await.output,
            "Now in ~/code/other"
        );
        assert!(saved.lock().unwrap().is_empty());
        assert_eq!(session.sandbox.roots().len(), 2, "the folder is open now");
        assert!(
            session.sandbox.resolve("../app").is_ok(),
            "the start folder stays open"
        );
        cd(&mut session, "~/code/app", &approver).await;
        cd(&mut session, "~/code/other", &approver).await;
        assert_eq!(
            approver.asked().len(),
            1,
            "an open folder isn't asked about again"
        );
    }

    #[tokio::test]
    async fn always_allowed_folders_are_saved_and_honored_by_later_sessions() {
        let (home, mut session, saved) = folder_session("cd-always");
        let approver = Answering::new(Decision::AllowAlways);
        cd(&mut session, "~/elsewhere", &approver).await;
        assert_eq!(*saved.lock().unwrap(), vec![home.join("elsewhere")]);

        let (_, mut later, _) = folder_session("cd-always-later");
        let (allowed, _) =
            crate::assistant::local_context::tests::recording_folders(vec![home.join("elsewhere")]);
        later.sandbox = Sandbox::with_home(&home.join("code/app"), Some(&home)).unwrap();
        later.context = LocalContext::new(None, None, allowed);
        later.set_local_context(Some(Limits::default()));
        let never = Answering::new(Decision::Deny);
        assert_eq!(
            cd(&mut later, "~/elsewhere", &never).await.output,
            "Now in ~/elsewhere"
        );
        assert!(never.asked().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn commands_run_where_change_directory_moved_to() {
        let (home, mut session, _) = folder_session("cd-command");
        std::fs::write(home.join("code/other/marker-here"), "").unwrap();
        let approver = Answering::new(Decision::AllowOnce);
        cd(&mut session, "~/code/other", &approver).await;
        let out = session
            .answer_one(
                "r",
                "run_command",
                &json!({"command": "ls"}),
                None,
                &approver,
                &mut |_| {},
            )
            .await;
        assert!(out.output.contains("marker-here"), "{}", out.output);
    }

    #[test]
    fn local_context_goes_out_only_when_the_server_takes_it() {
        let (home, mut session, _) = folder_session("context-body");
        std::fs::write(home.join("code/app/AGENTS.md"), "Run make test").unwrap();
        session.set_local_context(None);
        let body = session.start_body("hi");
        assert_eq!(body["clientTools"].as_array().unwrap().len(), 8);
        assert!(body.get("clientContext").is_none());

        let store = memory::MemoryStore::at(home.join("memory.json"), "default");
        store.save("Uses tabs", &Limits::default()).unwrap();
        let (allowed, _) = crate::assistant::local_context::tests::recording_folders(Vec::new());
        session.context = LocalContext::new(Some(store), None, allowed);
        session.set_local_context(Some(Limits::default()));
        let body = session.start_body("hi");
        let tools = body["clientTools"].as_array().unwrap();
        assert!(tools.contains(&json!("change_directory")) && tools.contains(&json!("memory")));
        let context = &body["clientContext"];
        assert_eq!(context["cwd"], "~/code/app");
        assert_eq!(context["roots"], json!(["~/code/app"]));
        assert_eq!(
            context["instructions"],
            json!([{"source": "project", "path": "~/code/app/AGENTS.md", "text": "Run make test"}])
        );
        assert_eq!(context["memories"][0]["text"], "Uses tabs");
        assert!(context.get("skills").is_none());

        session.set_memory(false);
        let body = session.start_body("hi");
        assert!(!body["clientTools"]
            .as_array()
            .unwrap()
            .contains(&json!("memory")));
        assert!(
            body["clientContext"].get("memories").is_none(),
            "off sends none"
        );
    }

    #[tokio::test]
    async fn the_memory_tool_saves_without_asking_and_tells_the_user() {
        let (home, mut session, _) = folder_session("memory-tool");
        let store = memory::MemoryStore::at(home.join("memory.json"), "default");
        let (allowed, _) = crate::assistant::local_context::tests::recording_folders(Vec::new());
        session.context = LocalContext::new(Some(store.clone()), None, allowed);
        session.set_local_context(Some(Limits::default()));
        session.audit = Some(AuditLog::at(home.join("audit.log")));
        let approver = Answering::new(Decision::Deny);
        let mut seen = Vec::new();
        let out = session
            .answer_one(
                "r",
                "memory",
                &json!({"action": "save", "text": "Uses snake_case for Python"}),
                None,
                &approver,
                &mut |event| seen.push(event),
            )
            .await;
        assert!(!out.is_error, "{}", out.output);
        assert!(approver.asked().is_empty());
        assert!(seen.iter().any(|e| matches!(e,
            AssistantEvent::ToolFinished { detail, .. } if detail == "Saved memory: Uses snake_case for Python")));
        let id = store.list().unwrap()[0].id.clone();
        let audit = std::fs::read_to_string(home.join("audit.log")).unwrap();
        assert!(audit.contains("\"tool\":\"memory/save\"") && audit.contains(&id));
        assert!(
            !audit.contains("snake_case"),
            "the text stays out of the log"
        );

        let secret = session
            .answer_one(
                "r",
                "memory",
                &json!({"action": "save", "text": "key AKIAIOSFODNN7EXAMPLE"}),
                None,
                &approver,
                &mut |_| {},
            )
            .await;
        assert!(secret.is_error);
        assert_eq!(store.list().unwrap().len(), 1);
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
        // Each output fills the per-call cap; together they overflow.
        let size = crate::assistant::tools::MAX_OUTPUT_CHARS;
        let fit = RESUME_OUTPUT_BUDGET / size;
        let mut results: Vec<_> = (0..=fit).map(|_| big(size)).collect();
        fit_to_budget(&mut results);
        assert!(results[..fit].iter().all(|r| !r.2.truncated));
        assert!(results[fit].2.truncated);
        let leftover = RESUME_OUTPUT_BUDGET - fit * size;
        assert!(results[fit].2.output.chars().count() < leftover + 100);
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
