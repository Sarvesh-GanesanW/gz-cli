//! The interactive `gz code` state machine, free of terminal I/O: it turns
//! session events into finished transcript items (printed once into
//! scrollback) and live-region state (streaming text, running tools,
//! spinner, input box, permission prompt), and turns keys into effects.

mod attachments;
mod auto;
mod background;
mod commands;
mod compaction;
mod context;
mod export;
mod grant;
mod hooks_cmd;
mod keys;
mod mcp;
mod pickers;
mod price_tier;
mod prompt;
mod questions;
mod queue;
mod rewind;
mod sandbox_cmd;
mod shells;
mod spend;
mod subagents;
#[cfg(test)]
pub(crate) mod test_support;
mod turn;

pub use commands::SHORTCUTS;
pub use context::FolderCommand;
pub use export::ExportTarget;
pub use mcp::{run as mcp_run, McpCommand};
pub use pickers::{Picker, PickerRow};
pub use price_tier::{PriceOffer, PriceTier};
pub use prompt::PromptView;
pub use questions::{QueryOption, QueryView};
pub use queue::Waiting;
pub use rewind::RewindRow;
pub use shells::Shells;
pub use spend::{CapChoice, SpendState};

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tui_textarea::TextArea;

use crate::assistant::audit::AuditLog;
use crate::assistant::auto_rules::AutoStore;
use crate::assistant::background::BackgroundRun;
use crate::assistant::memory::MemoryStore;
use crate::assistant::models::{Catalog, Selection};
use crate::assistant::sandbox::Sandbox;
use crate::assistant::steering::{SentNote, Steering};
use crate::assistant::subagent_settings::SubagentSettings;
use crate::code_ui::agent_pages::{AgentPages, AgentView};
use crate::code_ui::approver::{Ask, Mode, UiQuery};
use crate::code_ui::auto_mode::AutoContext;
use crate::code_ui::clipboard::{ImageClipboard, SystemClipboard};
use crate::code_ui::items::{Item, ToolItem};
use crate::code_ui::mentions::FileIndex;
use crate::code_ui::transcript::Viewer;
use crate::code_ui::usage::UsageBook;
use crate::subagents::Board;

/// A second Ctrl+C within this window exits.
const EXIT_WINDOW: Duration = Duration::from_secs(2);
pub const SCRIPT_CHILD_PREFIX: &str = "script · ";

/// What the driver must do after a key or event.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Send(String),
    Cancel,
    NewConversation,
    ClearScreen,
    Exit,
    /// Use this model from the next turn on (and remember it).
    SetModel(Selection),
    /// Fetch the account's usage (`/chat/metrics`, `/chat/budget-status`).
    FetchAccountUsage,
    /// List saved rules and chat approvals; with an id, revoke it first.
    Permissions(Option<String>),
    /// Save this rule, so gz stops asking about what it covers
    /// (`/permissions allow`).
    Grant(crate::assistant::saved_rules::SavedRule),
    /// Let the chat start subagents from the next turn on, or not (and
    /// remember it).
    SetSubagents(bool),
    /// Load earlier conversations for the `/resume` picker.
    ListConversations,
    /// Auto-compact at this many tokens from the next turn on (`None`: the
    /// server default), and remember it.
    SetAutoCompact(Option<u64>),
    /// Keep conversations on a two-price model this way from the next turn
    /// on, and remember it.
    SetPriceTier(PriceTier),
    /// Compact the conversation now, optionally focused by instructions.
    Compact(Option<String>),
    /// Hand the messages typed while gz works to the running turn.
    Steer,
    /// Run this subagent in the background for the chat.
    StartSubagent(BackgroundRun),
    /// Use these subagent settings from the next turn on (and remember them).
    SetSubagentSettings(SubagentSettings),
    /// Stop every background subagent (`/subagents stop`).
    StopSubagents,
    /// Hand these subagent reports to the main agent as a turn.
    SendReports(String),
    /// Continue this conversation and print its earlier messages.
    Resume {
        id: String,
        title: String,
    },
    /// Change, add or remove a folder (`/cd`, `/add-dir`, `/dirs remove`).
    Folder(FolderCommand),
    /// Offer the memory tool and send memories, or not (and remember it).
    SetMemory(bool),
    /// Open the personal instructions file in the user's editor.
    EditInstructions(PathBuf),
    /// A `/skills` action.
    Skills(crate::code_ui::skills::SkillsAction),
    /// Fetch the server budget (`/chat/budget-status`).
    FetchBudget,
    /// Use these spending caps from now on (and remember them).
    SetCaps(crate::spend::caps::Caps),
    /// Save or copy the conversation as Markdown (`/export`).
    Export(ExportTarget),
    /// Fetch a resumed conversation's whole history, then export it.
    ExportWhole(ExportTarget),
    /// Show the project hooks file and ask the user to trust it.
    TrustHooks,
    /// List the local MCP servers, or restart one (`/mcp`).
    Mcp(McpCommand),
}

pub struct UiState {
    /// Finished items the driver prints into scrollback, then clears.
    pub flush: Vec<Item>,
    pub streaming: String,
    pub running: Vec<ToolItem>,
    /// Finished read-only calls not yet printed, folded together on flush.
    pub group: Vec<ToolItem>,
    pub busy_since: Option<Instant>,
    pub received_chars: usize,
    pub input: TextArea<'static>,
    history: Vec<String>,
    history_pos: Option<usize>,
    pub queue: VecDeque<String>,
    pub mode: Arc<Mutex<Mode>>,
    pub prompt: Option<PromptView>,
    /// Prompts waiting while another is open (the main agent and
    /// background subagents can ask at the same time).
    pending_prompts: VecDeque<Ask>,
    pub show_shortcuts: bool,
    exit_armed: Option<Instant>,
    pub tick: u64,
    pub turns: usize,
    pub catalog: Option<Catalog>,
    /// True while the model list is still loading in the background.
    pub models_loading: bool,
    /// `/usage` is waiting for the account's numbers.
    pub account_loading: bool,
    /// The chosen model; `None` uses the server default.
    pub model: Option<Selection>,
    pub picker: Option<Picker>,
    /// Highlighted row of the slash-command menu.
    pub menu_selected: usize,
    /// The workspace, for `@` mentions; `None` turns mentions off.
    pub sandbox: Option<Sandbox>,
    /// Workspace files for the `@` popup, scanned on the first `@`.
    files: Option<FileIndex>,
    /// Highlighted row of the `@` popup.
    pub mention_selected: usize,
    /// Skills an `@name` may ask for (empty when the server can't load
    /// skills); also offered in the `@` popup.
    pub skill_names: Vec<String>,
    /// Images pasted into the input, shown as `[Image #N]` chips.
    pub images: Vec<PathBuf>,
    clipboard: Box<dyn ImageClipboard>,
    /// Every item of the session with full detail, for the Ctrl+O view.
    pub transcript: Vec<Item>,
    /// The open Ctrl+O view, if any.
    pub viewer: Option<Viewer>,
    /// Width the transcript was last drawn at (search works on those lines).
    pub transcript_width: usize,
    /// True once part of the current answer is printed into scrollback.
    answer_started: bool,
    /// The current answer in full, for the transcript.
    answer_full: String,
    pub usage: UsageBook,
    /// This turn's subagents; running ones show live, finished ones print.
    pub subagents: Board,
    /// Whether the chat may start subagents (`/subagents on|off`).
    pub subagents_enabled: bool,
    /// The user's subagent model, context and limits (`/subagents model …`).
    pub subagent_settings: SubagentSettings,
    /// Background subagents waiting for a slot (`/subagents parallel`).
    pub queued: VecDeque<BackgroundRun>,
    /// Where the chat auto-compacts, in tokens (`/autocompact`); `None` is
    /// the server default.
    pub auto_compact: Option<u64>,
    /// The user's choice for models that cost more past a size; `None`
    /// until gz has asked.
    pub price_tier: Option<PriceTier>,
    /// The message waiting for that choice.
    price_held: Option<Effect>,
    /// `/compact` is waiting for the server; messages typed meanwhile queue.
    pub compacting: bool,
    /// One page per subagent of this conversation.
    pub pages: AgentPages,
    /// The subagent page on screen; `None` shows the main page.
    pub agent_view: Option<AgentView>,
    /// Keys typed while a permission prompt was open, restored after it.
    typed_ahead: String,
    /// What auto mode remembers, shared with the approver; `None` when the
    /// session runs without auto mode.
    pub auto: Option<Arc<Mutex<AutoContext>>>,
    /// The user's auto mode rules, for `/auto`.
    pub auto_store: Option<AutoStore>,
    /// Messages for the running turn; `None` without live steering.
    pub steering: Option<Steering>,
    /// Notes the run took that the model hasn't read yet.
    pub sent_notes: Vec<SentNote>,
    /// Ctrl+X was pressed: Ctrl+S next sends now.
    ctrl_x_armed: bool,
    /// Background subagents still running, by id; they outlive turns.
    pub background: HashSet<String>,
    /// Subagent reports waiting for the main agent's next turn.
    pub reports: VecDeque<String>,
    /// The model's task list (`todo_write`), shown while items are open.
    pub todos: Vec<crate::assistant::todos::Todo>,
    /// The model's question or plan on screen (after any permission prompt).
    pub query: Option<QueryView>,
    pending_queries: VecDeque<UiQuery>,
    /// The saved memories, for `/memory`; `None` without a config folder.
    pub memory: Option<MemoryStore>,
    /// Whether memory is on (`/memory on|off`).
    pub memory_on: bool,
    /// The personal instructions file, for `/instructions`.
    pub user_instructions: Option<PathBuf>,
    /// Where `/memory forget|clear` are recorded.
    pub audit: Option<AuditLog>,
    /// Spend, caps and the server budget.
    pub spend: SpendState,
    /// The session's background commands and the user's own `!command`s.
    pub shells: Shells,
    /// Where the conversation on screen starts, its title and models, for
    /// `/export`.
    conversation: export::ConversationLog,
    /// What gz's tools changed in this conversation, for `/rewind` and
    /// `/diff`; `None` without one.
    pub edit_history: Option<crate::assistant::checkpoints::EditHistory>,
    /// Rewinds the model hears about with the user's next message.
    rewound: Vec<crate::assistant::checkpoints::Rewound>,
    /// The session's hooks, for `/hooks`; `None` until the session hands
    /// them over.
    pub hooks: Option<crate::assistant::hooks::Hooks>,
    /// The user's local MCP servers, for `/mcp`.
    pub local_mcp: Option<crate::assistant::mcp_local::LocalMcp>,
    /// The OS sandbox settings for the model's commands (`/sandbox`).
    pub os_sandbox: Option<crate::assistant::os_sandbox::OsSandbox>,
}

pub fn new_input() -> TextArea<'static> {
    let mut input = TextArea::default();
    input.set_placeholder_text("Ask gz to read, explain, or change this folder…");
    input.set_cursor_line_style(ratatui::style::Style::default());
    input
}

impl UiState {
    pub fn new(mode: Arc<Mutex<Mode>>) -> Self {
        Self {
            flush: Vec::new(),
            streaming: String::new(),
            running: Vec::new(),
            group: Vec::new(),
            busy_since: None,
            received_chars: 0,
            input: new_input(),
            history: Vec::new(),
            history_pos: None,
            queue: VecDeque::new(),
            mode,
            prompt: None,
            pending_prompts: VecDeque::new(),
            show_shortcuts: false,
            exit_armed: None,
            tick: 0,
            turns: 0,
            catalog: None,
            models_loading: false,
            account_loading: false,
            model: None,
            picker: None,
            menu_selected: 0,
            transcript: Vec::new(),
            viewer: None,
            pages: AgentPages::default(),
            agent_view: None,
            transcript_width: 80,
            answer_started: false,
            answer_full: String::new(),
            usage: UsageBook::default(),
            subagents: Board::default(),
            subagents_enabled: true,
            subagent_settings: SubagentSettings::default(),
            queued: VecDeque::new(),
            auto_compact: None,
            price_tier: None,
            price_held: None,
            compacting: false,
            typed_ahead: String::new(),
            sandbox: None,
            files: None,
            mention_selected: 0,
            skill_names: Vec::new(),
            images: Vec::new(),
            clipboard: Box::new(SystemClipboard),
            auto: None,
            auto_store: None,
            steering: None,
            sent_notes: Vec::new(),
            ctrl_x_armed: false,
            background: HashSet::new(),
            reports: VecDeque::new(),
            todos: Vec::new(),
            query: None,
            pending_queries: VecDeque::new(),
            memory: None,
            memory_on: true,
            user_instructions: None,
            audit: None,
            spend: SpendState::new(Default::default(), None),
            shells: Shells::default(),
            conversation: export::ConversationLog::default(),
            edit_history: None,
            rewound: Vec::new(),
            hooks: None,
            local_mcp: None,
            os_sandbox: None,
        }
    }

    /// Prints `item` into scrollback and keeps it for the transcript.
    pub fn record(&mut self, item: Item) {
        self.transcript.push(item.clone());
        self.flush.push(item);
    }

    /// What answers next, for the banner and mode line.
    pub fn model_label(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| self.catalog.as_ref()?.default_selection())
            .map(|s| s.label())
    }

    /// Whether Shift+Tab offers auto mode: the session has it and the
    /// server reviews actions.
    pub fn auto_available(&self) -> bool {
        self.auto.is_some() && self.catalog.as_ref().is_some_and(|c| c.action_review)
    }

    /// The server's models and features; auto mode learns whether its
    /// review reads recent blocks.
    pub fn set_catalog(&mut self, catalog: Catalog) {
        let takes_blocks = catalog.action_review_authorizations;
        self.with_auto(|auto| auto.set_review_takes_blocks(takes_blocks));
        self.catalog = Some(catalog);
    }

    /// Runs `change` on auto mode's context, when the session has one.
    pub(super) fn with_auto(&self, change: impl FnOnce(&mut AutoContext)) {
        if let Some(mut context) = self.auto.as_ref().and_then(|a| a.lock().ok()) {
            change(&mut context);
        }
    }

    /// The current folder when the session moved away from the one it
    /// started in (the banner shows that one), for the status line.
    pub fn moved_folder(&self) -> Option<String> {
        let sandbox = self.sandbox.as_ref()?;
        let cwd = sandbox.cwd();
        (cwd != sandbox.start()).then(|| sandbox.display_folder(&cwd))
    }

    pub fn mode(&self) -> Mode {
        self.mode.lock().map(|m| *m).unwrap_or_default()
    }

    pub fn is_busy(&self) -> bool {
        self.busy_since.is_some()
    }

    pub fn exit_armed(&self) -> bool {
        self.exit_armed.is_some_and(|at| at.elapsed() < EXIT_WINDOW)
    }

    pub(super) fn cycle_mode(&mut self) {
        if let Ok(mut mode) = self.mode.lock() {
            *mode = mode.next(self.auto_available());
        }
    }
}
