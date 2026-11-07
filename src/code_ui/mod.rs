//! The interactive `gz code` interface: a coding-CLI style transcript that
//! prints finished items into normal scrollback, with a live region (answer
//! in progress, spinner, input box, permission prompts) pinned below it.
//!
//! The session runs on its own task; its events and permission requests
//! reach this loop over channels, and [`state::UiState`] turns them (and
//! keys) into screen state. Terminal I/O lives only here.

pub mod approver;
pub mod blocks;
pub mod clipboard;
pub mod items;
pub mod labels;
pub mod mentions;
pub mod render;
pub mod resume;
pub mod state;
pub mod transcript;
pub mod usage;

use std::io::stdout;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal;
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::{mpsc, oneshot};

use crate::assistant::attachments::PreparedTurn;
use crate::assistant::models::{Catalog, Selection};
use crate::assistant::{history, AssistantEvent, Session, TurnEnd};
use crate::inline_term::TerminalGuard;
use approver::{Ask, ChannelApprover, Mode};
use items::Item;
pub use state::Effect;
use state::{PickerRow, UiState};

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// How often the loop redraws (spinner, elapsed time) and polls the keyboard.
const FRAME: Duration = Duration::from_millis(50);

enum Command {
    Send(PreparedTurn),
    NewConversation,
    /// Continue this earlier conversation.
    Continue(String),
    SetModel(Selection),
    SetSubagents(bool),
    /// Re-read saved "always" rules after `/permissions revoke`.
    ReloadRules,
}

/// Who the banner says you are: `preview.stage… · sagatest`, and which
/// profile remembers the model choice.
pub struct Identity {
    pub site: Option<String>,
    pub user: Option<String>,
    pub profile: String,
}

/// A background model-list load: the catalog and saved choice, or `None`
/// if the list couldn't be loaded.
pub type ModelLoad = oneshot::Receiver<Option<(Catalog, Option<Selection>)>>;

/// The model list: already loaded, or arriving from a background task.
pub enum Models {
    Ready(Box<Catalog>, Option<Selection>),
    Loading(ModelLoad),
}

/// `start` runs right after the banner: e.g. resuming a conversation
/// (`gz code --continue`) or opening the `/resume` picker (`--resume`).
pub async fn run(
    session: Session,
    identity: Identity,
    models: Models,
    saved_model: Option<String>,
    start: Vec<Effect>,
) -> Result<()> {
    let workspace = session.workspace_name();
    let cwd = session.workspace().display().to_string();
    let conversation = Arc::new(Mutex::new(session.conversation_id().to_string()));
    let canceller = session.canceller();
    let http = session.http();
    let sandbox = session.sandbox().clone();
    let subagents_enabled = session.subagents_enabled();
    let mode = Arc::new(Mutex::new(Mode::default()));
    let (commands, events, asks) = spawn_session(session, mode.clone(), conversation.clone());

    let mut state = UiState::new(mode);
    state.subagents_enabled = subagents_enabled;
    let loading = match models {
        Models::Ready(catalog, model) => {
            state.catalog = Some(*catalog);
            state.model = model;
            None
        }
        Models::Loading(rx) => {
            state.models_loading = true;
            Some(rx)
        }
    };
    state.sandbox = Some(sandbox);
    let profile = identity.profile.clone();
    let who = match (identity.site, identity.user) {
        (Some(site), Some(user)) => Some(format!("{site} · {user}")),
        (site, user) => site.or(user),
    };
    state.record(Item::Banner {
        workspace,
        cwd,
        who,
        model: state.model_label().or(saved_model),
    });

    let guard = TerminalGuard::enter()?;
    let channels = Channels {
        commands,
        events,
        asks,
        conversation: conversation.clone(),
    };
    let result = event_loop(
        &mut state, channels, canceller, http, &profile, loading, start,
    )
    .await;
    drop(guard);
    let id = conversation.lock().map(|c| c.clone()).unwrap_or_default();
    eprintln!("continue later: gz code --continue (or gz code -c {id})");
    result
}

/// What `/permissions` (optionally revoking an id) shows.
struct PermissionsReply {
    lines: Result<Vec<String>, String>,
    revoked: bool,
}

/// Lists saved rules and chat approvals; with `revoke`, removes that entry
/// first and shows the updated list.
async fn permissions(
    http: &crate::http::ApiClient,
    root: Option<std::path::PathBuf>,
    conversation: &str,
    revoke: Option<String>,
) -> PermissionsReply {
    use crate::assistant::grants;
    let chat = (!conversation.is_empty()).then_some(conversation);
    let load = || grants::load(http, root.as_deref(), chat);
    let mut header = Vec::new();
    let mut revoked = false;
    if let Some(id) = revoke {
        let done = match load().await {
            Ok(listing) => grants::revoke(http, &listing, &id).await,
            Err(err) => Err(err),
        };
        match done {
            Ok(what) => {
                revoked = true;
                header.push(format!("Revoked {}: {what}", id.to_ascii_uppercase()));
                header.push(String::new());
            }
            Err(err) => {
                return PermissionsReply {
                    lines: Err(format!("{err:#}")),
                    revoked: false,
                }
            }
        }
    }
    let lines = load()
        .await
        .map(|listing| header.into_iter().chain(listing.report()).collect())
        .map_err(|err| format!("{err:#}"));
    PermissionsReply { lines, revoked }
}

/// A resumed conversation's title and earlier messages.
type HistoryReply = (String, Result<history::Transcript, String>);

/// The `/resume` rows: this folder's conversations, then the rest.
async fn conversation_rows(
    http: &crate::http::ApiClient,
    root: Option<std::path::PathBuf>,
) -> Result<Vec<PickerRow>, String> {
    let local = match (history::Index::open(), root) {
        (Ok(index), Some(root)) => index.recent(&root),
        _ => Vec::new(),
    };
    let remote = history::conversations(http)
        .await
        .map_err(|err| format!("{err:#}"));
    resume::rows(&local, remote, history::now_millis())
}

/// Runs the session on its own task so the UI stays responsive.
fn spawn_session(
    mut session: Session,
    mode: Arc<Mutex<Mode>>,
    conversation: Arc<Mutex<String>>,
) -> (
    mpsc::UnboundedSender<Command>,
    mpsc::UnboundedReceiver<AssistantEvent>,
    mpsc::UnboundedReceiver<Ask>,
) {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (ask_tx, ask_rx) = mpsc::unbounded_channel();
    let approver = ChannelApprover::new(ask_tx, mode);
    tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            match command {
                Command::NewConversation => session.new_conversation(),
                Command::Continue(id) => session.continue_conversation(&id),
                Command::ReloadRules => session.reload_saved_rules(),
                Command::SetModel(selection) => session.set_model(Some(selection)),
                Command::SetSubagents(enabled) => session.set_subagents(enabled),
                Command::Send(turn) => {
                    let tx = event_tx.clone();
                    let mut on_event = move |event: AssistantEvent| {
                        let _ = tx.send(event);
                    };
                    if let Err(err) = session.send_prepared(&turn, &approver, &mut on_event).await {
                        let _ = event_tx
                            .send(AssistantEvent::Done(TurnEnd::Failed(format!("{err:#}"))));
                    }
                }
            }
            if let Ok(mut id) = conversation.lock() {
                *id = session.conversation_id().to_string();
            }
        }
    });
    (command_tx, event_rx, ask_rx)
}

/// The session task's ends of the conversation.
struct Channels {
    commands: mpsc::UnboundedSender<Command>,
    events: mpsc::UnboundedReceiver<AssistantEvent>,
    asks: mpsc::UnboundedReceiver<Ask>,
    /// The current conversation, for listing its "for this chat" grants.
    conversation: Arc<Mutex<String>>,
}

async fn event_loop(
    state: &mut UiState,
    channels: Channels,
    canceller: crate::assistant::Canceller,
    http: crate::http::ApiClient,
    profile: &str,
    mut loading: Option<ModelLoad>,
    start: Vec<Effect>,
) -> Result<()> {
    let Channels {
        commands,
        mut events,
        mut asks,
        conversation,
    } = channels;
    let (account_tx, mut account_rx) = mpsc::unbounded_channel::<Result<Vec<String>, String>>();
    let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<PermissionsReply>();
    let (list_tx, mut list_rx) = mpsc::unbounded_channel::<Result<Vec<PickerRow>, String>>();
    let (history_tx, mut history_rx) = mpsc::unbounded_channel::<HistoryReply>();
    let mut pending = start;
    let mut height = 0u16;
    // Prompts sent before the model list arrives wait for it, so they go
    // out with the saved model rather than the server default.
    let mut held: Vec<String> = Vec::new();
    let mut terminal: Option<Term> = None;
    let mut full_screen: Option<Term> = None;
    let mut ticker = tokio::time::interval(FRAME);
    loop {
        if state.viewer.is_some() {
            let term = enter_transcript(&mut terminal, &mut full_screen, &mut height)?;
            term.draw(|frame| render::draw_transcript(frame, state))
                .context("cannot draw")?;
        } else {
            leave_transcript(&mut full_screen);
            let term = resize_live(state, &mut terminal, &mut height)?;
            flush_items(state, term)?;
            let term = resize_live(state, &mut terminal, &mut height)?;
            term.draw(|frame| render::draw_live(frame, state))
                .context("cannot draw")?;
        }

        let mut effects = std::mem::take(&mut pending);
        tokio::select! {
            loaded = async { loading.as_mut().expect("guarded").await }, if loading.is_some() => {
                loading = None;
                state.models_loading = false;
                if let Ok(Some((catalog, selection))) = loaded {
                    state.catalog = Some(catalog);
                    if state.model.is_none() {
                        if let Some(selection) = selection {
                            commands.send(Command::SetModel(selection.clone())).ok();
                            state.model = Some(selection);
                        }
                    }
                }
                effects.extend(held.drain(..).map(Effect::Send));
            }
            Some(event) = events.recv() => effects.extend(state.apply(event)),
            Some(ask) = asks.recv() => state.open_prompt(ask),
            Some(panel) = account_rx.recv() => state.show_account_usage(panel),
            Some(rows) = list_rx.recv() => state.show_conversations(rows),
            Some((title, transcript)) = history_rx.recv() => state.show_history(&title, transcript),
            Some(reply) = perm_rx.recv() => {
                if reply.revoked {
                    commands.send(Command::ReloadRules).ok();
                }
                state.show_permissions(reply.lines);
            }
            _ = ticker.tick() => {
                state.on_tick();
                // Keyboard input is polled here, on the loop's own thread, so
                // the terminal's cursor-position replies are never stolen.
                while event::poll(Duration::ZERO).unwrap_or(false) {
                    match event::read().context("cannot read the keyboard")? {
                        Event::Key(key) if key.kind != KeyEventKind::Release => {
                            effects.extend(state.on_key(key));
                        }
                        Event::Paste(text) => state.on_paste(&text),
                        _ => {}
                    }
                }
            }
        }
        for effect in effects {
            match effect {
                Effect::Send(prompt) if loading.is_some() => held.push(prompt),
                Effect::Send(prompt) => {
                    if let Some(turn) = state.take_turn(&prompt) {
                        state.start_prepared_turn(&turn);
                        commands.send(Command::Send(turn)).ok();
                    }
                }
                Effect::Cancel => {
                    let canceller = canceller.clone();
                    tokio::spawn(async move { canceller.cancel().await.ok() });
                }
                Effect::SetSubagents(enabled) => {
                    if let Err(err) = crate::cmd::code::remember_subagents(profile, enabled) {
                        state.record(Item::Error(format!(
                            "cannot save the subagent setting: {err:#}"
                        )));
                    }
                    commands.send(Command::SetSubagents(enabled)).ok();
                }
                Effect::Permissions(revoke) => {
                    let root = state.sandbox.as_ref().map(|s| s.root().to_path_buf());
                    let chat = conversation.lock().map(|c| c.clone()).unwrap_or_default();
                    let (http, tx) = (http.clone(), perm_tx.clone());
                    tokio::spawn(async move {
                        tx.send(permissions(&http, root, &chat, revoke).await).ok();
                    });
                }
                Effect::FetchAccountUsage => {
                    let (http, tx) = (http.clone(), account_tx.clone());
                    tokio::spawn(async move {
                        let panel = crate::cmd::usage::fetch(&http)
                            .await
                            .map(|account| crate::cmd::usage::panel(&account))
                            .map_err(|err| format!("{err:#}"));
                        tx.send(panel).ok();
                    });
                }
                Effect::NewConversation => {
                    commands.send(Command::NewConversation).ok();
                }
                Effect::ListConversations => {
                    let root = state.sandbox.as_ref().map(|s| s.root().to_path_buf());
                    let (http, tx) = (http.clone(), list_tx.clone());
                    tokio::spawn(async move {
                        tx.send(conversation_rows(&http, root).await).ok();
                    });
                }
                Effect::Resume { id, title } => {
                    commands.send(Command::Continue(id.clone())).ok();
                    let (http, tx) = (http.clone(), history_tx.clone());
                    tokio::spawn(async move {
                        let transcript = history::transcript(&http, &id)
                            .await
                            .map_err(|err| format!("{err:#}"));
                        tx.send((title, transcript)).ok();
                    });
                }
                Effect::SetModel(selection) => {
                    if let Err(err) = crate::cmd::code::remember_model(profile, &selection) {
                        state.record(Item::Error(format!(
                            "cannot save the model choice: {err:#}"
                        )));
                    }
                    commands.send(Command::SetModel(selection)).ok();
                }
                Effect::ClearScreen => {
                    if let Some(term) = terminal.take() {
                        drop(term);
                    }
                    execute!(
                        stdout(),
                        terminal::Clear(terminal::ClearType::All),
                        cursor::MoveTo(0, 0)
                    )
                    .ok();
                    height = 0;
                }
                Effect::Exit => {
                    if let Some(term) = terminal.as_mut() {
                        term.clear().ok();
                    }
                    return Ok(());
                }
            }
        }
    }
}

/// Switches to the alternate screen for the Ctrl+O transcript. The inline
/// region is cleared first; scrollback is untouched and comes back intact.
fn enter_transcript<'a>(
    inline: &mut Option<Term>,
    full_screen: &'a mut Option<Term>,
    height: &mut u16,
) -> Result<&'a mut Term> {
    if full_screen.is_none() {
        if let Some(mut term) = inline.take() {
            term.clear().ok();
        }
        *height = 0;
        crate::inline_term::enter_alternate_screen().context("cannot open the transcript")?;
        let mut term =
            Terminal::new(CrosstermBackend::new(stdout())).context("cannot open the terminal")?;
        term.clear().ok();
        *full_screen = Some(term);
    }
    Ok(full_screen
        .as_mut()
        .expect("the transcript screen was just opened"))
}

/// Back to the inline view; the next frame recreates the live region.
fn leave_transcript(full_screen: &mut Option<Term>) {
    if full_screen.take().is_some() {
        crate::inline_term::leave_alternate_screen();
    }
}

/// Keeps the inline viewport exactly as tall as the live region needs,
/// recreating it in place when that changes.
fn resize_live<'a>(
    state: &UiState,
    terminal: &'a mut Option<Term>,
    height: &mut u16,
) -> Result<&'a mut Term> {
    let (cols, rows) = terminal::size().context("cannot read the terminal size")?;
    let wanted = render::live_height(state, cols, rows.saturating_sub(1));
    if terminal.is_none() || wanted != *height {
        if let Some(old) = terminal.as_mut() {
            old.clear().ok();
        }
        let backend = CrosstermBackend::new(stdout());
        let options = TerminalOptions {
            viewport: Viewport::Inline(wanted),
        };
        *terminal =
            Some(Terminal::with_options(backend, options).context("cannot open the terminal")?);
        *height = wanted;
    }
    Ok(terminal.as_mut().expect("terminal was just created"))
}

/// Prints finished items above the live region, into scrollback.
fn flush_items(state: &mut UiState, term: &mut Term) -> Result<()> {
    if state.flush.is_empty() {
        return Ok(());
    }
    let width = term.size().map(|s| s.width).unwrap_or(80) as usize;
    let lines: Vec<Line<'static>> = state
        .flush
        .drain(..)
        .flat_map(|item| item.render(width))
        .collect();
    let rows = lines.len() as u16;
    term.insert_before(rows, |buffer| {
        Paragraph::new(lines).render(buffer.area, buffer);
    })
    .context("cannot print to the terminal")
}
