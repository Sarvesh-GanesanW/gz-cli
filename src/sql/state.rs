//! The SQL shell's state: input, queued statements, the last result, the
//! warehouse's names for completion, and toggles. Keys and finished work
//! go in; screen entries and [`Effect`]s come out. No terminal or network
//! here.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;
use tui_textarea::TextArea;

use crate::sql::catalog::Catalog;
use crate::sql::describe::TableDescription;
use crate::sql::history::History;
use crate::sql::menu::Menu;
use crate::sql::meta::{self, Meta};
use crate::sql::results::{Page, Reply};
use crate::sql::save::{self, Destination, OutputTarget, Saved};
use crate::sql::session::Outcome;
use crate::sql::statements;

/// The server's own words: drops the `POST https://… → ` request prefix the
/// HTTP layer adds, keeping the status and message.
pub fn readable_error(error: &str) -> String {
    let message = crate::http::readable_error(error);
    match hint_for(&message) {
        Some(hint) => format!("{message}\n{hint}"),
        None => message,
    }
}

/// A next step for server errors whose wording doesn't explain the cause.
fn hint_for(message: &str) -> Option<&'static str> {
    // The provider's authorizer answers this for statements it can't parse
    // into an operation, which in practice means "not valid Spark SQL".
    message
        .contains("Unable to determine operations")
        .then_some(
            "The lakehouse couldn't read that statement. It runs Spark SQL, e.g. \
         SHOW DATABASES, SHOW TABLES IN db, DESCRIBE db.table, SELECT … \
         (or use \\d, \\dt db, \\d db.table).",
        )
}

/// How long a first Ctrl+C waits for the second one that exits.
const EXIT_WINDOW: Duration = Duration::from_secs(2);

/// Session states in which statements can run.
const LIVE_STATES: &[&str] = &["Running", "Starting"];

/// What the provider answers once a session's pod is gone
/// (groundzero-iceberg-provider-api `iceberg/lakehouse_query_client.py`).
const UNREACHABLE: &str = "session is not reachable";

/// Something to print into scrollback.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Statement(String),
    Rows {
        page: Page,
        elapsed: Option<Duration>,
        expanded: bool,
    },
    Described(TableDescription),
    Done {
        message: String,
        elapsed: Option<Duration>,
    },
    Error {
        message: String,
        statement: Option<String>,
    },
    Notice(String),
    Info(Vec<String>),
}

/// Rows to write to a file: a finished query to page through, or a page
/// already in hand when the result has no query id to page by.
#[derive(Debug, Clone, PartialEq)]
pub enum SaveSource {
    Query(String),
    Page(Page),
}

/// Work for the driver.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Run(String),
    NextPage {
        query: String,
        page: u32,
    },
    Save {
        source: SaveSource,
        to: Destination,
    },
    /// Read the warehouse's names for completion.
    LoadCatalog,
    /// Ask the server whether the session still runs.
    CheckSession,
    /// Start a new session in place of the stopped one.
    Restart,
    Cancel,
    Exit,
}

/// Finished work reported back by the driver.
#[derive(Debug)]
pub enum Done {
    Ran(Outcome),
    Failed {
        statement: String,
        error: String,
    },
    Paged(Result<Page, String>),
    Saved(Result<Saved, String>),
    Catalog(Result<Catalog, String>),
    /// The new session's id.
    Restarted(Result<String, String>),
}

/// How a statement's rows are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Rows,
    Describe,
}

#[derive(Debug, Clone)]
struct Queued {
    sql: String,
    view: View,
}

#[derive(Debug, Clone)]
struct LastResult {
    query: Option<String>,
    page: Page,
}

/// The warehouse's names, as far as they have been read.
#[derive(Debug, Clone)]
pub enum Names {
    Loading,
    Ready(Catalog),
    Failed(String),
}

impl Names {
    fn catalog(&self) -> Option<&Catalog> {
        match self {
            Names::Ready(catalog) => Some(catalog),
            Names::Loading | Names::Failed(_) => None,
        }
    }
}

pub struct ShellState {
    pub input: TextArea<'static>,
    history: History,
    pub entries: Vec<Entry>,
    /// What is running and since when (spinner text).
    pub running: Option<(String, Instant)>,
    queue: VecDeque<Queued>,
    /// How the running statement's rows are shown.
    view: View,
    pub expanded: bool,
    pub timing: bool,
    pub warehouse: String,
    pub session_state: String,
    last: Option<LastResult>,
    /// `\o`: where the next results go.
    pub output: Option<OutputTarget>,
    pub names: Names,
    /// `\refresh` asked for the names, so say when they arrive.
    refresh_asked: bool,
    pub menu: Option<Menu>,
    /// Why the session stopped, while the restart offer is open.
    pub dropped: Option<String>,
    exit_armed: Option<Instant>,
    pub tick: u64,
}

pub fn new_input() -> TextArea<'static> {
    let mut input = TextArea::default();
    input.set_placeholder_text("SQL ending in ;  ·  tab completes  ·  \\help for commands");
    input.set_cursor_line_style(ratatui::style::Style::default());
    input
}

impl ShellState {
    pub fn new(warehouse: String, history: History) -> Self {
        Self {
            input: new_input(),
            history,
            entries: Vec::new(),
            running: None,
            queue: VecDeque::new(),
            view: View::Rows,
            expanded: false,
            timing: true,
            warehouse,
            session_state: "Running".to_string(),
            last: None,
            output: None,
            names: Names::Loading,
            refresh_asked: false,
            menu: None,
            dropped: None,
            exit_armed: None,
            tick: 0,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.running.is_some()
    }

    pub fn exit_armed(&self) -> bool {
        self.exit_armed.is_some_and(|at| at.elapsed() < EXIT_WINDOW)
    }

    pub fn input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    fn catalog(&self) -> Option<&Catalog> {
        self.names.catalog()
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.menu = None;
            return self.on_ctrl_c();
        }
        self.exit_armed = None;
        if let Some(effects) = self.on_completion_key(key) {
            return effects;
        }
        if let Some(effects) = self.on_restart_key(key) {
            return effects;
        }
        match key.code {
            KeyCode::Char('d') if ctrl && self.input_text().is_empty() => vec![Effect::Exit],
            KeyCode::Char('u') if ctrl => {
                self.input = new_input();
                Vec::new()
            }
            KeyCode::Esc if self.is_busy() => vec![Effect::Cancel],
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.input.insert_newline();
                Vec::new()
            }
            KeyCode::Enter if statements::is_ready(&self.input_text()) => {
                let text = self.input_text();
                self.input = new_input();
                self.submit(&text)
            }
            KeyCode::Enter => {
                self.input.insert_newline();
                Vec::new()
            }
            KeyCode::Char(' ') if self.input_text().is_empty() && self.can_page() => {
                self.next_page()
            }
            KeyCode::Up if self.input.lines().len() == 1 => {
                self.recall(true);
                Vec::new()
            }
            KeyCode::Down if self.input.lines().len() == 1 => {
                self.recall(false);
                Vec::new()
            }
            _ => {
                self.input.input(key);
                Vec::new()
            }
        }
    }

    /// Tab opens or steps the completion menu; any other key closes it
    /// (Esc only closes it).
    fn on_completion_key(&mut self, key: KeyEvent) -> Option<Vec<Effect>> {
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                let forward =
                    key.code == KeyCode::Tab && !key.modifiers.contains(KeyModifiers::SHIFT);
                match self.menu.as_mut() {
                    Some(menu) => menu.step(&mut self.input, forward),
                    None => self.menu = Menu::open(&mut self.input, self.names.catalog()),
                }
                Some(Vec::new())
            }
            KeyCode::Esc if self.menu.is_some() => {
                self.menu = None;
                Some(Vec::new())
            }
            _ => {
                self.menu = None;
                None
            }
        }
    }

    /// `y` or `n` on an empty input answers the restart offer.
    fn on_restart_key(&mut self, key: KeyEvent) -> Option<Vec<Effect>> {
        if self.dropped.is_none() || !self.input_text().is_empty() || self.is_busy() {
            return None;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(self.restart()),
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.dropped = None;
                self.entries.push(Entry::Notice(
                    "Not restarted · statements fail until you \\restart · \\q quits".into(),
                ));
                Some(Vec::new())
            }
            _ => None,
        }
    }

    fn on_ctrl_c(&mut self) -> Vec<Effect> {
        if self.is_busy() {
            return vec![Effect::Cancel];
        }
        if !self.input_text().is_empty() {
            self.input = new_input();
            return Vec::new();
        }
        if self.exit_armed() {
            return vec![Effect::Exit];
        }
        self.exit_armed = Some(Instant::now());
        Vec::new()
    }

    fn recall(&mut self, older: bool) {
        let text = self.history.step(older).unwrap_or_default();
        let mut input = new_input();
        input.insert_str(&text);
        self.input = input;
    }

    fn remember(&mut self, text: &str) {
        match self.history.push(text) {
            Ok(true) => {}
            Ok(false) => self.entries.push(Entry::Notice(
                "Not saved to history: it looks like it holds a credential".into(),
            )),
            Err(err) => self
                .entries
                .push(Entry::Notice(format!("history not saved: {err:#}"))),
        }
    }

    /// Runs typed text: a meta command, or every complete statement in it.
    pub fn submit(&mut self, text: &str) -> Vec<Effect> {
        let text = text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        self.remember(text);
        if text.starts_with('\\') {
            return match meta::parse(text) {
                Ok(command) => self.meta(command),
                Err(message) => {
                    self.error(message);
                    Vec::new()
                }
            };
        }
        let parts = statements::split(text);
        let mut list = parts.statements;
        if statements::has_code(&parts.rest) {
            list.push(parts.rest);
        }
        self.queue.extend(list.into_iter().map(|sql| Queued {
            sql,
            view: View::Rows,
        }));
        self.start_next()
    }

    fn error(&mut self, message: String) {
        self.entries.push(Entry::Error {
            message,
            statement: None,
        });
    }

    fn notice(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::Notice(text.into()));
    }

    fn meta(&mut self, command: Meta) -> Vec<Effect> {
        match command {
            Meta::Describe(table) => self.describe(&table),
            Meta::Tables(None) => {
                self.list_all_tables();
                Vec::new()
            }
            Meta::Databases | Meta::Tables(Some(_)) => {
                let sql = command.sql().unwrap_or_default();
                self.queue.push_back(Queued {
                    sql,
                    view: View::Rows,
                });
                self.start_next()
            }
            Meta::Quit => vec![Effect::Exit],
            Meta::Help => {
                self.entries.push(Entry::Info(
                    meta::HELP.iter().map(|s| s.to_string()).collect(),
                ));
                Vec::new()
            }
            Meta::Expanded => {
                self.expanded = !self.expanded;
                let state = if self.expanded { "on" } else { "off" };
                self.notice(format!("Expanded display {state}"));
                Vec::new()
            }
            Meta::Timing => {
                self.timing = !self.timing;
                let state = if self.timing { "on" } else { "off" };
                self.notice(format!("Timing {state}"));
                Vec::new()
            }
            Meta::More => {
                if self.can_page() {
                    self.next_page()
                } else {
                    self.notice("No more rows");
                    Vec::new()
                }
            }
            Meta::Output(path) => {
                self.set_output(path);
                Vec::new()
            }
            Meta::Save(path) => self.save_last(path),
            Meta::Refresh => {
                self.names = Names::Loading;
                self.refresh_asked = true;
                self.notice("Reading the warehouse's names…");
                vec![Effect::LoadCatalog]
            }
            Meta::Restart if self.dropped.is_some() || !self.session_live() => self.restart(),
            Meta::Restart => {
                self.notice("The session is running; nothing to restart");
                Vec::new()
            }
            Meta::Use(db) => {
                self.notice(format!(
                    "The lakehouse doesn't support USE; qualify names instead, e.g. {db}.my_table"
                ));
                Vec::new()
            }
        }
    }

    fn session_live(&self) -> bool {
        LIVE_STATES.contains(&self.session_state.as_str())
    }

    /// `\d table`: a bare table name is qualified from the catalog when
    /// exactly one database has it (the lakehouse refuses bare names).
    fn describe(&mut self, table: &str) -> Vec<Effect> {
        let name = match self.qualified_table(table) {
            Ok(name) => name,
            Err(message) => {
                self.error(message);
                return Vec::new();
            }
        };
        let sql = Meta::Describe(name).sql().unwrap_or_default();
        self.queue.push_back(Queued {
            sql,
            view: View::Describe,
        });
        self.start_next()
    }

    fn qualified_table(&self, table: &str) -> Result<String, String> {
        let Some(catalog) = self.catalog().filter(|_| !table.contains('.')) else {
            return Ok(table.to_string());
        };
        match catalog.tables_named(table).as_slice() {
            [(database, found)] => Ok(format!("{}.{}", database.name, found.name)),
            [] => Err(format!(
                "No table {table} in {}; name it as db.table",
                self.warehouse
            )),
            several => Err(format!(
                "{table} is in several databases: {}",
                several
                    .iter()
                    .map(|(d, t)| format!("{}.{}", d.name, t.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// `\dt`: every table, from the catalog read at the start.
    fn list_all_tables(&mut self) {
        let message = match &self.names {
            Names::Ready(catalog) => {
                let rows = catalog
                    .databases
                    .iter()
                    .flat_map(|d| {
                        d.tables.iter().map(move |t| {
                            json!({"database": d.name, "table": t.name, "columns": t.columns.len()})
                        })
                    })
                    .collect();
                let page = Page {
                    columns: ["database", "table", "columns"].map(String::from).to_vec(),
                    column_types: Vec::new(),
                    rows,
                    page_number: 1,
                    has_more: false,
                    total_records: None,
                };
                self.entries.push(Entry::Rows {
                    page,
                    elapsed: None,
                    expanded: self.expanded,
                });
                return;
            }
            Names::Loading => "Still reading the warehouse's tables; try again in a moment, or \\dt <db>".to_string(),
            Names::Failed(why) => format!("The warehouse's tables could not be read ({why}); \\dt <db> lists one database, \\refresh tries again"),
        };
        self.notice(message);
    }

    fn set_output(&mut self, path: Option<PathBuf>) {
        let Some(path) = path else {
            match self.output.take() {
                Some(target) => self.notice(format!(
                    "Stopped saving results to {}",
                    target.path.display()
                )),
                None => self.notice("Results are not being saved"),
            }
            return;
        };
        match OutputTarget::new(save::expand_home(&path)) {
            Ok(target) => {
                self.notice(format!(
                    "Saving the next results to {} · \\o off stops",
                    target.path.display()
                ));
                self.output = Some(target);
            }
            Err(message) => self.error(message),
        }
    }

    fn save_last(&mut self, path: PathBuf) -> Vec<Effect> {
        let path = save::expand_home(&path);
        let format = match save::FileFormat::from_path(&path) {
            Ok(format) => format,
            Err(message) => {
                self.error(message);
                return Vec::new();
            }
        };
        let Some(last) = &self.last else {
            self.notice("No result to save yet; run a query first (or \\o file to save the next)");
            return Vec::new();
        };
        if self.is_busy() {
            self.notice("Wait for the running statement to finish, then \\save again");
            return Vec::new();
        }
        let source = match &last.query {
            Some(query) => SaveSource::Query(query.clone()),
            None => SaveSource::Page(last.page.clone()),
        };
        let to = Destination {
            path,
            format,
            append: false,
        };
        vec![self.start_save(source, to)]
    }

    fn start_save(&mut self, source: SaveSource, to: Destination) -> Effect {
        self.running = Some((format!("Saving {}", to.path.display()), Instant::now()));
        Effect::Save { source, to }
    }

    /// A problem beside the work itself, from the driver.
    pub fn warn(&mut self, text: String) {
        self.notice(text);
    }

    /// The driver's progress on long work (pages saved so far).
    pub fn progress(&mut self, label: String) {
        if let Some((text, _)) = self.running.as_mut() {
            *text = label;
        }
    }

    fn restart(&mut self) -> Vec<Effect> {
        self.dropped = None;
        self.queue.clear();
        self.running = Some(("Starting a new session".into(), Instant::now()));
        vec![Effect::Restart]
    }

    /// The session's state from the server; one that is gone opens the
    /// restart offer.
    pub fn session_state_is(&mut self, state: String) {
        let was_live = self.session_live();
        self.session_state = state;
        if was_live && !self.session_live() && self.dropped.is_none() {
            self.session_dropped(format!("the session is {}", self.session_state));
        }
    }

    fn session_dropped(&mut self, why: String) {
        self.queue.clear();
        self.last = None;
        self.entries.push(Entry::Error {
            message: format!("The session stopped: {why}."),
            statement: None,
        });
        self.notice(
            "Restart it? y starts a new session (same warehouse and compute) · n keeps this one",
        );
        self.dropped = Some(why);
    }

    fn can_page(&self) -> bool {
        !self.is_busy()
            && self
                .last
                .as_ref()
                .is_some_and(|l| l.page.has_more && l.query.is_some())
    }

    fn next_page(&mut self) -> Vec<Effect> {
        let Some(LastResult {
            query: Some(query),
            page,
        }) = &self.last
        else {
            return Vec::new();
        };
        let effect = Effect::NextPage {
            query: query.clone(),
            page: page.page_number + 1,
        };
        self.running = Some(("Loading the next page".into(), Instant::now()));
        vec![effect]
    }

    /// Starts the next queued statement unless one is already running.
    fn start_next(&mut self) -> Vec<Effect> {
        if self.is_busy() {
            return Vec::new();
        }
        if self.dropped.is_some() && !self.queue.is_empty() {
            self.queue.clear();
            self.notice("The session has stopped · y restarts it, or \\restart");
            return Vec::new();
        }
        let Some(next) = self.queue.pop_front() else {
            return Vec::new();
        };
        self.entries.push(Entry::Statement(next.sql.clone()));
        self.running = Some(("Running".into(), Instant::now()));
        self.view = next.view;
        vec![Effect::Run(next.sql)]
    }

    pub fn finish(&mut self, done: Done) -> Vec<Effect> {
        self.running = None;
        let mut effects = Vec::new();
        match done {
            Done::Ran(outcome) => effects.extend(self.ran(outcome)),
            Done::Failed { statement, error } => effects.extend(self.failed(statement, &error)),
            Done::Paged(Ok(page)) => {
                if let Some(last) = self.last.as_mut() {
                    last.page = page.clone();
                }
                self.entries.push(Entry::Rows {
                    page,
                    elapsed: None,
                    expanded: self.expanded,
                });
            }
            Done::Paged(Err(message)) | Done::Saved(Err(message)) => {
                self.error(readable_error(&message))
            }
            Done::Saved(Ok(saved)) => self.notice(saved.message()),
            Done::Catalog(result) => self.catalog_read(result),
            Done::Restarted(Ok(session)) => {
                self.session_state = "Running".into();
                self.notice(format!(
                    "Started session {}… · statements run on it now",
                    crate::sql::render::short_id(&session)
                ));
            }
            Done::Restarted(Err(message)) => {
                self.error(format!(
                    "Could not start a new session: {}",
                    readable_error(&message)
                ));
                self.dropped = Some("the restart failed".into());
                self.notice("y tries again · \\q quits");
            }
        }
        effects.extend(self.start_next());
        effects
    }

    fn ran(&mut self, outcome: Outcome) -> Vec<Effect> {
        let elapsed = self.timing.then_some(outcome.elapsed);
        let page = match outcome.reply {
            Reply::Rows(page) => page,
            Reply::Done(message) => {
                self.entries.push(Entry::Done { message, elapsed });
                return Vec::new();
            }
            Reply::Running | Reply::Stored => return Vec::new(),
        };
        self.last = Some(LastResult {
            query: outcome.query_id.clone(),
            page: page.clone(),
        });
        let entry = match self.view {
            View::Describe => Entry::Described(TableDescription::from_page(&page)),
            View::Rows => Entry::Rows {
                page: page.clone(),
                elapsed,
                expanded: self.expanded,
            },
        };
        self.entries.push(entry);
        let Some(target) = self.output.as_mut() else {
            return Vec::new();
        };
        let to = target.destination(&page.columns);
        let source = match outcome.query_id {
            Some(query) => SaveSource::Query(query),
            None => SaveSource::Page(page),
        };
        vec![self.start_save(source, to)]
    }

    fn failed(&mut self, statement: String, error: &str) -> Vec<Effect> {
        self.entries.push(Entry::Error {
            message: readable_error(error),
            statement: Some(statement),
        });
        let skipped = self.queue.len();
        self.queue.clear();
        if skipped > 0 {
            self.notice(format!(
                "Skipped {skipped} queued statement(s) after the error"
            ));
        }
        if error.contains(UNREACHABLE) && self.dropped.is_none() {
            self.session_state = "Stopped".into();
            self.session_dropped("its pod no longer answers".into());
            return Vec::new();
        }
        // Other wordings may also mean the session is gone; ask.
        vec![Effect::CheckSession]
    }

    fn catalog_read(&mut self, result: Result<Catalog, String>) {
        let asked = std::mem::take(&mut self.refresh_asked);
        match result {
            Ok(catalog) => {
                if asked {
                    self.notice(format!(
                        "Names refreshed: {} databases, {} tables, {} columns",
                        catalog.databases.len(),
                        catalog.table_count(),
                        catalog.column_count()
                    ));
                }
                self.names = Names::Ready(catalog);
            }
            Err(message) => {
                let why = readable_error(&message);
                self.notice(format!(
                    "Tab completes keywords only: the warehouse's names could not be read ({why}) · \\refresh tries again"
                ));
                self.names = Names::Failed(why);
            }
        }
    }

    /// The cancel reached the server (or failed); the run reports its end.
    pub fn cancelled(&mut self, result: Result<(), String>) {
        if let Err(message) = result {
            self.error(format!("could not cancel: {message}"));
        }
        self.queue.clear();
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::catalog::tests::sample;
    use crate::sql::save::FileFormat;
    use serde_json::json;

    fn state() -> ShellState {
        ShellState::new("wh".into(), History::load(None))
    }

    fn with_names() -> ShellState {
        let mut s = state();
        s.finish(Done::Catalog(Ok(sample())));
        s
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(state: &mut ShellState, text: &str) {
        state.input.insert_str(text);
    }

    fn rows_outcome(has_more: bool) -> Outcome {
        Outcome {
            query_id: Some("q1".into()),
            reply: Reply::Rows(crate::sql::results::parse_page(
                &json!({"data": [{"a": 1}], "pageNumber": 1, "hasMore": has_more}),
            )),
            elapsed: Duration::from_millis(5),
        }
    }

    fn last_notice(s: &ShellState) -> String {
        s.entries
            .iter()
            .rev()
            .find_map(|e| match e {
                Entry::Notice(text) => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn enter_runs_only_complete_statements() {
        let mut s = state();
        type_text(&mut s, "select 1");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(
            s.input.lines().len(),
            2,
            "unfinished statement gets a new line"
        );
        type_text(&mut s, "from t;");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Run("select 1\nfrom t".into())]
        );
        assert!(s.is_busy());
    }

    #[test]
    fn several_statements_run_one_after_another() {
        let mut s = state();
        assert_eq!(
            s.submit("select 1; select 2;"),
            vec![Effect::Run("select 1".into())]
        );
        let next = s.finish(Done::Ran(rows_outcome(false)));
        assert_eq!(next, vec![Effect::Run("select 2".into())]);
    }

    #[test]
    fn an_error_skips_the_rest_of_the_queue_and_checks_the_session() {
        let mut s = state();
        s.submit("select 1; select 2; select 3;");
        let next = s.finish(Done::Failed {
            statement: "select 1".into(),
            error: "boom".into(),
        });
        assert_eq!(next, vec![Effect::CheckSession]);
        assert!(matches!(&s.entries[1], Entry::Error { message, .. } if message == "boom"));
        assert!(matches!(&s.entries[2], Entry::Notice(n) if n.contains("Skipped 2")));
    }

    #[test]
    fn explains_statements_the_lakehouse_cannot_read() {
        let text = readable_error(
            "POST https://x/lakehouse/submitquery → HTTP 403 Forbidden: Access denied: Unable to determine operations for authorization.",
        );
        assert!(text.starts_with("HTTP 403 Forbidden: Access denied"));
        assert!(text.contains("SHOW DATABASES"), "{text}");
    }

    #[test]
    fn meta_commands_toggle_and_run_lookups() {
        let mut s = state();
        s.submit("\\x");
        assert!(s.expanded);
        s.submit("\\timing");
        assert!(!s.timing);
        assert_eq!(
            s.submit("\\dt sales"),
            vec![Effect::Run("SHOW TABLES IN sales".into())]
        );
        let mut s = state();
        assert_eq!(s.submit("\\dn"), vec![Effect::Run("SHOW DATABASES".into())]);
        let mut s = state();
        assert!(s.submit("\\use sales").is_empty());
        assert!(matches!(&s.entries[0], Entry::Notice(n) if n.contains("sales.my_table")));
        let mut s = state();
        s.submit("\\h");
        assert!(matches!(&s.entries[0], Entry::Info(lines) if lines.len() == meta::HELP.len()));
    }

    #[test]
    fn describe_qualifies_bare_names_and_draws_a_description() {
        let mut s = with_names();
        assert_eq!(
            s.submit("\\d productionmetrics"),
            vec![Effect::Run(
                "DESCRIBE TABLE EXTENDED DemoDB.ProductionMetrics".into()
            )]
        );
        let page = crate::sql::results::parse_page(&json!({"data": [
            {"col_name": "plant_id", "data_type": "string", "comment": null},
        ]}));
        s.finish(Done::Ran(Outcome {
            query_id: Some("q".into()),
            reply: Reply::Rows(page),
            elapsed: Duration::ZERO,
        }));
        assert!(matches!(s.entries.last(), Some(Entry::Described(d)) if d.columns.len() == 1));

        let mut s = with_names();
        assert!(s.submit("\\d nope").is_empty());
        assert!(
            matches!(s.entries.last(), Some(Entry::Error { message, .. }) if message.contains("db.table"))
        );
    }

    #[test]
    fn list_all_tables_reads_the_catalog() {
        let mut s = state();
        assert!(s.submit("\\dt").is_empty());
        assert!(last_notice(&s).contains("Still reading"));
        let mut s = with_names();
        s.submit("\\dt");
        let Some(Entry::Rows { page, .. }) = s.entries.last() else {
            panic!("expected rows: {:?}", s.entries);
        };
        assert_eq!(page.rows.len(), 3);
        assert_eq!(page.rows[2]["table"], "orders");
    }

    #[test]
    fn refresh_reloads_names_and_reports_counts() {
        let mut s = with_names();
        assert_eq!(s.submit("\\refresh"), vec![Effect::LoadCatalog]);
        assert!(matches!(s.names, Names::Loading));
        s.finish(Done::Catalog(Ok(sample())));
        assert_eq!(
            last_notice(&s),
            "Names refreshed: 3 databases, 3 tables, 5 columns"
        );
        s.finish(Done::Catalog(Err("HTTP 403 Forbidden: no access".into())));
        assert!(last_notice(&s).contains("keywords only"));
    }

    #[test]
    fn tab_completes_from_the_catalog_and_cycles() {
        let mut s = with_names();
        type_text(&mut s, "select * from demodb.p");
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "select * from demodb.Product");
        assert!(s.menu.is_some());
        s.on_key(key(KeyCode::Tab));
        assert_eq!(s.input_text(), "select * from demodb.ProductionMetrics");
        s.on_key(key(KeyCode::Char(' ')));
        assert!(s.menu.is_none(), "typing closes the menu");
        assert_eq!(s.input_text(), "select * from demodb.ProductionMetrics ");
    }

    #[test]
    fn space_pages_and_save_writes_the_last_result() {
        let mut s = state();
        s.submit("select 1;");
        s.finish(Done::Ran(rows_outcome(true)));
        assert_eq!(
            s.on_key(key(KeyCode::Char(' '))),
            vec![Effect::NextPage {
                query: "q1".into(),
                page: 2
            }]
        );
        s.finish(Done::Paged(Err("x".into())));
        assert_eq!(
            s.submit("\\save out.parquet"),
            vec![Effect::Save {
                source: SaveSource::Query("q1".into()),
                to: Destination {
                    path: "out.parquet".into(),
                    format: FileFormat::Parquet,
                    append: false
                }
            }]
        );
        assert!(s.is_busy(), "the save shows progress");
        s.progress("Saving out.parquet · 5000 rows".into());
        assert_eq!(
            s.running.as_ref().unwrap().0,
            "Saving out.parquet · 5000 rows"
        );
    }

    #[test]
    fn save_needs_a_result_and_a_known_format() {
        let mut s = state();
        assert!(s.submit("\\save out.csv").is_empty());
        assert!(last_notice(&s).contains("No result to save yet"));
        assert!(s.submit("\\save out.txt").is_empty());
        assert!(
            matches!(s.entries.last(), Some(Entry::Error { message, .. }) if message.contains(".csv, .jsonl or .parquet"))
        );
    }

    #[test]
    fn output_saves_every_following_result_until_off() {
        let mut s = state();
        assert!(s.submit("\\o later.csv").is_empty());
        s.submit("select 1; select 2;");
        let effects = s.finish(Done::Ran(rows_outcome(false)));
        assert_eq!(
            effects,
            vec![Effect::Save {
                source: SaveSource::Query("q1".into()),
                to: Destination {
                    path: "later.csv".into(),
                    format: FileFormat::Csv,
                    append: false
                }
            }],
            "the next statement waits for the save"
        );
        let saved = Saved {
            path: "later.csv".into(),
            rows: 1,
            capped: false,
            missing: None,
            notes: Vec::new(),
        };
        assert_eq!(
            s.finish(Done::Saved(Ok(saved))),
            vec![Effect::Run("select 2".into())]
        );
        let effects = s.finish(Done::Ran(rows_outcome(false)));
        assert!(
            matches!(&effects[..], [Effect::Save { to, .. }] if to.append),
            "same columns append: {effects:?}"
        );
        s.finish(Done::Saved(Err("x".into())));
        s.submit("\\o off");
        assert!(s.output.is_none());
        s.submit("select 3;");
        assert!(s.finish(Done::Ran(rows_outcome(false))).is_empty());
    }

    #[test]
    fn a_stopped_session_offers_a_restart() {
        let mut s = state();
        s.session_state_is("Stopped".into());
        assert!(s.dropped.is_some());
        assert!(
            matches!(&s.entries[0], Entry::Error { message, .. } if message.contains("session is Stopped"))
        );
        assert!(
            s.submit("select 1;").is_empty(),
            "nothing runs on a stopped session"
        );
        assert_eq!(s.on_key(key(KeyCode::Char('y'))), vec![Effect::Restart]);
        assert!(s.is_busy());
        s.finish(Done::Restarted(Ok("abcdef123456".into())));
        assert_eq!(s.session_state, "Running");
        assert!(last_notice(&s).contains("abcdef12"));
        assert_eq!(s.submit("select 1;"), vec![Effect::Run("select 1".into())]);
    }

    #[test]
    fn an_unreachable_pod_drops_the_session_and_n_declines() {
        let mut s = state();
        s.submit("select 1;");
        let effects = s.finish(Done::Failed {
            statement: "select 1".into(),
            error:
                "HTTP 503: The lakehouse session is not reachable. Start the session and try again."
                    .into(),
        });
        assert!(effects.is_empty());
        assert!(s.dropped.is_some());
        assert!(s.on_key(key(KeyCode::Char('n'))).is_empty());
        assert!(s.dropped.is_none());
        assert!(
            s.input_text().is_empty(),
            "the answer is not typed into the input"
        );
        assert_eq!(s.submit("\\restart"), vec![Effect::Restart]);
    }

    #[test]
    fn credentials_stay_out_of_history() {
        let mut s = state();
        s.submit("SET spark.hadoop.fs.s3a.secret.key = 'abc';");
        assert!(matches!(&s.entries[0], Entry::Notice(n) if n.contains("credential")));
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "");
    }

    #[test]
    fn esc_and_ctrl_c_cancel_then_ctrl_c_twice_exits() {
        let mut s = state();
        s.submit("select 1;");
        assert_eq!(s.on_key(key(KeyCode::Esc)), vec![Effect::Cancel]);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(s.on_key(ctrl_c), vec![Effect::Cancel]);
        s.finish(Done::Failed {
            statement: "select 1".into(),
            error: "Query cancelled".into(),
        });
        assert!(s.on_key(ctrl_c).is_empty());
        assert!(s.exit_armed());
        assert_eq!(s.on_key(ctrl_c), vec![Effect::Exit]);
    }

    #[test]
    fn errors_drop_the_request_prefix() {
        assert_eq!(
            readable_error("POST https://x/lakehouse/submitquery → HTTP 403 Forbidden: no access"),
            "HTTP 403 Forbidden: no access"
        );
        assert_eq!(readable_error("Query cancelled"), "Query cancelled");
    }

    #[test]
    fn up_recalls_history() {
        let mut s = state();
        s.submit("\\timing");
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.input_text(), "\\timing");
    }
}
