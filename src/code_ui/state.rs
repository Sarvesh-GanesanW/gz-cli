//! The interactive `gz code` state machine, free of terminal I/O: it turns
//! session events into finished transcript items (printed once into
//! scrollback) and live-region state (streaming text, running tools,
//! spinner, input box, permission prompt), and turns keys into effects.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_textarea::TextArea;

use crate::assistant::attachments::{self, PreparedTurn};
use crate::assistant::history::Transcript;
use crate::assistant::models::{Catalog, Model, Selection};
use crate::assistant::sandbox::Sandbox;
use crate::assistant::tools::Access;
use crate::assistant::{AssistantEvent, Decision, PermissionRequest, TurnEnd};
use crate::code_ui::agent_pages::{AgentPages, AgentView};
use crate::code_ui::approver::{Answer, Ask, Mode};
use crate::code_ui::blocks;
use crate::code_ui::clipboard::{ImageClipboard, SystemClipboard};
use crate::code_ui::items::{Item, ToolItem, ToolStatus};
use crate::code_ui::labels::{self, Label};
use crate::code_ui::mentions::{self, FileIndex};
use crate::code_ui::panel::{Panel, Row};
use crate::code_ui::resume;
use crate::code_ui::transcript::{self, Viewer, ViewerAction};
use crate::code_ui::usage::{self, UsageBook};
use crate::code_ui::PermissionsReply;
use crate::subagents::{Board, Status as SubagentStatus, Subagent, SubagentEvent};

/// A second Ctrl+C within this window exits.
const EXIT_WINDOW: Duration = Duration::from_secs(2);
pub const SCRIPT_CHILD_PREFIX: &str = "script · ";

/// Every slash command with its one-line description: the source for both
/// `/help` and the menu that opens when the input starts with `/`.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show this help"),
    ("/new", "start a new conversation"),
    ("/clear", "new conversation and a clean screen"),
    ("/resume", "continue an earlier conversation"),
    (
        "/models",
        "choose the model (Groundzero modes or your LLM connections)",
    ),
    (
        "/effort",
        "choose the reasoning effort for the current model",
    ),
    ("/mode", "cycle permission mode (also shift+tab)"),
    (
        "/usage",
        "context fill, this session's tokens, cost, and tools, plus your account's usage",
    ),
    ("/last", "full analytics of the previous turn"),
    (
        "/permissions",
        "what gz may do without asking; /permissions revoke <id>",
    ),
    (
        "/subagents",
        "helper agents the chat can start; /subagents on|off",
    ),
    ("/exit", "quit (also ctrl+d, or ctrl+c twice)"),
];

fn help_panel() -> Panel {
    let rows = COMMANDS
        .iter()
        .map(|(name, what)| Row::entry(name, *what))
        .collect();
    Panel::new("Commands")
        .aside("gz code")
        .section(None, rows)
        .footer("? for keyboard shortcuts · @ to attach files · esc to interrupt")
}

pub const SHORTCUTS: &[&str] = &[
    "enter send · shift/alt+enter newline · ↑↓ history",
    "esc interrupt · ctrl+u clear input · ctrl+c ctrl+c exit",
    "shift+tab cycle mode: ask → accept edits → plan (read-only)",
    "ctrl+o transcript: every step, fully expanded (esc to return)",
    "shift+↓ / shift+↑ switch between main and each subagent's page",
    "/help commands",
];

/// What the driver must do after a key or event.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Let the chat start subagents from the next turn on, or not (and
    /// remember it).
    SetSubagents(bool),
    /// Load earlier conversations for the `/resume` picker.
    ListConversations,
    /// Continue this conversation and print its earlier messages.
    Resume {
        id: String,
        title: String,
    },
}

/// One row of the model or effort picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerRow {
    Header(String),
    Model(Model),
    Effort(String),
    /// An earlier conversation in the `/resume` list.
    Conversation {
        id: String,
        title: String,
        /// Dim text after the title, e.g. `2h ago`.
        detail: String,
    },
}

/// The `/models`, `/effort` and `/resume` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    pub title: String,
    pub rows: Vec<PickerRow>,
    pub selected: usize,
    /// For the effort step: the model the effort applies to.
    pub model: Option<Model>,
}

impl Picker {
    fn selectable(&self, i: usize) -> bool {
        !matches!(self.rows.get(i), Some(PickerRow::Header(_)) | None)
    }

    fn step(&mut self, down: bool) {
        let mut i = self.selected;
        loop {
            let next = if down { i + 1 } else { i.wrapping_sub(1) };
            if next >= self.rows.len() {
                return;
            }
            i = next;
            if self.selectable(i) {
                self.selected = i;
                return;
            }
        }
    }
}

/// A permission prompt on screen.
pub struct PromptView {
    pub request: PermissionRequest,
    pub selected: usize,
    /// `Some` while typing the "tell gz what to do differently" note.
    pub note: Option<String>,
    reply: Option<tokio::sync::oneshot::Sender<Answer>>,
}

/// One answer a permission prompt offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Once,
    Chat,
    Always,
    /// "No, and tell gz what to do differently": opens the note field.
    Deny,
}

impl PromptView {
    /// The answers on offer, in order; "always" only when the request can
    /// be remembered forever.
    pub fn options(&self) -> Vec<(Choice, String)> {
        let mut options = vec![
            (Choice::Once, "Yes".to_string()),
            (
                Choice::Chat,
                format!("Yes, for this chat ({})", self.request.chat_scope),
            ),
        ];
        if let Some(scope) = &self.request.always_scope {
            options.push((Choice::Always, format!("Yes, always ({scope})")));
        }
        options.push((
            Choice::Deny,
            "No, and tell gz what to do differently (esc)".to_string(),
        ));
        options
    }
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
    /// One page per subagent of this conversation.
    pub pages: AgentPages,
    /// The subagent page on screen; `None` shows the main page.
    pub agent_view: Option<AgentView>,
    /// Keys typed while a permission prompt was open, restored after it.
    typed_ahead: String,
}

/// How a prompt was answered, for the transcript.
fn decision_item(request: &PermissionRequest, answer: &Answer) -> Item {
    let choice = match answer {
        (Decision::AllowOnce, _) => "allowed once".to_string(),
        (Decision::AllowChat, _) => format!("allowed for this chat: {}", request.chat_scope),
        (Decision::AllowAlways, _) => format!(
            "allowed always: {}",
            request
                .always_scope
                .as_deref()
                .unwrap_or(&request.chat_scope)
        ),
        (Decision::Deny, Some(note)) if !note.is_empty() => format!("denied: {note}"),
        (Decision::Deny, _) => "denied".to_string(),
    };
    Item::Decision {
        title: request.summary.clone(),
        choice,
        preview: request.preview.clone(),
    }
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
            typed_ahead: String::new(),
            sandbox: None,
            files: None,
            mention_selected: 0,
            images: Vec::new(),
            clipboard: Box::new(SystemClipboard),
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

    fn current_model_key(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| self.catalog.as_ref()?.default_selection())
            .map(|s| s.model.key())
    }

    fn current_effort(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| self.catalog.as_ref()?.default_selection())
            .and_then(|s| s.effort)
    }

    fn open_model_picker(&mut self) {
        let Some(catalog) = &self.catalog else {
            let message = if self.models_loading {
                "The model list is still loading; try /models again in a moment"
            } else {
                "The model list couldn't be loaded; check `gz doctor`"
            };
            self.record(Item::Error(message.into()));
            return;
        };
        let mut rows = vec![PickerRow::Header("Groundzero".into())];
        rows.extend(
            catalog
                .modes
                .iter()
                .cloned()
                .map(|m| PickerRow::Model(Model::Groundzero(m))),
        );
        if !catalog.connections.is_empty() {
            rows.push(PickerRow::Header("Your connections".into()));
            rows.extend(
                catalog
                    .connections
                    .iter()
                    .cloned()
                    .map(|c| PickerRow::Model(Model::Connection(c))),
            );
        }
        let current = self.current_model_key();
        let selected = rows
            .iter()
            .position(|r| matches!(r, PickerRow::Model(m) if Some(m.key()) == current))
            .unwrap_or(1);
        self.picker = Some(Picker {
            title: "Select model".into(),
            rows,
            selected,
            model: None,
        });
    }

    fn open_effort_picker(&mut self, model: Model) {
        let efforts = model.efforts().to_vec();
        if efforts.is_empty() {
            self.choose_model(Selection {
                model,
                effort: None,
            });
            return;
        }
        let current = self.current_effort();
        let rows: Vec<PickerRow> = efforts.into_iter().map(PickerRow::Effort).collect();
        let selected = rows
            .iter()
            .position(|r| matches!(r, PickerRow::Effort(e) if Some(e) == current.as_ref()))
            .unwrap_or(0);
        self.picker = Some(Picker {
            title: format!("Reasoning effort for {}", model.label()),
            rows,
            selected,
            model: Some(model),
        });
    }

    /// Opens the `/resume` picker with `rows`, or says why it can't.
    pub fn show_conversations(&mut self, rows: Result<Vec<PickerRow>, String>) {
        match rows {
            Err(err) => self.record(Item::Error(format!(
                "cannot load your conversations: {}",
                crate::http::readable_error(&err)
            ))),
            Ok(rows) => match rows
                .iter()
                .position(|r| matches!(r, PickerRow::Conversation { .. }))
            {
                None => self.record(Item::Notice("No earlier conversations yet".into())),
                Some(selected) => {
                    self.picker = Some(Picker {
                        title: "Resume a conversation".into(),
                        rows,
                        selected,
                        model: None,
                    })
                }
            },
        }
    }

    /// Prints a resumed conversation's earlier messages.
    pub fn show_history(&mut self, title: &str, transcript: Result<Transcript, String>) {
        match transcript {
            Ok(transcript) => {
                let title = transcript.title.as_deref().unwrap_or(title);
                for item in resume::history_items(title, &transcript.messages) {
                    self.record(item);
                }
            }
            Err(err) => self.record(Item::Error(format!(
                "Continuing “{title}”, but its earlier messages couldn't be loaded: {}",
                crate::http::readable_error(&err)
            ))),
        }
    }

    fn choose_model(&mut self, selection: Selection) {
        self.record(Item::Notice(format!("Model: {}", selection.label())));
        self.model = Some(selection);
        self.picker = None;
    }

    fn on_picker_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(picker) = self.picker.as_mut() else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up | KeyCode::Char('k') => picker.step(false),
            KeyCode::Down | KeyCode::Char('j') => picker.step(true),
            KeyCode::Enter => match picker.rows.get(picker.selected).cloned() {
                Some(PickerRow::Conversation { id, title, .. }) => {
                    self.picker = None;
                    self.leave_conversation();
                    self.record(Item::Notice(format!("Loading “{title}”…")));
                    return vec![Effect::Resume { id, title }];
                }
                Some(PickerRow::Model(model)) => self.open_effort_picker(model),
                Some(PickerRow::Effort(effort)) => {
                    if let Some(model) = picker.model.clone() {
                        self.choose_model(Selection {
                            model,
                            effort: Some(effort),
                        });
                    }
                }
                _ => {}
            },
            _ => {}
        }
        match (&self.picker, &self.model) {
            (None, Some(selection)) if key.code == KeyCode::Enter => {
                vec![Effect::SetModel(selection.clone())]
            }
            _ => Vec::new(),
        }
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

    fn cycle_mode(&mut self) {
        if let Ok(mut mode) = self.mode.lock() {
            *mode = mode.next();
        }
    }

    // ---------- session events ----------

    /// Begins a turn the driver is about to send.
    #[cfg(test)]
    pub fn start_turn(&mut self, prompt: &str) {
        self.start_prepared_turn(&PreparedTurn::plain(prompt));
    }

    /// Shows the user's message (with its attachment notes) and starts the
    /// spinner.
    pub fn start_prepared_turn(&mut self, turn: &PreparedTurn) {
        let item = if turn.notes.is_empty() {
            Item::User(turn.display.clone())
        } else {
            Item::UserWithContext {
                text: turn.display.clone(),
                notes: turn.notes.clone(),
            }
        };
        self.record(item);
        self.busy_since = Some(Instant::now());
        self.received_chars = 0;
        self.subagents = Board::default();
        self.turns += 1;
        self.usage.start_turn(self.model_label());
    }

    pub fn apply(&mut self, event: AssistantEvent) -> Vec<Effect> {
        match event {
            AssistantEvent::Text(text) => {
                self.close_group();
                let chars = text.chars().count();
                self.received_chars += chars;
                self.usage.turn.streamed(chars);
                self.streaming.push_str(&text);
                self.answer_full.push_str(&text);
                self.commit_finished_blocks();
            }
            AssistantEvent::Usage(report) => {
                self.subagents.record_usage(&report.subagents);
                self.usage.turn.record(&report);
            }
            AssistantEvent::Context(context) => self.usage.context = Some(context),
            AssistantEvent::Subagent(event) => self.subagent_event(&event),
            AssistantEvent::ForSubagent { id, event } => self.subagent_tool(&id, *event),
            AssistantEvent::ToolStarted {
                name,
                summary,
                local,
            } => self.tool_started(&name, &summary, local),
            AssistantEvent::ToolFinished {
                name,
                summary,
                is_error,
                detail,
                output,
                diff,
            } => self.tool_finished(&name, &summary, is_error, &detail, output, diff),
            AssistantEvent::Notice(text) => {
                self.close_text();
                self.close_group();
                self.record(Item::Notice(text));
            }
            AssistantEvent::Done(end) => return self.finish_turn(end),
        }
        Vec::new()
    }

    fn tool_started(&mut self, name: &str, summary: &str, local: bool) {
        let tools = &mut self.usage.turn.tools;
        match (summary.starts_with(SCRIPT_CHILD_PREFIX), local) {
            (true, _) => tools.helper += 1,
            (false, true) => tools.local += 1,
            (false, false) => tools.server += 1,
        }
        if let Some(child) = summary.strip_prefix(SCRIPT_CHILD_PREFIX) {
            if let Some(script) = self
                .running
                .iter_mut()
                .rev()
                .find(|t| t.label.verb == "Script")
            {
                script.children.push(child.to_string());
                return;
            }
        }
        self.close_text();
        let label = labels::label(name, summary);
        if !label.read_only {
            self.close_group();
        }
        let item = ToolItem {
            label,
            status: ToolStatus::Running,
            result: None,
            diff: None,
            children: Vec::new(),
            output: None,
        };
        if local {
            self.running.push(item);
        } else {
            // Server-side tools report only that they started.
            self.record(Item::Tool(ToolItem {
                status: ToolStatus::Ok,
                ..item
            }));
        }
    }

    fn tool_finished(
        &mut self,
        name: &str,
        summary: &str,
        failed: bool,
        detail: &str,
        output: String,
        diff: Option<String>,
    ) {
        let label = labels::label(name, summary);
        let position = self.running.iter().position(|t| t.label == label);
        let mut item = match position {
            Some(i) => self.running.remove(i),
            None => ToolItem {
                label: label.clone(),
                status: ToolStatus::Running,
                result: None,
                diff: None,
                children: Vec::new(),
                output: None,
            },
        };
        item.status = if failed {
            ToolStatus::Failed
        } else {
            ToolStatus::Ok
        };
        item.result = Some(labels::result_line(name, summary, detail, diff.as_deref()));
        item.diff = diff;
        // Everything a local tool returns is sent back to the model.
        self.usage.turn.tools.context_bytes += output.len();
        item.output = Some(output).filter(|o| !o.trim().is_empty());
        if label.read_only && !failed {
            self.group.push(item);
        } else {
            self.close_group();
            self.record(Item::Tool(item));
        }
    }

    /// Running subagents stay in the live region; a finished one prints
    /// once and joins the turn's analytics.
    fn subagent_event(&mut self, event: &SubagentEvent) {
        if matches!(event, SubagentEvent::Started { .. }) {
            self.close_text();
            self.close_group();
        }
        let done = self.subagents.apply(event, Instant::now());
        self.pages.on_subagent(event, &self.subagents);
        if let Some(done) = done {
            self.usage.turn.subagents.push(done.clone());
            self.record(Item::Subagent(Box::new(done)));
        }
    }

    /// A local tool gz ran for a subagent: it goes on that subagent's page,
    /// and counts toward the turn's analytics.
    fn subagent_tool(&mut self, id: &str, event: AssistantEvent) {
        let tools = &mut self.usage.turn.tools;
        match &event {
            AssistantEvent::ToolStarted { summary, .. }
                if summary.starts_with(SCRIPT_CHILD_PREFIX) =>
            {
                tools.helper += 1
            }
            AssistantEvent::ToolStarted { .. } => tools.local += 1,
            AssistantEvent::ToolFinished { output, .. } => tools.context_bytes += output.len(),
            _ => {}
        }
        self.pages.on_local(id, event, &self.subagents);
    }

    /// Back to a conversation without subagents (`/new`, `/clear`, resume).
    /// Drops what belonged to the conversation being left (`/new`,
    /// `/clear`, `/resume`): subagent pages and the context fill.
    fn leave_conversation(&mut self) {
        self.pages.clear();
        self.agent_view = None;
        self.usage.context = None;
    }

    /// Subagents still in flight when the turn ends stopped with it: as
    /// cancelled when the user interrupted, as failed when the run failed.
    fn stop_subagents(&mut self, end: &TurnEnd) {
        let now = Instant::now();
        let stopped: Vec<Subagent> = self.subagents.running().cloned().collect();
        for mut agent in stopped {
            agent.status = match end {
                TurnEnd::Failed(_) => SubagentStatus::Failed,
                _ => SubagentStatus::Cancelled,
            };
            if agent.summary.is_empty() && matches!(end, TurnEnd::Failed(_)) {
                agent.summary = "Stopped because the run failed (see the error below).".into();
            }
            agent.duration = Some(agent.elapsed(now));
            self.pages.stopped(&agent);
            self.usage.turn.subagents.push(agent.clone());
            self.record(Item::Subagent(Box::new(agent)));
        }
        self.subagents = Board::default();
    }

    fn finish_turn(&mut self, end: TurnEnd) -> Vec<Effect> {
        self.close_text();
        self.close_group();
        self.stop_subagents(&end);
        let stopped: Vec<ToolItem> = self.running.drain(..).collect();
        for mut tool in stopped {
            tool.status = ToolStatus::Failed;
            tool.result.get_or_insert_with(|| "Stopped".to_string());
            self.record(Item::Tool(tool));
        }
        let completed = end == TurnEnd::Completed;
        match end {
            TurnEnd::Completed => {}
            TurnEnd::Cancelled => self.record(Item::Notice("⎿  Interrupted by user".into())),
            TurnEnd::Failed(why) => self.record(Item::Error(crate::http::readable_error(&why))),
        }
        let stats = self.usage.end_turn().clone();
        if completed {
            self.record(Item::Summary(usage::turn_summary(&stats)));
            self.transcript
                .push(Item::Panel(usage::turn_details(&stats)));
        }
        self.busy_since = None;
        if self.prompt.take().is_some() {
            // The run ended; an open prompt can no longer matter.
            self.restore_typed_ahead();
        }
        match self.queue.pop_front() {
            Some(next) => vec![Effect::Send(next)],
            None => Vec::new(),
        }
    }

    /// The `/permissions` panel (after what was revoked), or why it
    /// couldn't load.
    pub fn show_permissions(&mut self, reply: PermissionsReply) {
        if let Some(revoked) = reply.revoked {
            self.record(Item::Notice(format!("✓ {revoked}")));
        }
        match reply.panel {
            Ok(panel) => self.record(Item::Panel(panel)),
            Err(why) => self.record(Item::Error(why)),
        }
    }

    /// The account panel for `/usage`, or why it couldn't load.
    pub fn show_account_usage(&mut self, panel: Result<Panel, String>) {
        self.account_loading = false;
        match panel {
            Ok(panel) => self.record(Item::Panel(panel)),
            Err(why) => self.record(Item::Error(format!(
                "couldn't load your account's usage: {why}"
            ))),
        }
    }

    /// The answer chunk still streaming, as it will print once finished.
    pub fn streaming_item(&self) -> Option<Item> {
        let text = self.streaming.trim_end();
        (!text.trim().is_empty()).then(|| self.answer_chunk(text.to_string()))
    }

    fn answer_chunk(&self, text: String) -> Item {
        if self.answer_started {
            Item::AssistantMore(text)
        } else {
            Item::Assistant(text)
        }
    }

    /// Prints the answer's finished blocks now, keeping only the block in
    /// progress live, so long answers scroll like they do in the transcript.
    fn commit_finished_blocks(&mut self) {
        let Some(cut) = blocks::last_safe_cut(&self.streaming) else {
            return;
        };
        let done = self.streaming[..cut.commit].trim_end().to_string();
        self.streaming = self.streaming[cut.rest..].to_string();
        if done.trim().is_empty() {
            return;
        }
        let chunk = self.answer_chunk(done);
        self.flush.push(chunk);
        self.answer_started = true;
    }

    fn close_text(&mut self) {
        let text = std::mem::take(&mut self.streaming);
        if !text.trim().is_empty() {
            let chunk = self.answer_chunk(text.trim_end().to_string());
            self.flush.push(chunk);
        }
        let full = std::mem::take(&mut self.answer_full);
        if !full.trim().is_empty() {
            self.transcript
                .push(Item::Assistant(full.trim_end().to_string()));
        }
        self.answer_started = false;
    }

    /// Prints pending read-only calls: one in full, several as a summary.
    fn close_group(&mut self) {
        match self.group.len() {
            0 => {}
            1 => {
                let item = self.group.remove(0);
                self.record(Item::Tool(item));
            }
            _ => {
                let tools: Vec<ToolItem> = self.group.drain(..).collect();
                let labels: Vec<Label> = tools.iter().map(|t| t.label.clone()).collect();
                self.flush.push(Item::Group(labels::group_summary(&labels)));
                // The transcript keeps each call with its full output.
                self.transcript.extend(tools.into_iter().map(Item::Tool));
            }
        }
    }

    // ---------- permission prompts ----------

    pub fn open_prompt(&mut self, ask: Ask) {
        // Prompts are answered on the main page.
        self.agent_view = None;
        self.close_text();
        self.close_group();
        self.prompt = Some(PromptView {
            request: ask.request,
            selected: 0,
            note: None,
            reply: Some(ask.reply),
        });
    }

    fn answer(&mut self, answer: Answer) {
        let Some(mut prompt) = self.prompt.take() else {
            return;
        };
        self.transcript
            .push(decision_item(&prompt.request, &answer));
        if let Some(reply) = prompt.reply.take() {
            reply.send(answer).ok();
        }
        self.restore_typed_ahead();
    }

    fn restore_typed_ahead(&mut self) {
        let typed = std::mem::take(&mut self.typed_ahead);
        if !typed.is_empty() {
            self.input.insert_str(&typed);
        }
    }

    /// Acts on the prompt's option at `index` (ignored when out of range).
    fn choose(&mut self, index: usize) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        let options = prompt.options();
        let Some((choice, _)) = options.get(index) else {
            return;
        };
        match choice {
            Choice::Once => self.answer((Decision::AllowOnce, None)),
            Choice::Chat => self.answer((Decision::AllowChat, None)),
            Choice::Always => self.answer((Decision::AllowAlways, None)),
            Choice::Deny => {
                prompt.selected = index;
                prompt.note = Some(String::new());
            }
        }
    }

    fn on_prompt_key(&mut self, key: KeyEvent) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        if let Some(note) = prompt.note.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let note = std::mem::take(note);
                    self.answer((Decision::Deny, Some(note)));
                }
                KeyCode::Esc => self.answer((Decision::Deny, None)),
                KeyCode::Backspace => {
                    note.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => note.push(c),
                _ => {}
            }
            return;
        }
        let last = prompt.options().len() - 1;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => prompt.selected = prompt.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => prompt.selected = (prompt.selected + 1).min(last),
            KeyCode::Enter => {
                let selected = prompt.selected;
                self.choose(selected);
            }
            KeyCode::Char(c @ '1'..='4') => self.choose(c as usize - '1' as usize),
            KeyCode::Char('y') => self.choose(0),
            KeyCode::Char('n') | KeyCode::Esc => self.choose(last),
            KeyCode::BackTab if prompt.request.access == Access::Write => {
                if let Ok(mut mode) = self.mode.lock() {
                    *mode = Mode::AcceptEdits;
                }
                self.answer((Decision::AllowChat, None));
            }
            _ => {}
        }
    }

    // ---------- keys ----------

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.viewer.is_some() {
            self.on_viewer_key(key);
            return Vec::new();
        }
        if self.agent_view.is_some() {
            self.on_agent_key(key);
            return Vec::new();
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if shift
            && matches!(key.code, KeyCode::Down | KeyCode::Up)
            && !self.pages.is_empty()
            && self.picker.is_none()
            && self.prompt.is_none()
        {
            self.step_page(key.code == KeyCode::Down);
            return Vec::new();
        }
        if ctrl && key.code == KeyCode::Char('o') && self.picker.is_none() {
            self.viewer = Some(Viewer::open());
            return Vec::new();
        }
        if self.picker.is_some() {
            return self.on_picker_key(key);
        }
        if self.prompt.is_some() {
            if ctrl && key.code == KeyCode::Char('c') {
                self.answer((Decision::Deny, None));
                return vec![Effect::Cancel];
            }
            if self.is_typed_ahead(key) {
                if let KeyCode::Char(c) = key.code {
                    self.typed_ahead.push(c);
                }
                return Vec::new();
            }
            self.on_prompt_key(key);
            return Vec::new();
        }
        if ctrl && key.code == KeyCode::Char('c') {
            return self.on_ctrl_c();
        }
        self.exit_armed = None;
        if let Some(effects) = self.on_menu_key(key) {
            return effects;
        }
        if let Some(effects) = self.on_mention_key(key) {
            return effects;
        }
        if ctrl && key.code == KeyCode::Char('v') {
            self.paste_clipboard_image();
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('d') if ctrl && self.input_text().is_empty() => vec![Effect::Exit],
            KeyCode::Char('u') if ctrl => {
                self.input = new_input();
                Vec::new()
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.input.insert_newline();
                Vec::new()
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Esc => {
                if self.show_shortcuts {
                    self.show_shortcuts = false;
                    Vec::new()
                } else if self.is_busy() {
                    vec![Effect::Cancel]
                } else {
                    Vec::new()
                }
            }
            KeyCode::BackTab => {
                self.cycle_mode();
                Vec::new()
            }
            KeyCode::Char('?') if self.input_text().is_empty() => {
                self.show_shortcuts = !self.show_shortcuts;
                Vec::new()
            }
            KeyCode::Up if self.input.lines().len() == 1 => {
                self.history_step(true);
                Vec::new()
            }
            KeyCode::Down if self.input.lines().len() == 1 => {
                self.history_step(false);
                Vec::new()
            }
            _ => {
                if key.code == KeyCode::Char('@') {
                    self.ensure_file_index();
                }
                self.input.input(key);
                self.menu_selected = 0;
                self.mention_selected = 0;
                Vec::new()
            }
        }
    }

    /// A printable key the open prompt doesn't use: kept for the input box
    /// instead of being lost.
    fn is_typed_ahead(&self, key: KeyEvent) -> bool {
        let Some(prompt) = &self.prompt else {
            return false;
        };
        let KeyCode::Char(c) = key.code else {
            return false;
        };
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let prompt_key = matches!(c, 'y' | 'n' | 'j' | 'k' | '1'..='3');
        plain && prompt.note.is_none() && !prompt_key
    }

    /// The transcript plus what the current turn hasn't finished yet
    /// (pending reads, running tools, the answer so far).
    pub fn transcript_items(&self) -> Vec<Item> {
        let mut items = self.transcript.clone();
        items.extend(
            self.group
                .iter()
                .chain(self.running.iter())
                .cloned()
                .map(Item::Tool),
        );
        items.extend(
            self.subagents
                .running()
                .cloned()
                .map(|agent| Item::Subagent(Box::new(agent))),
        );
        if !self.answer_full.trim().is_empty() {
            items.push(Item::Assistant(self.answer_full.trim_end().to_string()));
        }
        items
    }

    /// Keys on a subagent's page: shift+↑↓ switch pages, esc goes back to
    /// main, the rest scroll and search like the transcript.
    fn on_agent_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Down | KeyCode::Up)
        {
            self.step_page(key.code == KeyCode::Down);
            return;
        }
        let Some(view) = self.agent_view.as_mut() else {
            return;
        };
        let items = self
            .pages
            .get(view.index)
            .map(|page| page.items())
            .unwrap_or_default();
        let lines = transcript::plain(&transcript::lines(&items, self.transcript_width));
        if view.viewer.on_key(key, &lines) == ViewerAction::Close {
            self.agent_view = None;
        }
    }

    /// Moves to the next (or previous) page; main sits before the first
    /// subagent and after the last.
    fn step_page(&mut self, forward: bool) {
        let count = self.pages.len();
        let current = self.agent_view.as_ref().map(|v| v.index);
        let next = match (current, forward) {
            (None, true) => Some(0),
            (None, false) => count.checked_sub(1),
            (Some(i), true) => (i + 1 < count).then_some(i + 1),
            (Some(i), false) => i.checked_sub(1),
        };
        self.agent_view = next.map(AgentView::open);
    }

    fn on_viewer_key(&mut self, key: KeyEvent) {
        let width = self.transcript_width;
        let lines = transcript::plain(&transcript::lines(&self.transcript_items(), width));
        let Some(viewer) = self.viewer.as_mut() else {
            return;
        };
        if viewer.on_key(key, &lines) == ViewerAction::Close {
            self.viewer = None;
        }
    }

    fn on_ctrl_c(&mut self) -> Vec<Effect> {
        if !self.input_text().is_empty() {
            self.input = new_input();
            return Vec::new();
        }
        if self.exit_armed() {
            return vec![Effect::Exit];
        }
        self.exit_armed = Some(Instant::now());
        if self.is_busy() {
            vec![Effect::Cancel]
        } else {
            Vec::new()
        }
    }

    pub fn input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    fn submit(&mut self) -> Vec<Effect> {
        let text = self.input_text();
        let text = text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        let text = text.to_string();
        self.history.push(text.clone());
        self.history_pos = None;
        self.input = new_input();
        if text.starts_with('/') {
            return self.slash(&text);
        }
        if self.is_busy() {
            self.queue.push_back(text);
            return Vec::new();
        }
        vec![Effect::Send(text)]
    }

    /// `/subagents` lists the profiles; `/subagents on|off` toggles them.
    fn subagents_command(&mut self, command: &str) -> Vec<Effect> {
        match command.split_whitespace().nth(1) {
            Some("on") | Some("off") => {
                let enabled = command.split_whitespace().nth(1) == Some("on");
                self.subagents_enabled = enabled;
                let note = if enabled {
                    "Subagents on: the chat may start helper agents in parallel"
                } else {
                    "Subagents off: the chat works alone"
                };
                self.record(Item::Notice(note.into()));
                vec![Effect::SetSubagents(enabled)]
            }
            Some(other) => {
                self.record(Item::Error(format!(
                    "Unknown option {other} · use /subagents on or /subagents off"
                )));
                Vec::new()
            }
            None => {
                self.record(Item::Panel(self.subagent_overview()));
                Vec::new()
            }
        }
    }

    fn subagent_overview(&self) -> Panel {
        let (state, switch) = if self.subagents_enabled {
            ("on", "off")
        } else {
            ("off", "on")
        };
        let profiles = self
            .catalog
            .as_ref()
            .map(|c| c.subagent_profiles.as_slice())
            .unwrap_or_default();
        let mut rows: Vec<Row> = profiles
            .iter()
            .map(|profile| Row::entry(&profile.id, profile.description.clone()))
            .collect();
        if profiles.is_empty() {
            rows.push(Row::note(if self.models_loading {
                "The profile list is still loading; try again in a moment"
            } else {
                "This chat server doesn't list subagent profiles"
            }));
        }
        Panel::new("Subagents")
            .aside(state)
            .section(Some("Profiles"), rows)
            .footer(format!("/subagents {switch} to turn them {switch}"))
    }

    fn slash(&mut self, command: &str) -> Vec<Effect> {
        match command.split_whitespace().next().unwrap_or_default() {
            "/help" => {
                self.record(Item::Panel(help_panel()));
                Vec::new()
            }
            "/subagents" => self.subagents_command(command),
            "/new" => {
                self.leave_conversation();
                self.record(Item::Notice("New conversation".into()));
                vec![Effect::NewConversation]
            }
            "/usage" => {
                self.record(Item::Panel(usage::report(&self.usage)));
                self.account_loading = true;
                vec![Effect::FetchAccountUsage]
            }
            "/permissions" => {
                let mut words = command.split_whitespace().skip(1);
                match (words.next(), words.next()) {
                    (None, _) => vec![Effect::Permissions(None)],
                    (Some("revoke"), Some(id)) => vec![Effect::Permissions(Some(id.to_string()))],
                    _ => {
                        self.record(Item::Error(
                            "Use /permissions, or /permissions revoke <id> (e.g. L1)".into(),
                        ));
                        Vec::new()
                    }
                }
            }
            "/last" => {
                match &self.usage.last {
                    Some(stats) => self.record(Item::Panel(usage::turn_details(stats))),
                    None => self.record(Item::Notice("No finished turn yet".into())),
                }
                Vec::new()
            }
            "/clear" => {
                self.leave_conversation();
                vec![Effect::ClearScreen, Effect::NewConversation]
            }
            "/resume" | "/continue" => {
                if self.is_busy() {
                    self.record(Item::Error(
                        "Wait for the answer to finish (or esc) before resuming another conversation"
                            .into(),
                    ));
                    return Vec::new();
                }
                vec![Effect::ListConversations]
            }
            "/exit" | "/quit" => vec![Effect::Exit],
            "/models" | "/model" => {
                self.open_model_picker();
                Vec::new()
            }
            "/effort" => {
                let current = self
                    .model
                    .clone()
                    .or_else(|| self.catalog.as_ref()?.default_selection());
                match current {
                    Some(selection) if !selection.model.efforts().is_empty() => {
                        self.open_effort_picker(selection.model)
                    }
                    _ => self.record(Item::Error(
                        "This model has no reasoning-effort levels".into(),
                    )),
                }
                Vec::new()
            }
            "/mode" => {
                self.cycle_mode();
                let name = self.mode().banner().unwrap_or("ask before changes");
                self.record(Item::Notice(format!("Mode: {name}")));
                Vec::new()
            }
            other => {
                self.record(Item::Error(format!(
                    "Unknown command {other} · /help lists them"
                )));
                Vec::new()
            }
        }
    }

    /// Commands matching what's typed while the input is a lone `/word`:
    /// prefix matches first, then ones that merely contain it.
    pub fn command_menu(&self) -> Vec<(&'static str, &'static str)> {
        let text = self.input_text();
        let typed = text.trim_end();
        if !typed.starts_with('/')
            || typed.contains(char::is_whitespace)
            || self.input.lines().len() > 1
        {
            return Vec::new();
        }
        let needle = typed.to_lowercase();
        let (mut prefix, mut contains): (Vec<_>, Vec<_>) = (Vec::new(), Vec::new());
        for &(name, what) in COMMANDS {
            if name.starts_with(&needle) {
                prefix.push((name, what));
            } else if name.contains(needle.trim_start_matches('/')) {
                contains.push((name, what));
            }
        }
        prefix.extend(contains);
        prefix
    }

    /// Keys while the slash menu is showing; `None` means "not handled here".
    fn on_menu_key(&mut self, key: KeyEvent) -> Option<Vec<Effect>> {
        let menu = self.command_menu();
        if menu.is_empty() {
            return None;
        }
        let selected = self.menu_selected.min(menu.len() - 1);
        match key.code {
            KeyCode::Up => {
                self.menu_selected = selected.checked_sub(1).unwrap_or(menu.len() - 1);
                Some(Vec::new())
            }
            KeyCode::Down => {
                self.menu_selected = (selected + 1) % menu.len();
                Some(Vec::new())
            }
            KeyCode::Tab => {
                self.set_input(menu[selected].0);
                Some(Vec::new())
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                let typed = self.input_text().trim().to_string();
                let exact = menu.iter().any(|(name, _)| *name == typed);
                if !exact {
                    self.set_input(menu[selected].0);
                }
                Some(self.submit())
            }
            KeyCode::Esc => {
                self.input = new_input();
                Some(Vec::new())
            }
            _ => None,
        }
    }

    /// The input's last line up to the cursor: where an `@word` is typed.
    fn line_before_cursor(&self) -> String {
        let (row, col) = self.input.cursor();
        let line = self.input.lines().get(row).cloned().unwrap_or_default();
        line.chars().take(col).collect()
    }

    /// Workspace paths matching the `@word` being typed (empty when no
    /// mention is being typed, or the slash menu is open).
    pub fn mention_menu(&self) -> Vec<String> {
        if !self.command_menu().is_empty() {
            return Vec::new();
        }
        let (Some(files), Some(query)) = (
            &self.files,
            mentions::active_query(&self.line_before_cursor()),
        ) else {
            return Vec::new();
        };
        files.matches(&query)
    }

    /// Keys while the `@` popup is showing; `None` means "not handled here".
    fn on_mention_key(&mut self, key: KeyEvent) -> Option<Vec<Effect>> {
        let menu = self.mention_menu();
        if menu.is_empty() {
            return None;
        }
        let selected = self.mention_selected.min(menu.len() - 1);
        match key.code {
            KeyCode::Up => {
                self.mention_selected = selected.checked_sub(1).unwrap_or(menu.len() - 1)
            }
            KeyCode::Down => self.mention_selected = (selected + 1) % menu.len(),
            KeyCode::Tab | KeyCode::Enter if key.modifiers.is_empty() => {
                self.insert_mention(&menu[selected]);
            }
            KeyCode::Esc => {
                let line = self.line_before_cursor();
                let start = mentions::mention_start(&line).unwrap_or(line.len());
                self.replace_line_before_cursor(&line[..start]);
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    fn insert_mention(&mut self, path: &str) {
        let line = self.line_before_cursor();
        self.replace_line_before_cursor(&mentions::complete(&line, path));
        self.mention_selected = 0;
    }

    /// Replaces the current line's text before the cursor, keeping the
    /// other lines and the text after the cursor.
    fn replace_line_before_cursor(&mut self, replacement: &str) {
        let (row, col) = self.input.cursor();
        let mut lines: Vec<String> = self.input.lines().to_vec();
        let after: String = lines
            .get(row)
            .map(|l| l.chars().skip(col).collect())
            .unwrap_or_default();
        if let Some(line) = lines.get_mut(row) {
            *line = format!("{replacement}{after}");
        }
        self.input = new_input();
        self.input.insert_str(lines.join("\n"));
        let up = lines.len().saturating_sub(row + 1);
        for _ in 0..up {
            self.input.move_cursor(tui_textarea::CursorMove::Up);
        }
        self.input.move_cursor(tui_textarea::CursorMove::Head);
        for _ in 0..replacement.chars().count() {
            self.input.move_cursor(tui_textarea::CursorMove::Forward);
        }
    }

    /// Scans the workspace for the `@` popup the first time it's needed.
    fn ensure_file_index(&mut self) {
        if self.files.is_none() {
            if let Some(sandbox) = &self.sandbox {
                self.files = Some(FileIndex::scan(sandbox));
            }
        }
    }

    #[cfg(test)]
    pub fn set_clipboard(&mut self, clipboard: Box<dyn ImageClipboard>) {
        self.clipboard = clipboard;
    }

    /// Adds an image and drops its `[Image #N]` chip at the cursor.
    pub fn attach_image(&mut self, path: PathBuf) {
        self.images.push(path);
        let chip = attachments::image_chip(self.images.len());
        self.input.insert_str(format!("{chip} "));
    }

    fn paste_clipboard_image(&mut self) {
        match self.clipboard.paste_image() {
            Ok(Some(path)) => self.attach_image(path),
            Ok(None) => self
                .flush
                .push(Item::Notice("No image on the clipboard".into())),
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }

    /// Bracketed paste: a dragged-in image file becomes a chip; anything
    /// else is typed into the input.
    pub fn on_paste(&mut self, text: &str) {
        match mentions::pasted_image(text) {
            Some(path) => self.attach_image(path),
            None => {
                self.input.insert_str(text);
            }
        }
    }

    /// Turns a message about to be sent into a [`PreparedTurn`]: mentions
    /// read and checked, images queued for upload. On a bad mention the
    /// error is shown, the text goes back into the input, and `None` means
    /// nothing is sent.
    pub fn take_turn(&mut self, text: &str) -> Option<PreparedTurn> {
        let Some(sandbox) = &self.sandbox else {
            return Some(PreparedTurn::plain(text));
        };
        match attachments::prepare(sandbox, text, &self.images) {
            Ok(turn) => {
                if self.queue.is_empty() {
                    self.images.clear();
                }
                Some(turn)
            }
            Err(err) => {
                self.record(Item::Error(format!("{err:#}")));
                if self.input_text().is_empty() {
                    self.input.insert_str(text);
                }
                None
            }
        }
    }

    fn set_input(&mut self, text: &str) {
        self.input = new_input();
        self.input.insert_str(text);
        self.menu_selected = 0;
    }

    fn history_step(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() - 1;
        let next = match (self.history_pos, back) {
            (None, true) => Some(last),
            (None, false) => None,
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i >= last => None,
            (Some(i), false) => Some(i + 1),
        };
        self.history_pos = next;
        let text = next.map(|i| self.history[i].clone()).unwrap_or_default();
        let mut input = new_input();
        input.insert_str(&text);
        self.input = input;
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> UiState {
        UiState::new(Arc::new(Mutex::new(Mode::Ask)))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(s: &mut UiState, text: &str) {
        for c in text.chars() {
            s.on_key(key(KeyCode::Char(c)));
        }
    }

    fn started(name: &str, summary: &str) -> AssistantEvent {
        AssistantEvent::ToolStarted {
            name: name.into(),
            summary: summary.into(),
            local: true,
        }
    }

    fn finished(name: &str, summary: &str, detail: &str, diff: Option<&str>) -> AssistantEvent {
        AssistantEvent::ToolFinished {
            name: name.into(),
            summary: summary.into(),
            is_error: false,
            detail: detail.into(),
            output: detail.into(),
            diff: diff.map(str::to_string),
        }
    }

    #[test]
    fn enter_sends_and_shift_enter_adds_a_line() {
        let mut s = state();
        type_text(&mut s, "hi");
        s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        type_text(&mut s, "there");
        assert_eq!(s.input_text(), "hi\nthere");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Send("hi\nthere".into())]
        );
        assert_eq!(s.input_text(), "");
    }

    #[test]
    fn history_recalls_previous_prompts() {
        let mut s = state();
        type_text(&mut s, "first");
        s.on_key(key(KeyCode::Enter));
        type_text(&mut s, "second");
        s.on_key(key(KeyCode::Enter));
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "second");
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "first");
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.input_text(), "second");
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.input_text(), "");
    }

    #[test]
    fn typing_while_busy_queues_and_sends_after_the_turn() {
        let mut s = state();
        s.start_turn("one");
        type_text(&mut s, "two");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(s.queue.len(), 1);
        let effects = s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert_eq!(effects, vec![Effect::Send("two".into())]);
    }

    #[test]
    fn esc_interrupts_and_ctrl_c_twice_exits() {
        let mut s = state();
        s.start_turn("x");
        assert_eq!(s.on_key(key(KeyCode::Esc)), vec![Effect::Cancel]);
        s.apply(AssistantEvent::Done(TurnEnd::Cancelled));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        type_text(&mut s, "draft");
        assert!(s.on_key(ctrl_c).is_empty(), "first ctrl+c clears the input");
        assert_eq!(s.input_text(), "");
        assert!(s.on_key(ctrl_c).is_empty());
        assert!(s.exit_armed());
        assert_eq!(s.on_key(ctrl_c), vec![Effect::Exit]);
    }

    #[test]
    fn text_and_tools_flush_in_order_with_reads_grouped() {
        let mut s = state();
        s.start_turn("look");
        s.apply(AssistantEvent::Text("Let me look.".into()));
        s.apply(started("read_file", "read_file a.rs"));
        s.apply(finished("read_file", "read_file a.rs", "3 lines", None));
        s.apply(started("read_file", "read_file b.rs"));
        s.apply(finished("read_file", "read_file b.rs", "4 lines", None));
        assert_eq!(s.group.len(), 2, "reads wait to be grouped");
        s.apply(AssistantEvent::Text("Done.".into()));
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        let summary = s.flush.pop();
        assert_eq!(
            s.flush,
            vec![
                Item::User("look".into()),
                Item::Assistant("Let me look.".into()),
                Item::Group("Read 2 files".into()),
                Item::Assistant("Done.".into()),
            ]
        );
        assert!(
            matches!(&summary, Some(Item::Summary(line))
                if line.starts_with("✻ Worked for") && line.ends_with("· 2 tools")),
            "{summary:?}"
        );
        assert!(!s.is_busy());
    }

    #[test]
    fn edits_flush_with_their_diff_and_script_calls_nest() {
        let mut s = state();
        s.start_turn("go");
        s.apply(started("run_script", "run_script python (2 lines)"));
        s.apply(started("read_file", "script · read_file a.rs"));
        assert_eq!(s.running[0].children, vec!["read_file a.rs"]);
        s.apply(finished(
            "run_script",
            "run_script python (2 lines)",
            "2 lines",
            None,
        ));
        let diff = "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-x\n+y\n";
        s.apply(started("edit_file", "edit_file a.rs"));
        s.apply(finished(
            "edit_file",
            "edit_file a.rs",
            "edited",
            Some(diff),
        ));
        let Item::Tool(edit) = s.flush.last().unwrap() else {
            panic!()
        };
        assert_eq!(edit.label.title(), "Update(a.rs)");
        assert_eq!(
            edit.result.as_deref(),
            Some("Updated a.rs with 1 addition and 1 removal")
        );
        assert!(edit.diff.is_some());
        let Item::Tool(script) = &s.flush[1] else {
            panic!()
        };
        assert_eq!(script.children, vec!["read_file a.rs"]);
    }

    #[test]
    fn prompts_choose_by_arrow_number_or_note() {
        let mut s = state();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Write,
                summary: "edit_file a.rs".into(),
                preview: None,
                chat_scope: "all edits this session".into(),
                always_scope: Some("always scope".into()),
                action: None,
                asked_by: None,
            },
            reply: tx,
        });
        for _ in 0..3 {
            s.on_key(key(KeyCode::Down));
        }
        s.on_key(key(KeyCode::Enter));
        assert_eq!(
            s.prompt.as_ref().unwrap().note.as_deref(),
            Some(""),
            "option 4 asks for a note"
        );
        type_text(&mut s, "use a loop");
        s.on_key(key(KeyCode::Enter));
        assert!(s.prompt.is_none());
        assert_eq!(
            rx.try_recv().unwrap(),
            (Decision::Deny, Some("use a loop".into()))
        );

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Command,
                summary: "run_command ls".into(),
                preview: Some("ls".into()),
                chat_scope: "this exact command".into(),
                always_scope: Some("always scope".into()),
                action: None,
                asked_by: None,
            },
            reply: tx,
        });
        s.on_key(key(KeyCode::Char('2')));
        assert_eq!(rx.try_recv().unwrap(), (Decision::AllowChat, None));

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Command,
                summary: "run_command ls".into(),
                preview: Some("ls".into()),
                chat_scope: "this exact command".into(),
                always_scope: Some("this exact command in this folder".into()),
                action: None,
                asked_by: None,
            },
            reply: tx,
        });
        s.on_key(key(KeyCode::Char('3')));
        assert_eq!(rx.try_recv().unwrap(), (Decision::AllowAlways, None));
    }

    #[test]
    fn without_always_the_last_option_is_still_deny() {
        let mut s = state();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::ServerAction,
                summary: "Delete dataset".into(),
                preview: None,
                chat_scope: "this action in this chat".into(),
                always_scope: None,
                action: Some("delete_dataset".into()),
                asked_by: None,
            },
            reply: tx,
        });
        assert_eq!(s.prompt.as_ref().unwrap().options().len(), 3);
        s.on_key(key(KeyCode::Char('4')));
        assert!(
            s.prompt.is_some(),
            "4 does nothing when there are three options"
        );
        s.on_key(key(KeyCode::Char('n')));
        s.on_key(key(KeyCode::Esc));
        assert_eq!(rx.try_recv().unwrap(), (Decision::Deny, None));
    }

    #[test]
    fn permissions_command_lists_and_revokes() {
        let mut s = state();
        type_text(&mut s, "/permissions");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Permissions(None)]
        );
        type_text(&mut s, "/permissions revoke l2");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Permissions(Some("l2".into()))]
        );
        type_text(&mut s, "/permissions nope");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("revoke <id>")));
    }

    #[test]
    fn shift_tab_cycles_modes_and_on_an_edit_prompt_accepts_edits() {
        let mut s = state();
        s.on_key(key(KeyCode::BackTab));
        assert_eq!(s.mode(), Mode::AcceptEdits);
        s.on_key(key(KeyCode::BackTab));
        assert_eq!(s.mode(), Mode::Plan);
        s.on_key(key(KeyCode::BackTab));
        assert_eq!(s.mode(), Mode::Ask);
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Write,
                summary: "edit_file a.rs".into(),
                preview: None,
                chat_scope: "all edits this session".into(),
                always_scope: Some("always scope".into()),
                action: None,
                asked_by: None,
            },
            reply: tx,
        });
        s.on_key(key(KeyCode::BackTab));
        assert_eq!(rx.try_recv().unwrap().0, Decision::AllowChat);
        assert_eq!(s.mode(), Mode::AcceptEdits);
    }

    fn with_catalog() -> UiState {
        use crate::assistant::models::{GroundzeroMode, LlmConnection};
        let mut s = state();
        s.catalog = Some(Catalog {
            modes: vec![
                GroundzeroMode {
                    value: "opus".into(),
                    label: "Claude Opus 5".into(),
                    efforts: vec!["low".into(), "high".into()],
                },
                GroundzeroMode {
                    value: "sonnet55".into(),
                    label: "Claude Sonnet 5.5".into(),
                    efforts: vec![],
                },
            ],
            default_mode: Some("sonnet55".into()),
            default_effort: None,
            connections: vec![LlmConnection {
                provider: "OPENAI".into(),
                name: "my-gpt".into(),
                model_id: "gpt-5".into(),
            }],
            subagent_profiles: vec![crate::assistant::models::SubagentProfile {
                id: "analyst".into(),
                label: "Analyst".into(),
                description: "lakehouse and SQL reads".into(),
            }],
        });
        s
    }

    #[test]
    fn model_picker_marks_current_and_picks_mode_then_effort() {
        let mut s = with_catalog();
        assert_eq!(s.model_label().as_deref(), Some("Claude Sonnet 5.5"));
        type_text(&mut s, "/models");
        s.on_key(key(KeyCode::Enter));
        let picker = s.picker.as_ref().unwrap();
        assert_eq!(picker.rows[0], PickerRow::Header("Groundzero".into()));
        assert!(
            matches!(&picker.rows[picker.selected], PickerRow::Model(m) if m.key() == "sonnet55"),
            "current is preselected"
        );
        assert!(picker
            .rows
            .contains(&PickerRow::Header("Your connections".into())));
        s.on_key(key(KeyCode::Up));
        s.on_key(key(KeyCode::Up));
        assert_eq!(
            s.picker.as_ref().unwrap().selected,
            1,
            "headers are skipped"
        );
        assert!(
            s.on_key(key(KeyCode::Enter)).is_empty(),
            "opus has efforts: next step"
        );
        assert!(s.picker.as_ref().unwrap().title.contains("effort"));
        s.on_key(key(KeyCode::Down));
        let effects = s.on_key(key(KeyCode::Enter));
        let Effect::SetModel(selection) = &effects[0] else {
            panic!("{effects:?}")
        };
        assert_eq!(selection.label(), "Claude Opus 5 · high");
        assert_eq!(s.model_label().as_deref(), Some("Claude Opus 5 · high"));
    }

    #[test]
    fn connections_skip_the_effort_step_and_esc_cancels() {
        let mut s = with_catalog();
        type_text(&mut s, "/models");
        s.on_key(key(KeyCode::Enter));
        s.on_key(key(KeyCode::Esc));
        assert!(s.picker.is_none());
        type_text(&mut s, "/models");
        s.on_key(key(KeyCode::Enter));
        for _ in 0..5 {
            s.on_key(key(KeyCode::Down));
        }
        let effects = s.on_key(key(KeyCode::Enter));
        assert!(
            matches!(&effects[0], Effect::SetModel(sel) if sel.model.key() == "conn:OPENAI:my-gpt")
        );
    }

    fn workspace_state(name: &str) -> (PathBuf, UiState) {
        let dir = std::env::temp_dir().join(format!("gz-ui-{name}-{}", crate::cmd::new_uuid()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/math.rs"), "pub fn add() {}\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        let mut s = state();
        s.sandbox = Some(Sandbox::new(&dir).unwrap());
        (dir, s)
    }

    #[test]
    fn at_opens_a_file_popup_that_inserts_mentions() {
        let (_dir, mut s) = workspace_state("popup");
        type_text(&mut s, "explain @src/ma");
        assert_eq!(s.mention_menu(), vec!["src/main.rs", "src/math.rs"]);
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "explain @src/math.rs ");
        assert!(s.mention_menu().is_empty(), "closed after inserting a file");
        type_text(&mut s, "@");
        assert!(
            !s.mention_menu().iter().any(|p| p.contains(".env")),
            "secrets never listed"
        );
        type_text(&mut s, "sr");
        s.on_key(key(KeyCode::Enter));
        assert_eq!(
            s.input_text(),
            "explain @src/math.rs @src/",
            "folders stay open"
        );
        assert!(!s.mention_menu().is_empty());
        s.on_key(key(KeyCode::Esc));
        assert_eq!(s.input_text(), "explain @src/math.rs ");
    }

    #[test]
    fn a_bad_mention_is_shown_and_nothing_is_sent() {
        let (_dir, mut s) = workspace_state("bad");
        let turn = s.take_turn("read @missing.rs");
        assert!(turn.is_none());
        assert_eq!(s.input_text(), "read @missing.rs", "text comes back to fix");
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("doesn't exist")));
        let ok = s.take_turn("read @src/main.rs").unwrap();
        assert_eq!(ok.notes, vec!["Read src/main.rs (1 lines)"]);
        s.start_prepared_turn(&ok);
        assert!(
            matches!(s.flush.last(), Some(Item::UserWithContext { notes, .. }) if notes.len() == 1)
        );
    }

    #[test]
    fn pasted_image_paths_and_clipboard_images_become_chips() {
        let (dir, mut s) = workspace_state("chips");
        let image = dir.join("shot.png");
        std::fs::write(&image, [0x89, b'P']).unwrap();
        s.on_paste(&format!("'{}'", image.display()));
        assert_eq!(s.input_text(), "[Image #1] ");
        s.on_paste("plain text");
        assert_eq!(s.input_text(), "[Image #1] plain text");
        s.set_clipboard(Box::new(crate::code_ui::clipboard::fake::FakeClipboard(
            Some(image.clone()),
        )));
        s.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(s.input_text().ends_with("[Image #2] "));
        let turn = s.take_turn(&s.input_text()).unwrap();
        assert_eq!(turn.uploads.len(), 2);
        assert!(s.images.is_empty(), "chips are used up by the turn");
        s.set_clipboard(Box::new(crate::code_ui::clipboard::fake::FakeClipboard(
            None,
        )));
        s.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("No image")));
    }

    #[test]
    fn typing_a_slash_lists_every_command() {
        let mut s = state();
        type_text(&mut s, "/");
        assert_eq!(s.command_menu().len(), COMMANDS.len());
        type_text(&mut s, "mo");
        let names: Vec<&str> = s.command_menu().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["/models", "/mode"]);
        type_text(&mut s, " x");
        assert!(s.command_menu().is_empty(), "arguments close the menu");
    }

    #[test]
    fn menu_keys_move_complete_and_run() {
        let mut s = state();
        type_text(&mut s, "/");
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.menu_selected, 1);
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "/new");
        let mut s = with_catalog();
        type_text(&mut s, "/mod");
        let effects = s.on_key(key(KeyCode::Enter));
        assert!(
            effects.is_empty() && s.picker.is_some(),
            "Enter runs the highlighted /models"
        );
        let mut s = state();
        type_text(&mut s, "/ex");
        s.on_key(key(KeyCode::Esc));
        assert!(s.input_text().is_empty());
    }

    fn subagent(event: crate::subagents::SubagentEvent) -> AssistantEvent {
        AssistantEvent::Subagent(event)
    }

    #[test]
    fn subagents_run_live_print_when_done_and_roll_into_analytics() {
        use crate::subagents::{SubagentEvent, SubagentUsage};
        let mut s = state();
        s.start_turn("investigate");
        for (id, name) in [("a", "Logs"), ("b", "Schema")] {
            s.apply(subagent(SubagentEvent::Started {
                id: id.into(),
                name: name.into(),
                profile: Some("data_engineer".into()),
                agent_id: None,
                task: None,
            }));
        }
        assert_eq!(s.subagents.running().count(), 2);
        assert!(!s.flush.iter().any(|i| matches!(i, Item::Subagent(_))));
        s.apply(subagent(SubagentEvent::Finished {
            id: "a".into(),
            status: SubagentStatus::Completed,
            summary: "job 4 failed on a null key".into(),
            usage: SubagentUsage::default(),
            duration: Some(std::time::Duration::from_secs(12)),
        }));
        let printed: Vec<&Subagent> = s
            .flush
            .iter()
            .filter_map(|i| match i {
                Item::Subagent(agent) => Some(agent.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(printed.len(), 1);
        assert_eq!(printed[0].name, "Logs");
        let report = crate::cmd::stream::Usage {
            subagents: vec![(
                "a".into(),
                SubagentUsage {
                    input_tokens: 900,
                    output_tokens: 120,
                    cost_usd: Some(0.002),
                },
            )],
            ..Default::default()
        };
        s.apply(AssistantEvent::Usage(report));
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        let summary = s
            .flush
            .iter()
            .find_map(|i| match i {
                Item::Summary(text) => Some(text.clone()),
                _ => None,
            })
            .unwrap();
        assert!(summary.contains("2 subagents"), "{summary}");
        let stopped = s.flush.iter().any(|i| {
            matches!(i, Item::Subagent(agent) if agent.name == "Schema" && agent.status == SubagentStatus::Cancelled)
        });
        assert!(
            stopped,
            "a subagent still running at the end is shown as stopped"
        );
        let details = usage::turn_details(s.usage.last.as_ref().unwrap())
            .plain_lines()
            .join("\n");
        assert!(details.contains("  Subagents · 2\n"), "{details}");
        assert!(
            details.contains("Logs (data_engineer)    12s · ↑ 900 ↓ 120 · $0.0020 · done"),
            "{details}"
        );
        assert_eq!(s.subagents.running().count(), 0);
    }

    #[test]
    fn subagents_command_lists_profiles_and_toggles() {
        let mut s = with_catalog();
        type_text(&mut s, "/subagents");
        s.on_key(key(KeyCode::Enter));
        let Some(Item::Panel(panel)) = s.flush.last() else {
            panic!("no overview");
        };
        let lines = panel.plain_lines();
        assert_eq!(lines[0], "Subagents · on");
        assert!(lines
            .iter()
            .any(|l| l.contains("analyst") && l.contains("lakehouse and SQL reads")));
        type_text(&mut s, "/subagents off");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::SetSubagents(false)]
        );
        assert!(!s.subagents_enabled);
        type_text(&mut s, "/subagents maybe");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Error(_))));
    }

    #[test]
    fn resume_lists_conversations_and_continues_the_chosen_one() {
        let mut s = state();
        type_text(&mut s, "/resume");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::ListConversations]
        );
        s.show_conversations(Ok(vec![
            PickerRow::Header("This folder".into()),
            PickerRow::Conversation {
                id: "c1".into(),
                title: "Jobs".into(),
                detail: "2h ago".into(),
            },
            PickerRow::Conversation {
                id: "c2".into(),
                title: "Charts".into(),
                detail: String::new(),
            },
        ]));
        assert_eq!(s.picker.as_ref().unwrap().selected, 1);
        s.on_key(key(KeyCode::Down));
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Resume {
                id: "c2".into(),
                title: "Charts".into()
            }]
        );
        assert!(s.picker.is_none());
        s.show_history(
            "Charts",
            Ok(Transcript {
                title: Some("Creating charts".into()),
                messages: vec![crate::assistant::history::Message {
                    speaker: crate::assistant::history::Speaker::You,
                    text: "make a bar chart".into(),
                }],
            }),
        );
        assert!(s
            .flush
            .iter()
            .any(|i| matches!(i, Item::Notice(n) if n.contains("Creating charts"))));
        assert!(matches!(s.flush.last(), Some(Item::User(t)) if t == "make a bar chart"));
    }

    #[test]
    fn resume_with_nothing_to_list_says_so() {
        let mut s = state();
        s.show_conversations(Ok(Vec::new()));
        assert!(s.picker.is_none());
        assert!(matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("No earlier")));
        s.show_conversations(Err("POST https://x → HTTP 500: boom".into()));
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.ends_with("HTTP 500: boom")));
    }

    #[test]
    fn subagent_work_goes_to_its_page_not_the_main_one() {
        use crate::subagents::SubagentEvent;
        let mut s = state();
        s.start_turn("check the repo");
        s.apply(subagent(SubagentEvent::Started {
            id: "sa_1".into(),
            name: "Edit config".into(),
            profile: Some("general".into()),
            agent_id: None,
            task: Some("Fix the port in config.toml".into()),
        }));
        s.apply(AssistantEvent::ForSubagent {
            id: "sa_1".into(),
            event: Box::new(started("edit_file", "Update config.toml")),
        });
        s.apply(AssistantEvent::ForSubagent {
            id: "sa_1".into(),
            event: Box::new(finished(
                "edit_file",
                "Update config.toml",
                "1 line changed",
                Some("--- a/config.toml\n+++ b/config.toml\n@@ -1 +1 @@\n-port = 1\n+port = 2\n"),
            )),
        });
        assert!(
            !s.flush.iter().any(|i| matches!(i, Item::Tool(_))),
            "subagent tools stay off the main page"
        );
        assert!(s.running.is_empty());
        let page = s.pages.get(0).expect("a page per subagent");
        let shown: String = page
            .items()
            .iter()
            .flat_map(|i| i.render_expanded(80))
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(shown.contains("Fix the port in config.toml"), "{shown}");
        assert!(
            shown.contains("port = 2"),
            "the edit's diff is on the page: {shown}"
        );
    }

    #[test]
    fn shift_arrows_cycle_main_and_subagent_pages() {
        use crate::subagents::SubagentEvent;
        let mut s = state();
        s.start_turn("go");
        for i in 0..2 {
            s.apply(subagent(SubagentEvent::Started {
                id: format!("sa_{i}"),
                name: format!("Agent {i}"),
                profile: None,
                agent_id: None,
                task: None,
            }));
        }
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT);
        let page = |s: &UiState| s.agent_view.as_ref().map(|v| v.index);
        s.on_key(down);
        assert_eq!(page(&s), Some(0));
        s.on_key(down);
        assert_eq!(page(&s), Some(1));
        s.on_key(down);
        assert_eq!(page(&s), None, "past the last subagent is main again");
        s.on_key(up);
        assert_eq!(page(&s), Some(1));
        s.on_key(key(KeyCode::Esc));
        assert_eq!(page(&s), None, "esc goes back to main");
        type_text(&mut s, "/new");
        s.on_key(key(KeyCode::Enter));
        assert!(s.pages.is_empty(), "a new conversation has no pages");
    }

    #[test]
    fn slash_commands() {
        let mut s = state();
        type_text(&mut s, "/clear");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::ClearScreen, Effect::NewConversation]
        );
        type_text(&mut s, "/nope");
        s.on_key(key(KeyCode::Enter));
        assert!(matches!(s.flush.last(), Some(Item::Error(_))));
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            vec![Effect::Exit]
        );
    }

    fn ask(
        access: Access,
        summary: &str,
        preview: Option<&str>,
    ) -> (Ask, tokio::sync::oneshot::Receiver<Answer>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let request = PermissionRequest {
            access,
            summary: summary.into(),
            preview: preview.map(str::to_string),
            chat_scope: "all edits this session".into(),
            always_scope: Some("always scope".into()),
            action: None,
            asked_by: None,
        };
        (Ask { request, reply: tx }, rx)
    }

    /// Streams `text` one character at a time and returns everything the
    /// answer printed into scrollback.
    fn stream_answer(text: &str) -> Vec<Item> {
        let mut s = state();
        s.start_turn("q");
        s.flush.clear();
        for ch in text.chars() {
            s.apply(AssistantEvent::Text(ch.to_string()));
        }
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        s.flush
            .into_iter()
            .filter(|i| matches!(i, Item::Assistant(_) | Item::AssistantMore(_)))
            .collect()
    }

    const LONG_ANSWER: &str = "## Plan\n\nFirst we read the files.\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\n1. one\n\n2. two\n\n- tight\n- list\n\n| file | lines |\n|---|---:|\n| a.rs | 3 |\n\n> a quote\n\nDone with **bold** and `code`.";

    #[test]
    fn streamed_chunks_print_exactly_like_the_whole_answer() {
        let chunks = stream_answer(LONG_ANSWER);
        assert!(
            chunks.len() > 3,
            "long answers commit in pieces: {chunks:?}"
        );
        for width in [40, 80, 120] {
            let pieces: Vec<String> = chunks
                .iter()
                .flat_map(|item| item.render(width))
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            let whole: Vec<String> = Item::Assistant(LONG_ANSWER.into())
                .render(width)
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            assert_eq!(pieces, whole, "width {width}");
        }
    }

    #[test]
    fn finished_blocks_print_before_the_answer_ends() {
        let mut s = state();
        s.start_turn("q");
        s.flush.clear();
        s.apply(AssistantEvent::Text(
            "Para one.\n\nPara two starts\n".into(),
        ));
        assert_eq!(s.flush, vec![Item::Assistant("Para one.".into())]);
        assert_eq!(
            s.streaming_item(),
            Some(Item::AssistantMore("Para two starts".into())),
            "the rest stays live, without a second bullet"
        );
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert_eq!(
            s.transcript
                .iter()
                .rfind(|i| matches!(i, Item::Assistant(_))),
            Some(&Item::Assistant("Para one.\n\nPara two starts".into())),
            "the transcript keeps the answer whole"
        );
    }

    #[test]
    fn usage_reports_feed_the_summary_and_last() {
        let mut s = state();
        s.start_turn("q");
        s.apply(AssistantEvent::Text("hello".into()));
        s.apply(AssistantEvent::Usage(crate::cmd::stream::Usage {
            tokens: Some(crate::cmd::stream::TokenCounts {
                input: 900,
                output: 120,
                cache_read: 100,
                cache_write: 0,
            }),
            cost_usd: Some(0.004),
            ..Default::default()
        }));
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        let summary = s.flush.iter().find_map(|i| match i {
            Item::Summary(line) => Some(line.clone()),
            _ => None,
        });
        assert!(summary.unwrap().contains("↑ 1.0k ↓ 120 tokens · $0.0040"));
        s.flush.clear();
        type_text(&mut s, "/last");
        s.on_key(key(KeyCode::Enter));
        let Some(Item::Panel(panel)) = s.flush.last() else {
            panic!("no analytics: {:?}", s.flush);
        };
        let lines = panel.plain_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("cost") && l.ends_with("$0.0040")),
            "{lines:?}"
        );
    }

    #[test]
    fn usage_command_shows_the_session_and_fetches_the_account() {
        let mut s = state();
        type_text(&mut s, "/usage");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::FetchAccountUsage]
        );
        assert!(matches!(&s.flush[0], Item::Panel(panel) if panel.title == "This session"));
        assert!(s.account_loading, "the live area shows it's loading");
        s.show_account_usage(Err("offline".into()));
        assert!(!s.account_loading);
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("offline")));
    }

    #[test]
    fn keys_typed_during_a_prompt_land_in_the_input_after_it() {
        let mut s = state();
        let (prompt, mut answer) = ask(Access::Write, "edit_file a.rs", None);
        s.open_prompt(prompt);
        type_text(&mut s, "also ");
        assert!(s.prompt.is_some(), "typing doesn't answer the prompt");
        s.on_key(key(KeyCode::Char('y')));
        assert_eq!(answer.try_recv().unwrap(), (Decision::AllowOnce, None));
        assert_eq!(s.input_text(), "also ");
    }

    #[test]
    fn transcript_keeps_full_detail_and_ctrl_o_toggles_it() {
        let mut s = state();
        s.start_turn("look");
        s.apply(started("read_file", "read_file a.rs"));
        s.apply(finished(
            "read_file",
            "read_file a.rs",
            "line one\nline two",
            None,
        ));
        s.apply(started("read_file", "read_file b.rs"));
        s.apply(finished("read_file", "read_file b.rs", "b body", None));
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        let tools: Vec<&ToolItem> = s
            .transcript
            .iter()
            .filter_map(|i| match i {
                Item::Tool(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tools.len(), 2, "grouped reads are kept one by one");
        assert_eq!(tools[0].output.as_deref(), Some("line one\nline two"));
        assert!(s
            .transcript
            .iter()
            .any(|i| matches!(i, Item::Panel(p) if p.title == "Turn analytics")));
        let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        s.start_turn("more");
        s.apply(started("read_file", "read_file c.rs"));
        s.apply(finished("read_file", "read_file c.rs", "c body", None));
        s.apply(AssistantEvent::Text("partial".into()));
        let live = s.transcript_items();
        assert!(
            live.iter()
                .any(|i| matches!(i, Item::Tool(t) if t.output.as_deref() == Some("c body"))),
            "pending reads show while the turn runs"
        );
        assert_eq!(live.last(), Some(&Item::Assistant("partial".into())));
        s.on_key(ctrl_o);
        assert!(s.viewer.is_some());
        type_text(&mut s, "x");
        assert_eq!(s.input_text(), "", "keys go to the viewer while it's open");
        s.on_key(key(KeyCode::Esc));
        assert!(s.viewer.is_none());
    }

    #[test]
    fn decisions_are_kept_for_the_transcript() {
        let mut s = state();
        let (prompt, _answer) = ask(
            Access::Script,
            "run_script python (2 lines)",
            Some("import gz\nprint(1)"),
        );
        s.open_prompt(prompt);
        s.on_key(key(KeyCode::Char('1')));
        assert!(matches!(
            s.transcript.last(),
            Some(Item::Decision { choice, preview: Some(code), .. })
                if choice == "allowed once" && code.contains("import gz")
        ));
    }
}
