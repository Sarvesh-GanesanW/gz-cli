use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use serde_json::Value;

use crate::cmd::new_uuid;
use crate::config::Resolved;
use crate::tui::chat::{ActiveRun, ChatFocus, ChatView, Conversation, RunRequest};
use crate::tui::fetch::{FetchReq, FetchTarget, Request};
use crate::tui::model::Grid;
use crate::tui::nav::{self, GridSpec, ResourceId, Source, View, MODULES};
use crate::tui::palette::{Action, Entry, Palette};

const RECENT_LIMIT: usize = 8;
const WHEEL_STEP: isize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarRow {
    Module(usize),
    Resource(ResourceId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Ok,
    Error,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub text: String,
    pub tone: Tone,
}

/// One level of a drill-down: what was opened and its rows.
#[derive(Debug, Clone, Default)]
pub struct Level {
    pub label: String,
    pub id: String,
    pub grid: Grid,
}

/// A table resource with its back stack. `levels[0]` is the resource root.
#[derive(Debug, Clone)]
pub struct GridView {
    pub levels: Vec<Level>,
    pub loaded: bool,
    /// The request whose answer this view waits for; others are stale.
    awaiting: Option<u64>,
    pub error: Option<String>,
}

impl GridView {
    fn new(label: &str) -> Self {
        Self {
            levels: vec![Level {
                label: label.to_string(),
                ..Level::default()
            }],
            loaded: false,
            awaiting: None,
            error: None,
        }
    }

    pub fn depth(&self) -> usize {
        self.levels.len() - 1
    }

    pub fn grid(&self) -> &Grid {
        &self.levels[self.depth()].grid
    }

    pub fn grid_mut(&mut self) -> &mut Grid {
        let depth = self.depth();
        &mut self.levels[depth].grid
    }

    pub fn is_loading(&self) -> bool {
        self.awaiting.is_some()
    }

    /// Ids picked along the drill trail.
    fn trail_ids(&self) -> Vec<String> {
        self.levels[1..].iter().map(|l| l.id.clone()).collect()
    }
}

/// Screen areas from the last frame, for mouse hit-testing.
#[derive(Debug, Clone, Copy, Default)]
pub struct Hits {
    /// Sidebar rows area (inside borders and padding).
    pub sidebar_rows: Rect,
    pub sidebar_offset: usize,
    /// Data rows of the grid (below its header).
    pub grid_rows: Rect,
    pub chat_list: Rect,
    pub chat_list_offset: usize,
    pub transcript: Rect,
    pub content: Rect,
}

pub struct Modal {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: usize,
    /// Visible text rows, updated by the renderer every frame.
    pub viewport: usize,
}

impl Modal {
    pub fn json(title: String, value: &Value) -> Self {
        let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "?".to_string());
        Self {
            title,
            lines: text.lines().map(str::to_string).collect(),
            scroll: 0,
            viewport: 20,
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.lines.len().saturating_sub(self.viewport);
        self.scroll = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
    }
}

pub struct App {
    pub resolved: Resolved,
    pub focus: Focus,
    pub expanded: Vec<bool>,
    pub cursor: usize,
    pub active: ResourceId,
    pub grids: HashMap<ResourceId, GridView>,
    pub chat: ChatView,
    pub palette: Option<Palette>,
    pub help: bool,
    pub modal: Option<Modal>,
    awaiting_modal: Option<u64>,
    awaiting_chat_list: Option<u64>,
    awaiting_transcript: Option<u64>,
    pub editing_filter: bool,
    pub status: Status,
    pub outbox: Vec<Request>,
    next_id: u64,
    pub tick: u64,
    pub quit: bool,
    recent: Vec<Entry>,
    pub hits: Hits,
    /// Data rows visible in the grid, updated by the renderer.
    pub grid_viewport: usize,
}

const CHAT: ResourceId = ResourceId {
    module: 0,
    resource: 0,
};

impl App {
    pub fn new(resolved: Resolved) -> Self {
        let mut expanded = vec![false; MODULES.len()];
        expanded[CHAT.module] = true;
        let mut app = Self {
            resolved,
            focus: Focus::Sidebar,
            expanded,
            cursor: 0,
            active: CHAT,
            grids: HashMap::new(),
            chat: ChatView::default(),
            palette: None,
            help: false,
            modal: None,
            awaiting_modal: None,
            awaiting_chat_list: None,
            awaiting_transcript: None,
            editing_filter: false,
            status: Status {
                text: "↑↓ browse · → open · Ctrl+K jump anywhere · ? keys".to_string(),
                tone: Tone::Info,
            },
            outbox: Vec::new(),
            next_id: 1,
            tick: 0,
            quit: false,
            recent: Vec::new(),
            hits: Hits::default(),
            grid_viewport: 20,
        };
        app.cursor = app.row_of(CHAT).unwrap_or(0);
        app.open(CHAT);
        app
    }

    // ---------- state queries ----------

    pub fn signed_in(&self) -> bool {
        self.resolved.token().is_some() && !self.resolved.token_expired()
    }

    pub fn is_busy(&self) -> bool {
        self.awaiting_modal.is_some()
            || self.awaiting_chat_list.is_some()
            || self.awaiting_transcript.is_some()
            || self.chat.is_streaming()
            || self.grids.values().any(GridView::is_loading)
    }

    pub fn is_chat(&self) -> bool {
        matches!(self.active.resource().view, View::Chat)
    }

    pub fn view(&self) -> Option<&GridView> {
        self.grids.get(&self.active)
    }

    fn view_mut(&mut self) -> Option<&mut GridView> {
        self.grids.get_mut(&self.active)
    }

    fn spec(&self) -> Option<GridSpec> {
        match self.active.resource().view {
            View::Grid(spec) => Some(spec),
            View::Chat => None,
        }
    }

    pub fn sidebar_rows(&self) -> Vec<SidebarRow> {
        let mut rows = Vec::new();
        for (module, m) in MODULES.iter().enumerate() {
            rows.push(SidebarRow::Module(module));
            if self.expanded[module] {
                rows.extend(
                    (0..m.resources.len())
                        .map(|resource| SidebarRow::Resource(ResourceId { module, resource })),
                );
            }
        }
        rows
    }

    fn row_of(&self, id: ResourceId) -> Option<usize> {
        self.sidebar_rows()
            .iter()
            .position(|r| *r == SidebarRow::Resource(id))
    }

    /// The user is typing, so letter keys are text, not shortcuts.
    fn is_typing(&self) -> bool {
        self.editing_filter
            || (self.focus == Focus::Content
                && self.is_chat()
                && self.chat.focus == ChatFocus::Composer)
    }

    pub fn breadcrumb(&self) -> Vec<String> {
        let mut parts = vec![
            self.active.module().title.to_string(),
            self.active.resource().label.to_string(),
        ];
        if let Some(view) = self.view() {
            parts.extend(view.levels[1..].iter().map(|l| l.label.clone()));
        }
        if self.is_chat() {
            if let Some(open) = &self.chat.open {
                parts.push(open.title.clone());
            }
        }
        parts
    }

    fn set_status(&mut self, tone: Tone, text: impl Into<String>) {
        self.status = Status {
            text: text.into(),
            tone,
        };
    }

    // ---------- requests ----------

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn fetch(&mut self, target: FetchTarget, source: &Source, ids: &[String]) -> u64 {
        let id = self.next_id();
        let (path, body) = source.resolve(ids);
        self.outbox.push(Request::Fetch(FetchReq {
            id,
            target,
            service: source.service.to_string(),
            method: source.verb.method(),
            path,
            body,
        }));
        id
    }

    fn fetch_get(&mut self, target: FetchTarget, service: &str, path: String) -> u64 {
        let id = self.next_id();
        self.outbox.push(Request::Fetch(FetchReq {
            id,
            target,
            service: service.to_string(),
            method: reqwest::Method::GET,
            path,
            body: None,
        }));
        id
    }

    pub fn fetched(&mut self, id: u64, target: FetchTarget, result: Result<Value, String>) {
        match target {
            FetchTarget::Grid(resource) => self.grid_fetched(resource, id, result),
            FetchTarget::Detail { title } => {
                if self.awaiting_modal != Some(id) {
                    return;
                }
                self.awaiting_modal = None;
                match result {
                    Ok(value) => self.modal = Some(Modal::json(title, &value)),
                    Err(err) => self.set_status(Tone::Error, friendly_error(&err)),
                }
            }
            FetchTarget::ChatList => {
                if self.awaiting_chat_list != Some(id) {
                    return;
                }
                self.awaiting_chat_list = None;
                match result {
                    Ok(value) => {
                        self.chat.set_conversations(&value);
                        self.chat.error = None;
                    }
                    Err(err) => self.chat.error = Some(friendly_error(&err)),
                }
            }
            FetchTarget::ChatTranscript { conversation_id } => {
                let still_open = self
                    .chat
                    .open
                    .as_ref()
                    .is_some_and(|c| c.id == conversation_id);
                if self.awaiting_transcript != Some(id) || !still_open {
                    return;
                }
                self.awaiting_transcript = None;
                match result {
                    Ok(value) => self.chat.set_transcript(&value),
                    Err(err) => self.chat.error = Some(friendly_error(&err)),
                }
            }
        }
    }

    fn grid_fetched(&mut self, resource: ResourceId, id: u64, result: Result<Value, String>) {
        let Some(view) = self.grids.get_mut(&resource) else {
            return;
        };
        if view.awaiting != Some(id) {
            return;
        }
        view.awaiting = None;
        match result {
            Ok(value) => {
                view.loaded = true;
                view.error = None;
                let filter = view.grid().filter.clone();
                let grid = view.grid_mut();
                grid.replace(&value);
                grid.set_filter(&filter);
                let rows = grid.visible_len();
                if resource == self.active {
                    self.set_status(Tone::Ok, format!("Loaded {rows} row(s)"));
                }
            }
            Err(err) => {
                view.error = Some(friendly_error(&err));
                if resource == self.active {
                    self.set_status(Tone::Error, "Request failed · r to retry");
                }
            }
        }
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    // ---------- navigation ----------

    /// Shows a resource, loading it the first time.
    fn open(&mut self, id: ResourceId) {
        self.active = id;
        self.editing_filter = false;
        match id.resource().view {
            View::Chat => {
                if !self.chat.list_loaded && self.awaiting_chat_list.is_none() {
                    self.load_chat_list();
                }
            }
            View::Grid(_) => {
                let view = self
                    .grids
                    .entry(id)
                    .or_insert_with(|| GridView::new(id.resource().label));
                if !view.loaded && !view.is_loading() {
                    self.reload();
                }
            }
        }
    }

    fn open_and_focus(&mut self, id: ResourceId) {
        self.expanded[id.module] = true;
        if let Some(row) = self.row_of(id) {
            self.cursor = row;
        }
        self.open(id);
        self.focus = Focus::Content;
    }

    /// Refetches what the current view shows.
    fn reload(&mut self) {
        if self.is_chat() {
            self.load_chat_list();
            if let Some(open) = self.chat.open.clone() {
                self.load_transcript(&open.id);
            }
            return;
        }
        let Some(spec) = self.spec() else { return };
        let resource = self.active;
        let Some(view) = self.grids.get(&resource) else {
            return;
        };
        let depth = view.depth();
        let ids = view.trail_ids();
        let source = if depth == 0 {
            spec.source
        } else {
            spec.drills[depth - 1].source
        };
        let id = self.fetch(FetchTarget::Grid(resource), &source, &ids);
        if let Some(view) = self.grids.get_mut(&resource) {
            view.awaiting = Some(id);
            view.error = None;
        }
        self.set_status(Tone::Info, "Loading…");
    }

    fn move_cursor(&mut self, delta: isize) {
        let rows = self.sidebar_rows();
        let last = rows.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
        // Resources preview as the cursor lands on them.
        if let SidebarRow::Resource(id) = rows[self.cursor] {
            self.open(id);
        }
    }

    fn sidebar_enter(&mut self) {
        match self.sidebar_rows()[self.cursor] {
            SidebarRow::Module(m) if !self.expanded[m] => self.expanded[m] = true,
            SidebarRow::Module(m) => {
                self.cursor += 1;
                self.open_and_focus(ResourceId {
                    module: m,
                    resource: 0,
                });
            }
            SidebarRow::Resource(id) => self.open_and_focus(id),
        }
    }

    fn sidebar_left(&mut self) {
        match self.sidebar_rows()[self.cursor] {
            SidebarRow::Module(m) => self.expanded[m] = false,
            SidebarRow::Resource(id) => {
                if let Some(row) = self
                    .sidebar_rows()
                    .iter()
                    .position(|r| *r == SidebarRow::Module(id.module))
                {
                    self.cursor = row;
                }
            }
        }
    }

    fn toggle_module(&mut self) {
        if let SidebarRow::Module(m) = self.sidebar_rows()[self.cursor] {
            self.expanded[m] = !self.expanded[m];
        }
    }

    /// Enter on a grid row: drill a level deeper, or open its details.
    fn open_row(&mut self) {
        let Some(spec) = self.spec() else { return };
        let Some(view) = self.view() else { return };
        let depth = view.depth();
        let Some(row) = view.grid().selected_raw().cloned() else {
            self.set_status(Tone::Error, "Nothing selected");
            return;
        };
        if let Some(drill) = spec.drills.get(depth) {
            let Some(id) = nav::row_id(&row, drill.id_keys) else {
                self.set_status(Tone::Error, format!("Select a {} first", drill.noun));
                return;
            };
            let label = if id.is_empty() {
                "(root)".to_string()
            } else {
                id.clone()
            };
            if let Some(view) = self.view_mut() {
                view.levels.push(Level {
                    label,
                    id,
                    grid: Grid::default(),
                });
            }
            self.reload();
            return;
        }
        let mut ids = view.trail_ids();
        match (spec.detail, nav::row_id(&row, nav::ID_KEYS)) {
            (Some(detail), Some(id)) => {
                let title = format!("{} · {id}", self.active.resource().label);
                ids.push(id);
                let req = self.fetch(FetchTarget::Detail { title }, &detail, &ids);
                self.awaiting_modal = Some(req);
                self.set_status(Tone::Info, "Loading details…");
            }
            _ => {
                let title = format!("{} · row", self.active.resource().label);
                self.modal = Some(Modal::json(title, &row));
            }
        }
    }

    /// One level up; at the root, back to the sidebar.
    fn back(&mut self) {
        let popped = self
            .view_mut()
            .filter(|v| v.depth() > 0)
            .map(|v| v.levels.pop())
            .is_some();
        if popped {
            if let Some(view) = self.view_mut() {
                view.awaiting = None;
                view.error = None;
            }
            self.set_status(Tone::Info, self.breadcrumb().join(" › "));
        } else {
            self.focus = Focus::Sidebar;
        }
    }

    // ---------- chat ----------

    fn load_chat_list(&mut self) {
        let id = self.fetch_get(
            FetchTarget::ChatList,
            "chat",
            "/chat/conversations?assistantMode=chat".to_string(),
        );
        self.awaiting_chat_list = Some(id);
    }

    fn load_transcript(&mut self, conversation_id: &str) {
        let path = format!("/chat/conversations/{}", crate::cmd::seg(conversation_id));
        let id = self.fetch_get(
            FetchTarget::ChatTranscript {
                conversation_id: conversation_id.to_string(),
            },
            "chat",
            path,
        );
        self.awaiting_transcript = Some(id);
    }

    fn open_conversation(&mut self, conversation: Conversation) {
        if self.chat.is_streaming() {
            self.set_status(Tone::Error, "A reply is still streaming · Esc cancels it");
            return;
        }
        self.remember(Entry {
            label: conversation.title.clone(),
            hint: "recent chat",
            action: Action::OpenConversation {
                id: conversation.id.clone(),
                title: conversation.title.clone(),
            },
        });
        let id = conversation.id.clone();
        self.chat.open_conversation(conversation);
        self.chat.focus = ChatFocus::Composer;
        self.load_transcript(&id);
    }

    fn new_chat(&mut self) {
        if self.chat.is_streaming() {
            self.set_status(Tone::Error, "A reply is still streaming · Esc cancels it");
            return;
        }
        self.open_and_focus(CHAT);
        self.chat.new_chat();
        self.set_status(Tone::Info, "New chat · type your question and press Enter");
    }

    fn send_prompt(&mut self) {
        let Some((conversation_id, prompt, title)) = self.chat.take_prompt(new_uuid) else {
            return;
        };
        let tag = self.next_id();
        self.chat.run = Some(ActiveRun { tag, run_id: None });
        self.outbox.push(Request::StartChat(RunRequest {
            tag,
            conversation_id,
            prompt,
            title,
        }));
        self.set_status(Tone::Info, "Thinking… · Esc to cancel");
    }

    fn cancel_run(&mut self) {
        if let Some(run) = &self.chat.run {
            self.outbox.push(Request::CancelChat {
                tag: run.tag,
                run_id: run.run_id.clone(),
            });
            self.set_status(Tone::Info, "Cancelling…");
        }
    }

    pub fn chat_update(&mut self, tag: u64, update: crate::tui::chat::ChatUpdate) {
        if self.chat.run.as_ref().map(|r| r.tag) != Some(tag) {
            return;
        }
        let ending = matches!(
            update,
            crate::tui::chat::ChatUpdate::Finished { .. }
                | crate::tui::chat::ChatUpdate::Failed(_)
                | crate::tui::chat::ChatUpdate::Paused(_)
        );
        self.chat.apply(update);
        if ending {
            // The list picks up the new chat's title and ordering.
            self.load_chat_list();
            self.set_status(Tone::Ok, "Reply finished");
        }
    }

    fn remember(&mut self, entry: Entry) {
        self.recent.retain(|e| e.action != entry.action);
        self.recent.insert(0, entry);
        self.recent.truncate(RECENT_LIMIT);
    }

    // ---------- palette ----------

    fn run_action(&mut self, action: Action) {
        match action {
            Action::Open(id) => self.open_and_focus(id),
            Action::OpenConversation { id, title } => {
                self.open_and_focus(CHAT);
                self.open_conversation(Conversation { id, title });
            }
            Action::NewChat => self.new_chat(),
            Action::Reload => self.reload(),
            Action::Help => self.help = true,
            Action::SignOutHint => self.set_status(
                Tone::Info,
                "To sign out, quit and run `gz auth logout` (then `gz auth login` to switch users).",
            ),
            Action::Quit => self.quit = true,
        }
    }

    // ---------- keys ----------

    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            if self.chat.is_streaming() {
                self.cancel_run();
            } else {
                self.quit = true;
            }
            return;
        }
        if self.palette.is_some() {
            return self.on_palette_key(key);
        }
        if self.modal.is_some() {
            return self.on_modal_key(key);
        }
        if self.help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
            ) {
                self.help = false;
            }
            return;
        }
        if ctrl && key.code == KeyCode::Char('k') {
            self.palette = Some(Palette::new(&self.recent));
            return;
        }
        if self.editing_filter {
            return self.on_filter_key(key);
        }
        if self.is_typing() {
            return self.on_composer_key(key);
        }
        match key.code {
            KeyCode::Char(':') => self.palette = Some(Palette::new(&self.recent)),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Content,
                    Focus::Content => Focus::Sidebar,
                }
            }
            KeyCode::Char('r') => self.reload(),
            _ => match self.focus {
                Focus::Sidebar => self.on_sidebar_key(key),
                Focus::Content if self.is_chat() => self.on_chat_list_key(key),
                Focus::Content => self.on_grid_key(key),
            },
        }
    }

    fn on_sidebar_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => self.sidebar_enter(),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => self.sidebar_left(),
            KeyCode::Char(' ') => self.toggle_module(),
            KeyCode::Char('n') => self.new_chat(),
            _ => {}
        }
    }

    fn on_grid_key(&mut self, key: KeyEvent) {
        let page = self.grid_viewport;
        let Some(view) = self.view_mut() else { return };
        let grid = view.grid_mut();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => grid.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => grid.move_sel(1),
            KeyCode::PageUp => grid.page(page, false),
            KeyCode::PageDown => grid.page(page, true),
            KeyCode::Home | KeyCode::Char('g') => grid.home(),
            KeyCode::End | KeyCode::Char('G') => grid.end(),
            KeyCode::Char(']') => grid.scroll_cols(1),
            KeyCode::Char('[') => grid.scroll_cols(-1),
            KeyCode::Char('/') => self.editing_filter = true,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open_row(),
            KeyCode::Esc if !grid.filter.is_empty() => grid.set_filter(""),
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.back(),
            _ => {}
        }
    }

    fn on_chat_list_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.chat.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.chat.move_sel(1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(conversation) = self.chat.selected_conversation().cloned() {
                    self.open_conversation(conversation);
                }
            }
            KeyCode::Char('n') => self.new_chat(),
            KeyCode::Char('i') => self.chat.focus = ChatFocus::Composer,
            KeyCode::PageUp => self.chat.scroll += self.grid_viewport / 2,
            KeyCode::PageDown => {
                self.chat.scroll = self.chat.scroll.saturating_sub(self.grid_viewport / 2)
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                self.focus = Focus::Sidebar
            }
            _ => {}
        }
    }

    fn on_composer_key(&mut self, key: KeyEvent) {
        let newline = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Enter if newline => {
                self.chat.composer.insert_newline();
            }
            KeyCode::Enter => self.send_prompt(),
            KeyCode::Esc if self.chat.is_streaming() => self.cancel_run(),
            KeyCode::Esc => self.chat.focus = ChatFocus::List,
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => self.new_chat(),
            KeyCode::PageUp => self.chat.scroll += self.grid_viewport / 2,
            KeyCode::PageDown => {
                self.chat.scroll = self.chat.scroll.saturating_sub(self.grid_viewport / 2)
            }
            _ => {
                self.chat.composer.input(key);
            }
        }
    }

    fn on_palette_key(&mut self, key: KeyEvent) {
        let Some(palette) = self.palette.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Up => palette.move_sel(-1),
            KeyCode::Down | KeyCode::Tab => palette.move_sel(1),
            KeyCode::Backspace => palette.pop(),
            KeyCode::Enter => {
                let chosen = palette.chosen();
                self.palette = None;
                if let Some(action) = chosen {
                    self.run_action(action);
                }
            }
            KeyCode::Char(c) => palette.push(c),
            _ => {}
        }
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        let page = modal.viewport.max(1) as isize;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => self.modal = None,
            KeyCode::Up | KeyCode::Char('k') => modal.scroll_by(-1),
            KeyCode::Down | KeyCode::Char('j') => modal.scroll_by(1),
            KeyCode::PageUp => modal.scroll_by(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => modal.scroll_by(page),
            KeyCode::Home | KeyCode::Char('g') => modal.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => modal.scroll_by(isize::MAX / 2),
            _ => {}
        }
    }

    /// Filtering is live: every keystroke re-filters the grid.
    fn on_filter_key(&mut self, key: KeyEvent) {
        let Some(view) = self.view_mut() else {
            self.editing_filter = false;
            return;
        };
        let mut text = view.grid().filter.clone();
        match key.code {
            KeyCode::Esc => {
                text.clear();
                self.editing_filter = false;
            }
            KeyCode::Enter => self.editing_filter = false,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) => text.push(c),
            _ => return,
        }
        if let Some(view) = self.view_mut() {
            view.grid_mut().set_filter(&text);
        }
    }

    // ---------- mouse ----------

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        let (x, y) = (mouse.column, mouse.row);
        let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        let overlay = self.palette.is_some() || self.help;
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -WHEEL_STEP
                } else {
                    WHEEL_STEP
                };
                if let Some(modal) = self.modal.as_mut() {
                    modal.scroll_by(delta);
                } else if overlay {
                } else if inside(self.hits.transcript) {
                    self.chat.scroll = (self.chat.scroll as isize - delta).max(0) as usize;
                } else if inside(self.hits.chat_list) {
                    self.chat.move_sel(delta.signum());
                } else if inside(self.hits.sidebar_rows) {
                    self.move_cursor(delta.signum());
                } else if let Some(view) = self.view_mut() {
                    view.grid_mut().move_sel(delta);
                }
            }
            MouseEventKind::Down(MouseButton::Left) if !overlay && self.modal.is_none() => {
                self.click(
                    x,
                    y,
                    inside(self.hits.sidebar_rows),
                    inside(self.hits.chat_list),
                    inside(self.hits.grid_rows),
                );
            }
            _ => {}
        }
    }

    fn click(&mut self, _x: u16, y: u16, on_sidebar: bool, on_chat_list: bool, on_grid: bool) {
        if on_sidebar {
            let row = (y - self.hits.sidebar_rows.y) as usize + self.hits.sidebar_offset;
            let rows = self.sidebar_rows();
            match rows.get(row) {
                Some(SidebarRow::Module(m)) => {
                    self.cursor = row;
                    self.expanded[*m] = !self.expanded[*m];
                }
                Some(SidebarRow::Resource(id)) => {
                    self.cursor = row;
                    self.open_and_focus(*id);
                }
                None => {}
            }
        } else if on_chat_list {
            let index = (y - self.hits.chat_list.y) as usize + self.hits.chat_list_offset;
            if let Some(conversation) = self.chat.conversations.get(index).cloned() {
                self.focus = Focus::Content;
                self.chat.selected = index;
                self.open_conversation(conversation);
            }
        } else if on_grid {
            self.focus = Focus::Content;
            let offset = (y - self.hits.grid_rows.y) as usize;
            if let Some(view) = self.view_mut() {
                let grid = view.grid_mut();
                let target = grid.row_offset + offset;
                if target < grid.visible_len() {
                    grid.selected = target;
                }
            }
        } else if self.hits.content.width > 0
            && y >= self.hits.content.y
            && y < self.hits.content.y + self.hits.content.height
        {
            self.focus = Focus::Content;
        }
    }
}

/// Turns transport errors into one readable line with a next step.
fn friendly_error(err: &str) -> String {
    let lower = err.to_lowercase();
    if lower.contains("401") || lower.contains("403") {
        "Not authorized. Run `gz auth login`, then press r.".to_string()
    } else if lower.contains("dns error") || lower.contains("connect") {
        "Can't reach the service. Check your network/VPN and `gz doctor`.".to_string()
    } else if lower.contains("timed out") {
        "The service took too long to answer. Press r to retry.".to_string()
    } else {
        err.lines().next().unwrap_or(err).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tui::chat::ChatUpdate;
    use serde_json::json;

    fn app() -> App {
        let config = Config::default();
        let resolved = crate::config::resolve(&config, Some("t"), None, None);
        App::new(resolved)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(app: &mut App, codes: &[KeyCode]) {
        for code in codes {
            app.on_key(key(*code));
        }
    }

    fn last_fetch(app: &App) -> FetchReq {
        app.outbox
            .iter()
            .rev()
            .find_map(|r| match r {
                Request::Fetch(f) => Some(f.clone()),
                _ => None,
            })
            .unwrap()
    }

    fn answer(app: &mut App, value: Value) {
        let req = last_fetch(app);
        app.fetched(req.id, req.target, Ok(value));
    }

    fn resource(module: &str, label: &str) -> ResourceId {
        nav::all_resources()
            .find(|id| id.module().title == module && id.resource().label == label)
            .unwrap()
    }

    #[test]
    fn starts_on_chat_and_loads_conversations() {
        let app = app();
        assert!(app.is_chat());
        assert_eq!(app.focus, Focus::Sidebar);
        assert_eq!(
            last_fetch(&app).path,
            "/chat/conversations?assistantMode=chat"
        );
    }

    #[test]
    fn sidebar_expands_collapses_and_previews() {
        let mut app = app();
        let rows_before = app.sidebar_rows().len();
        // Down past Chat's resources to the Lakehouse module, then expand it.
        while app.sidebar_rows()[app.cursor] != SidebarRow::Module(1) {
            press(&mut app, &[KeyCode::Down]);
        }
        press(&mut app, &[KeyCode::Right]);
        assert!(app.expanded[1]);
        assert_eq!(
            app.sidebar_rows().len(),
            rows_before + MODULES[1].resources.len()
        );
        press(&mut app, &[KeyCode::Down]);
        assert_eq!(app.active, resource("Lakehouse", "Warehouses"));
        assert_eq!(last_fetch(&app).path, "/lakehouse/warehouse");
        press(&mut app, &[KeyCode::Left, KeyCode::Left]);
        assert!(!app.expanded[1]);
    }

    #[test]
    fn enter_opens_into_content_and_tab_switches_focus() {
        let mut app = app();
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(app.focus, Focus::Content);
        press(&mut app, &[KeyCode::Tab]);
        assert_eq!(app.focus, Focus::Sidebar);
    }

    #[test]
    fn drills_down_and_back_without_refetching() {
        let mut app = app();
        app.open_and_focus(resource("Lakehouse", "Warehouses"));
        answer(
            &mut app,
            json!({"status": 200, "response": [{"name": "wh1"}, {"name": "wh2"}]}),
        );
        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
        assert_eq!(last_fetch(&app).path, "/lakehouse/warehouse/wh2/namespaces");
        answer(&mut app, json!([{"name": "sales"}]));
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(last_fetch(&app).path, "/lakehouse/schema/wh2/sales");
        assert_eq!(
            app.breadcrumb(),
            vec!["Lakehouse", "Warehouses", "wh2", "sales"]
        );
        let fetches = app.outbox.len();
        press(&mut app, &[KeyCode::Backspace, KeyCode::Backspace]);
        assert_eq!(app.outbox.len(), fetches);
        assert_eq!(app.view().unwrap().grid().selected, 1);
        press(&mut app, &[KeyCode::Backspace]);
        assert_eq!(app.focus, Focus::Sidebar);
    }

    #[test]
    fn post_sources_carry_their_bodies() {
        let mut app = app();
        app.open_and_focus(resource("Files", "Storage"));
        answer(&mut app, json!({"data": [{"Name": "GZS3"}]}));
        press(&mut app, &[KeyCode::Enter]);
        let req = last_fetch(&app);
        assert_eq!(req.path, "/storage/listFolders");
        assert_eq!(req.body.unwrap()["ConnectionName"], "GZS3");
    }

    #[test]
    fn stale_answers_are_dropped() {
        let mut app = app();
        app.open_and_focus(resource("Designer", "Datasets"));
        let first = last_fetch(&app);
        press(&mut app, &[KeyCode::Char('r')]);
        app.fetched(first.id, first.target, Ok(json!([{"a": 1}])));
        assert!(app.view().unwrap().grid().is_empty());
        answer(&mut app, json!([{"a": 2}, {"a": 3}]));
        assert_eq!(app.view().unwrap().grid().visible_len(), 2);
    }

    #[test]
    fn palette_fuzzy_jumps_and_runs_actions() {
        let mut app = app();
        app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(app.palette.is_some());
        for c in "dashb".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.palette.is_none());
        assert_eq!(app.active, resource("Designer", "Dashboards"));
        assert_eq!(app.focus, Focus::Content);
        press(&mut app, &[KeyCode::Char(':')]);
        for c in "quit".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.quit);
    }

    #[test]
    fn chat_send_streams_and_cancels() {
        let mut app = app();
        answer(
            &mut app,
            json!({"conversations": [{"conversationId": "c1", "conversationName": "First"}]}),
        );
        press(&mut app, &[KeyCode::Enter, KeyCode::Enter]);
        assert_eq!(app.chat.open.as_ref().unwrap().id, "c1");
        assert_eq!(last_fetch(&app).path, "/chat/conversations/c1");
        answer(
            &mut app,
            json!({"messages": [{"speaker": "Human", "message": "hi"}]}),
        );
        assert_eq!(app.chat.focus, ChatFocus::Composer);
        for c in "next q".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        press(&mut app, &[KeyCode::Enter]);
        let Some(Request::StartChat(run)) = app.outbox.last().cloned() else {
            panic!("no chat run queued");
        };
        assert_eq!(
            (run.conversation_id.as_str(), run.prompt.as_str()),
            ("c1", "next q")
        );
        app.chat_update(
            run.tag,
            ChatUpdate::Started {
                run_id: "r1".into(),
            },
        );
        app.chat_update(run.tag, ChatUpdate::Text("answer".into()));
        app.chat_update(run.tag + 99, ChatUpdate::Text("stale".into()));
        assert_eq!(app.chat.messages.last().unwrap().text, "answer");
        press(&mut app, &[KeyCode::Esc]);
        assert!(matches!(
            app.outbox.last(),
            Some(Request::CancelChat { run_id: Some(id), .. }) if id == "r1"
        ));
        app.chat_update(
            run.tag,
            ChatUpdate::Finished {
                status: Some("cancelled".into()),
            },
        );
        assert!(!app.chat.is_streaming());
    }

    #[test]
    fn new_chat_gets_a_fresh_conversation_id() {
        let mut app = app();
        app.run_action(Action::NewChat);
        for c in "hello".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        press(&mut app, &[KeyCode::Enter]);
        let Some(Request::StartChat(run)) = app.outbox.last().cloned() else {
            panic!("no chat run queued");
        };
        assert_eq!(run.conversation_id.len(), 36);
        assert_eq!(run.title.as_deref(), Some("hello"));
    }

    #[test]
    fn letters_type_into_the_composer_not_shortcuts() {
        let mut app = app();
        app.run_action(Action::NewChat);
        press(&mut app, &[KeyCode::Char('q'), KeyCode::Char('?')]);
        assert!(!app.quit && !app.help);
        assert_eq!(app.chat.composer_text(), "q?");
    }

    #[test]
    fn ctrl_c_quits_when_idle() {
        let mut app = app();
        app.help = true;
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn filter_applies_while_typing() {
        let mut app = app();
        app.open_and_focus(resource("Designer", "Datasets"));
        answer(&mut app, json!([{"n": "alpha"}, {"n": "beta"}]));
        press(&mut app, &[KeyCode::Char('/'), KeyCode::Char('b')]);
        assert_eq!(app.view().unwrap().grid().visible_len(), 1);
        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(app.view().unwrap().grid().visible_len(), 2);
    }

    #[test]
    fn clicking_a_sidebar_resource_opens_it() {
        let mut app = app();
        app.hits.sidebar_rows = Rect::new(1, 2, 20, 30);
        let row = app
            .row_of(ResourceId {
                module: 0,
                resource: 1,
            })
            .unwrap();
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 2 + row as u16,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.active,
            ResourceId {
                module: 0,
                resource: 1
            }
        );
        assert_eq!(app.focus, Focus::Content);
    }

    #[test]
    fn errors_become_actionable_messages() {
        let mut app = app();
        app.open_and_focus(resource("Designer", "Datasets"));
        let req = last_fetch(&app);
        app.fetched(
            req.id,
            req.target,
            Err("GET x → HTTP 401 Unauthorized".into()),
        );
        assert!(app
            .view()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("gz auth login"));
        assert_eq!(app.status.tone, Tone::Error);
    }
}
