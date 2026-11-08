//! State of the full-screen `gz tui`: what is open, what has focus, and
//! the requests in flight. Keys, mouse and fetch answers update it; ui.rs
//! draws it.

mod actions;
mod chat;
mod history;
mod keys;
mod links;
mod mouse;
mod navigation;
mod requests;
mod saved;
#[cfg(test)]
pub(crate) mod test_support;
mod views;

use std::collections::HashMap;

use ratatui::layout::Rect;
use serde_json::Value;

use crate::config::Resolved;
use crate::tui::chat::{ChatFocus, ChatView};
use crate::tui::clock::Now;
use crate::tui::fetch::Request;
use crate::tui::help::{self, HelpSection};
use crate::tui::model::Grid;
use crate::tui::nav::{self, GridSpec, ResourceId, View, MODULES};
use crate::tui::notify::Notifier;
use crate::tui::palette::{Entry, Palette};
use crate::tui::saved_views::SavedViews;
use crate::tui::screen::ScreenSpec;
use crate::tui::views::{Poller, RowContext, ViewStack};

pub use actions::Overlay;
pub use history::{Place, SortBy, Step};
pub use links::Pending;

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
    /// The row drilled into to open this level (Null at the root), so
    /// templates can use its fields by name.
    pub row: Value,
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
    /// What to select once rows arrive (after following a link).
    pending: Option<Pending>,
    /// Refetches while the resource is on screen, when its spec polls.
    poller: Option<Poller>,
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
            pending: None,
            poller: None,
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

    /// Rows drilled through, nearest first.
    fn trail_rows(&self) -> Vec<Value> {
        self.levels[1..]
            .iter()
            .rev()
            .map(|l| l.row.clone())
            .collect()
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
    pub(crate) recent: Vec<Entry>,
    pub hits: Hits,
    /// Data rows visible in the grid, updated by the renderer.
    pub grid_viewport: usize,
    /// The time the app acts on; set every tick.
    pub now: Now,
    /// Full-screen views over the browser.
    pub views: ViewStack,
    /// The action menu, a confirmation or a form.
    pub overlay: Option<Overlay>,
    pub notifier: Notifier,
    /// Places left by links, jumps and saved views, for Back.
    history: Vec<Place>,
    pub saved: SavedViews,
    /// The name being typed for a saved view.
    pub naming: Option<String>,
}

const CHAT: ResourceId = ResourceId {
    module: 0,
    resource: 0,
};

/// "run" → "Runs", for breadcrumbs.
fn plural_title(noun: &str) -> String {
    let mut chars = noun.chars();
    let first = chars.next().map(|c| c.to_uppercase().collect::<String>());
    format!("{}{}s", first.unwrap_or_default(), chars.as_str())
}

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
            now: Now::system(),
            views: ViewStack::default(),
            overlay: None,
            notifier: Notifier::default(),
            history: Vec::new(),
            saved: SavedViews::default(),
            naming: None,
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
            || self.views.any_busy()
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
        self.active.grid_spec()
    }

    /// The screen spec of the rows on screen, if they have one.
    pub fn screen(&self) -> Option<&'static ScreenSpec> {
        let depth = self.view()?.depth();
        self.spec()?.screen_at(depth)
    }

    /// The selected row with everything actions and links need.
    pub(super) fn row_context(&self) -> Option<RowContext> {
        let view = self.view()?;
        let row = view.grid().selected_raw()?.clone();
        let screen = self.screen();
        let id = match screen {
            Some(spec) => spec.identity.id_of(&row),
            None => nav::row_id(&row, nav::ID_KEYS),
        };
        let mut ids = view.trail_ids();
        ids.extend(id);
        Some(RowContext {
            title: screen.and_then(|s| s.identity.title_of(&row)),
            noun: screen.map_or("item", |s| s.noun),
            spec: screen,
            parents: view.trail_rows(),
            ids,
            resource: Some(self.active),
            view: None,
            row,
        })
    }

    /// What `?` lists: the view's or screen's own keys first, then the
    /// global ones.
    pub fn help_sections(&self) -> Vec<HelpSection> {
        let mut sections = Vec::new();
        if let Some((_, view)) = self.views.top() {
            sections.extend(view.help());
        } else if let Some(spec) = self.screen() {
            let row = self.view().and_then(|v| v.grid().selected_raw());
            sections.push(help::for_screen("This screen", spec, row));
        }
        sections.extend(help::global());
        sections
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
            || self.naming.is_some()
            || (self.focus == Focus::Content
                && self.is_chat()
                && self.chat.focus == ChatFocus::Composer)
    }

    /// Where the user is: "Agents › InvoiceExtractor › Runs". A resource
    /// named like its module shows once; a drilled level with a screen
    /// spec ends with what it lists.
    pub fn breadcrumb(&self) -> Vec<String> {
        let module = self.active.module().title;
        let label = self.active.resource().label;
        let mut parts = vec![module.to_string()];
        if label != module {
            parts.push(label.to_string());
        }
        if let Some(view) = self.view() {
            parts.extend(view.levels[1..].iter().map(|l| l.label.clone()));
            let listing = (view.depth() > 0)
                .then(|| self.screen())
                .flatten()
                .map(|spec| plural_title(spec.noun));
            parts.extend(listing);
        }
        if self.is_chat() {
            if let Some(open) = &self.chat.open {
                parts.push(open.title.clone());
            }
        }
        parts.extend(self.views.titles());
        parts
    }

    pub(super) fn set_status(&mut self, tone: Tone, text: impl Into<String>) {
        self.status = Status {
            text: text.into(),
            tone,
        };
    }

    pub fn on_tick(&mut self, now: Now) {
        self.now = now;
        self.tick = self.tick.wrapping_add(1);
        self.notifier.current(now.instant);
        self.tick_views();
        self.poll_grid();
    }
}
