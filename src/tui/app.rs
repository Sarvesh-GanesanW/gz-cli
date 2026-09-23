use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reqwest::Method;
use serde_json::{json, Value};
use tui_textarea::TextArea;

use crate::config::Resolved;
use crate::tui::fetch::{FetchReq, FetchTarget};
use crate::tui::model::Grid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Warehouses,
    Sql,
    Jobs,
    Files,
    Agents,
    Logs,
}

impl Screen {
    pub const ALL: [Screen; 7] = [
        Screen::Home,
        Screen::Warehouses,
        Screen::Sql,
        Screen::Jobs,
        Screen::Files,
        Screen::Agents,
        Screen::Logs,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Screen::Home => "Home",
            Screen::Warehouses => "Warehouses",
            Screen::Sql => "SQL",
            Screen::Jobs => "Jobs",
            Screen::Files => "Files",
            Screen::Agents => "Agents",
            Screen::Logs => "Logs",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

pub struct Field {
    pub area: TextArea<'static>,
    pub masked: bool,
}

impl Field {
    pub fn new(masked: bool) -> Self {
        let mut area = TextArea::default();
        if masked {
            area.set_mask_char('•');
        }
        Self { area, masked }
    }

    pub fn value(&self) -> String {
        self.area.lines().join("\n").trim().to_string()
    }

    pub fn set(&mut self, text: &str) {
        let mut area = TextArea::new(text.lines().map(str::to_string).collect::<Vec<_>>());
        if self.masked {
            area.set_mask_char('•');
        }
        self.area = area;
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WhLevel {
    List,
    Namespaces,
    Schema,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobsView {
    Status,
    Logs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentLevel {
    List,
    Runs,
}

pub struct App {
    pub resolved: Resolved,
    pub screen: Screen,
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
    // warehouses
    pub wh: Grid,
    pub wh_loaded: bool,
    pub wh_level: WhLevel,
    pub wh_name: String,
    pub wh_ns: String,
    pub wh_trail: Vec<String>,
    // sql
    pub sql_fields: Vec<Field>,
    pub sql_focus: usize,
    pub sql_editing: bool,
    pub sql_editor: TextArea<'static>,
    pub sql_results: Grid,
    pub sql_status: String,
    // jobs
    pub job_fields: Vec<Field>,
    pub job_focus: usize,
    pub job_editing: bool,
    pub job_view: JobsView,
    pub job_grid: Grid,
    pub job_auto: bool,
    // files
    pub fs_conn: Field,
    pub fs_conn_editing: bool,
    pub fs_prefix: String,
    pub fs_grid: Grid,
    pub fs_files: Option<Value>,
    pub fs_folders: Option<Value>,
    // agents
    pub agent_level: AgentLevel,
    pub agents_loaded: bool,
    pub agent_grid: Grid,
    pub agent_id: String,
    pub agent_name: String,
    // logs
    pub logs_loaded: bool,
    pub log_grid: Grid,
}

impl App {
    pub fn new(resolved: Resolved) -> Self {
        let mut sql_editor = TextArea::default();
        sql_editor.set_placeholder_text("SELECT * FROM ...  (Ctrl+R to run)");
        let mut filter_area = TextArea::default();
        filter_area.set_placeholder_text("type to filter, Enter to apply");
        let mut compute = Field::new(false);
        compute.set("6");
        let site = resolved.site().unwrap_or_default();
        let _ = site;
        Self {
            resolved,
            screen: Screen::Home,
            quit: false,
            help: false,
            modal: None,
            editing_filter: false,
            filter_area,
            status: "1-7 switch screens · ? help · q quit".to_string(),
            pending: 0,
            outbox: Vec::new(),
            next_id: 1,
            tick: 0,
            wh: Grid::from_value(&Value::Null, "press r to load"),
            wh_loaded: false,
            wh_level: WhLevel::List,
            wh_name: String::new(),
            wh_ns: String::new(),
            wh_trail: Vec::new(),
            sql_fields: vec![
                Field::new(false),
                Field::new(false),
                Field::new(true),
                compute,
            ],
            sql_focus: 0,
            sql_editing: true,
            sql_editor,
            sql_results: Grid::from_value(&Value::Null, "run a query to see results"),
            sql_status: String::new(),
            job_fields: vec![Field::new(false), Field::new(false)],
            job_focus: 0,
            job_editing: false,
            job_view: JobsView::Status,
            job_grid: Grid::from_value(&Value::Null, "enter a job id, then Enter"),
            job_auto: false,
            fs_conn: Field::new(false),
            fs_conn_editing: false,
            fs_prefix: String::new(),
            fs_grid: Grid::from_value(&Value::Null, "enter a connection, then Enter"),
            fs_files: None,
            fs_folders: None,
            agent_level: AgentLevel::List,
            agents_loaded: false,
            agent_grid: Grid::from_value(&Value::Null, "press r to load"),
            agent_id: String::new(),
            agent_name: String::new(),
            logs_loaded: false,
            log_grid: Grid::from_value(&Value::Null, "press r to load"),
        }
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
            FetchTarget::Warehouses => {
                self.wh_level = WhLevel::List;
                self.wh_trail.clear();
                self.wh = Grid::from_value(&value, "no warehouses");
            }
            FetchTarget::Namespaces => {
                self.wh_level = WhLevel::Namespaces;
                self.wh = Grid::from_value(&value, "no namespaces");
            }
            FetchTarget::NamespaceSchema => {
                self.wh_level = WhLevel::Schema;
                self.wh = Grid::from_value(&value, "empty schema");
            }
            FetchTarget::TableDescribe => {
                self.modal = Some(Modal::json(
                    format!("describe {}.{}", self.wh_ns, self.table_name()),
                    &value,
                ));
            }
            FetchTarget::TableSample => {
                self.modal = Some(Modal::json(
                    format!("sample {}.{}", self.wh_ns, self.table_name()),
                    &value,
                ));
            }
            FetchTarget::SqlRun => {
                self.sql_results = Grid::from_value(&value, "no rows");
                self.sql_status = format!("{} row(s)", self.sql_results.visible_len());
            }
            FetchTarget::JobStatus => {
                if self.job_view == JobsView::Status {
                    self.job_grid = Grid::from_value(&value, "no status");
                }
            }
            FetchTarget::JobLogs => {
                if self.job_view == JobsView::Logs {
                    self.job_grid = Grid::from_value(&value, "no logs yet");
                }
            }
            FetchTarget::JobSubmit => {
                self.modal = Some(Modal::json("submit result".to_string(), &value));
            }
            FetchTarget::Files => {
                self.fs_files = Some(value);
                self.merge_fs();
            }
            FetchTarget::Folders => {
                self.fs_folders = Some(value);
                self.merge_fs();
            }
            FetchTarget::FileDownload { .. } => {
                self.modal = Some(Modal::json("download".to_string(), &value));
            }
            FetchTarget::Agents => {
                self.agent_level = AgentLevel::List;
                self.agent_grid = Grid::from_value(&value, "no agents");
            }
            FetchTarget::AgentRuns => {
                self.agent_level = AgentLevel::Runs;
                self.agent_grid = Grid::from_value(&value, "no runs");
            }
            FetchTarget::RunDetail => {
                self.modal = Some(Modal::json(format!("run {}", self.agent_name), &value));
            }
            FetchTarget::Schedules => {
                self.log_grid = Grid::from_value(&value, "no schedules");
            }
            FetchTarget::ScheduleDetail => {
                self.modal = Some(Modal::json("schedule".to_string(), &value));
            }
        }
    }

    fn table_name(&self) -> String {
        if !self.wh.filter.is_empty() {
            return self.wh.filter.clone();
        }
        self.wh
            .selected_cells()
            .and_then(|r| r.first().cloned())
            .unwrap_or_default()
    }

    fn merge_fs(&mut self) {
        let mut rows: Vec<Value> = Vec::new();
        for (kind, slot) in [("dir", &self.fs_folders), ("file", &self.fs_files)] {
            if let Some(Value::Array(items)) = slot.as_ref().map(crate::output::tabular) {
                for item in items {
                    let name = item
                        .get("key")
                        .or_else(|| item.get("name"))
                        .or_else(|| item.get("prefix"))
                        .cloned()
                        .unwrap_or_else(|| item.clone());
                    rows.push(json!({"type": kind, "name": name, "size": item.get("size").cloned().unwrap_or(Value::Null)}));
                }
            }
        }
        if self.fs_files.is_some() || self.fs_folders.is_some() {
            self.fs_grid = Grid::from_value(&Value::Array(rows), "empty folder");
            self.fs_grid.filter = self.fs_grid.filter.clone();
            self.fs_grid.apply_filter();
        }
    }

    pub fn on_tick(&mut self) {
        self.tick += 1;
        if self.job_auto && self.tick % 40 == 0 && !self.job_fields[0].value().is_empty() {
            self.fetch_job(false);
        }
    }

    // ----- fetch builders -----

    fn fetch_warehouses(&mut self) {
        self.request(
            FetchTarget::Warehouses,
            "warehouses".to_string(),
            "lakehouse",
            Method::GET,
            "/lakehouse/warehouse".to_string(),
            None,
        );
    }

    fn fetch_namespaces(&mut self) {
        let name = self.selected_name(&self.wh.clone(), &["name", "warehouseName", "warehouse"]);
        if name.is_empty() {
            self.status = "select a warehouse first".to_string();
            return;
        }
        self.wh_name = name.clone();
        self.wh_trail = vec![name.clone()];
        self.request(
            FetchTarget::Namespaces,
            format!("namespaces of {name}"),
            "lakehouse",
            Method::GET,
            format!("/lakehouse/warehouse/{name}/namespaces"),
            None,
        );
    }

    fn fetch_schema(&mut self) {
        let ns = self.selected_name(&self.wh.clone(), &["name", "namespace", "namespaceName"]);
        if ns.is_empty() {
            self.status = "select a namespace first".to_string();
            return;
        }
        self.wh_ns = ns.clone();
        self.wh_trail = vec![self.wh_name.clone(), ns.clone()];
        let wh = self.wh_name.clone();
        self.request(
            FetchTarget::NamespaceSchema,
            format!("schema {wh}.{ns}"),
            "lakehouse",
            Method::GET,
            format!("/lakehouse/schema/{wh}/{ns}"),
            None,
        );
    }

    fn selected_name(&self, grid: &Grid, keys: &[&str]) -> String {
        let raw = grid.selected_raw().cloned().unwrap_or(Value::Null);
        if let Value::Object(map) = &raw {
            for key in keys {
                if let Some(Value::String(s)) = map.get(*key) {
                    return s.clone();
                }
            }
        }
        grid.selected_cells()
            .and_then(|r| r.first().cloned())
            .unwrap_or_default()
    }

    fn fetch_job(&mut self, logs: bool) {
        let job = self.job_fields[0].value();
        if job.is_empty() {
            self.status = "enter a job id first (i to edit)".to_string();
            return;
        }
        if logs {
            self.job_view = JobsView::Logs;
            self.request(
                FetchTarget::JobLogs,
                format!("logs {job}"),
                "etl",
                Method::POST,
                "/etl/logs".to_string(),
                Some(json!({"jobIdentifier": job})),
            );
        } else {
            self.job_view = JobsView::Status;
            self.request(
                FetchTarget::JobStatus,
                format!("status {job}"),
                "etl",
                Method::POST,
                "/etl/jobStatus".to_string(),
                Some(json!({"jobIdentifier": job})),
            );
        }
    }

    fn submit_job(&mut self) {
        let path = self.job_fields[1].value();
        if path.is_empty() {
            self.status = "enter a job spec file first".to_string();
            return;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(body) => self.request(
                    FetchTarget::JobSubmit,
                    "submit".to_string(),
                    "etl",
                    Method::POST,
                    "/etl/job".to_string(),
                    Some(body),
                ),
                Err(e) => self.status = format!("{path} is not valid JSON: {e}"),
            },
            Err(e) => self.status = format!("cannot read {path}: {e}"),
        }
    }

    fn fetch_fs(&mut self) {
        let conn = self.fs_conn.value();
        if conn.is_empty() {
            self.status = "enter a connection first (i to edit)".to_string();
            return;
        }
        self.fs_files = None;
        self.fs_folders = None;
        let body = json!({"ConnectionName": conn, "prefix": self.fs_prefix});
        self.request(
            FetchTarget::Files,
            "files".to_string(),
            "data",
            Method::POST,
            "/storage/listFiles".to_string(),
            Some(body.clone()),
        );
        self.request(
            FetchTarget::Folders,
            "folders".to_string(),
            "data",
            Method::POST,
            "/storage/listFolders".to_string(),
            Some(body),
        );
    }

    fn fetch_agents(&mut self) {
        self.request(
            FetchTarget::Agents,
            "agents".to_string(),
            "agents",
            Method::GET,
            "/v1/agents".to_string(),
            None,
        );
    }

    fn fetch_runs(&mut self) {
        let id = self.selected_name(&self.agent_grid.clone(), &["id", "agent_id"]);
        if id.is_empty() {
            self.status = "select an agent first".to_string();
            return;
        }
        self.agent_id = id.clone();
        self.agent_name = self.selected_name(&self.agent_grid.clone(), &["name", "title"]);
        self.request(
            FetchTarget::AgentRuns,
            "runs".to_string(),
            "agents",
            Method::GET,
            format!("/v1/agents/{id}/runs"),
            None,
        );
    }

    fn fetch_run_detail(&mut self, observability: bool) {
        let run = self.selected_name(&self.agent_grid.clone(), &["id", "run_id"]);
        if run.is_empty() || self.agent_id.is_empty() {
            return;
        }
        let id = self.agent_id.clone();
        let path = if observability {
            format!("/v1/agents/{id}/runs/{run}/observability")
        } else {
            format!("/v1/agents/{id}/runs/{run}/events")
        };
        self.request(
            FetchTarget::RunDetail,
            "run detail".to_string(),
            "agents",
            Method::GET,
            path,
            None,
        );
    }

    fn fetch_schedules(&mut self) {
        self.request(
            FetchTarget::Schedules,
            "schedules".to_string(),
            "logs",
            Method::GET,
            "/log-export-schedules".to_string(),
            None,
        );
    }

    fn run_sql(&mut self) {
        let sql = self.sql_editor.lines().join("\n");
        if sql.trim().is_empty() {
            self.status = "empty query".to_string();
            return;
        }
        let warehouse = self.sql_fields[0].value();
        let username = self.sql_fields[1].value();
        let password = if self.sql_fields[2].value().is_empty() {
            std::env::var("GZ_PASSWORD").unwrap_or_default()
        } else {
            self.sql_fields[2].value()
        };
        let compute: i64 = self.sql_fields[3].value().parse().unwrap_or(6);
        if warehouse.is_empty() || username.is_empty() || password.is_empty() {
            self.status =
                "warehouse, username and password are required (password field or GZ_PASSWORD)"
                    .to_string();
            return;
        }
        let body = json!({
            "query": sql,
            "computeId": compute,
            "waitForOutput": true,
            "connectionConfig": {"config": {"userName": username, "password": password, "warehouseName": warehouse}},
        });
        self.request(
            FetchTarget::SqlRun,
            "query".to_string(),
            "lakehouse",
            Method::POST,
            "/lakehouse/runquery".to_string(),
            Some(body),
        );
    }

    // ----- keys -----

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.help {
            self.help = false;
            return;
        }
        if let Some(modal) = self.modal.as_mut() {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.modal = None,
                KeyCode::Up | KeyCode::Char('k') => modal.scroll = modal.scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => modal.scroll += 1,
                KeyCode::PageUp => modal.scroll = modal.scroll.saturating_sub(10),
                KeyCode::PageDown => modal.scroll += 10,
                _ => {}
            }
            return;
        }
        if self.editing_filter {
            match key.code {
                KeyCode::Esc => self.editing_filter = false,
                KeyCode::Enter => {
                    self.editing_filter = false;
                    self.current_grid_mut().filter = self.filter_area.lines().join("");
                    self.current_grid_mut().apply_filter();
                }
                _ => {
                    use tui_textarea::Input;
                    self.filter_area.input(Input::from(key));
                }
            }
            return;
        }
        if self.screen == Screen::Sql && self.sql_editing {
            self.sql_key(key);
            return;
        }
        if self.screen == Screen::Jobs && self.job_editing {
            match key.code {
                KeyCode::Esc => self.job_editing = false,
                KeyCode::Tab => {
                    self.job_focus = (self.job_focus + 1) % self.job_fields.len();
                }
                KeyCode::Enter => {
                    self.job_editing = false;
                    self.fetch_job(false);
                }
                _ => {
                    use tui_textarea::Input;
                    self.job_fields[self.job_focus].area.input(Input::from(key));
                }
            }
            return;
        }
        if self.screen == Screen::Files && self.fs_conn_editing {
            match key.code {
                KeyCode::Esc => self.fs_conn_editing = false,
                KeyCode::Enter => {
                    self.fs_conn_editing = false;
                    self.fs_prefix.clear();
                    self.fetch_fs();
                }
                _ => {
                    use tui_textarea::Input;
                    self.fs_conn.area.input(Input::from(key));
                }
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('1') => self.goto(Screen::Home),
            KeyCode::Char('2') => self.goto(Screen::Warehouses),
            KeyCode::Char('3') => self.goto(Screen::Sql),
            KeyCode::Char('4') => self.goto(Screen::Jobs),
            KeyCode::Char('5') => self.goto(Screen::Files),
            KeyCode::Char('6') => self.goto(Screen::Agents),
            KeyCode::Char('7') => self.goto(Screen::Logs),
            KeyCode::Tab => {
                let next = (self.screen.index() + 1) % Screen::ALL.len();
                self.goto(Screen::ALL[next]);
            }
            KeyCode::BackTab => {
                let next = (self.screen.index() + Screen::ALL.len() - 1) % Screen::ALL.len();
                self.goto(Screen::ALL[next]);
            }
            _ => self.screen_key(key),
        }
    }

    fn goto(&mut self, screen: Screen) {
        self.screen = screen;
        self.editing_filter = false;
        self.modal = None;
        match screen {
            Screen::Warehouses if !self.wh_loaded => {
                self.wh_loaded = true;
                self.fetch_warehouses();
            }
            Screen::Agents if !self.agents_loaded => {
                self.agents_loaded = true;
                self.fetch_agents();
            }
            Screen::Logs if !self.logs_loaded => {
                self.logs_loaded = true;
                self.fetch_schedules();
            }
            _ => {}
        }
    }

    fn sql_key(&mut self, key: KeyEvent) {
        use tui_textarea::Input;
        if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.run_sql();
            return;
        }
        match key.code {
            KeyCode::Esc => self.sql_editing = false,
            KeyCode::Tab => {
                self.sql_focus = (self.sql_focus + 1) % (self.sql_fields.len() + 1);
            }
            _ => {
                if self.sql_focus < self.sql_fields.len() {
                    if key.code == KeyCode::Enter {
                        self.sql_focus += 1;
                    } else {
                        self.sql_fields[self.sql_focus].area.input(Input::from(key));
                    }
                } else {
                    self.sql_editor.input(Input::from(key));
                }
            }
        }
    }

    fn screen_key(&mut self, key: KeyEvent) {
        match self.screen {
            Screen::Home => {
                if key.code == KeyCode::Char('r') {
                    self.status = "home is live already".to_string();
                }
            }
            Screen::Warehouses => match key.code {
                KeyCode::Enter => match self.wh_level {
                    WhLevel::List => self.fetch_namespaces(),
                    WhLevel::Namespaces => self.fetch_schema(),
                    WhLevel::Schema => self.describe_table(),
                },
                KeyCode::Backspace => self.wh_up(),
                KeyCode::Char('r') => self.wh_refresh(),
                KeyCode::Char('d') => self.describe_table(),
                KeyCode::Char('s') => self.sample_table(),
                key => self.grid_key(key, true),
            },
            Screen::Sql => match key.code {
                KeyCode::Char('i') => self.sql_editing = true,
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.run_sql()
                }
                key => self.grid_key(key, false),
            },
            Screen::Jobs => match key.code {
                KeyCode::Char('i') => self.job_editing = true,
                KeyCode::Enter => self.fetch_job(self.job_view == JobsView::Logs),
                KeyCode::Char('t') => {
                    self.job_view = if self.job_view == JobsView::Status {
                        JobsView::Logs
                    } else {
                        JobsView::Status
                    };
                    self.fetch_job(self.job_view == JobsView::Logs);
                }
                KeyCode::Char('a') => {
                    self.job_auto = !self.job_auto;
                    self.status = format!(
                        "auto-refresh {}",
                        if self.job_auto { "on (5s)" } else { "off" }
                    );
                }
                KeyCode::Char('s') => self.submit_job(),
                KeyCode::Char('r') => self.fetch_job(self.job_view == JobsView::Logs),
                key => self.grid_key(key, false),
            },
            Screen::Files => match key.code {
                KeyCode::Char('i') => self.fs_conn_editing = true,
                KeyCode::Enter => self.fs_descend(),
                KeyCode::Backspace => self.fs_up(),
                KeyCode::Char('d') => self.fs_download(),
                KeyCode::Char('r') => self.fetch_fs(),
                key => self.grid_key(key, true),
            },
            Screen::Agents => match key.code {
                KeyCode::Enter => match self.agent_level {
                    AgentLevel::List => self.fetch_runs(),
                    AgentLevel::Runs => self.fetch_run_detail(false),
                },
                KeyCode::Backspace => {
                    if self.agent_level == AgentLevel::Runs {
                        self.fetch_agents();
                    }
                }
                KeyCode::Char('o') => {
                    if self.agent_level == AgentLevel::Runs {
                        self.fetch_run_detail(true);
                    }
                }
                KeyCode::Char('r') => match self.agent_level {
                    AgentLevel::List => self.fetch_agents(),
                    AgentLevel::Runs => {
                        let id = self.agent_id.clone();
                        self.request(
                            FetchTarget::AgentRuns,
                            "runs".to_string(),
                            "agents",
                            Method::GET,
                            format!("/v1/agents/{id}/runs"),
                            None,
                        );
                    }
                },
                key => self.grid_key(key, true),
            },
            Screen::Logs => match key.code {
                KeyCode::Enter => self.log_detail(),
                KeyCode::Char('r') => self.fetch_schedules(),
                key => self.grid_key(key, false),
            },
        }
    }

    fn grid_key(&mut self, code: KeyCode, allow_filter: bool) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.current_grid_mut().move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.current_grid_mut().move_sel(1),
            KeyCode::PageUp => self.current_grid_mut().page(20, false),
            KeyCode::PageDown => self.current_grid_mut().page(20, true),
            KeyCode::Home => self.current_grid_mut().home(),
            KeyCode::End => self.current_grid_mut().end(),
            KeyCode::Left | KeyCode::Char('h') => {
                let grid = self.current_grid_mut();
                grid.col_offset = grid.col_offset.saturating_sub(1);
            }
            KeyCode::Right | KeyCode::Char('l') => {
                let grid = self.current_grid_mut();
                grid.col_offset += 1;
            }
            KeyCode::Char('/') if allow_filter => {
                self.filter_area = TextArea::default();
                self.editing_filter = true;
            }
            KeyCode::Char('v') => self.view_row(),
            KeyCode::Char('c') => {
                self.current_grid_mut().filter.clear();
                self.current_grid_mut().apply_filter();
                self.status = "filter cleared".to_string();
            }
            _ => {}
        }
    }

    fn current_grid_mut(&mut self) -> &mut Grid {
        match self.screen {
            Screen::Warehouses => &mut self.wh,
            Screen::Sql => &mut self.sql_results,
            Screen::Jobs => &mut self.job_grid,
            Screen::Files => &mut self.fs_grid,
            Screen::Agents => &mut self.agent_grid,
            Screen::Logs => &mut self.log_grid,
            Screen::Home => &mut self.wh,
        }
    }

    fn view_row(&mut self) {
        if let Some(raw) = self.current_grid_mut().selected_raw().cloned() {
            self.modal = Some(Modal::json("row".to_string(), &raw));
        }
    }

    fn describe_table(&mut self) {
        let table = self.table_name();
        if table.is_empty() || self.wh_name.is_empty() || self.wh_ns.is_empty() {
            self.status =
                "open a namespace, then type a table name in / filter (or select its row)"
                    .to_string();
            return;
        }
        let (wh, ns) = (self.wh_name.clone(), self.wh_ns.clone());
        self.request(
            FetchTarget::TableDescribe,
            format!("describe {table}"),
            "lakehouse",
            Method::GET,
            format!("/lakehouse/schema/{wh}/{ns}/{table}"),
            None,
        );
    }

    fn sample_table(&mut self) {
        let table = self.table_name();
        if table.is_empty() || self.wh_name.is_empty() || self.wh_ns.is_empty() {
            self.status =
                "open a namespace, then type a table name in / filter (or select its row)"
                    .to_string();
            return;
        }
        let (wh, ns) = (self.wh_name.clone(), self.wh_ns.clone());
        self.request(
            FetchTarget::TableSample,
            format!("sample {table}"),
            "lakehouse",
            Method::GET,
            format!("/lakehouse/samplerecords/{wh}/{ns}/{table}"),
            None,
        );
    }

    fn wh_up(&mut self) {
        match self.wh_level {
            WhLevel::List => {}
            WhLevel::Namespaces => self.fetch_warehouses(),
            WhLevel::Schema => {
                self.wh_level = WhLevel::Namespaces;
                let name = self.wh_name.clone();
                self.wh_trail = vec![name.clone()];
                self.request(
                    FetchTarget::Namespaces,
                    format!("namespaces of {name}"),
                    "lakehouse",
                    Method::GET,
                    format!("/lakehouse/warehouse/{name}/namespaces"),
                    None,
                );
            }
        }
    }

    fn wh_refresh(&mut self) {
        match self.wh_level {
            WhLevel::List => self.fetch_warehouses(),
            WhLevel::Namespaces => {
                let name = self.wh_name.clone();
                self.request(
                    FetchTarget::Namespaces,
                    "namespaces".to_string(),
                    "lakehouse",
                    Method::GET,
                    format!("/lakehouse/warehouse/{name}/namespaces"),
                    None,
                );
            }
            WhLevel::Schema => {
                let (wh, ns) = (self.wh_name.clone(), self.wh_ns.clone());
                self.request(
                    FetchTarget::NamespaceSchema,
                    "schema".to_string(),
                    "lakehouse",
                    Method::GET,
                    format!("/lakehouse/schema/{wh}/{ns}"),
                    None,
                );
            }
        }
    }

    fn fs_descend(&mut self) {
        let Some(cells) = self.fs_grid.selected_cells().cloned() else {
            return;
        };
        let kind = cells.first().cloned().unwrap_or_default();
        let name = cells.get(1).cloned().unwrap_or_default();
        if kind != "dir" || name.is_empty() || name == "—" {
            return;
        }
        if name == ".." {
            self.fs_up();
            return;
        }
        if !self.fs_prefix.is_empty() && !self.fs_prefix.ends_with('/') {
            self.fs_prefix.push('/');
        }
        self.fs_prefix.push_str(name.trim_end_matches('/'));
        self.fs_prefix.push('/');
        self.fetch_fs();
    }

    fn fs_up(&mut self) {
        let trimmed = self.fs_prefix.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(i) => self.fs_prefix = trimmed[..=i].to_string(),
            None => self.fs_prefix.clear(),
        }
        self.fetch_fs();
    }

    fn fs_download(&mut self) {
        let Some(cells) = self.fs_grid.selected_cells().cloned() else {
            return;
        };
        let kind = cells.first().cloned().unwrap_or_default();
        let name = cells.get(1).cloned().unwrap_or_default();
        if kind != "file" || name.is_empty() || name == "—" {
            self.status = "select a file to download".to_string();
            return;
        }
        let conn = self.fs_conn.value();
        let key = format!("{}{}", self.fs_prefix, name);
        let dest = name
            .rsplit('/')
            .next()
            .unwrap_or("download.bin")
            .to_string();
        let body = json!({"ConnectionName": conn, "fileNameWithPrefix": key});
        self.request(
            FetchTarget::FileDownload { dest },
            format!("download {name}"),
            "files",
            Method::POST,
            "/api/storage/getFile".to_string(),
            Some(body),
        );
    }

    pub fn agent_trail(&self) -> Vec<String> {
        if self.agent_level == AgentLevel::Runs && !self.agent_name.is_empty() {
            vec![self.agent_name.clone()]
        } else {
            Vec::new()
        }
    }

    pub fn job_view_title(&self) -> &'static str {
        match self.job_view {
            JobsView::Status => "status",
            JobsView::Logs => "logs",
        }
    }

    fn log_detail(&mut self) {
        let name = self.selected_name(&self.log_grid.clone(), &["name", "schedule", "id"]);
        if name.is_empty() {
            return;
        }
        self.request(
            FetchTarget::ScheduleDetail,
            "schedule".to_string(),
            "logs",
            Method::GET,
            format!("/log-export-schedules/{name}"),
            None,
        );
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
    fn switches_screens_and_queues_loads() {
        let mut app = app();
        app.on_key(key(KeyCode::Char('6')));
        assert_eq!(app.screen, Screen::Agents);
        assert_eq!(app.outbox.len(), 1);
        assert_eq!(app.outbox[0].path, "/v1/agents");
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn sql_ctrl_r_validates_before_fetch() {
        let mut app = app();
        app.goto(Screen::Sql);
        app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(app.outbox.is_empty());
        assert!(!app.status.is_empty());
    }

    #[test]
    fn modal_eats_keys_until_closed() {
        let mut app = app();
        app.modal = Some(Modal::json("t".to_string(), &serde_json::json!({"a": 1})));
        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.screen, Screen::Home);
        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }
}
