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
use crate::assistant::chat_grants::ChatGrantStore;
use crate::assistant::checkpoints::{self, EditHistory};
use crate::assistant::diagnostics::{AfterChecks, Batch, Diagnostics};
use crate::assistant::history;
use crate::assistant::hooks::{self, Hooks};
use crate::assistant::interrupt::{Interrupt, Interruptible, TurnSignal};
use crate::assistant::jobs::{Finished, Jobs};
use crate::assistant::local_context::{ContextTool, Limits, LocalContext};
use crate::assistant::memory;
use crate::assistant::models::Selection;
use crate::assistant::os_sandbox::Confinement;
use crate::assistant::permissions::{Approver, Decision, PermissionRequest, Policy, Refusal};
use crate::assistant::questions::{self, Query};
use crate::assistant::read_only;
use crate::assistant::rules;
use crate::assistant::sandbox::{FolderCheck, Sandbox};
use crate::assistant::saved_rules;
use crate::assistant::script::{self, ScriptContext};
use crate::assistant::session_tools::{self, Abilities, SessionTool};
use crate::assistant::steering::{SentNote, Steering};
use crate::assistant::subagent_settings::SubagentSettings;
use crate::assistant::todos::Todo;
use crate::assistant::tools::{self, Access, Planned, ToolName, ToolOutput};
use crate::assistant::turn_marks::{CommandMark, StopNote, StopOutcome, StoppedCall};
use crate::cmd::stream::{self, Approval, RunEvent};
use crate::cmd::{new_uuid, seg};
use crate::http::ApiClient;
use crate::skills;
use crate::sse::Flow;
use crate::subagents::{self, SubagentEvent, SubagentReport, SubagentRequest};
use parallel::Authorized;

mod flag_rules;
mod hook_turns;
mod local_mcp;
mod server_tools;
mod side_tool_hooks;

use side_tool_hooks::{SideCall, SideTool};
mod parallel;
mod stop;
#[cfg(test)]
mod stub_chat;

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
    /// The model's task list changed (`todo_write`).
    Todos(Vec<Todo>),
    /// A background subagent finished; its report goes to the main agent.
    SubagentReport(SubagentReport),
    /// The run (or the one resuming it) takes notes now: the UI sends the
    /// waiting ones, apart from the turn, so a slow note never holds back
    /// the run's events, its stop or its end.
    RunTakesNotes,
    /// The model read a note the user sent while it worked.
    SteerApplied(SentNote),
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
    /// Tokens one step of the turn used, as it ends; the turn's
    /// [`Self::Usage`] still follows with the cost.
    StepUsage(stream::TokenCounts),
    /// The run's event stream dropped and gz is reconnecting (why, and
    /// when it tries again). Any later event means it is back.
    Reconnecting(String),
    /// A piece of the model's reasoning summary: shown dim and folded,
    /// never sent back to the model or exported.
    Thinking(String),
    /// A tool the server ran finished: `detail` is a one-line result
    /// summary, `output` the full redacted result.
    ServerToolFinished {
        name: String,
        summary: String,
        is_error: bool,
        detail: String,
        output: String,
    },
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
    turn: TurnSignal,
}

impl Canceller {
    /// A front end is about to send a turn: [`Self::cancel`] from now on
    /// stops that turn, even before the chat has given it a run id.
    pub fn expect_turn(&self) {
        self.turn.expect_turn();
    }

    /// Stops the turn's local work at once: open prompts, running commands
    /// and scripts, the rest of the batch. The turn then ends its run on
    /// the server itself and reports [`TurnEnd::Cancelled`].
    pub fn interrupt(&self) {
        self.turn.interrupt();
    }

    /// Stops the turn's local work at once (prompts, commands, the rest of
    /// the batch) and asks the server to cancel the active run; false if no
    /// run was active yet (the turn cancels it when it gets one).
    pub async fn cancel(&self) -> Result<bool> {
        self.interrupt();
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
    /// Subagents that finished this leg: name, status, summary.
    finished: Vec<(String, String, String)>,
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
    /// The chat server's client-tool catalog (`clientTools` in its
    /// capabilities); `None` for a server too old to list it, which gets
    /// only the original tools.
    server_tools: Option<Vec<String>>,
    jobs: Jobs,
    /// Where background commands report their end, until a front end takes
    /// it ([`Self::take_command_notices`]); only then may commands run in
    /// the background.
    job_notices: Option<tokio::sync::mpsc::UnboundedReceiver<Finished>>,
    /// What gz's tools changed in which file, for `/rewind` and `/diff`;
    /// shared with background subagents.
    edits: EditHistory,
    /// The user's hooks, shared with background subagents and the UI.
    hooks: Hooks,
    /// The user's local MCP servers (`mcp.toml`), shared with background
    /// subagents and the UI; `None` until a front end sets them.
    local_mcp: Option<crate::assistant::mcp_local::LocalMcp>,
    /// Scripted runs' tool filter, extra instructions and step guard.
    controls: crate::assistant::run_controls::RunControls,
    /// Checks the files tools write; off until a front end loads it.
    diagnostics: Diagnostics,
    /// Where the front end's stop requests arrive, and the running turn's.
    turn: TurnSignal,
    interrupt: Interrupt,
    /// What a stop cut short, told to the model with the next message.
    stop_note: StopNote,
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
        let mut context = LocalContext::for_profile(
            &resolved.profile_name,
            &resolved.profile,
            crate::skills::team::team_key(resolved),
            &sandbox.start(),
        );
        context.skills = context.skills.map(|home| home.with_project(root));
        context.prefetch_environment(&sandbox);
        let mut session = Self::with_sandbox(http, sandbox, context);
        session.edits = EditHistory::open(&session.conversation_id);
        checkpoints::prune_stale();
        Ok(session)
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
        let (jobs, job_notices) = Jobs::new();
        let confinement = Confinement::new(sandbox.clone(), context.os_sandbox.clone());
        let jobs = jobs
            .confined(confinement)
            .with_spill(sandbox.spill().clone());
        let conversation_id = new_uuid();
        let mut policy = Policy::with_saved(always);
        policy.set_rules(rules::RuleSet::for_sandbox(&sandbox));
        policy.open_chat(ChatGrantStore::open().ok(), &conversation_id);
        context.obey_read_rules(policy.read_rules());
        Self {
            http,
            sandbox,
            policy,
            saved,
            audit: AuditLog::open().ok(),
            history: history::Index::open().ok(),
            conversation_id,
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
            server_tools: None,
            jobs,
            job_notices: Some(job_notices),
            edits: EditHistory::disabled(),
            hooks: Hooks::default(),
            local_mcp: None,
            controls: Default::default(),
            diagnostics: Default::default(),
            turn: TurnSignal::default(),
            interrupt: Interrupt::default(),
            stop_note: StopNote::default(),
        }
    }

    /// Sets the controls of a scripted run (see [`crate::assistant::run_controls`]).
    pub fn set_run_controls(&mut self, controls: crate::assistant::run_controls::RunControls) {
        self.controls = controls;
    }

    /// The run controls, for background subagents to follow.
    pub fn run_controls(&self) -> crate::assistant::run_controls::RunControls {
        self.controls.clone()
    }

    /// Whether the model is offered the client tool `name` on the next run.
    pub fn offers_tool(&self, name: &str) -> bool {
        self.client_tools().contains(&name)
    }

    /// The conversation's edit history, for the UI and background subagents.
    /// Copies this chat's permission grants to the fork `conversation_id`.
    pub fn copy_chat_grants_to(&self, conversation_id: &str) -> Result<()> {
        self.policy.copy_chat_to(conversation_id)
    }

    /// The documents attached in this chat, sent with every run.
    pub fn attached_documents(&self) -> &[String] {
        &self.documents
    }

    /// Carries documents into a copy of the chat (`/fork`), after
    /// [`Self::continue_conversation`] cleared them.
    pub fn keep_documents(&mut self, documents: Vec<String>) {
        self.documents = documents;
    }

    pub fn edit_history(&self) -> EditHistory {
        self.edits.clone()
    }

    /// Records this session's edits in `edits` (a background subagent
    /// shares its main conversation's).
    pub fn with_edit_history(mut self, edits: EditHistory) -> Self {
        self.edits = edits;
        self
    }

    /// The client tools the chat server knows, from its capabilities.
    pub fn set_server_tools(&mut self, names: Option<Vec<String>>) {
        self.server_tools = names;
    }

    /// Hands a front end the notices of background commands ending, which
    /// it delivers to the main agent; from then on commands may run in the
    /// background.
    pub fn take_command_notices(
        &mut self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<Finished>> {
        self.job_notices.take()
    }

    /// The session's commands, for a front end that lists, reads and stops
    /// them and runs the user's own next to the model's.
    pub fn jobs(&self) -> Jobs {
        self.jobs.clone()
    }

    /// Runs `hooks` around this session's tool calls and turns.
    pub fn set_hooks(&mut self, hooks: Hooks) {
        self.hooks = hooks;
    }

    pub fn set_diagnostics(&mut self, diagnostics: Diagnostics) {
        self.diagnostics = diagnostics;
    }

    /// The session's hooks, for the UI and background subagents.
    pub fn hooks(&self) -> Hooks {
        self.hooks.clone()
    }

    fn abilities(&self) -> Abilities {
        Abilities {
            interactive: self.subagent.is_none(),
            background_commands: self.job_notices.is_none() && self.subagent.is_none(),
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

    /// The grants this session shares: with its background subagents, and
    /// with the front end that saves or revokes rules while it runs.
    pub fn shared_policy(&self) -> Policy {
        self.policy.shared()
    }

    /// Turns a fresh session into a background subagent's: its runs carry
    /// the profile, it delegates nothing, it stays out of `/resume`, and
    /// it uses (and adds to) the main session's grants in `policy`.
    pub fn into_subagent(mut self, profile: &str, name: &str, policy: Policy) -> Self {
        self.subagent = Some((profile.to_string(), name.to_string()));
        self.policy = policy;
        self.context.obey_read_rules(self.policy.read_rules());
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

    /// Resolves mentions (files, folders and skills) and pasted images in
    /// `text` (see [`attachments::prepare`]); nothing is uploaded yet.
    pub fn prepare(&self, text: &str, images: &[PathBuf]) -> Result<PreparedTurn> {
        attachments::prepare(&self.sandbox, text, images, &self.mentionable_skills())
    }

    /// Skills an `@name` mention may ask for: none when the server can't
    /// load them.
    pub fn mentionable_skills(&self) -> Vec<String> {
        if !self.controls.allows(ContextTool::LoadSkill.as_str()) {
            return Vec::new();
        }
        self.context.skill_names()
    }

    /// Every skill the user installed, offered in this run or not.
    pub fn installed_skills(&self) -> Vec<String> {
        self.context.skill_names()
    }

    /// Whether edits are checked for problems after they are made.
    #[cfg(test)]
    pub fn checks_edits(&self) -> bool {
        self.diagnostics.is_on()
    }

    /// `--bare`: no hooks, command files, skills, MCP servers, memories,
    /// instruction files or status-line command.
    pub fn is_bare(&self) -> bool {
        self.controls.bare
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
        self.hooks
            .left_conversation(&self.conversation_id, hooks::StartSource::Clear);
        self.conversation_id = new_uuid();
        self.is_new = true;
        self.documents.clear();
        // A stop note is about the conversation that was stopped.
        self.stop_note = StopNote::default();
        let store = self.policy.chat_store();
        self.policy.open_chat(store, &self.conversation_id);
        self.edits.set_conversation(&self.conversation_id);
        self.context.on_demand.clear();
        self.sandbox.reads().clear();
    }

    /// The conversation exists on the server (a turn was sent, or it was
    /// continued), so it can be renamed or forked.
    pub fn is_saved(&self) -> bool {
        !self.is_new
    }

    pub fn continue_conversation(&mut self, conversation_id: &str) {
        if conversation_id != self.conversation_id {
            let store = self.policy.chat_store();
            self.policy.open_chat(store, conversation_id);
            self.context.on_demand.clear();
            // Documents belong to the chat they were attached in.
            self.documents.clear();
            // A stop note is about its own conversation; one left from a
            // stop before gz quit is told with the next message.
            self.stop_note = self
                .history
                .as_ref()
                .map(|index| index.stop_notes().load(conversation_id))
                .unwrap_or_default();
            self.hooks
                .left_conversation(&self.conversation_id, hooks::StartSource::Resume);
        }
        self.conversation_id = conversation_id.to_string();
        self.is_new = false;
        self.edits.set_conversation(&self.conversation_id);
        // The model re-reads a file before changing it in a resumed chat.
        self.sandbox.reads().clear();
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

    /// The model turns run on (`None`: the server's default).
    pub fn model(&self) -> Option<&Selection> {
        self.model.as_ref()
    }

    /// The notes the user sends while gz works, shared with the UI.
    pub fn steering(&self) -> Steering {
        self.steering.clone()
    }

    pub fn canceller(&self) -> Canceller {
        Canceller {
            http: self.http.clone(),
            active_run: self.active_run.clone(),
            turn: self.turn.clone(),
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
        self.interrupt = self.turn.claim();
        self.send_claimed(prompt, approver, on_event).await
    }

    /// [`Self::send`] once the turn has its stop signal: the prompt hooks,
    /// the turn, and as many more as `stop` hooks ask for.
    async fn send_claimed(
        &mut self,
        prompt: &str,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Result<TurnEnd> {
        let mut prompt = match self.hooked_prompt(prompt, on_event).await {
            Ok(prompt) => prompt,
            Err(why) => {
                on_event(AssistantEvent::Done(TurnEnd::Failed(why.clone())));
                return Ok(TurnEnd::Failed(why));
            }
        };
        let mut carried_on = 0;
        let end = loop {
            let end = self.run_turn(&prompt, approver, on_event).await;
            self.set_active(None);
            let end = end?;
            if end != TurnEnd::Completed {
                break end;
            }
            match self.carry_on(carried_on, on_event).await {
                Some(next) => (prompt, carried_on) = (next, carried_on + 1),
                None => break end,
            }
        };
        // A background subagent's turn is not the user's turn.
        if end == TurnEnd::Completed && self.subagent.is_none() {
            let cwd = self.sandbox.cwd();
            self.hooks
                .turn_end(&cwd, &self.hook_log(""), on_event)
                .await;
        }
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
        self.interrupt = self.turn.claim();
        for path in &turn.uploads {
            let upload = attachments::upload(&self.http, path);
            let Some(reply) = self.interrupt.unless_stopped(upload).await else {
                on_event(AssistantEvent::Done(TurnEnd::Cancelled));
                return Ok(TurnEnd::Cancelled);
            };
            let reply = reply.with_context(|| format!("cannot attach {}", path.display()))?;
            self.documents.push(attachments::document_id(&reply)?);
        }
        for read in &turn.reads {
            self.audit("", "mention", read, ("allowed", None), false);
            self.sandbox.record_mention(read);
        }
        self.send_claimed(&turn.prompt, approver, on_event).await
    }

    async fn run_turn(
        &mut self,
        prompt: &str,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Result<TurnEnd> {
        crate::timing::mark("session took the turn");
        let interrupt = self.interrupt.clone();
        let approver = &Interruptible::new(approver, interrupt.clone());
        if interrupt.is_requested() {
            return Ok(TurnEnd::Cancelled);
        }
        if self.subagent.is_none() {
            if let Err(err) = self.edits.begin_turn(prompt) {
                on_event(AssistantEvent::Notice(format!(
                    "/rewind will not cover this message: {err:#}"
                )));
            }
        }
        if let Some(why) = self.controls.before_leg() {
            return Ok(TurnEnd::Failed(why));
        }
        let started = self
            .post_run(
                "/chat/runs".to_string(),
                self.start_body(&self.with_stop_note(&self.with_mode_note(prompt, approver))),
            )
            .await?;
        if !self.stop_note.is_empty() {
            self.stop_note = StopNote::default();
            self.keep_stop_note();
        }
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
        // Per turn, not per leg: a server tool's result can arrive after a
        // client-tool resume, in a later leg's stream.
        let mut server_calls = server_tools::ServerCalls::default();
        loop {
            self.set_active(Some(run_id.clone()));
            self.steering.run_takes_notes();
            // A stop that came before the run id was known lands here.
            if interrupt.is_requested() {
                self.cancel_run(&run_id, on_event).await;
                return Ok(TurnEnd::Cancelled);
            }
            let mut leg = Leg::default();
            let streamed = self
                .stream_and_deliver(
                    &interrupt,
                    &run_id,
                    &mut last_id,
                    &mut leg,
                    &mut server_calls,
                    on_event,
                )
                .await;
            let Some(streamed) = streamed else {
                self.stop_leg(&run_id, leg, last_id, approver, on_event)
                    .await;
                return Ok(TurnEnd::Cancelled);
            };
            streamed?;
            self.subagent_names.extend(leg.subagents.drain());
            self.chat_subagents_stopped(&leg.finished, on_event).await;
            if leg.cancelled {
                return Ok(TurnEnd::Cancelled);
            }
            if let Some(error) = leg.error {
                return Ok(TurnEnd::Failed(error));
            }
            let continues = !leg.client_calls.is_empty() || leg.approval.is_some();
            if continues {
                // The server refuses notes until gz answers; they wait here.
                self.steering.run_waits_on_gz();
            }
            if let Some(why) = continues.then(|| self.controls.before_leg()).flatten() {
                return Ok(TurnEnd::Failed(why));
            }
            let resumed = if !leg.client_calls.is_empty() {
                let results = self
                    .answer_client_calls(&run_id, leg.client_calls, approver, on_event)
                    .await;
                if let Some(end) = self.hook_halt() {
                    return Ok(end);
                }
                let path = format!("/chat/runs/{}/client-tools/resume", seg(&run_id));
                let body = json!({"toolResults": results});
                if interrupt.is_requested() {
                    self.abandon(path, body, on_event).await;
                    return Ok(TurnEnd::Cancelled);
                }
                self.post_run(path, body).await?
            } else if let Some(pause) = leg.approval {
                let body = self.decide_server_action(&pause, approver, on_event).await;
                let path = format!("/chat/runs/{}/resume", seg(&run_id));
                if interrupt.is_requested() {
                    self.abandon(path, body, on_event).await;
                    return Ok(TurnEnd::Cancelled);
                }
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
        self.controls
            .apply_to_body(&mut body, self.context.limits());
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
        if let Some(server) = &self.server_tools {
            names.extend(session_tools::extra_tools(server, self.abilities()));
        }
        names.extend(crate::assistant::mcp_local::offered(
            self.server_tools.as_deref(),
            self.local_mcp.as_ref(),
        ));
        self.controls.filter_tools(&mut names);
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

    /// Streams one leg (see [`Self::stream_leg`]) after telling the UI the
    /// run takes notes. The server takes none before the resume hands back
    /// the run's id; the UI sends them on its own task, because the run
    /// works whether or not gz listens: waiting on a note's reply would
    /// hold back the run's events, its stop and its end. A `steer_applied`
    /// that arrives before the note's reply is matched by the UI. None
    /// when the user stopped the turn.
    async fn stream_and_deliver(
        &self,
        interrupt: &Interrupt,
        run_id: &str,
        last_id: &mut Option<String>,
        leg: &mut Leg,
        server_calls: &mut server_tools::ServerCalls,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Option<Result<()>> {
        on_event(AssistantEvent::RunTakesNotes);
        interrupt
            .unless_stopped(self.stream_leg(run_id, last_id, leg, server_calls, on_event))
            .await
    }

    /// Streams one leg of the run into `leg`, which keeps what arrived even
    /// when the turn is interrupted mid-way.
    async fn stream_leg(
        &self,
        run_id: &str,
        last_id: &mut Option<String>,
        leg: &mut Leg,
        server_calls: &mut server_tools::ServerCalls,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Result<()> {
        let path = format!("/chat/runs/{}/events", seg(run_id));
        stream::read_run(&self.http, CHAT, &path, last_id, |_, event| {
            crate::timing::mark("first event");
            match event {
                RunEvent::Text(text) if !text.is_empty() => {
                    crate::timing::mark("first text");
                    on_event(AssistantEvent::Text(text))
                }
                // The server announces client tools too ("Read file") just
                // before asking gz to run them; gz shows its own line instead.
                RunEvent::ServerTool(call)
                    if ToolName::from_display(&call.name).is_some()
                        || ContextTool::from_display(&call.name).is_some()
                        || crate::assistant::mcp_local::LocalMcpTool::from_display(&call.name)
                            .is_some() => {}
                RunEvent::ServerTool(call) => on_event(server_calls.started(&call)),
                RunEvent::ServerToolResult(result) => {
                    if let Some(event) = server_calls.finished(&result) {
                        on_event(event);
                    }
                }
                RunEvent::Reasoning(text) if !text.is_empty() => {
                    on_event(AssistantEvent::Thinking(text))
                }
                RunEvent::ToolError(why) => {
                    on_event(AssistantEvent::Notice(format!("tool failed: {why}")))
                }
                RunEvent::Warning(why) => on_event(AssistantEvent::Notice(why)),
                RunEvent::Reconnecting(why) => on_event(AssistantEvent::Reconnecting(why)),
                RunEvent::Usage(usage) => on_event(AssistantEvent::Usage(usage)),
                RunEvent::StepUsage(tokens) => on_event(AssistantEvent::StepUsage(tokens)),
                RunEvent::Context(context) => {
                    self.hooks.note_context(&context);
                    on_event(AssistantEvent::Context(context))
                }
                RunEvent::SteerApplied(note) => on_event(AssistantEvent::SteerApplied(note)),
                RunEvent::SubagentRequested(request) => {
                    on_event(AssistantEvent::SubagentRequested(request))
                }
                RunEvent::Todos(todos) => on_event(AssistantEvent::Todos(todos)),
                RunEvent::SteerUnapplied(notes) => on_event(AssistantEvent::SteerUnapplied(notes)),
                RunEvent::Subagent(event) => {
                    if let SubagentEvent::Started { id, name, .. } = &event {
                        leg.subagents.insert(id.clone(), name.clone());
                    }
                    leg.note_finished(&event, &self.subagent_names);
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
        .await
    }

    /// Plans, asks, runs, and audits each requested call; every call gets a
    /// result (refusals and failures included) so the run can continue.
    /// Once the turn is interrupted, the call in progress stops and it and
    /// the rest get [`crate::assistant::interrupt::INTERRUPTED`].
    async fn answer_client_calls(
        &mut self,
        run_id: &str,
        calls: Vec<Value>,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Vec<Value> {
        let interrupt = self.interrupt.clone();
        let mut answers: Vec<Option<ToolOutput>> = calls.iter().map(|_| None).collect();
        let mut start = 0;
        while start < calls.len() && !interrupt.is_requested() {
            let reads = calls[start..]
                .iter()
                .take_while(|call| parallel::only_reads(call))
                .count();
            if reads > 1 {
                let end = start + reads;
                // Stops itself, keeping the reads that finished.
                self.answer_reads(
                    run_id,
                    &calls[start..end],
                    approver,
                    on_event,
                    &mut answers[start..end],
                )
                .await;
                start = end;
                continue;
            }
            let call = &calls[start];
            let tool_name = call["toolName"].as_str().unwrap_or_default();
            // answer_one stops itself, so a call whose work is done is
            // still finished and audited when the stop lands late.
            let answer = self.answer_call(run_id, call, tool_name, approver, on_event);
            answers[start] = Some(answer.await);
            start += 1;
        }
        let mut results = Vec::with_capacity(calls.len());
        for (call, answer) in calls.iter().zip(answers) {
            let interrupt_id = call["interruptId"].as_str().unwrap_or_default().to_string();
            let tool_name = call["toolName"].as_str().unwrap_or_default().to_string();
            let output = answer.unwrap_or_else(|| {
                let target = stop::skipped_target(&call["input"]);
                self.audit(run_id, &tool_name, target, ("skipped", None), true);
                self.note_stopped(&tool_name, target, StopOutcome::NotRun, None);
                Interrupt::tool_output()
            });
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

    /// One requested call, its events shown on the page of the subagent
    /// that asked for it, if one did.
    async fn answer_call(
        &mut self,
        run_id: &str,
        call: &Value,
        tool_name: &str,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> ToolOutput {
        let asked_by = self.subagent_id(call);
        let mut sink = parallel::routed(call, on_event);
        self.answer_one(
            run_id,
            tool_name,
            &call["input"],
            asked_by,
            approver,
            &mut sink,
        )
        .await
    }

    /// Whether a call may be looked at at all: a hook that stopped the turn
    /// and the run's tool filter refuse it before anything else.
    fn admit(&mut self, tool_name: &str, input: &Value) -> Result<(), ToolOutput> {
        if let Some(why) = self.hooks.halted() {
            return Err(ToolOutput::error(format!(
                "Not run: a hook stopped the turn ({why})"
            )));
        }
        self.controls.tool_requested(tool_name, input);
        if !self.controls.allows(tool_name) {
            let offered = match self.client_tools().join(", ") {
                none if none.is_empty() => "this run offers no tools".to_string(),
                some => format!("use one of: {some}"),
            };
            return Err(ToolOutput::error(format!(
                "Not run: {tool_name} is not offered in this run (--tools or --bare left it out); \
                 {offered}"
            )));
        }
        Ok(())
    }

    /// Answers one call; `asked_by` is the id of the subagent that made it
    /// (`None`: the main conversation). Each subagent keeps its own record
    /// of reads, so it edits only files it read itself.
    async fn answer_one(
        &mut self,
        run_id: &str,
        tool_name: &str,
        input: &Value,
        asked_by: Option<String>,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> ToolOutput {
        if let Err(refused) = self.admit(tool_name, input) {
            // Shown and reported like any finished call, so a script sees
            // a result for the call and the reason.
            on_event(AssistantEvent::ToolStarted {
                name: tool_name.to_string(),
                summary: tool_name.to_string(),
                local: true,
            });
            on_event(AssistantEvent::ToolFinished {
                name: tool_name.to_string(),
                summary: tool_name.to_string(),
                is_error: true,
                detail: result_detail(tool_name, &refused),
                output: refused.output.clone(),
                diff: None,
            });
            return refused;
        }
        let interrupt = self.interrupt.clone();
        // Only tools gz offered this run are run.
        if let Some(tool) = crate::assistant::mcp_local::LocalMcpTool::parse(tool_name)
            .filter(|t| self.client_tools().contains(&t.as_str()))
        {
            let asked_by = asked_by.map(|id| self.subagent_label(&id));
            let work = self.local_mcp_tool(run_id, tool, input, asked_by, approver, on_event);
            return match interrupt.unless_stopped(work).await {
                Some(out) => out,
                None => self.stopped_call(tool_name, stop::skipped_target(input)),
            };
        }
        let offered = self.context.tool_names().contains(&tool_name);
        let side_tool = SessionTool::parse(tool_name)
            .filter(|t| self.client_tools().contains(&t.as_str()))
            .map(SideTool::Session)
            .or_else(|| {
                ContextTool::parse(tool_name)
                    .filter(|_| offered)
                    .map(SideTool::Context)
            });
        if let Some(tool) = side_tool {
            let call = SideCall {
                run_id,
                tool,
                tool_name,
                input,
                asked_by: asked_by.map(|id| self.subagent_label(&id)),
            };
            let work = self.hooked_side_tool(call, approver, on_event);
            return match interrupt.unless_stopped(work).await {
                Some(out) => out,
                None => self.stopped_call(tool_name, stop::skipped_target(input)),
            };
        }
        let checked = self.authorize(run_id, tool_name, input, asked_by, approver, on_event);
        let call = match interrupt.unless_stopped(checked).await {
            Some(Ok(call)) => call,
            Some(Err(out)) => {
                // Esc at the prompt answers it with a denial; the run
                // carrying that denial is cancelled, so the note tells it.
                if interrupt.is_requested() {
                    let target = stop::skipped_target(input);
                    self.note_stopped(tool_name, target, StopOutcome::NotRunAtPrompt, None);
                }
                return out;
            }
            None => {
                let target = stop::skipped_target(input);
                self.audit(run_id, tool_name, target, ("interrupted", None), true);
                self.note_stopped(tool_name, target, StopOutcome::NotRun, None);
                return Interrupt::tool_output();
            }
        };
        call.announce(on_event);
        // A command or script watches the stop itself, to report what it
        // printed.
        let watches_stop = call.planned.command().is_some() || call.planned.script().is_some();
        let ran = self.execute(run_id, &call, approver, on_event);
        let ran = match watches_stop {
            true => Some(ran.await),
            false => interrupt.unless_stopped(ran).await,
        };
        let Some((out, checks)) = ran else {
            let summary = &call.planned.summary;
            self.audit(run_id, tool_name, summary, ("interrupted", None), true);
            return self.stopped_call(tool_name, summary);
        };
        let out = stop::after_checks(checks, out, &interrupt, on_event).await;
        let mut call = call;
        if stop::was_cut_short(&out, &interrupt) {
            call.decided = "interrupted";
            let printed = watches_stop.then(|| out.output.clone());
            let summary = &call.planned.summary;
            self.note_stopped(tool_name, summary, StopOutcome::Stopped, printed);
        }
        self.finish(run_id, call, out, on_event).await
    }

    /// Plans one of gz's own tools and settles whether it may run. Hooks
    /// come first (a deny refuses, an ask always reaches the user), then
    /// rules (deny, then ask, then allow), then grants and the mode, then
    /// gz's automatic read-only approvals, then the prompt. `before_tool`
    /// hooks run with the hooks, before any rule. A refusal or failure
    /// comes back as the call's result. `asked_by` is the asking subagent's
    /// id.
    async fn authorize(
        &mut self,
        run_id: &str,
        tool_name: &str,
        input: &Value,
        asked_by: Option<String>,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> Result<Authorized, ToolOutput> {
        let reader = self.sandbox.for_reader(asked_by.as_deref());
        let asked_by = asked_by.map(|id| self.subagent_label(&id));
        let Some(tool) = ToolName::parse(tool_name).filter(|t| self.offers(*t)) else {
            return Err(ToolOutput::error(format!(
                "gz cannot run the tool '{tool_name}'"
            )));
        };
        let input = self.with_old_defaults(tool, input);
        let planned = match tools::plan(&reader, tool, &input) {
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
                return Err(out);
            }
        };
        let hook_call = hooks::ToolCall::new(&self.sandbox, tool_name, &planned.summary, &input);
        let pre = self
            .hooks
            .pre_tool_use(&hook_call, &self.hook_log(run_id), on_event)
            .await;
        if let Some(refusal) = pre.refusal() {
            self.audit(run_id, tool_name, &planned.summary, ("hook", None), true);
            return Err(ToolOutput::error(refusal));
        }
        // A `before_tool` block refuses before any rule, grant or prompt,
        // so the user is never asked about a call a hook would stop.
        let blocked = self
            .hooks
            .before_tool(&hook_call, &self.hook_log(run_id), on_event)
            .await;
        if let Some(blocked) = blocked {
            self.audit(run_id, tool_name, &planned.summary, ("hook", None), true);
            return Err(blocked);
        }
        let gated = rules::Gated::new(approver, self.policy.rule_verdict(&planned), planned.access);
        let approver: &dyn Approver = &gated;
        let by_hook = gated.hook_allows(&pre);
        let granted =
            self.policy.is_preapproved(&planned) || planned.reads_saved_output(&self.sandbox);
        let preapproved = by_hook || (!pre.asks() && gated.preapproved(granted));
        let automatic = (!preapproved && !pre.asks() && !gated.rule_decides())
            .then(|| self.automatic(&planned, approver))
            .flatten();
        let decision = if preapproved || automatic.is_some() {
            Decision::AllowOnce
        } else {
            let label = self.sandbox.display(&planned.scope);
            let folder = self.sandbox.display_folder(&self.grant_folder(&planned));
            let mut request = Policy::request_for(&planned, &label, &folder);
            request.asked_by = asked_by;
            if pre.asks() {
                request.note = Some(pre.ask_note());
            }
            gated.decide_hooked(&request, pre.asks()).await
        };
        if decision == Decision::Deny {
            let refusal = approver.take_refusal();
            let entry = (refusal.audit_label(), refusal.audit_reason());
            self.audit(run_id, tool_name, &planned.summary, entry, true);
            on_event(AssistantEvent::Notice(refusal.notice(&planned.summary)));
            return Err(ToolOutput::error(refusal.message()));
        }
        let decided = if by_hook {
            "hook"
        } else if preapproved {
            "preapproved"
        } else if let Some(automatic) = automatic {
            automatic.audit_label()
        } else {
            approver.take_decided_by().audit_label(decision)
        };
        let folder = self.grant_folder(&planned);
        self.policy.remember(&planned, decision, &folder);
        if decision == Decision::AllowAlways {
            self.save_always(&planned, on_event);
        }
        Ok(Authorized {
            tool_name: tool_name.to_string(),
            input,
            planned,
            decided,
            reader,
            automatic,
            hook_context: pre.context,
        })
    }

    /// Runs an authorized call: a script, a command, or a file tool. The
    /// checks on the files it wrote come back to run after it, outside the
    /// work a stop drops.
    async fn execute(
        &mut self,
        run_id: &str,
        call: &Authorized,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> (ToolOutput, Option<AfterChecks>) {
        let planned = &call.planned;
        let automatic = call.automatic;
        let sandbox = &self.sandbox;
        let check = self
            .diagnostics
            .before(
                planned,
                &sandbox.cwd(),
                |file| sandbox.display(file),
                on_event,
            )
            .await;
        let out = match planned.script() {
            Some(spec) => {
                let mut ctx = ScriptContext::new(
                    &call.reader,
                    &self.policy,
                    approver,
                    self.audit.as_ref(),
                    (&self.conversation_id, run_id),
                    &mut *on_event,
                )
                .with_edit_history(self.edits.clone());
                ctx.hooks = Some(self.hooks.clone());
                ctx.local_context = Some(&self.context);
                ctx.read_only = automatic == Some(read_only::Automatic::ReadOnlyScript);
                ctx.diagnostics = Batch::new(self.diagnostics.clone());
                let confinement = read_only::script_confinement(self.jobs.confinement(), automatic);
                let stopped = self.interrupt.requested();
                let root = self.sandbox.cwd();
                let mut out =
                    script::execute(spec, &root, confinement.as_ref(), &mut ctx, stopped).await;
                if ctx.read_only {
                    let refused = ctx.write_refused;
                    read_only::after_read_only_script(&mut out, &spec.code, refused, &self.policy);
                    out = out.refit();
                }
                let out = hooks::with_feedback(out, &ctx.hook_feedback);
                let checks = std::mem::take(&mut ctx.diagnostics);
                let edits_always = ctx.edits_always;
                if let Some(edit) = edits_always {
                    let folder = self.grant_folder(&edit);
                    self.policy.remember(&edit, Decision::AllowAlways, &folder);
                    self.save_always(&edit, on_event);
                }
                return (out, Some(AfterChecks::Script(checks)));
            }
            None => match planned.command() {
                Some((command, timeout, background)) => {
                    let may_background = self.abilities().background_commands;
                    let folder = self.sandbox.cwd();
                    let background = background && may_background;
                    let stopped = self.interrupt.requested();
                    read_only::jobs_for(&self.jobs, automatic)
                        .run_until(
                            command,
                            &folder,
                            timeout,
                            background,
                            may_background,
                            stopped,
                        )
                        .await
                }
                None => checkpoints::run_tool(&self.edits, &call.reader, planned, on_event).await,
            },
        };
        (out, check.map(AfterChecks::File))
    }

    /// After a call ran: the after-tool hooks, the sandbox's notes, the
    /// audit entry and the finished event; returns what the model gets.
    async fn finish(
        &mut self,
        run_id: &str,
        call: Authorized,
        out: ToolOutput,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> ToolOutput {
        let Authorized {
            tool_name,
            input,
            planned,
            decided,
            hook_context,
            ..
        } = call;
        let tool_name = tool_name.as_str();
        let hook_call = hooks::ToolCall::new(&self.sandbox, tool_name, &planned.summary, &input);
        // The call's work is done; a stop now only skips the after-hooks.
        let (hook_log, interrupt) = (self.hook_log(run_id), self.interrupt.clone());
        let hooked = self
            .hooks
            .after_tool(&hook_call, out.clone(), &hook_log, on_event);
        let out = interrupt.unless_stopped(hooked).await.unwrap_or(out);
        let out = hooks::with_notes(out, hooks::HookEvent::PreToolUse, &hook_context);
        if let Some(confinement) = self.jobs.confinement() {
            for notice in confinement.take_user_notices() {
                on_event(AssistantEvent::Notice(notice));
            }
        }
        self.audit(
            run_id,
            tool_name,
            &planned.summary,
            (decided, None),
            out.is_error,
        );
        if !out.is_error {
            self.context
                .note_tool_call(&self.sandbox, tool_name, &input);
        }
        let diff = (planned.access == Access::Write && !out.is_error)
            .then(|| planned.preview.clone())
            .flatten();
        on_event(AssistantEvent::ToolFinished {
            name: tool_name.to_string(),
            summary: planned.summary,
            is_error: out.is_error,
            detail: result_detail(tool_name, &out),
            output: out.output.clone(),
            diff,
        });
        out
    }

    /// How `planned` runs without asking, after rules, grants and the mode.
    fn automatic(
        &self,
        planned: &Planned,
        approver: &dyn Approver,
    ) -> Option<read_only::Automatic> {
        let session = read_only::Session {
            sandbox: &self.sandbox,
            confinement: self.jobs.confinement(),
            policy: &self.policy,
            accepts_edits: approver.accepts_edits(),
            in_plan_mode: approver.in_plan_mode(),
        };
        let automatic = read_only::automatic(planned, &session)?;
        // Don't-ask mode runs only what rules and grants allow; reads in the
        // open folders need no permission in any mode.
        if approver.refuses_unasked() && automatic != read_only::Automatic::Read {
            return None;
        }
        Some(automatic)
    }

    /// `prompt` with the note on what the last stop cut short in front of
    /// it, when there is one.
    fn with_stop_note(&self, prompt: &str) -> String {
        self.stop_note.render_onto(prompt)
    }

    /// Remembers a call the stop cut short or skipped, for the note the
    /// model reads with the user's next message.
    pub(super) fn note_stopped(
        &mut self,
        tool: &str,
        target: &str,
        outcome: StopOutcome,
        output: Option<String>,
    ) {
        // A planned call's summary already starts with the tool's name.
        let target = target
            .strip_prefix(tool)
            .and_then(|rest| rest.strip_prefix(' '))
            .unwrap_or(target);
        self.stop_note.record(StoppedCall {
            tool: tool.to_string(),
            target: target.to_string(),
            outcome,
            output,
        });
        self.keep_stop_note();
    }

    /// Saves the pending stop note for this conversation (an empty one
    /// clears it), so a quit before the next message loses nothing. Best
    /// effort, like the history index: losing it only loses the note.
    fn keep_stop_note(&self) {
        if let Some(index) = &self.history {
            let _ = index
                .stop_notes()
                .save(&self.conversation_id, &self.stop_note);
        }
    }

    /// The result of a call the stop cut short, noted for the next message.
    fn stopped_call(&mut self, tool: &str, target: &str) -> ToolOutput {
        self.note_stopped(tool, target, StopOutcome::Stopped, None);
        Interrupt::tool_output()
    }

    /// `prompt`, with a note when plan mode is on: leave it with
    /// `exit_plan_mode`, or, where that isn't offered, answer with the plan.
    fn with_mode_note(&self, prompt: &str, approver: &dyn Approver) -> String {
        if !approver.in_plan_mode() {
            return prompt.to_string();
        }
        let can_exit = self
            .client_tools()
            .contains(&SessionTool::ExitPlanMode.as_str());
        let note = match can_exit {
            true => session_tools::PLAN_MODE_NOTE,
            false => session_tools::PLAN_MODE_REPLY_NOTE,
        };
        format!("{prompt}\n\n{note}")
    }

    /// Whether gz offered `tool` this run.
    fn offers(&self, tool: ToolName) -> bool {
        ToolName::ALL.contains(&tool) || self.client_tools().contains(&tool.as_str())
    }

    /// A server too old to list its tools still describes search_files as
    /// returning matching lines, so gz keeps that default for it.
    fn with_old_defaults(&self, tool: ToolName, input: &Value) -> Value {
        let mut input = input.clone();
        if tool == ToolName::SearchFiles
            && self.server_tools.is_none()
            && input.get("output_mode").is_none()
        {
            input["output_mode"] = json!("content");
        }
        input
    }

    /// Runs one of the tools the session answers itself (see
    /// [`session_tools`]).
    async fn session_tool(
        &mut self,
        run_id: &str,
        tool: SessionTool,
        input: &Value,
        approver: &dyn Approver,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> ToolOutput {
        let id = input["id"].as_str().unwrap_or_default();
        let summary = match tool {
            SessionTool::CommandOutput | SessionTool::StopCommand => {
                format!("{} {id}", tool.as_str())
            }
            _ => tool.as_str().to_string(),
        };
        let shows_as_tool = matches!(tool, SessionTool::CommandOutput | SessionTool::StopCommand);
        if shows_as_tool {
            on_event(AssistantEvent::ToolStarted {
                name: tool.as_str().to_string(),
                summary: summary.clone(),
                local: true,
            });
        }
        let out = match tool {
            SessionTool::CommandOutput => {
                let wait = input["wait_seconds"].as_u64().unwrap_or(0);
                self.jobs
                    .output(id, std::time::Duration::from_secs(wait))
                    .await
            }
            SessionTool::StopCommand => self.jobs.stop_command(id),
            SessionTool::AskUserQuestion => match questions::parse_questions(input) {
                Ok(asked) => {
                    let reply = approver.ask(&Query::Questions(asked.clone())).await;
                    ToolOutput::ok(questions::answers_text(&asked, &reply))
                }
                Err(err) => ToolOutput::error(format!("{err:#}")),
            },
            SessionTool::EnterPlanMode => session_tools::enter_plan_mode(approver, on_event),
            SessionTool::ExitPlanMode => session_tools::exit_plan_mode(input, approver).await,
            SessionTool::TodoWrite => {
                ToolOutput::error("todo_write runs on the chat server, not in gz")
            }
        };
        self.audit(
            run_id,
            tool.as_str(),
            &summary,
            ("allowed", None),
            out.is_error,
        );
        if shows_as_tool {
            on_event(AssistantEvent::ToolFinished {
                name: tool.as_str().to_string(),
                summary,
                is_error: out.is_error,
                detail: result_detail(tool.as_str(), &out),
                output: out.output.clone(),
                diff: None,
            });
        }
        out
    }

    /// The folder an "always" answer for `planned` is saved under: the
    /// current folder when the call works inside it, else the open folder
    /// that holds what it touches.
    fn grant_folder(&self, planned: &Planned) -> PathBuf {
        crate::assistant::permissions::grant_folder(&self.sandbox, planned)
    }

    /// `load_skill`: read-only and confined to one verified skill folder, so
    /// it runs without a prompt (and is still shown and audited).
    fn load_skill(
        &self,
        run_id: &str,
        input: &Value,
        on_event: &mut (dyn FnMut(AssistantEvent) + Send),
    ) -> ToolOutput {
        let name = ContextTool::LoadSkill.as_str();
        let target = [input["name"].as_str(), input["path"].as_str()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let summary = format!("{name} {target}");
        on_event(AssistantEvent::ToolStarted {
            name: name.to_string(),
            summary: summary.clone(),
            local: true,
        });
        let out = match &self.context.skills {
            Some(home) => skills::load::run(home, input),
            None => ToolOutput::error("gz did not offer load_skill in this chat"),
        };
        self.audit(run_id, name, &target, ("read-only", None), out.is_error);
        on_event(AssistantEvent::ToolFinished {
            name: name.to_string(),
            summary,
            is_error: out.is_error,
            detail: result_detail(name, &out),
            output: out.output.clone(),
            diff: None,
        });
        out
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
            detail: result_detail(tool, &out),
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
        self.subagent_id(data).map(|id| self.subagent_label(&id))
    }

    /// [`Self::subagent_name`]'s subagent by id, which unlike its name is
    /// never shared with another subagent.
    fn subagent_id(&self, data: &Value) -> Option<String> {
        subagents::asked_by(data).or_else(|| self.subagent.as_ref().map(|(id, _)| id.clone()))
    }

    /// The name shown for the subagent `id`.
    fn subagent_label(&self, id: &str) -> String {
        match &self.subagent {
            Some((own, name)) if own == id => name.clone(),
            _ => self
                .subagent_names
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.to_string()),
        }
    }

    /// Where this session's hook runs are logged.
    fn hook_log<'a>(&'a self, run_id: &'a str) -> hooks::Log<'a> {
        hooks::Log {
            audit: self.audit.as_ref(),
            conversation_id: &self.conversation_id,
            run_id,
        }
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
    let prompt = CommandMark::typed(prompt);
    let line = prompt.lines().next().unwrap_or(prompt).trim();
    if line.chars().count() <= TITLE_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(TITLE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// The `⎿` line under a finished call: a failure's first line, the one
/// line a result has, or how many lines it has.
fn result_detail(tool: &str, out: &ToolOutput) -> String {
    if out.is_error {
        return out.output.lines().next().unwrap_or("failed").to_string();
    }
    let tool = ToolName::parse(tool);
    let rows = result_rows(tool, &out.output);
    // A read names its count even for one line: "Read 1 line".
    if tool == Some(ToolName::ReadFile) && rows > 0 {
        return format!("{rows} lines");
    }
    match rows {
        0 => out.output.trim().to_string(),
        1 => shown_text(tool, &out.output).trim().to_string(),
        n => format!("{n} lines"),
    }
}

/// How gz's tools start the note on a capped list (`… 40 more files`).
const MORE_NOTE: &str = "… ";

/// The lines of a result the user would count: a command's output without
/// the `exit code N` line gz puts first, a read's numbered file lines
/// without gz's notes, and a capped list with what the cap left out.
fn result_rows(tool: Option<ToolName>, output: &str) -> usize {
    match tool {
        Some(ToolName::ReadFile) => output.lines().filter(|l| is_numbered_row(l)).count(),
        Some(ToolName::ListDirectory | ToolName::FindFiles | ToolName::SearchFiles) => {
            output.lines().map(listed_count).sum()
        }
        _ => shown_text(tool, output).lines().count(),
    }
}

/// `output` without the status line a command's result starts with.
fn shown_text(tool: Option<ToolName>, output: &str) -> &str {
    if tool != Some(ToolName::RunCommand) {
        return output;
    }
    output.split_once('\n').map_or("", |(_, rest)| rest)
}

/// `read_file`'s `    12\tcode` rows.
fn is_numbered_row(line: &str) -> bool {
    line.trim_start()
        .split_once('\t')
        .is_some_and(|(number, _)| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

/// One listed row, or the N of a cap's `… N more files` note.
fn listed_count(line: &str) -> usize {
    line.strip_prefix(MORE_NOTE)
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
        .unwrap_or(1)
}

/// Keeps one resume body under the backend's size limit by trimming the
/// later outputs once the budget is spent. Sizes are the JSON-escaped UTF-8
/// bytes each output takes in the body (as the per-result cap measures
/// them), and the cut note counts against the budget too.
fn fit_to_budget(results: &mut [(String, String, ToolOutput)]) {
    let mut remaining = RESUME_OUTPUT_BUDGET;
    for (_, _, out) in results.iter_mut() {
        let size = tools::json_size(&out.output);
        if size <= remaining {
            remaining -= size;
            continue;
        }
        let note_size = tools::json_size(BUDGET_CUT_NOTE);
        out.output = match remaining.checked_sub(note_size) {
            Some(room) => format!("{}{BUDGET_CUT_NOTE}", json_prefix(&out.output, room)),
            // Not even the note fits; `truncated` still says it was cut.
            None => String::new(),
        };
        out.truncated = true;
        remaining -= tools::json_size(&out.output);
    }
}

/// What ends an output [`fit_to_budget`] cut.
const BUDGET_CUT_NOTE: &str = "\n… output truncated by gz";

/// The longest start of `text` that takes at most `max` bytes in JSON.
fn json_prefix(text: &str, max: usize) -> &str {
    let mut used = 0;
    for (at, c) in text.char_indices() {
        used += tools::json_size(c.encode_utf8(&mut [0; 4]));
        if used > max {
            return &text[..at];
        }
    }
    text
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
#[path = "session_edit_tests.rs"]
mod edit_tests;

#[cfg(all(test, unix))]
#[path = "session_diagnostics_tests.rs"]
mod diagnostics_tests;

#[cfg(test)]
#[path = "session_read_only_tests.rs"]
mod read_only_tests;

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

    /// The longest the stub holds a steer reply for the test to release it.
    const STEER_REPLY_HOLD: std::time::Duration = std::time::Duration::from_secs(10);

    /// A chat server that follows the steering contract
    /// (api/routes/chat_steering.py, run_start.start_chat_run): r1 pauses
    /// for one `run_command` and refuses notes while paused; the resumed r2
    /// starts working as the resume returns, whether or not gz listens, so
    /// its first step takes only notes queued before the resume (none can
    /// be) and hands later ones back unapplied as the turn closes. Steer
    /// replies wait until `release` fires. Returns its address and every
    /// request line in arrival order, plus "replied <path>" once a steer
    /// reply went out.
    fn steering_chat_server(
        release: std::sync::mpsc::Receiver<()>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let release = Arc::new(Mutex::new(release));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = lines.clone();
        let request = json!({
            "type": "client_tool_request",
            "interruptId": "v1:tool_call:tu_1:x",
            "toolUseId": "tu_1",
            "toolName": "run_command",
            "input": {"command": "touch steered.txt"},
        });
        let pause = format!(
            "id: 1\ndata: {request}\n\nid: 2\ndata: {{\"type\":\"run_status\",\"status\":\"interrupted\",\"reason\":\"client_tools\"}}\n\ndata: [DONE]\n\n"
        );
        // Notes r2 accepted, by message.
        let queued: Arc<Mutex<Vec<String>>> = Arc::default();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (seen, queued, pause) = (seen.clone(), queued.clone(), pause.clone());
                let release = release.clone();
                std::thread::spawn(move || {
                    let (line, body) = read_request(&mut stream);
                    seen.lock().unwrap().push(line.clone());
                    let path = line.split_whitespace().nth(1).unwrap_or_default();
                    let path = path.split('?').next().unwrap_or_default().to_string();
                    match path.as_str() {
                        "/chat/runs" => reply(&mut stream, "application/json", r#"{"runId":"r1"}"#),
                        "/chat/runs/r1/events" => reply(&mut stream, "text/event-stream", &pause),
                        "/chat/runs/r1/steer" => {
                            let refused = r#"{"detail":{"error":"run_awaiting_approval","message":"waiting"}}"#;
                            let text = format!(
                                "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{refused}",
                                refused.len()
                            );
                            stream.write_all(text.as_bytes()).ok();
                        }
                        "/chat/runs/r1/client-tools/resume" => {
                            reply(&mut stream, "application/json", r#"{"runId":"r2"}"#)
                        }
                        "/chat/runs/r2/steer" => {
                            let message = body["message"].as_str().unwrap_or_default().to_string();
                            queued.lock().unwrap().push(message);
                            release.lock().unwrap().recv_timeout(STEER_REPLY_HOLD).ok();
                            reply(
                                &mut stream,
                                "application/json",
                                r#"{"accepted":true,"steerId":"steer_1"}"#,
                            );
                            seen.lock().unwrap().push(format!("replied {path}"));
                        }
                        "/chat/runs/r2/events" => {
                            // The run's first step already ran; it ends once
                            // the note gz sends has come in, handing it back.
                            for _ in 0..200 {
                                if !queued.lock().unwrap().is_empty() {
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            }
                            let unapplied = json!({
                                "type": "steer_unapplied",
                                "steers": queued.lock().unwrap().iter()
                                    .map(|m| json!({"steerId": "steer_1", "message": m}))
                                    .collect::<Vec<_>>(),
                            });
                            let events = format!(
                                "id: 9\ndata: {{\"type\":\"llm_chunk\",\"text\":\"The command printed done.\"}}\n\nid: 10\ndata: {unapplied}\n\nid: 11\ndata: {{\"type\":\"run_status\",\"status\":\"completed\"}}\n\ndata: [DONE]\n\n"
                            );
                            reply(&mut stream, "text/event-stream", &events)
                        }
                        _ => reply(&mut stream, "application/json", "{}"),
                    }
                });
            }
        });
        (base, lines)
    }

    /// Approves the command and, like the UI when the user presses Enter
    /// while it waits, queues a note and asks for its delivery at once.
    struct ApproveAndSteer(Steering);

    impl Approver for ApproveAndSteer {
        fn decide<'a>(&'a self, _: &'a PermissionRequest) -> DecisionFuture<'a> {
            Box::pin(async move {
                self.0.queue("Now reply with just the word MARK5");
                assert!(self.0.deliver().await.is_empty(), "the run is paused");
                assert_eq!(self.0.deliverable(), 1, "it goes once the tool is done");
                Decision::AllowOnce
            })
        }
    }

    #[tokio::test]
    async fn a_note_on_its_way_to_the_resumed_run_does_not_hold_back_the_turn() {
        let (release, held) = std::sync::mpsc::channel();
        let (base, lines) = steering_chat_server(held);
        let dir = std::env::temp_dir().join(format!("gz-session-steer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        let steering = session.steering();
        let approver = ApproveAndSteer(steering.clone());
        let mut steer_events = Vec::new();
        // Like the UI: each time the run takes notes, deliver them on a
        // task of their own.
        let mut deliveries = Vec::new();
        let mut on_event = |event| match event {
            AssistantEvent::RunTakesNotes => {
                let steering = steering.clone();
                deliveries.push(tokio::spawn(async move { steering.deliver().await }));
            }
            AssistantEvent::SteerApplied(note) => {
                steer_events.push(format!("read {}", note.steer_id))
            }
            AssistantEvent::SteerUnapplied(notes) => {
                steer_events.push(format!("unapplied {}", notes.join(",")))
            }
            _ => {}
        };
        let turn = session.send("sleep then echo", &approver, &mut on_event);
        let end = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
            .await
            .expect("the turn ends while the note's reply is held")
            .unwrap();
        assert_eq!(end, TurnEnd::Completed);
        let replied = |lines: &Arc<Mutex<Vec<String>>>| {
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l == "replied /chat/runs/r2/steer")
        };
        assert!(!replied(&lines), "the turn did not wait for the reply");
        release.send(()).unwrap();
        let mut sent = Vec::new();
        for delivery in deliveries {
            sent.extend(delivery.await.unwrap());
        }
        // The resumed run's first step began before any note could reach
        // it, so the server hands the note back: it goes out once, as the
        // next message, through the UI's unread-note requeue.
        let sent: Vec<String> = sent.into_iter().map(|n| n.text).collect();
        assert_eq!(sent, vec!["Now reply with just the word MARK5"]);
        assert_eq!(
            steer_events,
            vec!["unapplied Now reply with just the word MARK5"]
        );
        let lines = lines.lock().unwrap().clone();
        let paths: Vec<&str> = lines
            .iter()
            .map(|l| l.split_whitespace().nth(1).unwrap_or_default())
            .map(|p| p.split('?').next().unwrap_or_default())
            .collect();
        assert!(!paths.contains(&"/chat/runs/r1/steer"), "{paths:?}");
        let at = |path: &str| paths.iter().position(|p| *p == path).unwrap();
        assert!(at("/chat/runs/r1/client-tools/resume") < at("/chat/runs/r2/steer"));
        assert!(
            !crate::error_log::test_log::urls()
                .iter()
                .any(|url| url.contains("/steer")),
            "a paused run's refusal is not an error"
        );
        assert!(steering.pending().is_empty());
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

    /// Approves everything, answers questions and plans as scripted, and
    /// keeps a plan mode switch.
    struct Scripted {
        plan_mode: Mutex<bool>,
        reply: crate::assistant::questions::Reply,
    }

    impl Approver for Scripted {
        fn decide<'a>(&'a self, _: &'a PermissionRequest) -> DecisionFuture<'a> {
            Box::pin(async { Decision::AllowOnce })
        }
        fn ask<'a>(&'a self, _: &'a Query) -> crate::assistant::permissions::ReplyFuture<'a> {
            let reply = self.reply.clone();
            Box::pin(async move { reply })
        }
        fn in_plan_mode(&self) -> bool {
            *self.plan_mode.lock().unwrap()
        }
        fn enter_plan_mode(&self) -> bool {
            *self.plan_mode.lock().unwrap() = true;
            true
        }
    }

    fn scripted(reply: crate::assistant::questions::Reply) -> Scripted {
        Scripted {
            plan_mode: Mutex::new(false),
            reply,
        }
    }

    fn every_tool() -> Vec<String> {
        crate::assistant::session_tools::SessionTool::ALL
            .iter()
            .map(|t| t.as_str())
            .chain(["notebook_edit", "web_fetch", "read_file"])
            .map(str::to_string)
            .collect()
    }

    async fn call(
        session: &mut Session,
        approver: &dyn Approver,
        tool: &str,
        input: Value,
    ) -> ToolOutput {
        session
            .answer_one("run_1", tool, &input, None, approver, &mut |_| {})
            .await
    }

    #[test]
    fn newer_tools_are_offered_only_when_the_server_lists_them() {
        let mut session = offline_session("offer");
        assert_eq!(session.client_tools().len(), ToolName::ALL.len());
        session.set_server_tools(Some(every_tool()));
        assert!(
            !session.client_tools().contains(&"command_output"),
            "nobody hears their end yet"
        );
        assert!(session.client_tools().contains(&"ask_user_question"));
        session.take_command_notices();
        assert!(session.client_tools().contains(&"command_output"));
        let main = offline_session("offer-main");
        let mut child =
            offline_session("offer-child").into_subagent("general", "Look", main.shared_policy());
        child.set_server_tools(Some(every_tool()));
        let tools = child.client_tools();
        assert!(tools.contains(&"web_fetch") && !tools.contains(&"ask_user_question"));
    }

    #[tokio::test]
    async fn questions_and_plans_go_to_the_user_and_back_to_the_model() {
        use crate::assistant::questions::{Reply, WorkMode};
        let mut session = offline_session("questions");
        session.set_server_tools(Some(every_tool()));
        let asked = json!({"questions": [{"question": "Which db?", "header": "DB",
            "options": [{"label": "Postgres"}, {"label": "SQLite"}]}]});
        let answers = scripted(Reply::Answers(vec![Some("SQLite".into())]));
        let out = call(&mut session, &answers, "ask_user_question", asked).await;
        assert_eq!(out.output, "Which db? → SQLite");

        let approver = scripted(Reply::PlanApproved(WorkMode::AcceptEdits));
        let early = call(
            &mut session,
            &approver,
            "exit_plan_mode",
            json!({"plan": "p"}),
        )
        .await;
        assert!(early.output.contains("not on"), "{}", early.output);
        call(&mut session, &approver, "enter_plan_mode", json!({})).await;
        assert!(approver.in_plan_mode());
        assert!(session
            .with_mode_note("hi", &approver)
            .ends_with("call exit_plan_mode with your plan.</gz_mode>"));
        let done = call(
            &mut session,
            &approver,
            "exit_plan_mode",
            json!({"plan": "1. Do it"}),
        )
        .await;
        assert!(done.output.contains("approved the plan"), "{}", done.output);
    }

    #[test]
    fn without_exit_plan_mode_the_plan_is_the_answer() {
        let session = offline_session("plan-reply");
        let approver = scripted(crate::assistant::questions::Reply::NoAnswer);
        assert_eq!(session.with_mode_note("hi", &approver), "hi");
        approver.enter_plan_mode();
        assert!(session
            .with_mode_note("hi", &approver)
            .ends_with("switches modes to carry it out.</gz_mode>"));
    }

    #[tokio::test]
    async fn background_commands_start_report_and_read_back() {
        let mut session = offline_session("bg");
        session.set_server_tools(Some(every_tool()));
        let mut notices = session.take_command_notices().unwrap();
        let approver = scripted(crate::assistant::questions::Reply::NoAnswer);
        let started = call(
            &mut session,
            &approver,
            "run_command",
            json!({"command": "echo done", "run_in_background": true}),
        )
        .await;
        assert!(
            started
                .output
                .starts_with("Started background command bg_1"),
            "{}",
            started.output
        );
        let finished = tokio::time::timeout(std::time::Duration::from_secs(5), notices.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(finished.exit, "0");
        let out = call(
            &mut session,
            &approver,
            "command_output",
            json!({"id": "bg_1"}),
        )
        .await;
        assert_eq!(out.output, "exit code 0\ndone\n");
    }

    #[tokio::test]
    async fn an_older_server_keeps_search_results_as_lines() {
        let mut session = offline_session("old-search");
        std::fs::write(session.workspace().join("a.txt"), "needle\n").unwrap();
        let approver = ApproveAll;
        let input = json!({"pattern": "needle"});
        let old = call(&mut session, &approver, "search_files", input.clone()).await;
        assert_eq!(old.output, "a.txt:1: needle");
        session.set_server_tools(Some(every_tool()));
        let new = call(&mut session, &approver, "search_files", input).await;
        assert_eq!(new.output, "a.txt");
    }

    #[test]
    fn documents_stay_with_the_chat_they_were_attached_in() {
        let mut session = offline_session("documents");
        session.continue_conversation("chat-a");
        session.keep_documents(vec!["doc-1".into()]);
        assert_eq!(session.start_body("hi")["docIds"], json!(["doc-1"]));
        session.continue_conversation("chat-a");
        assert_eq!(session.start_body("hi")["docIds"], json!(["doc-1"]));
        session.continue_conversation("chat-b");
        let body = session.start_body("hi");
        assert!(body.get("docIds").is_none());
        assert_eq!(body["useRag"], json!(false));
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

    #[tokio::test]
    async fn load_skill_is_offered_indexed_and_served_only_with_local_context() {
        let mut session = offline_session("skills");
        let home = crate::skills::test_support::home("session");
        crate::skills::test_support::write_skill(
            &home.skills_dir(),
            "pdf",
            "PDF forms",
            "# Use me\n",
        );
        let (allowed, _) = crate::assistant::local_context::tests::recording_folders(Vec::new());
        let mut context = LocalContext::new(None, None, allowed);
        context.skills = Some(home);
        session.context = context;
        let offered = |s: &Session| {
            s.start_body("hi")["clientTools"]
                .as_array()
                .unwrap()
                .contains(&json!("load_skill"))
        };
        assert!(!offered(&session));
        assert!(session.mentionable_skills().is_empty());
        let refused = session
            .answer_one(
                "r",
                "load_skill",
                &json!({"name": "pdf"}),
                None,
                &ApproveAll,
                &mut |_| {},
            )
            .await;
        assert!(refused.is_error, "not offered, not served");

        session.set_local_context(Some(Limits::default()));
        assert!(offered(&session));
        let body = session.start_body("hi");
        assert_eq!(
            body["clientContext"]["skills"],
            json!([{"name": "pdf", "description": "PDF forms"}])
        );
        assert_eq!(session.mentionable_skills(), vec!["pdf"]);
        let mut events = Vec::new();
        let out = session
            .answer_one(
                "r",
                "load_skill",
                &json!({"name": "pdf"}),
                None,
                &ApproveAll,
                &mut |e| events.push(e),
            )
            .await;
        assert_eq!(out.output, "# Use me");
        assert!(events.iter().any(
            |e| matches!(e, AssistantEvent::ToolStarted { summary, .. } if summary == "load_skill pdf")
        ));
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
        let size = crate::assistant::tools::MAX_OUTPUT_BYTES;
        let fit = RESUME_OUTPUT_BUDGET / size;
        let mut results: Vec<_> = (0..=fit + 1).map(|_| big(size)).collect();
        fit_to_budget(&mut results);
        assert!(results[..fit].iter().all(|r| !r.2.truncated));
        assert!(results[fit].2.truncated);
        assert!(results[fit].2.output.ends_with(BUDGET_CUT_NOTE));
        assert!(results[fit + 1].2.truncated);
        assert!(body_bytes(&results) <= RESUME_OUTPUT_BUDGET);
    }

    /// The bytes the outputs take inside the resume body's JSON strings.
    fn body_bytes(results: &[(String, String, ToolOutput)]) -> usize {
        results
            .iter()
            .map(|r| serde_json::to_string(&r.2.output).unwrap().len() - 2)
            .sum()
    }

    #[test]
    fn budget_counts_escaped_multibyte_bytes_and_keeps_the_note_inside() {
        // "é€\n" takes 7 bytes in the body but counts as 3 characters, so
        // counting characters would let this batch run far over.
        let capped = ToolOutput::ok("é€\n".repeat(tools::MAX_OUTPUT_BYTES));
        let each = tools::json_size(&capped.output);
        let fit = RESUME_OUTPUT_BUDGET / each;
        let call = || {
            (
                "i".to_string(),
                "read_file".to_string(),
                ToolOutput {
                    truncated: false,
                    ..capped.clone()
                },
            )
        };
        let mut results: Vec<_> = (0..fit + 2).map(|_| call()).collect();
        fit_to_budget(&mut results);
        assert!(results[..fit].iter().all(|r| !r.2.truncated));
        assert!(results[fit].2.output.ends_with(BUDGET_CUT_NOTE));
        assert!(results[fit + 1].2.truncated);
        assert!(
            body_bytes(&results) <= RESUME_OUTPUT_BUDGET,
            "{}",
            body_bytes(&results)
        );
        assert!(body_bytes(&results) > RESUME_OUTPUT_BUDGET - 8);
    }

    #[test]
    fn summarises_results_in_one_line() {
        let detail = |tool: &str, text: &str| result_detail(tool, &ToolOutput::ok(text.into()));
        assert_eq!(detail("mcp__x__y", "a\nb\nc"), "3 lines");
        assert_eq!(detail("mcp__x__y", "a\nb\nc\n"), "3 lines");
        assert_eq!(detail("write_file", "wrote a.rs"), "wrote a.rs");
        assert_eq!(
            result_detail("run_command", &ToolOutput::error("exit code 1\nboom")),
            "exit code 1"
        );
    }

    #[test]
    fn a_commands_line_count_leaves_out_its_exit_code_line() {
        let detail = |text: &str| result_detail("run_command", &ToolOutput::ok(text.into()));
        assert_eq!(detail("exit code 0\n4878b96 init\n"), "4878b96 init");
        let ls = "exit code 0\ntotal 8\n.\n..\n.git\na\nb\n";
        assert_eq!(detail(ls), "6 lines");
        assert_eq!(detail("exit code 0\n"), "exit code 0");
    }

    #[test]
    fn a_reads_line_count_leaves_out_gzs_notes() {
        let detail = |text: &str| result_detail("read_file", &ToolOutput::ok(text.into()));
        let rows = "     1\tone\n     2\ttwo\n";
        assert_eq!(detail(rows), "2 lines");
        assert_eq!(
            detail(&format!(
                "{rows}… 40 more lines; read again with offset 3\n"
            )),
            "2 lines"
        );
        assert_eq!(
            detail("     7\tonly\n"),
            "1 lines",
            "shown as \"Read 1 line\""
        );
        assert_eq!(detail("(empty file)"), "(empty file)");
    }

    #[test]
    fn a_capped_lists_count_includes_what_the_cap_left_out() {
        let detail = |tool: &str, text: &str| result_detail(tool, &ToolOutput::ok(text.into()));
        assert_eq!(
            detail("find_files", "a.rs\nb.rs\n… 3 more files"),
            "5 lines"
        );
        assert_eq!(detail("list_directory", "a.rs\nsrc/"), "2 lines");
        assert_eq!(detail("list_directory", "a.rs"), "a.rs");
        assert_eq!(detail("run_script", "one\ntwo\n"), "2 lines");
    }

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(default_title("fix the build\nplease"), "fix the build");
    }

    /// A session in a temp folder with `hooks_toml` as the user's hooks;
    /// returns the folder too.
    fn hooked_session(name: &str, hooks_toml: &str) -> (PathBuf, Session) {
        let (dir, hooks) = crate::assistant::hooks::test_support::user_hooks(name, hooks_toml);
        let mut session = offline_session(name);
        session.sandbox = Sandbox::new(&dir).unwrap();
        session.set_hooks(hooks);
        (dir, session)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_before_tool_exit_2_stops_the_call_and_tells_the_model_why() {
        let (dir, mut session) = hooked_session(
            "hook-block",
            "[[before_tool]]\ntools = [\"run_command\"]\ncommand = \"echo 'use the make target' >&2; exit 2\"",
        );
        let out = call(
            &mut session,
            &ApproveAll,
            "run_command",
            json!({"command": "touch made"}),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(
            out.output,
            "The user's before_tool hook blocked this call: use the make target"
        );
        assert!(!dir.join("made").exists(), "the command never ran");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn before_tool_runs_before_the_prompt_and_after_tool_only_for_a_run_call() {
        let (dir, mut session) = hooked_session(
            "hook-denied",
            "[[before_tool]]\ncommand = \"touch before\"\n[[after_tool]]\ncommand = \"touch after\"",
        );
        let input = json!({"path": "a.txt", "content": "x"});
        let denied = Answering::new(Decision::Deny);
        let out = call(&mut session, &denied, "write_file", input.clone()).await;
        assert!(out.is_error);
        assert!(
            dir.join("before").exists(),
            "before_tool ran before the prompt"
        );
        assert!(
            !dir.join("after").exists(),
            "no after_tool for a denied call"
        );
        call(&mut session, &ApproveAll, "write_file", input).await;
        assert!(dir.join("after").exists(), "an allowed call runs it");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_after_tool_exit_2_reaches_the_result_the_model_gets() {
        let (dir, mut session) = hooked_session(
            "hook-after",
            "[[after_tool]]\npath = \"*.rs\"\ncommand = \"echo \\\"lint failed in $(basename $GZ_HOOK_PATH)\\\" >&2; exit 2\"",
        );
        let out = call(
            &mut session,
            &ApproveAll,
            "write_file",
            json!({"path": "src/lib.rs", "content": "fn x() {}"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.output);
        assert!(
            out.output
                .ends_with("\n\n[after_tool hook]\nlint failed in lib.rs"),
            "{}",
            out.output
        );
        assert!(dir.join("src/lib.rs").exists(), "the write happened first");
        let other = call(
            &mut session,
            &ApproveAll,
            "write_file",
            json!({"path": "notes.md", "content": "x"}),
        )
        .await;
        assert!(
            !other.output.contains("after_tool hook"),
            "glob didn't match"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn script_helper_calls_run_hooks_too() {
        if !std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            eprintln!("skipped: python3 not installed");
            return;
        }
        let (_dir, mut session) = hooked_session(
            "hook-script",
            "[[before_tool]]\ntools = [\"list_directory\"]\ncommand = \"echo 'no listing' >&2; exit 2\"\n[[after_tool]]\ntools = [\"read_file\"]\ncommand = \"echo 'read noted' >&2; exit 2\"",
        );
        std::fs::write(session.workspace().join("a.txt"), "hi").unwrap();
        let code = "import gz\nprint(gz.read_file('a.txt')[:0], end='')\ntry:\n    gz.list_directory('.')\nexcept Exception as e:\n    print('raised:', e)\n";
        let out = call(
            &mut session,
            &ApproveAll,
            "run_script",
            json!({"language": "python", "code": code}),
        )
        .await;
        assert!(
            out.output.contains("raised:") && out.output.contains("no listing"),
            "{}",
            out.output
        );
        assert!(
            out.output.ends_with("[after_tool hook]\nread noted"),
            "{}",
            out.output
        );
    }

    #[test]
    fn resuming_another_conversation_forgets_instructions_picked_up_on_demand() {
        let mut session = offline_session("resume-on-demand");
        let start = session.sandbox.start();
        std::fs::create_dir_all(start.join("sub")).unwrap();
        std::fs::write(start.join("sub/GZ.md"), "sub rules").unwrap();
        std::fs::write(start.join("sub/x.txt"), "x").unwrap();
        let picked_up = |session: &Session| {
            session
                .context
                .instruction_report(&session.sandbox, &Limits::default())
                .sent
                .iter()
                .any(|entry| entry.text == "sub rules")
        };
        session.context.note_tool_call(
            &session.sandbox,
            "read_file",
            &json!({"path": "sub/x.txt"}),
        );
        assert!(picked_up(&session));
        let same = session.conversation_id.clone();
        session.continue_conversation(&same);
        assert!(picked_up(&session), "the same conversation keeps them");
        session.continue_conversation("another-conversation");
        assert!(
            !picked_up(&session),
            "another conversation starts without them"
        );
    }
}
