use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reqwest::Method;
use serde_json::Value;

use crate::config::Resolved;
use crate::tui::fetch::{FetchReq, FetchTarget};
use crate::tui::model::Grid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Chat,
    Workspaces,
    Connections,
    Designer,
    Deml,
    Lakehouse,
    Catalog,
    Schedules,
}

impl Screen {
    pub const ALL: [Screen; 8] = [
        Screen::Chat,
        Screen::Workspaces,
        Screen::Connections,
        Screen::Designer,
        Screen::Deml,
        Screen::Lakehouse,
        Screen::Catalog,
        Screen::Schedules,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Screen::Chat => "Chat",
            Screen::Workspaces => "Workspaces",
            Screen::Connections => "Connections",
            Screen::Designer => "Designer",
            Screen::Deml => "DE/ML",
            Screen::Lakehouse => "Lakehouse",
            Screen::Catalog => "Catalog",
            Screen::Schedules => "Schedules",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Screen::Chat => "◎",
            Screen::Workspaces => "▣",
            Screen::Connections => "⇄",
            Screen::Designer => "◧",
            Screen::Deml => "⚙",
            Screen::Lakehouse => "≋",
            Screen::Catalog => "☰",
            Screen::Schedules => "◷",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    pub fn kinds(self) -> &'static [Kind] {
        match self {
            Screen::Chat => &CHAT_KINDS,
            Screen::Workspaces => &WORKSPACE_KINDS,
            Screen::Connections => &CONNECTION_KINDS,
            Screen::Designer => &DESIGNER_KINDS,
            Screen::Deml => &DEML_KINDS,
            Screen::Lakehouse => &LAKEHOUSE_KINDS,
            Screen::Catalog => &CATALOG_KINDS,
            Screen::Schedules => &SCHEDULE_KINDS,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Kind {
    pub label: &'static str,
    pub service: &'static str,
    pub path: &'static str,
    /// `(service, path prefix)` for fetching one row by id on Enter.
    pub detail: Option<(&'static str, &'static str)>,
}

impl Kind {
    const fn list(label: &'static str, service: &'static str, path: &'static str) -> Self {
        Self {
            label,
            service,
            path,
            detail: None,
        }
    }

    const fn detail(mut self, service: &'static str, prefix: &'static str) -> Self {
        self.detail = Some((service, prefix));
        self
    }
}

static CHAT_KINDS: [Kind; 3] = [
    Kind::list(
        "Conversations",
        "chat",
        "/chat/conversations?assistantMode=chat",
    )
    .detail("chat", "/chat/conversations"),
    Kind::list("Budget", "chat", "/chat/budget-status"),
    Kind::list("Metrics", "chat", "/chat/metrics"),
];

static WORKSPACE_KINDS: [Kind; 1] = [Kind::list("Workspaces", "workspaces", "/workspace")];

static CONNECTION_KINDS: [Kind; 1] = [
    Kind::list("Connections", "connections", "/connection").detail("connections", "/connection")
];

static DESIGNER_KINDS: [Kind; 4] = [
    Kind::list("Datasets", "datasets", "/dataset").detail("datasets", "/dataset"),
    Kind::list("Dashboards", "dashboards", "/dashboard").detail("dashboards", "/dashboard"),
    Kind::list("Visualizations", "visualizations", "/visualization")
        .detail("visualizations", "/visualization"),
    Kind::list("Filters", "filters", "/filter").detail("filters", "/filter"),
];

static DEML_KINDS: [Kind; 2] = [
    Kind::list("Projects", "etlprojects", "/etl/etlProjects"),
    Kind::list("Published jobs", "etlprojects", "/etl/etlJobs/publish"),
];

static LAKEHOUSE_KINDS: [Kind; 1] = [Kind::list(
    "Warehouses",
    "lakehouse",
    "/lakehouse/warehouse",
)];

static CATALOG_KINDS: [Kind; 2] = [
    Kind::list("Ontology models", "etl", "/etl/ontology/models")
        .detail("etl", "/etl/ontology/models"),
    Kind::list("Lineage assets", "etl", "/etl/lineage/assets"),
];

static SCHEDULE_KINDS: [Kind; 2] = [
    Kind::list("Schedules", "schedules", "/schedule").detail("schedules", "/schedule"),
    Kind::list("Executions", "schedules", "/schedule/executions"),
];

/// Row fields that commonly hold an id, in lookup order.
const ID_KEYS: &[&str] = &[
    "id",
    "Id",
    "ID",
    "conversationId",
    "scheduleId",
    "projectId",
    "connectionId",
    "name",
];

/// One drill-down level: what was opened and where its rows come from.
#[derive(Debug, Clone)]
pub struct Trail {
    pub label: String,
    service: String,
    path: String,
    id: String,
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

#[derive(Debug, Clone)]
pub struct ModuleState {
    pub kind: usize,
    pub grids: Vec<Grid>,
    pub loaded: Vec<bool>,
    pub trail: Vec<Trail>,
    /// Id of the request whose answer this module is waiting for; any other
    /// answer is stale and dropped.
    awaiting: Option<u64>,
    pub error: Option<String>,
}

impl ModuleState {
    fn new(screen: Screen) -> Self {
        let count = screen.kinds().len();
        Self {
            kind: 0,
            grids: vec![Grid::default(); count],
            loaded: vec![false; count],
            trail: Vec::new(),
            awaiting: None,
            error: None,
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grids[self.kind]
    }

    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grids[self.kind]
    }

    pub fn is_loading(&self) -> bool {
        self.awaiting.is_some()
    }

    pub fn breadcrumb(&self, screen: Screen) -> Vec<String> {
        let mut parts = vec![
            screen.title().to_string(),
            screen.kinds()[self.kind].label.to_string(),
        ];
        parts.extend(self.trail.iter().map(|t| t.label.clone()));
        parts
    }
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
        let next = self.scroll as isize + delta;
        self.scroll = next.clamp(0, max as isize) as usize;
    }
}

pub struct App {
    pub resolved: Resolved,
    pub screen: Screen,
    pub modules: Vec<ModuleState>,
    pub quit: bool,
    pub help: bool,
    pub modal: Option<Modal>,
    awaiting_modal: Option<u64>,
    pub editing_filter: bool,
    pub status: Status,
    pub outbox: Vec<FetchReq>,
    next_id: u64,
    pub tick: u64,
    /// Data rows visible in the grid, updated by the renderer.
    pub grid_viewport: usize,
}

impl App {
    pub fn new(resolved: Resolved) -> Self {
        let mut app = Self {
            resolved,
            screen: Screen::Chat,
            modules: Screen::ALL.iter().map(|s| ModuleState::new(*s)).collect(),
            quit: false,
            help: false,
            modal: None,
            awaiting_modal: None,
            editing_filter: false,
            status: Status {
                text: "Press ? for keys".to_string(),
                tone: Tone::Info,
            },
            outbox: Vec::new(),
            next_id: 1,
            tick: 0,
            grid_viewport: 20,
        };
        app.reload();
        app
    }

    pub fn module(&self) -> &ModuleState {
        &self.modules[self.screen.index()]
    }

    pub fn module_mut(&mut self) -> &mut ModuleState {
        let index = self.screen.index();
        &mut self.modules[index]
    }

    pub fn is_busy(&self) -> bool {
        self.awaiting_modal.is_some() || self.modules.iter().any(ModuleState::is_loading)
    }

    pub fn signed_in(&self) -> bool {
        self.resolved.token().is_some() && !self.resolved.token_expired()
    }

    fn set_status(&mut self, tone: Tone, text: impl Into<String>) {
        self.status = Status {
            text: text.into(),
            tone,
        };
    }

    fn enqueue(&mut self, target: FetchTarget, service: &str, path: String) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.outbox.push(FetchReq {
            id,
            target,
            service: service.to_string(),
            method: Method::GET,
            path,
            body: None,
        });
        id
    }

    pub fn fetched(&mut self, id: u64, target: FetchTarget, result: Result<Value, String>) {
        match target {
            FetchTarget::Module { screen } => {
                let Some(state) = self.modules.get_mut(screen) else {
                    return;
                };
                if state.awaiting != Some(id) {
                    return;
                }
                state.awaiting = None;
                match result {
                    Ok(value) => {
                        state.error = None;
                        state.loaded[state.kind] = true;
                        let filter = state.grid().filter.clone();
                        let grid = state.grid_mut();
                        grid.replace(&value);
                        grid.set_filter(&filter);
                        let rows = grid.visible_len();
                        self.set_status(Tone::Ok, format!("Loaded {rows} row(s)"));
                    }
                    Err(err) => {
                        state.error = Some(friendly_error(&err));
                        self.set_status(Tone::Error, "Request failed · r to retry");
                    }
                }
            }
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
        }
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    fn goto(&mut self, screen: Screen) {
        self.screen = screen;
        self.modal = None;
        self.editing_filter = false;
        let state = self.module();
        if !state.loaded[state.kind] && !state.is_loading() {
            self.reload();
        }
    }

    /// Fetches whatever the current module shows: the deepest drill level, or
    /// the resource list at the root.
    fn reload(&mut self) {
        let screen = self.screen;
        let state = self.module();
        let (service, path) = match state.trail.last() {
            Some(step) => (step.service.clone(), step.path.clone()),
            None => {
                let def = screen.kinds()[state.kind];
                (def.service.to_string(), def.path.to_string())
            }
        };
        let id = self.enqueue(
            FetchTarget::Module {
                screen: screen.index(),
            },
            &service,
            path,
        );
        let state = self.module_mut();
        state.awaiting = Some(id);
        state.error = None;
        self.set_status(Tone::Info, "Loading…");
    }

    fn switch_kind(&mut self, delta: isize) {
        let count = self.screen.kinds().len();
        if count < 2 {
            return;
        }
        let state = self.module_mut();
        state.kind = (state.kind as isize + delta).rem_euclid(count as isize) as usize;
        state.trail.clear();
        self.modal = None;
        self.reload();
    }

    fn selected_id(&self, keys: &[&str]) -> Option<String> {
        let raw = self.module().grid().selected_raw()?;
        keys.iter().find_map(|k| match raw.get(k)? {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
    }

    fn drill(&mut self, label: String, service: &str, path: String, id: String) {
        self.module_mut().trail.push(Trail {
            label,
            service: service.to_string(),
            path,
            id,
        });
        self.module_mut().grid_mut().filter.clear();
        self.reload();
    }

    fn back(&mut self) {
        if self.module_mut().trail.pop().is_some() {
            self.reload();
        }
    }

    fn open_selected(&mut self) {
        let screen = self.screen;
        let kind = self.module().kind;
        let depth = self.module().trail.len();
        let drill_to: Option<(&str, &[&str])> = match (screen, kind, depth) {
            (Screen::Lakehouse, 0, 0) => {
                Some(("warehouse", &["name", "warehouseName", "warehouse"]))
            }
            (Screen::Lakehouse, 0, 1) => {
                Some(("namespace", &["name", "namespace", "namespaceName"]))
            }
            (Screen::Workspaces, 0, 0) => Some(("workspace", &["id", "Id", "workspaceId", "name"])),
            (Screen::Deml, 0, 0) => Some(("project", &["id", "projectId", "name"])),
            _ => None,
        };
        if let Some((noun, keys)) = drill_to {
            let Some(id) = self.selected_id(keys) else {
                self.set_status(Tone::Error, format!("Select a {noun} first"));
                return;
            };
            let (label, service, path) = match (screen, depth) {
                (Screen::Lakehouse, 0) => (
                    id.clone(),
                    "lakehouse",
                    format!("/lakehouse/warehouse/{id}/namespaces"),
                ),
                (Screen::Lakehouse, _) => {
                    let wh = self.module().trail[0].id.clone();
                    (
                        id.clone(),
                        "lakehouse",
                        format!("/lakehouse/schema/{wh}/{id}"),
                    )
                }
                (Screen::Workspaces, _) => (
                    id.clone(),
                    "workspaces",
                    format!("/workspace/{id}/contents"),
                ),
                _ => (id.clone(), "etlprojects", format!("/etl/etlJobs/{id}/jobs")),
            };
            self.drill(label, service, path, id);
            return;
        }
        let def = screen.kinds()[kind];
        let Some(raw) = self.module().grid().selected_raw().cloned() else {
            self.set_status(Tone::Error, "Nothing selected");
            return;
        };
        match (def.detail, self.selected_id(ID_KEYS)) {
            (Some((service, prefix)), Some(id)) => {
                let title = format!("{} · {id}", def.label);
                let req = self.enqueue(
                    FetchTarget::Detail { title },
                    service,
                    format!("{prefix}/{id}"),
                );
                self.awaiting_modal = Some(req);
                self.set_status(Tone::Info, "Loading details…");
            }
            _ => self.modal = Some(Modal::json(format!("{} · row", def.label), &raw)),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.modal.is_some() {
            self.on_modal_key(key);
            return;
        }
        if self.editing_filter {
            self.on_filter_key(key);
            return;
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
        let page = self.grid_viewport;
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(c @ '1'..='8') => {
                let index = c as usize - '1' as usize;
                self.goto(Screen::ALL[index]);
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.switch_kind(1),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.switch_kind(-1),
            KeyCode::Char(']') => self.module_mut().grid_mut().scroll_cols(1),
            KeyCode::Char('[') => self.module_mut().grid_mut().scroll_cols(-1),
            KeyCode::Char('/') => self.editing_filter = true,
            KeyCode::Char('r') => self.reload(),
            KeyCode::Enter => self.open_selected(),
            KeyCode::Backspace => self.back(),
            KeyCode::Esc => {
                if self.module().grid().filter.is_empty() {
                    self.back();
                } else {
                    self.module_mut().grid_mut().set_filter("");
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.module_mut().grid_mut().move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.module_mut().grid_mut().move_sel(1),
            KeyCode::PageUp => self.module_mut().grid_mut().page(page, false),
            KeyCode::PageDown => self.module_mut().grid_mut().page(page, true),
            KeyCode::Home | KeyCode::Char('g') => self.module_mut().grid_mut().home(),
            KeyCode::End | KeyCode::Char('G') => self.module_mut().grid_mut().end(),
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
        let mut text = self.module().grid().filter.clone();
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
        let grid = self.module_mut().grid_mut();
        grid.filter = text;
        grid.apply_filter();
    }
}

/// Turns transport errors into one readable line with a next step.
fn friendly_error(err: &str) -> String {
    let lower = err.to_lowercase();
    if lower.contains("401") || lower.contains("403") {
        "Not authorized. Run `gz auth login`, then press r.".to_string()
    } else if lower.contains("no client configured") || lower.contains("has no url configured") {
        err.lines().next().unwrap_or(err).to_string()
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

    fn app() -> App {
        let config = Config::default();
        let resolved = crate::config::resolve(&config, Some("t"), None, None);
        App::new(resolved)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn answer(app: &mut App, rows: Value) {
        let req = app.outbox.last().unwrap().clone();
        app.fetched(req.id, req.target, Ok(rows));
    }

    #[test]
    fn switches_modules_and_queues_loads() {
        let mut app = app();
        assert_eq!(app.outbox.len(), 1);
        assert_eq!(app.outbox[0].path, "/chat/conversations?assistantMode=chat");
        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.screen, Screen::Designer);
        assert_eq!(app.outbox.len(), 2);
        assert_eq!(app.outbox[1].path, "/dataset");
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn tab_cycles_resource_kinds() {
        let mut app = app();
        app.goto(Screen::Designer);
        assert_eq!(app.module().kind, 0);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.module().kind, 1);
        assert_eq!(app.outbox.last().unwrap().path, "/dashboard");
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.module().kind, 0);
    }

    #[test]
    fn modal_eats_keys_until_closed() {
        let mut app = app();
        app.modal = Some(Modal::json("t".to_string(), &serde_json::json!({"a": 1})));
        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.screen, Screen::Chat);
        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    #[test]
    fn arrow_keys_move_selection() {
        let mut app = app();
        answer(&mut app, serde_json::json!([{"a": 1}, {"a": 2}, {"a": 3}]));
        assert_eq!(app.module().grid().visible_len(), 3);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.module().grid().selected, 2);
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.module().grid().selected, 1);
    }

    #[test]
    fn stale_answers_are_dropped() {
        let mut app = app();
        let first = app.outbox[0].clone();
        app.on_key(key(KeyCode::Char('r')));
        app.fetched(first.id, first.target, Ok(serde_json::json!([{"a": 1}])));
        assert!(app.module().grid().is_empty());
        assert!(app.module().is_loading());
        answer(&mut app, serde_json::json!([{"a": 2}, {"a": 3}]));
        assert_eq!(app.module().grid().visible_len(), 2);
        assert!(!app.is_busy());
    }

    #[test]
    fn filter_applies_while_typing() {
        let mut app = app();
        answer(&mut app, serde_json::json!([{"n": "alpha"}, {"n": "beta"}]));
        app.on_key(key(KeyCode::Char('/')));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.module().grid().visible_len(), 1);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.module().grid().visible_len(), 2);
        assert!(!app.editing_filter);
    }

    #[test]
    fn ctrl_c_quits_from_anywhere() {
        let mut app = app();
        app.help = true;
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn errors_become_actionable_messages() {
        let mut app = app();
        let req = app.outbox[0].clone();
        app.fetched(
            req.id,
            req.target,
            Err("GET x → HTTP 401 Unauthorized".into()),
        );
        assert!(app
            .module()
            .error
            .as_deref()
            .unwrap()
            .contains("gz auth login"));
        assert_eq!(app.status.tone, Tone::Error);
    }
}
