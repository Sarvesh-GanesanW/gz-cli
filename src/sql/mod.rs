//! `gz sql`: an interactive SQL shell on a lakehouse session, plus piped
//! and file scripts. Picks (or remembers) a warehouse and compute size,
//! reuses a session still running for this profile, and stops the session
//! on exit unless asked to keep it.

pub mod catalog;
mod cells;
pub mod complete;
pub mod describe;
pub mod estimate;
pub mod history;
pub mod menu;
pub mod meta;
mod parquet_out;
pub mod render;
pub mod results;
pub mod save;
pub mod scan_limit;
pub mod session;
pub mod state;
pub mod statements;
pub mod tables;
pub mod tokens;

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::cli::ShellArgs;
use crate::cmd::Runtime;
use crate::inline_term::{InlineTerm, TerminalGuard};
use crate::output::{self, OutputFormat};
use results::{Page, Reply};
use session::{Compute, Lakehouse, RunningQuery, Warehouse};
use state::{Done, Effect, SaveSource, ShellState};

/// Redraw and keyboard poll interval.
const FRAME: Duration = Duration::from_millis(50);
/// How often the shell re-reads the session state for the mode line.
const SESSION_CHECK: Duration = Duration::from_secs(30);
/// Most pages read for an export or a script's full result.
const MAX_PAGES: u32 = 1000;
/// Width used for tables when output is not a terminal.
const PIPE_WIDTH: usize = 200;

/// Where the SQL comes from.
pub enum Source {
    /// Interactive in a terminal, else statements piped on stdin.
    Auto,
    Interactive,
    File(PathBuf),
    /// A script gz wrote itself (`gz quality history`).
    Text(String),
}

/// A session ready for statements.
#[derive(Clone)]
struct Connection {
    session: String,
    warehouse: Warehouse,
    compute: String,
    reused: bool,
}

/// Runs the shell, or a script under the scan limit (`--max-scan`, else
/// the profile's `sql_max_scan`; the interactive shell has none).
pub async fn run(
    rt: &Runtime,
    options: &ShellArgs,
    source: Source,
    max_scan: Option<&str>,
) -> Result<()> {
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let script = match &source {
        Source::Interactive if !terminal => {
            bail!("`gz sql shell` needs a terminal; pipe SQL into `gz sql` instead")
        }
        Source::Interactive => None,
        Source::Auto if terminal => None,
        Source::Auto => Some(read_stdin()?),
        Source::File(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?,
        ),
        Source::Text(text) => Some(text.clone()),
    };
    if let Some(text) = &script {
        if script_statements(text).is_empty() {
            bail!("no SQL statements to run");
        }
    }
    if let Some(text) = &script {
        check_scan(rt, options, text, max_scan).await?;
    }
    let lake = Lakehouse::new(rt.http.clone());
    let connection = connect(rt, &lake, options, script.is_none()).await?;
    let (outcome, connection) = match &script {
        None => repl(rt, options, &lake, connection).await,
        Some(text) => (run_script(rt, &lake, &connection, text).await, connection),
    };
    finish_session(rt, &lake, &connection, options.keep_session).await;
    outcome
}

/// Refuses a script estimated to scan more than the limit, before a
/// session starts.
async fn check_scan(
    rt: &Runtime,
    options: &ShellArgs,
    script: &str,
    max_scan: Option<&str>,
) -> Result<()> {
    let saved = rt.http.resolved().profile.sql_max_scan.clone();
    let limit = scan_limit::ScanLimit::resolve(max_scan, saved.as_deref())?;
    if limit == scan_limit::ScanLimit::Off {
        return Ok(());
    }
    let warehouse = warehouse(rt, options).await?;
    let checked = scan_limit::enforce(&rt.http, &warehouse.name, script, limit).await?;
    if let (Some(estimate), false) = (checked, rt.quiet) {
        output::info(&format!("scan check: {}", estimate.headline()));
    }
    Ok(())
}

/// The warehouse a command works on: `--warehouse`, else the one the
/// profile remembers, else a choice on the terminal.
pub(crate) async fn warehouse(rt: &Runtime, options: &ShellArgs) -> Result<Warehouse> {
    let lake = Lakehouse::new(rt.http.clone());
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    choose_warehouse(
        &lake.warehouses().await?,
        options.warehouse.as_deref(),
        rt.http.resolved().profile.sql_warehouse.as_deref(),
        interactive,
    )
}

/// A lakehouse session for commands that run statements of their own
/// (`gz diff --rows`, `gz bisect`): the shell's warehouse, compute size and
/// remembered session, stopped on [`Runner::close`] unless kept.
pub(crate) struct Runner {
    lake: Lakehouse,
    connection: Connection,
    running: RunningQuery,
}

impl Runner {
    pub async fn open(rt: &Runtime, options: &ShellArgs) -> Result<Self> {
        let lake = Lakehouse::new(rt.http.clone());
        let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        let connection = connect(rt, &lake, options, interactive).await?;
        Ok(Self {
            lake,
            connection,
            running: Arc::new(Mutex::new(None)),
        })
    }

    /// Runs one statement and returns the first page of its rows.
    pub async fn rows(&self, statement: &str) -> Result<Page> {
        let outcome = self
            .lake
            .run(
                &self.connection.session,
                &self.connection.warehouse.name,
                statement,
                &self.running,
            )
            .await
            .map_err(|err| anyhow::anyhow!("{}", state::readable_error(&format!("{err:#}"))))?;
        match outcome.reply {
            Reply::Rows(page) => Ok(page),
            Reply::Done(message) => bail!("the statement returned no rows: {message}"),
            Reply::Running | Reply::Stored => bail!("the session returned no result"),
        }
    }

    pub async fn close(self, rt: &Runtime, keep: bool) {
        finish_session(rt, &self.lake, &self.connection, keep).await;
    }
}

fn read_stdin() -> Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("cannot read SQL from stdin")?;
    Ok(text)
}

fn script_statements(text: &str) -> Vec<String> {
    let parts = statements::split(text);
    let mut list = parts.statements;
    if statements::has_code(&parts.rest) {
        list.push(parts.rest);
    }
    list
}

// ---------- session lifecycle ----------

async fn connect(
    rt: &Runtime,
    lake: &Lakehouse,
    options: &ShellArgs,
    interactive: bool,
) -> Result<Connection> {
    let profile = &rt.http.resolved().profile;
    let warehouses = lake.warehouses().await?;
    let warehouse = choose_warehouse(
        &warehouses,
        options.warehouse.as_deref(),
        profile.sql_warehouse.as_deref(),
        interactive,
    )?;

    if let Some(id) = reusable_session(profile, &warehouse) {
        if lake.session_state(&id).await.ok().as_deref() == Some("Running") {
            return Ok(Connection {
                session: id,
                compute: profile.sql_compute.clone().unwrap_or_else(|| "?".into()),
                warehouse,
                reused: true,
            });
        }
    }
    let computes = lake.computes().await?;
    let compute = choose_compute(
        &computes,
        options.compute.as_deref(),
        profile.sql_compute.as_deref(),
        interactive,
    )?;
    let session = start_with_spinner(rt, lake, &warehouse, &compute).await?;
    if let Err(err) = remember_session(
        &rt.http.resolved().profile_name,
        &warehouse,
        &compute,
        &session,
    ) {
        output::warn(&format!("cannot save gz sql settings: {err:#}"));
    }
    Ok(Connection {
        session,
        warehouse,
        compute: compute.name,
        reused: false,
    })
}

/// Saves the new session as the profile's, so `gz sql` and the TUI reuse it.
fn remember_session(
    profile: &str,
    warehouse: &Warehouse,
    compute: &Compute,
    session: &str,
) -> Result<()> {
    remember_in_profile(profile, |p| {
        p.sql_warehouse = Some(warehouse.name.clone());
        p.sql_compute = Some(compute.name.clone());
        p.sql_session_started = Some(crate::config::SessionStart {
            session: session.to_string(),
            at_ms: crate::cmd::job_fix::now_ms(),
        });
        p.sql_session = Some(session.to_string());
        p.sql_session_warehouse = Some(warehouse.name.clone());
    })
}

fn reusable_session(profile: &crate::config::Profile, warehouse: &Warehouse) -> Option<String> {
    remembered_session(profile, &warehouse.name)
}

fn choose_warehouse(
    list: &[Warehouse],
    flag: Option<&str>,
    remembered: Option<&str>,
    interactive: bool,
) -> Result<Warehouse> {
    let names = || {
        list.iter()
            .map(|w| w.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(wanted) = flag {
        return session::pick_warehouse(list, wanted)
            .cloned()
            .with_context(|| format!("no warehouse '{wanted}' (have: {})", names()));
    }
    if let Some(found) = remembered.and_then(|w| session::pick_warehouse(list, w)) {
        return Ok(found.clone());
    }
    if !interactive {
        bail!("pass --warehouse NAME (have: {})", names());
    }
    let labels: Vec<String> = list.iter().map(|w| w.name.clone()).collect();
    Ok(list[pick("Warehouse", &labels)?].clone())
}

fn choose_compute(
    list: &[Compute],
    flag: Option<&str>,
    remembered: Option<&str>,
    interactive: bool,
) -> Result<Compute> {
    let names = || {
        list.iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(wanted) = flag {
        return session::pick_compute(list, wanted)
            .cloned()
            .with_context(|| format!("no compute size '{wanted}' (have: {})", names()));
    }
    if let Some(found) = remembered.and_then(|c| session::pick_compute(list, c)) {
        return Ok(found.clone());
    }
    if !interactive {
        bail!("pass --compute NAME (have: {})", names());
    }
    let labels: Vec<String> = list
        .iter()
        .map(|c| format!("{:<10} {}", c.name, c.description))
        .collect();
    Ok(list[pick("Compute", &labels)?].clone())
}

/// A numbered choice on the terminal, before the shell takes over.
fn pick(what: &str, labels: &[String]) -> Result<usize> {
    if labels.is_empty() {
        bail!("no {} available on this site", what.to_lowercase());
    }
    eprintln!("{what}:");
    for (i, label) in labels.iter().enumerate() {
        eprintln!("  {:>2}) {label}", i + 1);
    }
    loop {
        let answer = crate::cmd::prompt_line(&format!("{what} [1-{}]: ", labels.len()))?;
        match answer.parse::<usize>() {
            Ok(n) if (1..=labels.len()).contains(&n) => return Ok(n - 1),
            _ if answer.is_empty() => bail!("no {} chosen", what.to_lowercase()),
            _ => eprintln!("Pick a number from 1 to {}", labels.len()),
        }
    }
}

async fn start_with_spinner(
    rt: &Runtime,
    lake: &Lakehouse,
    warehouse: &Warehouse,
    compute: &Compute,
) -> Result<String> {
    let show = std::io::stderr().is_terminal();
    let bar = show.then(|| {
        let bar = indicatif::ProgressBar::new_spinner();
        bar.set_style(
            indicatif::ProgressStyle::with_template("{spinner:.cyan} {msg} {elapsed:.dim}")
                .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
        );
        bar.set_message(format!(
            "Starting a session on {} ({})…",
            warehouse.name, compute.name
        ));
        bar.enable_steady_tick(Duration::from_millis(80));
        bar
    });
    let requests_quiet = bar.is_some().then(|| rt.http.caller_progress());
    let started = lake.start(compute.id, warehouse.id).await;
    if let Some(bar) = bar {
        bar.finish_and_clear();
    }
    drop(requests_quiet);
    let session = started?;
    if !rt.quiet && !show {
        eprintln!("session {session} started");
    }
    Ok(session)
}

async fn finish_session(rt: &Runtime, lake: &Lakehouse, connection: &Connection, keep: bool) {
    if keep {
        eprintln!(
            "session kept running ({}); `gz sql` will reuse it",
            connection.session
        );
        return;
    }
    match lake.stop(&connection.session).await {
        Ok(()) => {
            // stderr: piped results on stdout must stay clean.
            output::info(&format!(
                "session {}… stopped",
                render::short_id(&connection.session)
            ));
            remember(rt, |p| {
                p.sql_session = None;
                p.sql_session_warehouse = None;
                p.sql_session_started = None;
            })
        }
        Err(err) => eprintln!("could not stop session {}: {err:#}", connection.session),
    }
}

/// Updates the active profile in the config file; failures only warn.
fn remember(rt: &Runtime, change: impl FnOnce(&mut crate::config::Profile)) {
    if let Err(err) = remember_in_profile(&rt.http.resolved().profile_name, change) {
        output::warn(&format!("cannot save gz sql settings: {err:#}"));
    }
}

/// Saves SQL settings (warehouse, compute, session) to `profile`, so the
/// shell and the TUI share one session per warehouse.
pub(crate) fn remember_in_profile(
    profile: &str,
    change: impl FnOnce(&mut crate::config::Profile),
) -> Result<()> {
    let mut config = crate::config::load_config()?;
    change(config.profile_mut(profile));
    crate::config::save_config(&config).map(|_| ())
}

/// The session the profile remembers for `warehouse`, if any (it may have
/// stopped since; check its state before using it).
pub(crate) fn remembered_session(
    profile: &crate::config::Profile,
    warehouse: &str,
) -> Option<String> {
    let same_warehouse = profile.sql_session_warehouse.as_deref() == Some(warehouse);
    same_warehouse
        .then(|| profile.sql_session.clone())
        .flatten()
}

// ---------- results beyond the first page ----------

/// Every page of a result (up to [`MAX_PAGES`]), starting from `first`.
async fn all_pages(
    lake: &Lakehouse,
    connection: &Connection,
    query: &str,
    first: Page,
) -> Result<Page> {
    let mut page = first;
    let mut number = page.page_number;
    while page.has_more && number < MAX_PAGES {
        number += 1;
        let next = lake
            .page(
                &connection.session,
                query,
                &connection.warehouse.name,
                number,
            )
            .await?;
        page.rows.extend(next.rows);
        page.has_more = next.has_more;
    }
    Ok(page)
}

// ---------- scripts (piped or from a file) ----------

async fn run_script(
    rt: &Runtime,
    lake: &Lakehouse,
    connection: &Connection,
    text: &str,
) -> Result<()> {
    let running: RunningQuery = Arc::new(Mutex::new(None));
    let width = crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .ok()
        .filter(|_| std::io::stdout().is_terminal())
        .unwrap_or(PIPE_WIDTH);
    let mut collected: Vec<Value> = Vec::new();
    for statement in script_statements(text) {
        let outcome = match lake
            .run(
                &connection.session,
                &connection.warehouse.name,
                &statement,
                &running,
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                print_collected(rt.format, &collected)?;
                let first = statement.lines().next().unwrap_or_default();
                bail!(
                    "{}\n  in: {first}",
                    state::readable_error(&format!("{err:#}"))
                );
            }
        };
        match outcome.reply {
            Reply::Rows(first) => {
                let page = match &outcome.query_id {
                    Some(id) => all_pages(lake, connection, id, first).await?,
                    None => first,
                };
                match rt.format {
                    OutputFormat::Table => {
                        print!("{}", render::plain(&results::render(&page, width, false)));
                        let summary = results::summary(&page, Some(outcome.elapsed));
                        eprintln!("{}", render::plain(&[summary]).trim_end());
                    }
                    OutputFormat::Csv => print!("{}", output::csv_text(&page.columns, &page.rows)),
                    OutputFormat::Json | OutputFormat::Yaml => collected.push(json!({
                        "statement": statement,
                        "columns": page.columns,
                        "rows": page.rows,
                    })),
                }
            }
            Reply::Done(message) => match rt.format {
                OutputFormat::Json | OutputFormat::Yaml => {
                    collected.push(json!({"statement": statement, "message": message}))
                }
                _ => eprintln!("✓ {message}"),
            },
            Reply::Running | Reply::Stored => {}
        }
    }
    print_collected(rt.format, &collected)
}

fn print_collected(format: OutputFormat, collected: &[Value]) -> Result<()> {
    if collected.is_empty() || !matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        return Ok(());
    }
    output::print_value(&Value::Array(collected.to_vec()), format, None)
}

// ---------- the interactive shell ----------

enum Message {
    Done(Done),
    Cancelled(Result<(), String>),
    SessionState(String),
    /// Progress on long work, for the spinner.
    Progress(String),
    /// Something went wrong beside the work itself.
    Warning(String),
}

/// Runs the shell; returns how it ended and the session in use at the end
/// (a restart replaces the one it began with).
async fn repl(
    rt: &Runtime,
    options: &ShellArgs,
    lake: &Lakehouse,
    connection: Connection,
) -> (Result<()>, Connection) {
    let history = history::History::load(history::profile_path(&rt.http.resolved().profile_name));
    let mut state = ShellState::new(connection.warehouse.name.clone(), history);
    let mut shell = Shell {
        rt,
        options,
        lake,
        connection,
        running: Arc::new(Mutex::new(None)),
    };
    let guard = match TerminalGuard::enter() {
        Ok(guard) => guard,
        Err(err) => return (Err(err), shell.connection),
    };
    let mut term = InlineTerm::default();
    let result = match InlineTerm::size() {
        Ok((cols, _)) => match term.print(render::banner(
            &shell.connection.warehouse.name,
            &shell.connection.session,
            &shell.connection.compute,
            shell.connection.reused,
            cols as usize,
        )) {
            Ok(()) => shell.event_loop(&mut state, &mut term).await,
            Err(err) => Err(err),
        },
        Err(err) => Err(err),
    };
    term.clear();
    drop(guard);
    (result, shell.connection)
}

/// What the interactive loop works with.
struct Shell<'a> {
    rt: &'a Runtime,
    options: &'a ShellArgs,
    lake: &'a Lakehouse,
    connection: Connection,
    running: RunningQuery,
}

impl Shell<'_> {
    async fn event_loop(&mut self, state: &mut ShellState, term: &mut InlineTerm) -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let mut ticker = tokio::time::interval(FRAME);
        let mut session_check = tokio::time::interval(SESSION_CHECK);
        session_check.tick().await;
        self.spawn_effect(Effect::LoadCatalog, &tx);
        loop {
            let (cols, rows) = InlineTerm::size()?;
            let width = cols as usize;
            let lines: Vec<_> = state
                .entries
                .drain(..)
                .flat_map(|entry| render::entry_lines(&entry, width))
                .collect();
            term.print(lines)?;
            let height = render::live_height(state, rows.saturating_sub(1));
            term.draw(height, |frame| render::draw_live(frame, state))?;

            let mut effects = Vec::new();
            tokio::select! {
                Some(message) = rx.recv() => match message {
                    Message::Done(done) => {
                        if let Done::Restarted(Ok(session)) = &done {
                            self.connection.session = session.clone();
                        }
                        effects.extend(state.finish(done));
                    }
                    Message::Cancelled(result) => state.cancelled(result),
                    Message::SessionState(value) => state.session_state_is(value),
                    Message::Progress(label) => state.progress(label),
                    Message::Warning(text) => state.warn(text),
                },
                _ = session_check.tick() => self.spawn_effect(Effect::CheckSession, &tx),
                _ = ticker.tick() => {
                    state.on_tick();
                    while event::poll(Duration::ZERO).unwrap_or(false) {
                        match event::read().context("cannot read the keyboard")? {
                            Event::Key(key) if key.kind != KeyEventKind::Release => {
                                effects.extend(state.on_key(key));
                            }
                            Event::Paste(text) => {
                                state.input.insert_str(&text);
                            }
                            _ => {}
                        }
                    }
                }
            }
            for effect in effects {
                if effect == Effect::Exit {
                    return Ok(());
                }
                self.spawn_effect(effect, &tx);
            }
        }
    }

    fn spawn_effect(&self, effect: Effect, tx: &mpsc::UnboundedSender<Message>) {
        let (lake, tx) = (self.lake.clone(), tx.clone());
        let connection = self.connection.clone();
        let (session, warehouse) = (
            connection.session.clone(),
            connection.warehouse.name.clone(),
        );
        match effect {
            Effect::Run(statement) => {
                let running = self.running.clone();
                tokio::spawn(async move {
                    let done = match lake.run(&session, &warehouse, &statement, &running).await {
                        Ok(outcome) => Done::Ran(outcome),
                        Err(err) => Done::Failed {
                            statement,
                            error: format!("{err:#}"),
                        },
                    };
                    let _ = tx.send(Message::Done(done));
                });
            }
            Effect::NextPage { query, page } => {
                tokio::spawn(async move {
                    let result = lake
                        .page(&session, &query, &warehouse, page)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Message::Done(Done::Paged(result)));
                });
            }
            Effect::Save { source, to } => {
                tokio::spawn(async move {
                    let progress = |label: String| {
                        let _ = tx.send(Message::Progress(label));
                    };
                    let result =
                        save_result(&lake, &connection, source, &to, save::ROW_CAP, progress)
                            .await
                            .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Message::Done(Done::Saved(result)));
                });
            }
            Effect::LoadCatalog => {
                tokio::spawn(async move {
                    let result = load_catalog(&lake, &warehouse)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Message::Done(Done::Catalog(result)));
                });
            }
            Effect::CheckSession => {
                tokio::spawn(async move {
                    // A failed check says nothing about the session; the
                    // next one tries again.
                    if let Ok(value) = lake.session_state(&session).await {
                        let _ = tx.send(Message::SessionState(value));
                    }
                });
            }
            Effect::Restart => {
                let profile = self.rt.http.resolved().profile.clone();
                let profile_name = self.rt.http.resolved().profile_name.clone();
                let compute = self
                    .options
                    .compute
                    .clone()
                    .or(profile.sql_compute)
                    .or(Some(connection.compute.clone()).filter(|c| c != "?"));
                tokio::spawn(async move {
                    let result = restart_session(&lake, &connection, compute.as_deref())
                        .await
                        .map_err(|e| format!("{e:#}"));
                    if let Ok((session, compute)) = &result {
                        let saved = remember_session(
                            &profile_name,
                            &connection.warehouse,
                            compute,
                            session,
                        );
                        if let Err(err) = saved {
                            let _ = tx.send(Message::Warning(format!(
                                "The new session is not saved for the next gz sql: {err:#}"
                            )));
                        }
                    }
                    let restarted = result.map(|(session, _)| session);
                    let _ = tx.send(Message::Done(Done::Restarted(restarted)));
                });
            }
            Effect::Cancel => {
                let query = self.running.lock().ok().and_then(|q| q.clone());
                tokio::spawn(async move {
                    let result = match query {
                        Some(id) => lake
                            .cancel(&session, &id)
                            .await
                            .map_err(|e| format!("{e:#}")),
                        // Still inside the submit's short inline window; it ends on its own.
                        None => Ok(()),
                    };
                    let _ = tx.send(Message::Cancelled(result));
                });
            }
            Effect::Exit => {}
        }
    }
}

/// The warehouse's names for completion.
async fn load_catalog(lake: &Lakehouse, warehouse: &str) -> Result<catalog::Catalog> {
    let (schema, warehouses) = tokio::join!(lake.warehouse_schema(warehouse), lake.warehouses());
    let names = warehouses
        .map(|list| list.into_iter().map(|w| w.name).collect())
        .unwrap_or_default();
    Ok(catalog::Catalog::from_schema(warehouse, names, &schema?))
}

/// Starts a session like the stopped one (same warehouse, and `compute`),
/// stopping the old one first in case its pod still holds resources.
async fn restart_session(
    lake: &Lakehouse,
    old: &Connection,
    compute: Option<&str>,
) -> Result<(String, Compute)> {
    // Best effort: a session that is already gone refuses the stop.
    let _ = lake.stop(&old.session).await;
    let computes = lake.computes().await?;
    let compute = choose_compute(&computes, compute, None, false)?;
    let session = lake.start(compute.id, old.warehouse.id).await?;
    Ok((session, compute))
}

/// Writes a result to a file, page by page, up to `cap` rows, reporting
/// progress after each page.
async fn save_result(
    lake: &Lakehouse,
    connection: &Connection,
    source: SaveSource,
    to: &save::Destination,
    cap: usize,
    progress: impl Fn(String),
) -> Result<save::Saved> {
    let query = match source {
        SaveSource::Query(query) => query,
        SaveSource::Page(page) => {
            let mut file = save::ResultFile::create(to, &page.columns, &page.column_types)?;
            file.write_rows(&page.rows)?;
            return Ok(save::Saved {
                path: to.path.clone(),
                rows: page.rows.len(),
                capped: false,
                missing: None,
                notes: file.finish()?,
            });
        }
    };
    let mut file: Option<save::ResultFile> = None;
    let mut written = 0usize;
    let mut number = 1u32;
    let (capped, total) = loop {
        let page = lake
            .page_sized(
                &connection.session,
                &query,
                &connection.warehouse.name,
                number,
                save::PAGE_SIZE,
            )
            .await
            .map_err(|err| incomplete(err, written, &to.path))?;
        let writer = match file.as_mut() {
            Some(writer) => writer,
            None => file.insert(save::ResultFile::create(
                to,
                &page.columns,
                &page.column_types,
            )?),
        };
        let room = cap - written;
        let rows = &page.rows[..page.rows.len().min(room)];
        writer.write_rows(rows)?;
        written += rows.len();
        progress(format!("Saving {} · {written} rows", to.path.display()));
        let has_more = page.has_more || page.rows.len() > rows.len();
        if written >= cap {
            break (
                has_more || page.total_records.is_some_and(|t| t > written as u64),
                page.total_records,
            );
        }
        if !has_more || page.rows.is_empty() {
            break (false, page.total_records);
        }
        number += 1;
    };
    let notes = match file {
        Some(writer) => writer.finish()?,
        None => Vec::new(),
    };
    let missing = total
        .filter(|t| !capped && *t > written as u64)
        .map(|t| t - written as u64);
    Ok(save::Saved {
        path: to.path.clone(),
        rows: written,
        capped,
        missing,
        notes,
    })
}

/// A page that failed partway through a save: what was written stays,
/// and the error says the file is incomplete.
fn incomplete(err: anyhow::Error, written: usize, path: &Path) -> anyhow::Error {
    if written == 0 {
        return err;
    }
    err.context(format!(
        "{} holds only the first {written} rows; the save stopped here",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Answers each request with the next reply (status, body); returns
    /// the base URL and the request lines seen.
    async fn stub(replies: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap();
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                log.lock()
                    .unwrap()
                    .push(head.lines().next().unwrap_or_default().to_string());
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (base, seen)
    }

    fn page(rows: &[i64], more: bool, total: u64) -> (u16, String) {
        let data: Vec<Value> = rows.iter().map(|n| json!({"n": n})).collect();
        let body = json!({"status": 200, "response": {
            "data": data, "schema": [{"name": "n", "type": "long"}],
            "hasMore": more, "totalRecords": total, "pageNumber": 1,
        }});
        (200, body.to_string())
    }

    fn connection() -> Connection {
        Connection {
            session: "s1".into(),
            warehouse: Warehouse {
                id: 1,
                name: "WH".into(),
            },
            compute: "small".into(),
            reused: false,
        }
    }

    fn lake_at(base: &str) -> Lakehouse {
        let config = crate::config::Config::default();
        let resolved = crate::config::resolve(
            &config,
            Some("t"),
            Some(base.to_string()),
            Some("tok".into()),
        );
        let http = crate::http::ApiClient::new(resolved, OutputFormat::Json, true, 0, 5).unwrap();
        Lakehouse::new(http)
    }

    fn destination(name: &str) -> save::Destination {
        let dir = std::env::temp_dir().join(format!("gz-sqlfetch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        save::Destination {
            path: dir.join(name),
            format: save::FileFormat::Csv,
            append: false,
        }
    }

    async fn save(
        replies: Vec<(u16, String)>,
        to: &save::Destination,
        cap: usize,
    ) -> (Result<save::Saved>, Vec<String>, Vec<String>) {
        let (base, seen) = stub(replies).await;
        let progress = Mutex::new(Vec::new());
        let result = save_result(
            &lake_at(&base),
            &connection(),
            SaveSource::Query("q1".into()),
            to,
            cap,
            |label| progress.lock().unwrap().push(label),
        )
        .await;
        let requests = seen.lock().unwrap().clone();
        (result, requests, progress.into_inner().unwrap())
    }

    #[tokio::test]
    async fn saves_every_page_in_large_pages_with_progress() {
        let to = destination("all.csv");
        let replies = vec![
            page(&[1, 2], true, 5),
            page(&[3, 4], true, 5),
            page(&[5], false, 5),
        ];
        let (result, requests, progress) = save(replies, &to, 100).await;
        let saved = result.unwrap();
        assert_eq!((saved.rows, saved.capped, saved.missing), (5, false, None));
        assert_eq!(
            std::fs::read_to_string(&to.path).unwrap(),
            "n\n1\n2\n3\n4\n5\n"
        );
        assert!(
            requests[0].contains("pageNumber=1") && requests[0].contains("pageSize=5000"),
            "{requests:?}"
        );
        assert!(requests[2].contains("pageNumber=3"), "{requests:?}");
        assert_eq!(
            progress.last().unwrap(),
            &format!("Saving {} · 5 rows", to.path.display())
        );
    }

    #[tokio::test]
    async fn stops_at_the_row_cap_and_says_so() {
        let to = destination("capped.csv");
        let replies = vec![page(&[1, 2], true, 9), page(&[3, 4], true, 9)];
        let (result, requests, _) = save(replies, &to, 3).await;
        let saved = result.unwrap();
        assert_eq!((saved.rows, saved.capped), (3, true));
        assert_eq!(requests.len(), 2, "no page past the cap is read");
        assert_eq!(std::fs::read_to_string(&to.path).unwrap(), "n\n1\n2\n3\n");
        assert!(saved.message().contains("row cap"));
    }

    #[tokio::test]
    async fn names_rows_the_session_could_not_page_out() {
        let to = destination("short.csv");
        let (result, _, _) = save(vec![page(&[1, 2], false, 10)], &to, 100).await;
        assert_eq!(result.unwrap().missing, Some(8));
    }

    #[tokio::test]
    async fn a_failed_page_says_the_file_is_incomplete() {
        let to = destination("broken.csv");
        let replies = vec![
            page(&[1, 2], true, 4),
            (500, r#"{"message": "pod went away"}"#.to_string()),
        ];
        let (result, _, _) = save(replies, &to, 100).await;
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("holds only the first 2 rows"), "{error}");
    }
}
