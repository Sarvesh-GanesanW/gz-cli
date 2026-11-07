//! `gz sql`: an interactive SQL shell on a lakehouse session, plus piped
//! and file scripts. Picks (or remembers) a warehouse and compute size,
//! reuses a session still running for this profile, and stops the session
//! on exit unless asked to keep it.

pub mod history;
pub mod meta;
pub mod render;
pub mod results;
pub mod session;
pub mod state;
pub mod statements;

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
use state::{Done, Effect, ShellState};

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
}

/// A session ready for statements.
#[derive(Clone)]
struct Connection {
    session: String,
    warehouse: Warehouse,
    compute: String,
    reused: bool,
}

pub async fn run(rt: &Runtime, options: &ShellArgs, source: Source) -> Result<()> {
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
    };
    if let Some(text) = &script {
        if script_statements(text).is_empty() {
            bail!("no SQL statements to run");
        }
    }
    let lake = Lakehouse::new(rt.http.clone());
    let connection = connect(rt, &lake, options, script.is_none()).await?;
    let outcome = match &script {
        None => repl(&lake, &connection).await,
        Some(text) => run_script(rt, &lake, &connection, text).await,
    };
    finish_session(rt, &lake, &connection, options.keep_session).await;
    outcome
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
    remember(rt, |p| {
        p.sql_warehouse = Some(warehouse.name.clone());
        p.sql_compute = Some(compute.name.clone());
        p.sql_session = Some(session.clone());
        p.sql_session_warehouse = Some(warehouse.name.clone());
    });
    Ok(Connection {
        session,
        warehouse,
        compute: compute.name,
        reused: false,
    })
}

fn reusable_session(profile: &crate::config::Profile, warehouse: &Warehouse) -> Option<String> {
    let same_warehouse = profile.sql_session_warehouse.as_deref() == Some(warehouse.name.as_str());
    same_warehouse
        .then(|| profile.sql_session.clone())
        .flatten()
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
    let started = lake.start(compute.id, warehouse.id).await;
    if let Some(bar) = bar {
        bar.finish_and_clear();
    }
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
        Ok(()) => remember(rt, |p| {
            p.sql_session = None;
            p.sql_session_warehouse = None;
        }),
        Err(err) => eprintln!("could not stop session {}: {err:#}", connection.session),
    }
}

/// Updates the active profile in the config file; failures only warn.
fn remember(rt: &Runtime, change: impl FnOnce(&mut crate::config::Profile)) {
    let saved = crate::config::load_config().and_then(|mut config| {
        change(config.profile_mut(&rt.http.resolved().profile_name));
        crate::config::save_config(&config).map(|_| ())
    });
    if let Err(err) = saved {
        output::warn(&format!("cannot save gz sql settings: {err:#}"));
    }
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

async fn export_csv(
    lake: &Lakehouse,
    connection: &Connection,
    query: &str,
    path: &Path,
) -> Result<usize> {
    let first = lake
        .page(&connection.session, query, &connection.warehouse.name, 1)
        .await?;
    let page = all_pages(lake, connection, query, first).await?;
    std::fs::write(path, output::csv_text(&page.columns, &page.rows))
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(page.rows.len())
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
}

async fn repl(lake: &Lakehouse, connection: &Connection) -> Result<()> {
    let mut state = ShellState::new(
        connection.warehouse.name.clone(),
        history::History::load(history::default_path()),
    );
    let guard = TerminalGuard::enter()?;
    let mut term = InlineTerm::default();
    let (cols, _) = InlineTerm::size()?;
    term.print(render::banner(
        &connection.warehouse.name,
        &connection.session,
        &connection.compute,
        connection.reused,
        cols as usize,
    ))?;
    let result = event_loop(&mut state, &mut term, lake, connection).await;
    term.clear();
    drop(guard);
    result
}

async fn event_loop(
    state: &mut ShellState,
    term: &mut InlineTerm,
    lake: &Lakehouse,
    connection: &Connection,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let running: RunningQuery = Arc::new(Mutex::new(None));
    let mut ticker = tokio::time::interval(FRAME);
    let mut session_check = tokio::time::interval(SESSION_CHECK);
    session_check.tick().await;
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
                Message::Done(done) => effects.extend(state.finish(done)),
                Message::Cancelled(result) => state.cancelled(result),
                Message::SessionState(value) => state.session_state = value,
            },
            _ = session_check.tick() => {
                let (lake, tx, id) = (lake.clone(), tx.clone(), connection.session.clone());
                tokio::spawn(async move {
                    if let Ok(value) = lake.session_state(&id).await {
                        let _ = tx.send(Message::SessionState(value));
                    }
                });
            }
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
            spawn_effect(effect, lake, connection, &running, &tx);
        }
    }
}

fn spawn_effect(
    effect: Effect,
    lake: &Lakehouse,
    connection: &Connection,
    running: &RunningQuery,
    tx: &mpsc::UnboundedSender<Message>,
) {
    let (lake, tx) = (lake.clone(), tx.clone());
    let (session, warehouse) = (
        connection.session.clone(),
        connection.warehouse.name.clone(),
    );
    match effect {
        Effect::Run(statement) => {
            let running = running.clone();
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
        Effect::Export { query, path } => {
            let connection = connection.clone();
            tokio::spawn(async move {
                let result = export_csv(&lake, &connection, &query, &path)
                    .await
                    .map(|rows| (path, rows))
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Message::Done(Done::Exported(result)));
            });
        }
        Effect::Cancel => {
            let query = running.lock().ok().and_then(|q| q.clone());
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
