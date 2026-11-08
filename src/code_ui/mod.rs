//! The interactive `gz code` interface: a coding-CLI style transcript that
//! prints finished items into normal scrollback, with a live region (answer
//! in progress, spinner, input box, permission prompts) pinned below it.
//!
//! The session runs on its own task; its events and permission requests
//! reach this loop over channels, and [`state::UiState`] turns them (and
//! keys) into screen state. Terminal I/O lives only here.

pub mod agent_pages;
pub mod approver;
pub mod auto_mode;
pub mod background;
pub mod blocks;
pub mod clipboard;
pub mod items;
pub mod labels;
pub mod mentions;
pub mod panel;
pub mod render;
pub mod resume;
pub mod skills;
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

use crate::assistant::action_review::HttpReviewer;
use crate::assistant::attachments::PreparedTurn;
use crate::assistant::audit::AuditLog;
use crate::assistant::auto_rules::AutoStore;
use crate::assistant::local_context::Limits;
use crate::assistant::models::{Catalog, Selection};
use crate::assistant::sandbox::Sandbox;
use crate::assistant::steering::SentNote;
use crate::assistant::subagent_settings::SubagentSettings;
use crate::assistant::{compaction, history, AssistantEvent, Session, TurnEnd};
use crate::inline_term::TerminalGuard;
use approver::{Ask, AutoReview, ChannelApprover, Mode};
use auto_mode::AutoContext;
use items::Item;
pub use state::Effect;
use state::{FolderCommand, PickerRow, UiState};

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Shown above a prepared message waiting in the input box.
const DRAFT_NOTICE: &str =
    "A prepared message is in the input box: review or edit it, then press Enter to send";

/// How often the loop redraws (spinner, elapsed time) and polls the keyboard.
const FRAME: Duration = Duration::from_millis(50);

enum Command {
    Send(PreparedTurn),
    NewConversation,
    /// Continue this earlier conversation.
    Continue(String),
    SetModel(Selection),
    SetSubagents(bool),
    /// The chat may hand subagents to gz (the server supports it).
    SetBackgroundSubagents(bool),
    SetAutoCompact(Option<u64>),
    SetSubagentSettings(SubagentSettings),
    /// The server takes `subagentSettings`.
    SetSubagentSettingsSupported(bool),
    /// Re-read saved "always" rules after `/permissions revoke`.
    ReloadRules,
    /// What the server takes as local context (`None`: nothing).
    SetLocalContext(Option<Limits>),
    /// `/memory on|off`.
    SetMemory(bool),
    /// `/cd`, `/add-dir`, `/dirs remove`.
    Folder(FolderCommand),
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
/// `draft` starts in the input box, unsent, for the user to review.
pub async fn run(
    mut session: Session,
    identity: Identity,
    models: Models,
    saved_model: Option<String>,
    mut start: Vec<Effect>,
    start_mode: Mode,
    draft: Option<String>,
) -> Result<()> {
    let workspace = session.workspace_name();
    let cwd = session.workspace().display().to_string();
    let context = session.local_context();
    let conversation = Arc::new(Mutex::new(session.conversation_id().to_string()));
    let canceller = session.canceller();
    let steering = session.steering();
    let http = session.http();
    let sandbox = session.sandbox().clone();
    let subagents_enabled = session.subagents_enabled();
    let auto_compact = session.auto_compact();
    // Already checked against the model list when it came preloaded.
    let subagent_settings = session.subagent_settings().clone();
    let skill_names = session.mentionable_skills();
    let mode = Arc::new(Mutex::new(start_mode));
    let auto = auto_review(&http, &sandbox);
    let auto_context = auto.as_ref().map(|a| a.context.clone());
    let auto_store = auto.as_ref().map(|a| a.store.clone());
    if let Models::Ready(catalog, _) = &models {
        session.set_background_subagents(catalog.background_subagents);
        session.set_local_context(catalog.local_context);
    }
    let (event_tx, events) = mpsc::unbounded_channel();
    let (ask_tx, asks) = mpsc::unbounded_channel();
    let launcher = background::Launcher::new(
        http.clone(),
        sandbox.clone(),
        context.clone(),
        event_tx.clone(),
        ask_tx.clone(),
        mode.clone(),
        auto.clone(),
        session.shared_policy(),
    );
    let mut approver = ChannelApprover::new(ask_tx.clone(), mode.clone());
    if let Some(auto) = auto {
        approver = approver.with_auto(auto);
    }
    let commands = spawn_session(session, approver, event_tx, conversation.clone());

    let mut state = UiState::new(mode);
    state.auto = auto_context;
    state.steering = Some(steering);
    state.auto_store = auto_store;
    state.subagents_enabled = subagents_enabled;
    state.auto_compact = auto_compact;
    state.subagent_settings = subagent_settings;
    state.skill_names = skill_names;
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
    state.memory = context.memory.clone();
    state.memory_on = context.memory_on();
    state.user_instructions = context.user_instructions.clone();
    state.audit = AuditLog::open().ok();
    let profile = identity.profile.clone();
    let caps = http.resolved().profile.code_spend_caps.unwrap_or_default();
    // Without a ledger, day caps count this session only.
    let ledger = crate::spend::ledger::Ledger::open(&profile).ok();
    state.spend = state::SpendState::new(caps, ledger);
    start.extend(state.refresh_budget(true));
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
    if let Some(draft) = draft {
        state.set_draft(&draft);
        state.record(Item::Notice(DRAFT_NOTICE.into()));
    }

    let guard = TerminalGuard::enter()?;
    let channels = Channels {
        commands,
        events,
        asks,
        conversation: conversation.clone(),
        launcher,
        ask_tx,
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
pub struct PermissionsReply {
    pub panel: Result<panel::Panel, String>,
    /// What was revoked first, if anything.
    pub revoked: Option<String>,
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
    let mut revoked = None;
    if let Some(id) = revoke {
        let done = match load().await {
            Ok(listing) => grants::revoke(http, &listing, &id).await,
            Err(err) => Err(err),
        };
        match done {
            Ok(what) => revoked = Some(format!("Revoked {}: {what}", id.to_ascii_uppercase())),
            Err(err) => {
                return PermissionsReply {
                    panel: Err(format!("{err:#}")),
                    revoked: None,
                }
            }
        }
    }
    let panel = load()
        .await
        .map(|listing| listing.report())
        .map_err(|err| format!("{err:#}"));
    PermissionsReply { panel, revoked }
}

/// A resumed conversation's title and earlier messages.
type HistoryReply = (String, Result<history::Transcript, String>);
/// What `/compact` did, or why it failed.
type CompactReply = Result<compaction::Report, String>;

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

/// Auto mode's review and memory for this folder; `None` (no auto mode)
/// when the config folder can't be found.
fn auto_review(http: &crate::http::ApiClient, sandbox: &Sandbox) -> Option<AutoReview> {
    let store = match AutoStore::open() {
        Ok(store) => store,
        Err(err) => {
            crate::output::warn(&format!("auto mode is off: {err:#}"));
            return None;
        }
    };
    Some(AutoReview {
        reviewer: Arc::new(HttpReviewer::new(http.clone())),
        context: Arc::new(Mutex::new(AutoContext::default())),
        store,
        sandbox: sandbox.clone(),
    })
}

/// Runs the session on its own task so the UI stays responsive.
fn spawn_session(
    mut session: Session,
    approver: ChannelApprover,
    event_tx: mpsc::UnboundedSender<AssistantEvent>,
    conversation: Arc<Mutex<String>>,
) -> mpsc::UnboundedSender<Command> {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<Command>();
    tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            match command {
                Command::NewConversation => session.new_conversation(),
                Command::Continue(id) => session.continue_conversation(&id),
                Command::ReloadRules => session.reload_saved_rules(),
                Command::SetModel(selection) => session.set_model(Some(selection)),
                Command::SetSubagents(enabled) => session.set_subagents(enabled),
                Command::SetBackgroundSubagents(on) => session.set_background_subagents(on),
                Command::SetAutoCompact(tokens) => session.set_auto_compact(tokens),
                Command::SetSubagentSettings(settings) => session.set_subagent_settings(settings),
                Command::SetSubagentSettingsSupported(on) => {
                    session.set_subagent_settings_supported(on)
                }
                Command::SetLocalContext(limits) => session.set_local_context(limits),
                Command::SetMemory(on) => session.set_memory(on),
                Command::Folder(command) => {
                    let done = match &command {
                        FolderCommand::Cd(path) => session.change_dir(path),
                        FolderCommand::Add(path) => session.add_dir(path),
                        FolderCommand::Remove(path) => session.remove_dir(path),
                    };
                    let _ =
                        event_tx.send(AssistantEvent::Folders(done.map_err(|e| format!("{e:#}"))));
                }
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
    command_tx
}

/// The session task's ends of the conversation.
struct Channels {
    commands: mpsc::UnboundedSender<Command>,
    events: mpsc::UnboundedReceiver<AssistantEvent>,
    asks: mpsc::UnboundedReceiver<Ask>,
    /// The current conversation, for listing its "for this chat" grants.
    conversation: Arc<Mutex<String>>,
    launcher: background::Launcher,
    /// For prompts the UI raises itself (skill installs).
    ask_tx: mpsc::UnboundedSender<Ask>,
}

/// Why `/skills` can't work: gz has no config folder.
const NO_SKILLS_HOME: &str = "cannot find the gz config folder for skills";

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
        mut launcher,
        ask_tx,
    } = channels;
    let (skills_tx, mut skills_rx) =
        mpsc::unbounded_channel::<Result<skills::SkillsView, String>>();
    let (account_tx, mut account_rx) = mpsc::unbounded_channel::<Result<panel::Panel, String>>();
    let (steer_tx, mut steer_rx) = mpsc::unbounded_channel::<Vec<SentNote>>();
    let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<PermissionsReply>();
    let (list_tx, mut list_rx) = mpsc::unbounded_channel::<Result<Vec<PickerRow>, String>>();
    let (history_tx, mut history_rx) = mpsc::unbounded_channel::<HistoryReply>();
    let (compact_tx, mut compact_rx) = mpsc::unbounded_channel::<CompactReply>();
    // The skills folder, with the signed-in site's team skills.
    let skills_home = crate::skills::Home::open()
        .ok()
        .map(|home| home.with_team_key(crate::skills::team::team_key(http.resolved())));
    let (budget_tx, mut budget_rx) = mpsc::unbounded_channel::<Result<serde_json::Value, String>>();
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
        } else if state.agent_view.is_some() {
            let term = enter_transcript(&mut terminal, &mut full_screen, &mut height)?;
            term.draw(|frame| render::draw_agent_page(frame, state))
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
                    commands
                        .send(Command::SetBackgroundSubagents(catalog.background_subagents))
                        .ok();
                    commands
                        .send(Command::SetSubagentSettingsSupported(
                            catalog.subagent_settings.is_some(),
                        ))
                        .ok();
                    commands.send(Command::SetLocalContext(catalog.local_context)).ok();
                    // A saved model the server dropped would fail every
                    // run; this session goes without it.
                    state.set_skill_names(skills::mentionable(skills_home.as_ref(), catalog.local_context));
                    if let (Some(home), Some(limits)) = (&skills_home, catalog.team_skills) {
                        skills::start_team_sync(http.clone(), home.clone(), limits, skills_tx.clone());
                    }
                    if let Some(note) = state.subagent_settings.drop_unavailable(&catalog) {
                        state.record(Item::Notice(note));
                        commands
                            .send(Command::SetSubagentSettings(state.subagent_settings.clone()))
                            .ok();
                    }
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
            Some(event) = events.recv() => effects.extend(state.on_event(event)),
            Some(ask) = asks.recv() => state.open_prompt(ask),
            Some(panel) = account_rx.recv() => state.show_account_usage(panel),
            Some(answer) = budget_rx.recv() => state.budget_arrived(answer),
            Some(notes) = steer_rx.recv() => state.notes_sent(notes),
            Some(rows) = list_rx.recv() => state.show_conversations(rows),
            Some((title, transcript)) = history_rx.recv() => state.show_history(&title, transcript),
            Some(reply) = compact_rx.recv() => effects.extend(state.show_compaction(reply)),
            Some(view) = skills_rx.recv() => {
                state.show_skills(view);
                state.set_skill_names(skills::mentionable(skills_home.as_ref(), state.catalog.as_ref().and_then(|c| c.local_context)));
            }
            Some(reply) = perm_rx.recv() => {
                if reply.revoked.is_some() {
                    commands.send(Command::ReloadRules).ok();
                }
                state.show_permissions(reply);
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
        // Sends past a cap or a used-up budget wait here.
        let effects: Vec<Effect> = effects.into_iter().flat_map(|e| state.gate(e)).collect();
        for effect in effects {
            match effect {
                Effect::Send(prompt) if loading.is_some() => held.push(prompt),
                Effect::Send(prompt) => {
                    if let Some(turn) = state.take_turn(&prompt) {
                        state.start_prepared_turn(&turn);
                        commands.send(Command::Send(turn)).ok();
                    }
                }
                Effect::SendReports(message) => {
                    state.start_report_turn(&message);
                    commands
                        .send(Command::Send(PreparedTurn::plain(&message)))
                        .ok();
                }
                Effect::StartSubagent(plan) => launcher.start(plan),
                Effect::Skills(
                    action @ (skills::SkillsAction::Install { .. }
                    | skills::SkillsAction::Update(_)),
                ) => match &skills_home {
                    Some(home) => skills::start_install(
                        home.clone(),
                        action,
                        ask_tx.clone(),
                        skills_tx.clone(),
                    ),
                    None => state.show_skills(Err(NO_SKILLS_HOME.to_string())),
                },
                Effect::Skills(skills::SkillsAction::Trust(name)) => match &skills_home {
                    Some(home) => {
                        skills::start_trust(home.clone(), name, ask_tx.clone(), skills_tx.clone())
                    }
                    None => state.show_skills(Err(NO_SKILLS_HOME.to_string())),
                },
                Effect::Skills(action) => {
                    let view = skills_home
                        .as_ref()
                        .ok_or_else(|| NO_SKILLS_HOME.to_string())
                        .and_then(|home| skills::apply(home, &action));
                    state.show_skills(view);
                    state.set_skill_names(skills::mentionable(
                        skills_home.as_ref(),
                        state.catalog.as_ref().and_then(|c| c.local_context),
                    ));
                    let team = state.catalog.as_ref().and_then(|c| c.team_skills);
                    if let (skills::SkillsAction::Show, Some(home), Some(limits)) =
                        (&action, &skills_home, team)
                    {
                        skills::start_team_sync(
                            http.clone(),
                            home.clone(),
                            limits,
                            skills_tx.clone(),
                        );
                    }
                }
                Effect::StopSubagents => launcher.stop_all(),
                Effect::Cancel => {
                    let canceller = canceller.clone();
                    tokio::spawn(async move { canceller.cancel().await.ok() });
                }
                Effect::Steer => {
                    if let Some(steering) = state.steering.clone() {
                        let tx = steer_tx.clone();
                        tokio::spawn(async move {
                            tx.send(steering.deliver().await).ok();
                        });
                    }
                }
                Effect::SetSubagents(enabled) => {
                    if let Err(err) = crate::cmd::code::remember_subagents(profile, enabled) {
                        state.record(Item::Error(format!(
                            "cannot save the subagent setting: {err:#}"
                        )));
                    }
                    commands.send(Command::SetSubagents(enabled)).ok();
                }
                Effect::SetSubagentSettings(settings) => {
                    if let Err(err) =
                        crate::cmd::code::remember_subagent_settings(profile, &settings)
                    {
                        state.record(Item::Error(format!(
                            "cannot save the subagent settings: {err:#}"
                        )));
                    }
                    commands.send(Command::SetSubagentSettings(settings)).ok();
                }
                Effect::SetAutoCompact(tokens) => {
                    if let Err(err) = crate::cmd::code::remember_auto_compact(profile, tokens) {
                        state.record(Item::Error(format!(
                            "cannot save the auto-compact setting: {err:#}"
                        )));
                    }
                    commands.send(Command::SetAutoCompact(tokens)).ok();
                }
                Effect::Compact(focus) => {
                    let chat = conversation.lock().map(|c| c.clone()).unwrap_or_default();
                    let (http, tx) = (http.clone(), compact_tx.clone());
                    tokio::spawn(async move {
                        let reply = compaction::compact(&http, &chat, focus.as_deref())
                            .await
                            .map_err(|err| format!("{err:#}"));
                        tx.send(reply).ok();
                    });
                }
                Effect::Permissions(revoke) => {
                    let root = state.sandbox.as_ref().map(|s| s.cwd());
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
                Effect::FetchBudget => {
                    let (http, tx) = (http.clone(), budget_tx.clone());
                    tokio::spawn(async move {
                        let answer = crate::cmd::usage::fetch_budget(&http)
                            .await
                            .map_err(|err| format!("{err:#}"));
                        tx.send(answer).ok();
                    });
                }
                Effect::SetCaps(caps) => {
                    if let Err(err) = crate::cmd::code::remember_caps(profile, &caps) {
                        state.record(Item::Error(format!("cannot save your caps: {err:#}")));
                    }
                }
                Effect::NewConversation => {
                    commands.send(Command::NewConversation).ok();
                }
                Effect::ListConversations => {
                    let root = state.sandbox.as_ref().map(|s| s.start());
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
                Effect::Folder(command) => {
                    commands.send(Command::Folder(command)).ok();
                }
                Effect::SetMemory(on) => {
                    if let Err(err) = crate::cmd::code::remember_memory(profile, on) {
                        state.record(Item::Error(format!(
                            "cannot save the memory setting: {err:#}"
                        )));
                    }
                    commands.send(Command::SetMemory(on)).ok();
                }
                Effect::EditInstructions(path) => {
                    if let Some(term) = terminal.take() {
                        drop(term);
                    }
                    height = 0;
                    let edited = edit_in_editor(&path).await;
                    state.edited_instructions(&path, edited.map_err(|err| format!("{err:#}")));
                }
                Effect::Exit => {
                    if let Some(term) = terminal.as_mut() {
                        term.clear().ok();
                    }
                    launcher.stop_on_exit().await;
                    return Ok(());
                }
            }
        }
    }
}

/// Opens `path` in `$VISUAL`/`$EDITOR` (creating it if missing) with the
/// terminal handed over, and takes it back afterwards.
async fn edit_in_editor(path: &std::path::Path) -> Result<()> {
    if !path.exists() {
        crate::assistant::private_file::write_private(path, "")?;
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| if cfg!(windows) { "notepad" } else { "vi" }.to_string());
    // `code -w` style values carry arguments; they are split, never run
    // through a shell.
    let mut words = editor.split_whitespace();
    let program = words.next().context("no editor is set; set $EDITOR")?;
    terminal::disable_raw_mode().ok();
    execute!(stdout(), cursor::Show).ok();
    let status = tokio::process::Command::new(program)
        .args(words)
        .arg(path)
        .status()
        .await;
    terminal::enable_raw_mode().context("cannot take the terminal back")?;
    let status = status.with_context(|| format!("cannot start the editor '{program}'"))?;
    if !status.success() {
        anyhow::bail!("the editor '{program}' exited with {status}");
    }
    Ok(())
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
