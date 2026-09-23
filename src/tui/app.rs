use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reqwest::Method;
use serde_json::Value;
use tui_textarea::TextArea;

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

static CHAT_KINDS: [Kind; 2] = [
    Kind::list("Conversations", "chatbot", "/api/conversations")
        .detail("chats", "/chat/conversations"),
    Kind::list("Chats", "chatbot", "/api/chat"),
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

#[derive(Debug, Clone)]
pub(crate) struct Trail {
    label: String,
    service: String,
    path: String,
    id: String,
}

#[derive(Debug, Clone)]
pub struct ModuleState {
    pub kind: usize,
    pub grids: Vec<Grid>,
    pub loaded: Vec<bool>,
    pub trail: Vec<Trail>,
}

impl ModuleState {
    fn new(screen: Screen) -> Self {
        let kinds = screen.kinds();
        Self {
            kind: 0,
            grids: kinds
                .iter()
                .map(|k| Grid::from_value(&Value::Null, &format!("no {}", k.label.to_lowercase())))
                .collect(),
            loaded: vec![false; kinds.len()],
            trail: Vec::new(),
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grids[self.kind]
    }

    pub fn grid_mut(&mut self) -> &mut Grid {
        let kind = self.kind;
        &mut self.grids[kind]
    }

    pub fn breadcrumb(&self, screen: Screen) -> String {
        let mut parts = vec![
            screen.title().to_string(),
            screen.kinds()[self.kind].label.to_string(),
        ];
        for step in &self.trail {
            parts.push(step.label.clone());
        }
        parts.join(" › ")
    }
}

pub struct Modal {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: usize,
}

impl Modal {
    pub fn json(title: String, value: &Value) -> Self {
        let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "?".to_string());
        Self {
            title,
            lines: text.lines().map(str::to_string).collect(),
            scroll: 0,
        }
    }

    pub fn scroll_by(&mut self, delta: isize, height: usize) {
        let max = self.lines.len().saturating_sub(height);
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
    pub editing_filter: bool,
    pub filter_area: TextArea<'static>,
    pub status: String,
    pub pending: usize,
    pub outbox: Vec<FetchReq>,
    pub next_id: u64,
    pub tick: u64,
}

impl App {
    pub fn new(resolved: Resolved) -> Self {
        let mut filter_area = TextArea::default();
        filter_area.set_placeholder_text("type to filter, Enter to apply");
        let mut app = Self {
            resolved,
            screen: Screen::Chat,
            modules: Screen::ALL.iter().map(|s| ModuleState::new(*s)).collect(),
            quit: false,
            help: false,
            modal: None,
            editing_filter: false,
            filter_area,
            status: "1-8 switch module · Tab resource · Enter open · / filter · ? help · q quit"
                .to_string(),
            pending: 0,
            outbox: Vec::new(),
            next_id: 1,
            tick: 0,
        };
        app.fetch_current();
        app
    }

    pub fn module(&self) -> &ModuleState {
        &self.modules[self.screen.index()]
    }

    pub fn module_mut(&mut self) -> &mut ModuleState {
        let index = self.screen.index();
        &mut self.modules[index]
    }

    fn request(
        &mut self,
        target: FetchTarget,
        label: String,
        service: &str,
        method: Method,
        path: String,
        body: Option<Value>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        self.pending += 1;
        self.status = format!("loading {label}…");
        self.outbox.push(FetchReq {
            id,
            target,
            label,
            service: service.to_string(),
            method,
            path,
            body,
        });
    }

    pub fn fetched(
        &mut self,
        id: u64,
        target: FetchTarget,
        label: String,
        result: Result<Value, String>,
    ) {
        self.pending = self.pending.saturating_sub(1);
        if id + 20 < self.next_id {
            return;
        }
        match result {
            Ok(value) => {
                self.status = format!("{label} · ok");
                self.apply(target, value);
            }
            Err(err) => {
                self.status = format!("{label} failed: {err}");
            }
        }
    }

    fn apply(&mut self, target: FetchTarget, value: Value) {
        match target {
            FetchTarget::Module {
                screen,
                kind,
                depth,
            } => {
                let index = screen as usize;
                if index >= self.modules.len() {
                    return;
                }
                let state = &mut self.modules[index];
                if state.trail.len() != depth as usize || kind as usize != state.kind {
                    return;
                }
                state.loaded[state.kind] = true;
                let note = format!(
                    "no {}",
                    Screen::ALL[index].kinds()[state.kind].label.to_lowercase()
                );
                state.grid_mut().replace(&value);
                state.grid_mut().empty_note = note;
            }
            FetchTarget::ModuleDetail { title } => {
                self.modal = Some(Modal::json(title, &value));
            }
        }
    }

    pub fn on_tick(&mut self) {
        self.tick += 1;
    }

    fn goto(&mut self, screen: Screen) {
        self.screen = screen;
        self.modal = None;
        self.editing_filter = false;
        if !self.module().loaded[self.module().kind] {
            self.fetch_current();
        } else {
            self.status = self.module().breadcrumb(screen);
        }
    }

    fn fetch_current(&mut self) {
        let screen = self.screen;
        let state = self.module();
        let kind = state.kind;
        let depth = state.trail.len() as u8;
        let def = screen.kinds()[kind];
        let (service, path, label, at_root) = {
            let trail = state.trail.last();
            match trail {
                Some(step) => (
                    step.service.clone(),
                    step.path.clone(),
                    step.label.clone(),
                    false,
                ),
                None => (
                    def.service.to_string(),
                    def.path.to_string(),
                    def.label.to_string(),
                    true,
                ),
            }
        };
        if at_root {
            self.module_mut().loaded[kind] = false;
        }
        self.request(
            FetchTarget::Module {
                screen: screen.index() as u8,
                kind: kind as u8,
                depth,
            },
            label,
            &service,
            Method::GET,
            path,
            None,
        );
    }

    fn switch_kind(&mut self, delta: isize) {
        let screen = self.screen;
        let count = screen.kinds().len();
        if count < 2 {
            return;
        }
        let next = (self.module().kind as isize + delta).rem_euclid(count as isize) as usize;
        self.module_mut().kind = next;
        self.module_mut().trail.clear();
        self.modal = None;
        self.fetch_current();
    }

    fn selected_id(&self, keys: &[&str]) -> String {
        let grid = self.module().grid();
        let Some(raw) = grid.selected_raw() else {
            return String::new();
        };
        if let Some(id) = keys.iter().find_map(|k| raw.get(k)) {
            return match id {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            };
        }
        String::new()
    }

    fn drill(&mut self, label: String, service: &str, path: String, id: String) {
        let screen = self.screen;
        let kind = self.module().kind;
        self.module_mut().trail.push(Trail {
            label: label.clone(),
            service: service.to_string(),
            path: path.clone(),
            id,
        });
        let depth = self.module().trail.len() as u8;
        self.request(
            FetchTarget::Module {
                screen: screen.index() as u8,
                kind: kind as u8,
                depth,
            },
            label,
            service,
            Method::GET,
            path,
            None,
        );
    }

    fn back(&mut self) {
        if self.module().trail.is_empty() {
            return;
        }
        self.module_mut().trail.pop();
        let screen = self.screen;
        let kind = self.module().kind;
        let depth = self.module().trail.len() as u8;
        let def = screen.kinds()[kind];
        let (service, path, label) = match self.module().trail.last() {
            Some(step) => (step.service.clone(), step.path.clone(), step.label.clone()),
            None => (
                def.service.to_string(),
                def.path.to_string(),
                def.label.to_string(),
            ),
        };
        self.request(
            FetchTarget::Module {
                screen: screen.index() as u8,
                kind: kind as u8,
                depth,
            },
            label,
            &service,
            Method::GET,
            path,
            None,
        );
    }

    fn open_selected(&mut self) {
        let screen = self.screen;
        let kind = self.module().kind;
        let depth = self.module().trail.len();
        match (screen, kind, depth) {
            (Screen::Lakehouse, 0, 0) => {
                let id = self.selected_id(&["name", "warehouseName", "warehouse"]);
                if id.is_empty() {
                    self.status = "select a warehouse first".to_string();
                    return;
                }
                self.drill(
                    format!("namespaces of {id}"),
                    "lakehouse",
                    format!("/lakehouse/warehouse/{id}/namespaces"),
                    id,
                );
            }
            (Screen::Lakehouse, 0, 1) => {
                let ns = self.selected_id(&["name", "namespace", "namespaceName"]);
                if ns.is_empty() {
                    self.status = "select a namespace first".to_string();
                    return;
                }
                let wh = self.module().trail[0].id.clone();
                self.drill(
                    format!("schema {wh}.{ns}"),
                    "lakehouse",
                    format!("/lakehouse/schema/{wh}/{ns}"),
                    ns,
                );
            }
            (Screen::Workspaces, 0, 0) => {
                let id = self.selected_id(&["id", "Id", "workspaceId", "name"]);
                if id.is_empty() {
                    self.status = "select a workspace first".to_string();
                    return;
                }
                self.drill(
                    format!("contents of {id}"),
                    "workspaces",
                    format!("/workspace/{id}/contents"),
                    id,
                );
            }
            (Screen::Deml, 0, 0) => {
                let id = self.selected_id(&["id", "projectId", "name"]);
                if id.is_empty() {
                    self.status = "select a project first".to_string();
                    return;
                }
                self.drill(
                    format!("jobs of {id}"),
                    "etlprojects",
                    format!("/etl/etlJobs/{id}/jobs"),
                    id,
                );
            }
            _ => {
                let def = screen.kinds()[kind];
                match def.detail {
                    Some((service, prefix)) => {
                        let id = self.selected_id(&[
                            "id",
                            "Id",
                            "ID",
                            "conversationId",
                            "scheduleId",
                            "projectId",
                            "connectionId",
                            "name",
                        ]);
                        if id.is_empty() {
                            self.status = "select a row first".to_string();
                            return;
                        }
                        self.request(
                            FetchTarget::ModuleDetail {
                                title: format!("{} {id}", def.label),
                            },
                            format!("{} {id}", def.label),
                            service,
                            Method::GET,
                            format!("{prefix}/{id}"),
                            None,
                        );
                    }
                    None => {
                        let title = format!("{} detail", def.label);
                        match self.module().grid().selected_raw().cloned() {
                            Some(raw) => self.modal = Some(Modal::json(title, &raw)),
                            None => self.status = "nothing selected".to_string(),
                        }
                    }
                }
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if let Some(modal) = self.modal.as_mut() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.modal = None,
                KeyCode::Up | KeyCode::Char('k') => modal.scroll_by(-1, 20),
                KeyCode::Down | KeyCode::Char('j') => modal.scroll_by(1, 20),
                KeyCode::PageUp => modal.scroll_by(-20, 20),
                KeyCode::PageDown => modal.scroll_by(20, 20),
                _ => {}
            }
            return;
        }
        if self.editing_filter {
            match key.code {
                KeyCode::Esc => {
                    self.editing_filter = false;
                }
                KeyCode::Enter => {
                    let value = self.filter_area.lines().join(" ");
                    self.module_mut().grid_mut().filter = value.trim().to_string();
                    self.module_mut().grid_mut().apply_filter();
                    self.editing_filter = false;
                }
                _ => {
                    self.filter_area.input(key);
                }
            }
            return;
        }
        if self.help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => self.help = false,
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => {
                self.quit = true;
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('1') => self.goto(Screen::Chat),
            KeyCode::Char('2') => self.goto(Screen::Workspaces),
            KeyCode::Char('3') => self.goto(Screen::Connections),
            KeyCode::Char('4') => self.goto(Screen::Designer),
            KeyCode::Char('5') => self.goto(Screen::Deml),
            KeyCode::Char('6') => self.goto(Screen::Lakehouse),
            KeyCode::Char('7') => self.goto(Screen::Catalog),
            KeyCode::Char('8') => self.goto(Screen::Schedules),
            KeyCode::Tab => self.switch_kind(1),
            KeyCode::BackTab => self.switch_kind(-1),
            KeyCode::Char('/') => {
                self.editing_filter = true;
            }
            KeyCode::Char('r') if key.modifiers == KeyModifiers::NONE => self.fetch_current(),
            KeyCode::Enter => self.open_selected(),
            KeyCode::Backspace => self.back(),
            KeyCode::Esc => {
                if !self.module().grid().filter.is_empty() {
                    self.module_mut().grid_mut().filter.clear();
                    self.module_mut().grid_mut().apply_filter();
                } else {
                    self.back();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.module_mut().grid_mut().move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.module_mut().grid_mut().move_sel(1),
            KeyCode::Left | KeyCode::Char('h') => self.switch_kind(-1),
            KeyCode::Right | KeyCode::Char('l') => self.switch_kind(1),
            KeyCode::PageUp => self.module_mut().grid_mut().page(20, false),
            KeyCode::PageDown => self.module_mut().grid_mut().page(20, true),
            KeyCode::Home => self.module_mut().grid_mut().home(),
            KeyCode::End => self.module_mut().grid_mut().end(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crossterm::event::KeyModifiers;

    fn app() -> App {
        let config = Config::default();
        let resolved = crate::config::resolve(&config, Some("t"), None, None);
        App::new(resolved)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn switches_modules_and_queues_loads() {
        let mut app = app();
        assert_eq!(app.outbox.len(), 1);
        assert_eq!(app.outbox[0].path, "/api/conversations");
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
        app.fetched(
            1,
            FetchTarget::Module {
                screen: 0,
                kind: 0,
                depth: 0,
            },
            "test".to_string(),
            Ok(serde_json::json!([{"a": 1}, {"a": 2}, {"a": 3}])),
        );
        assert_eq!(app.module().grid().visible_len(), 3);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.module().grid().selected, 2);
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.module().grid().selected, 1);
    }
}
