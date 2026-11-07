//! The interactive `gz code` interface: a coding-CLI style transcript that
//! prints finished items into normal scrollback, with a live region (answer
//! in progress, spinner, input box, permission prompts) pinned below it.
//!
//! The session runs on its own task; its events and permission requests
//! reach this loop over channels, and [`state::UiState`] turns them (and
//! keys) into screen state. Terminal I/O lives only here.

pub mod approver;
pub mod items;
pub mod labels;
pub mod render;
pub mod state;

use std::io::stdout;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{self, disable_raw_mode, enable_raw_mode};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use crate::assistant::models::{Catalog, Selection};
use crate::assistant::{AssistantEvent, Session, TurnEnd};
use approver::{Ask, ChannelApprover, Mode};
use items::Item;
use state::{Effect, UiState};

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// How often the loop redraws (spinner, elapsed time) and polls the keyboard.
const FRAME: Duration = Duration::from_millis(50);

enum Command {
    Send(String),
    NewConversation,
    SetModel(Selection),
}

/// Who the banner says you are: `preview.stage… · sagatest`, and which
/// profile remembers the model choice.
pub struct Identity {
    pub site: Option<String>,
    pub user: Option<String>,
    pub profile: String,
}

pub async fn run(
    session: Session,
    identity: Identity,
    catalog: Option<Catalog>,
    model: Option<Selection>,
) -> Result<()> {
    let workspace = session.workspace_name();
    let cwd = session.workspace().display().to_string();
    let conversation = Arc::new(Mutex::new(session.conversation_id().to_string()));
    let canceller = session.canceller();
    let mode = Arc::new(Mutex::new(Mode::default()));
    let (commands, events, asks) = spawn_session(session, mode.clone(), conversation.clone());

    let mut state = UiState::new(mode);
    state.catalog = catalog;
    state.model = model;
    let profile = identity.profile.clone();
    let who = match (identity.site, identity.user) {
        (Some(site), Some(user)) => Some(format!("{site} · {user}")),
        (site, user) => site.or(user),
    };
    state.flush.push(Item::Banner {
        workspace,
        cwd,
        who,
        model: state.model_label(),
    });

    let guard = TerminalGuard::enter()?;
    let result = event_loop(&mut state, commands, events, asks, canceller, &profile).await;
    drop(guard);
    let id = conversation.lock().map(|c| c.clone()).unwrap_or_default();
    eprintln!("continue later: gz code -c {id}");
    result
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
                Command::SetModel(selection) => session.set_model(Some(selection)),
                Command::Send(prompt) => {
                    let tx = event_tx.clone();
                    let mut on_event = move |event: AssistantEvent| {
                        let _ = tx.send(event);
                    };
                    if let Err(err) = session.send(&prompt, &approver, &mut on_event).await {
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

async fn event_loop(
    state: &mut UiState,
    commands: mpsc::UnboundedSender<Command>,
    mut events: mpsc::UnboundedReceiver<AssistantEvent>,
    mut asks: mpsc::UnboundedReceiver<Ask>,
    canceller: crate::assistant::Canceller,
    profile: &str,
) -> Result<()> {
    let mut height = 0u16;
    let mut terminal: Option<Term> = None;
    let mut ticker = tokio::time::interval(FRAME);
    loop {
        let term = resize_live(state, &mut terminal, &mut height)?;
        flush_items(state, term)?;
        let term = resize_live(state, &mut terminal, &mut height)?;
        term.draw(|frame| render::draw_live(frame, state))
            .context("cannot draw")?;

        let mut effects = Vec::new();
        tokio::select! {
            Some(event) = events.recv() => effects.extend(state.apply(event)),
            Some(ask) = asks.recv() => state.open_prompt(ask),
            _ = ticker.tick() => {
                state.on_tick();
                // Keyboard input is polled here, on the loop's own thread, so
                // the terminal's cursor-position replies are never stolen.
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
            match effect {
                Effect::Send(prompt) => {
                    state.start_turn(&prompt);
                    commands.send(Command::Send(prompt)).ok();
                }
                Effect::Cancel => {
                    let canceller = canceller.clone();
                    tokio::spawn(async move { canceller.cancel().await.ok() });
                }
                Effect::NewConversation => {
                    commands.send(Command::NewConversation).ok();
                }
                Effect::SetModel(selection) => {
                    if let Err(err) = crate::cmd::code::remember_model(profile, &selection) {
                        state.flush.push(Item::Error(format!(
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

/// Raw mode (and richer key reporting where supported) for the session;
/// restored on drop and on panic.
struct TerminalGuard {
    enhanced: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("cannot enter raw mode (need a terminal)")?;
        let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
        if enhanced {
            execute!(
                stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .ok();
        }
        execute!(stdout(), event::EnableBracketedPaste).ok();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore(enhanced);
            previous(info);
        }));
        Ok(Self { enhanced })
    }
}

fn restore(enhanced: bool) {
    if enhanced {
        execute!(stdout(), PopKeyboardEnhancementFlags).ok();
    }
    execute!(stdout(), event::DisableBracketedPaste, cursor::Show).ok();
    disable_raw_mode().ok();
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore(self.enhanced);
    }
}
