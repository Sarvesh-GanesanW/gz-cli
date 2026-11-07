//! The SQL shell's state: input, queued statements, the last result, and
//! toggles. Keys and finished work go in; screen entries and [`Effect`]s
//! come out. No terminal or network here.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_textarea::TextArea;

use crate::sql::history::History;
use crate::sql::meta::{self, Meta};
use crate::sql::results::{Page, Reply};
use crate::sql::session::Outcome;
use crate::sql::statements;

/// The server's own words: drops the `POST https://… → ` request prefix the
/// HTTP layer adds, keeping the status and message.
pub fn readable_error(error: &str) -> String {
    let methods = ["GET ", "POST ", "PUT ", "PATCH ", "DELETE "];
    let message = match error.split_once(" → ") {
        Some((request, rest)) if methods.iter().any(|m| request.starts_with(m)) => rest.to_string(),
        _ => error.to_string(),
    };
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

/// Something to print into scrollback.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Statement(String),
    Rows {
        page: Page,
        elapsed: Option<Duration>,
        expanded: bool,
    },
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

/// Work for the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Run(String),
    NextPage { query: String, page: u32 },
    Export { query: String, path: PathBuf },
    Cancel,
    Exit,
}

/// Finished work reported back by the driver.
#[derive(Debug)]
pub enum Done {
    Ran(Outcome),
    Failed { statement: String, error: String },
    Paged(Result<Page, String>),
    Exported(Result<(PathBuf, usize), String>),
}

#[derive(Debug, Clone)]
struct LastResult {
    query: String,
    page: Page,
}

pub struct ShellState {
    pub input: TextArea<'static>,
    history: History,
    pub entries: Vec<Entry>,
    /// What is running and since when (spinner text).
    pub running: Option<(String, Instant)>,
    queue: VecDeque<String>,
    pub expanded: bool,
    pub timing: bool,
    pub warehouse: String,
    pub session_state: String,
    last: Option<LastResult>,
    pending_csv: Option<PathBuf>,
    exit_armed: Option<Instant>,
    pub tick: u64,
}

pub fn new_input() -> TextArea<'static> {
    let mut input = TextArea::default();
    input.set_placeholder_text("SQL ending in ;  ·  \\help for commands");
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
            expanded: false,
            timing: true,
            warehouse,
            session_state: "Running".to_string(),
            last: None,
            pending_csv: None,
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

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return self.on_ctrl_c();
        }
        self.exit_armed = None;
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

    /// Runs typed text: a meta command, or every complete statement in it.
    pub fn submit(&mut self, text: &str) -> Vec<Effect> {
        let text = text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        if let Err(err) = self.history.push(text) {
            self.entries
                .push(Entry::Notice(format!("history not saved: {err:#}")));
        }
        if text.starts_with('\\') {
            return match meta::parse(text) {
                Ok(command) => self.meta(command),
                Err(message) => {
                    self.entries.push(Entry::Error {
                        message,
                        statement: None,
                    });
                    Vec::new()
                }
            };
        }
        let parts = statements::split(text);
        self.queue.extend(parts.statements);
        if statements::has_code(&parts.rest) {
            self.queue.push_back(parts.rest);
        }
        self.start_next()
    }

    fn meta(&mut self, command: Meta) -> Vec<Effect> {
        if let Some(sql) = command.sql() {
            self.queue.push_back(sql);
            return self.start_next();
        }
        match command {
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
                self.entries
                    .push(Entry::Notice(format!("Expanded display {state}")));
                Vec::new()
            }
            Meta::Timing => {
                self.timing = !self.timing;
                let state = if self.timing { "on" } else { "off" };
                self.entries.push(Entry::Notice(format!("Timing {state}")));
                Vec::new()
            }
            Meta::More => {
                if self.can_page() {
                    self.next_page()
                } else {
                    self.entries.push(Entry::Notice("No more rows".into()));
                    Vec::new()
                }
            }
            Meta::Output(path) => match &self.last {
                Some(last) => vec![Effect::Export {
                    query: last.query.clone(),
                    path,
                }],
                None => {
                    self.entries.push(Entry::Notice(format!(
                        "The next result will be saved to {}",
                        path.display()
                    )));
                    self.pending_csv = Some(path);
                    Vec::new()
                }
            },
            Meta::Use(db) => {
                self.entries.push(Entry::Notice(format!(
                    "The lakehouse doesn't support USE; qualify names instead, e.g. {db}.my_table"
                )));
                Vec::new()
            }
            Meta::Databases | Meta::Tables(_) | Meta::Describe(_) => Vec::new(),
        }
    }

    fn can_page(&self) -> bool {
        !self.is_busy() && self.last.as_ref().is_some_and(|l| l.page.has_more)
    }

    fn next_page(&mut self) -> Vec<Effect> {
        let Some(last) = &self.last else {
            return Vec::new();
        };
        let effect = Effect::NextPage {
            query: last.query.clone(),
            page: last.page.page_number + 1,
        };
        self.running = Some(("Loading the next page".into(), Instant::now()));
        vec![effect]
    }

    /// Starts the next queued statement unless one is already running.
    fn start_next(&mut self) -> Vec<Effect> {
        if self.is_busy() {
            return Vec::new();
        }
        let Some(statement) = self.queue.pop_front() else {
            return Vec::new();
        };
        self.entries.push(Entry::Statement(statement.clone()));
        self.running = Some(("Running".into(), Instant::now()));
        vec![Effect::Run(statement)]
    }

    pub fn finish(&mut self, done: Done) -> Vec<Effect> {
        self.running = None;
        let mut effects = Vec::new();
        match done {
            Done::Ran(outcome) => {
                let elapsed = self.timing.then_some(outcome.elapsed);
                match outcome.reply {
                    Reply::Rows(page) => {
                        if let Some(query) = outcome.query_id {
                            self.last = Some(LastResult {
                                query: query.clone(),
                                page: page.clone(),
                            });
                            if let Some(path) = self.pending_csv.take() {
                                effects.push(Effect::Export { query, path });
                            }
                        }
                        self.entries.push(Entry::Rows {
                            page,
                            elapsed,
                            expanded: self.expanded,
                        });
                    }
                    Reply::Done(message) => self.entries.push(Entry::Done { message, elapsed }),
                    Reply::Running | Reply::Stored => {}
                }
            }
            Done::Failed { statement, error } => {
                self.entries.push(Entry::Error {
                    message: readable_error(&error),
                    statement: Some(statement),
                });
                let skipped = self.queue.len();
                self.queue.clear();
                if skipped > 0 {
                    self.entries.push(Entry::Notice(format!(
                        "Skipped {skipped} queued statement(s) after the error"
                    )));
                }
            }
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
            Done::Paged(Err(message)) | Done::Exported(Err(message)) => {
                self.entries.push(Entry::Error {
                    message: readable_error(&message),
                    statement: None,
                })
            }
            Done::Exported(Ok((path, rows))) => self.entries.push(Entry::Notice(format!(
                "Saved {rows} row(s) to {}",
                path.display()
            ))),
        }
        effects.extend(self.start_next());
        effects
    }

    /// The cancel reached the server (or failed); the run reports its end.
    pub fn cancelled(&mut self, result: Result<(), String>) {
        if let Err(message) = result {
            self.entries.push(Entry::Error {
                message: format!("could not cancel: {message}"),
                statement: None,
            });
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
    use serde_json::json;

    fn state() -> ShellState {
        ShellState::new("wh".into(), History::load(None))
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
    fn an_error_skips_the_rest_of_the_queue() {
        let mut s = state();
        s.submit("select 1; select 2; select 3;");
        let next = s.finish(Done::Failed {
            statement: "select 1".into(),
            error: "boom".into(),
        });
        assert!(next.is_empty());
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
        assert_eq!(
            s.submit("\\dt sales"),
            vec![Effect::Run("SHOW TABLES IN sales".into())]
        );
        let mut s = state();
        assert!(s.submit("\\use sales").is_empty());
        assert!(matches!(&s.entries[0], Entry::Notice(n) if n.contains("sales.my_table")));
    }

    #[test]
    fn space_pages_and_output_exports() {
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
            s.submit("\\o out.csv"),
            vec![Effect::Export {
                query: "q1".into(),
                path: "out.csv".into()
            }]
        );
    }

    #[test]
    fn output_before_a_result_saves_the_next_one() {
        let mut s = state();
        assert!(s.submit("\\o later.csv").is_empty());
        s.submit("select 1;");
        let effects = s.finish(Done::Ran(rows_outcome(false)));
        assert_eq!(
            effects,
            vec![Effect::Export {
                query: "q1".into(),
                path: "later.csv".into()
            }]
        );
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
