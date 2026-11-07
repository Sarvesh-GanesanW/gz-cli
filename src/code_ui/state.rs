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
use crate::assistant::models::{Catalog, Model, Selection};
use crate::assistant::sandbox::Sandbox;
use crate::assistant::tools::Access;
use crate::assistant::{AssistantEvent, Decision, PermissionRequest, TurnEnd};
use crate::code_ui::approver::{Answer, Ask, Mode};
use crate::code_ui::clipboard::{ImageClipboard, SystemClipboard};
use crate::code_ui::items::{Item, ToolItem, ToolStatus};
use crate::code_ui::labels::{self, Label};
use crate::code_ui::mentions::{self, FileIndex};

/// A second Ctrl+C within this window exits.
const EXIT_WINDOW: Duration = Duration::from_secs(2);
const SCRIPT_CHILD_PREFIX: &str = "script · ";

/// Every slash command with its one-line description: the source for both
/// `/help` and the menu that opens when the input starts with `/`.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show this help"),
    ("/new", "start a new conversation"),
    ("/clear", "new conversation and a clean screen"),
    (
        "/models",
        "choose the model (Groundzero modes or your LLM connections)",
    ),
    (
        "/effort",
        "choose the reasoning effort for the current model",
    ),
    ("/mode", "cycle permission mode (also shift+tab)"),
    ("/exit", "quit (also ctrl+d, or ctrl+c twice)"),
];

fn help_lines() -> Vec<String> {
    COMMANDS
        .iter()
        .map(|(name, what)| format!("{name:<9}{what}"))
        .collect()
}

pub const SHORTCUTS: &[&str] = &[
    "enter send · shift/alt+enter newline · ↑↓ history",
    "esc interrupt · ctrl+u clear input · ctrl+c ctrl+c exit",
    "shift+tab cycle mode: ask → accept edits → plan (read-only)",
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
}

/// One row of the model or effort picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerRow {
    Header(String),
    Model(Model),
    Effort(String),
}

/// The `/models` and `/effort` list.
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

impl PromptView {
    pub const OPTIONS: usize = 3;

    pub fn options(&self) -> [String; 3] {
        [
            "Yes".to_string(),
            format!(
                "Yes, and don't ask again for {}",
                self.request.session_scope
            ),
            "No, and tell gz what to do differently (esc)".to_string(),
        ]
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
            model: None,
            picker: None,
            menu_selected: 0,
            sandbox: None,
            files: None,
            mention_selected: 0,
            images: Vec::new(),
            clipboard: Box::new(SystemClipboard),
        }
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
            self.flush.push(Item::Error(
                "The model list couldn't be loaded; check `gz doctor`".into(),
            ));
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

    fn choose_model(&mut self, selection: Selection) {
        self.flush
            .push(Item::Notice(format!("Model: {}", selection.label())));
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
        self.flush.push(item);
        self.busy_since = Some(Instant::now());
        self.received_chars = 0;
        self.turns += 1;
    }

    pub fn apply(&mut self, event: AssistantEvent) -> Vec<Effect> {
        match event {
            AssistantEvent::Text(text) => {
                self.close_group();
                self.received_chars += text.chars().count();
                self.streaming.push_str(&text);
            }
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
                diff,
            } => self.tool_finished(&name, &summary, is_error, &detail, diff),
            AssistantEvent::Notice(text) => {
                self.close_text();
                self.close_group();
                self.flush.push(Item::Notice(text));
            }
            AssistantEvent::Done(end) => return self.finish_turn(end),
        }
        Vec::new()
    }

    fn tool_started(&mut self, name: &str, summary: &str, local: bool) {
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
        };
        if local {
            self.running.push(item);
        } else {
            // Server-side tools report only that they started.
            self.flush.push(Item::Tool(ToolItem {
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
            },
        };
        item.status = if failed {
            ToolStatus::Failed
        } else {
            ToolStatus::Ok
        };
        item.result = Some(labels::result_line(name, summary, detail, diff.as_deref()));
        item.diff = diff;
        if label.read_only && !failed {
            self.group.push(item);
        } else {
            self.close_group();
            self.flush.push(Item::Tool(item));
        }
    }

    fn finish_turn(&mut self, end: TurnEnd) -> Vec<Effect> {
        self.close_text();
        self.close_group();
        for mut tool in self.running.drain(..) {
            tool.status = ToolStatus::Failed;
            tool.result.get_or_insert_with(|| "Stopped".to_string());
            self.flush.push(Item::Tool(tool));
        }
        match end {
            TurnEnd::Completed => {}
            TurnEnd::Cancelled => self
                .flush
                .push(Item::Notice("⎿  Interrupted by user".into())),
            TurnEnd::Failed(why) => self.flush.push(Item::Error(why)),
        }
        self.busy_since = None;
        if let Some(prompt) = self.prompt.take() {
            drop(prompt); // the run ended; an open prompt can no longer matter
        }
        match self.queue.pop_front() {
            Some(next) => vec![Effect::Send(next)],
            None => Vec::new(),
        }
    }

    fn close_text(&mut self) {
        let text = std::mem::take(&mut self.streaming);
        if !text.trim().is_empty() {
            self.flush
                .push(Item::Assistant(text.trim_end().to_string()));
        }
    }

    /// Prints pending read-only calls: one in full, several as a summary.
    fn close_group(&mut self) {
        match self.group.len() {
            0 => {}
            1 => {
                let item = self.group.remove(0);
                self.flush.push(Item::Tool(item));
            }
            _ => {
                let labels: Vec<Label> = self.group.drain(..).map(|t| t.label).collect();
                self.flush.push(Item::Group(labels::group_summary(&labels)));
            }
        }
    }

    // ---------- permission prompts ----------

    pub fn open_prompt(&mut self, ask: Ask) {
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
        if let Some(mut prompt) = self.prompt.take() {
            if let Some(reply) = prompt.reply.take() {
                reply.send(answer).ok();
            }
        }
    }

    fn choose(&mut self, option: usize) {
        match option {
            0 => self.answer((Decision::AllowOnce, None)),
            1 => self.answer((Decision::AllowSession, None)),
            _ => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.selected = 2;
                    prompt.note = Some(String::new());
                }
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
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => prompt.selected = prompt.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                prompt.selected = (prompt.selected + 1).min(PromptView::OPTIONS - 1)
            }
            KeyCode::Enter => {
                let selected = prompt.selected;
                self.choose(selected);
            }
            KeyCode::Char(c @ '1'..='3') => self.choose(c as usize - '1' as usize),
            KeyCode::Char('y') => self.choose(0),
            KeyCode::Char('n') | KeyCode::Esc => self.choose(2),
            KeyCode::BackTab if prompt.request.access == Access::Write => {
                if let Ok(mut mode) = self.mode.lock() {
                    *mode = Mode::AcceptEdits;
                }
                self.answer((Decision::AllowSession, None));
            }
            _ => {}
        }
    }

    // ---------- keys ----------

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.picker.is_some() {
            return self.on_picker_key(key);
        }
        if self.prompt.is_some() {
            if ctrl && key.code == KeyCode::Char('c') {
                self.answer((Decision::Deny, None));
                return vec![Effect::Cancel];
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

    fn slash(&mut self, command: &str) -> Vec<Effect> {
        match command.split_whitespace().next().unwrap_or_default() {
            "/help" => {
                self.flush.push(Item::Info(help_lines()));
                Vec::new()
            }
            "/new" => {
                self.flush.push(Item::Notice("New conversation".into()));
                vec![Effect::NewConversation]
            }
            "/clear" => vec![Effect::ClearScreen, Effect::NewConversation],
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
                    _ => self.flush.push(Item::Error(
                        "This model has no reasoning-effort levels".into(),
                    )),
                }
                Vec::new()
            }
            "/mode" => {
                self.cycle_mode();
                let name = self.mode().banner().unwrap_or("ask before changes");
                self.flush.push(Item::Notice(format!("Mode: {name}")));
                Vec::new()
            }
            other => {
                self.flush.push(Item::Error(format!(
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
            Err(err) => self.flush.push(Item::Error(format!("{err:#}"))),
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
                self.flush.push(Item::Error(format!("{err:#}")));
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
        assert_eq!(
            s.flush,
            vec![
                Item::User("look".into()),
                Item::Assistant("Let me look.".into()),
                Item::Group("Read 2 files".into()),
                Item::Assistant("Done.".into()),
            ]
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
                session_scope: "all edits this session".into(),
            },
            reply: tx,
        });
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        assert_eq!(
            s.prompt.as_ref().unwrap().note.as_deref(),
            Some(""),
            "option 3 asks for a note"
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
                session_scope: "this exact command".into(),
            },
            reply: tx,
        });
        s.on_key(key(KeyCode::Char('2')));
        assert_eq!(rx.try_recv().unwrap(), (Decision::AllowSession, None));
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
                session_scope: "all edits this session".into(),
            },
            reply: tx,
        });
        s.on_key(key(KeyCode::BackTab));
        assert_eq!(rx.try_recv().unwrap().0, Decision::AllowSession);
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
}
