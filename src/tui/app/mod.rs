//! State of the full-screen `gz tui`: what is open, what has focus, and
//! the requests in flight. Keys, mouse and fetch answers update it; ui.rs
//! draws it.

mod chat;
mod keys;
mod mouse;
mod navigation;
mod requests;
#[cfg(test)]
mod test_support;

use std::collections::HashMap;

use ratatui::layout::Rect;
use serde_json::Value;

use crate::config::Resolved;
use crate::tui::chat::{ChatFocus, ChatView};
use crate::tui::fetch::Request;
use crate::tui::model::Grid;
use crate::tui::nav::{GridSpec, ResourceId, View, MODULES};
use crate::tui::palette::{Entry, Palette};

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

    pub(super) fn view_mut(&mut self) -> Option<&mut GridView> {
        self.grids.get_mut(&self.active)
    }

    pub(super) fn spec(&self) -> Option<GridSpec> {
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

    pub(super) fn row_of(&self, id: ResourceId) -> Option<usize> {
        self.sidebar_rows()
            .iter()
            .position(|r| *r == SidebarRow::Resource(id))
    }

    /// The user is typing, so letter keys are text, not shortcuts.
    pub(super) fn is_typing(&self) -> bool {
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

    pub(super) fn set_status(&mut self, tone: Tone, text: impl Into<String>) {
        self.status = Status {
            text: text.into(),
            tone,
        };
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }
}
