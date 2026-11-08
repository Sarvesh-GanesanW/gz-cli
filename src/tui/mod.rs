pub mod app;
pub mod chat;
pub mod clipboard;
pub mod clock;
pub mod compose;
pub mod errors;
pub mod fetch;
pub mod fields;
pub mod format;
pub mod help;
pub mod input;
pub mod markdown;
pub mod model;
pub mod nav;
pub mod notify;
pub mod overlay;
pub mod palette;
pub mod saved_views;
pub mod screen;
pub mod session;
pub mod shape;
pub mod specs;
pub mod template;
#[cfg(test)]
pub mod test_stub;
pub mod theme;
pub mod ui;
pub mod ui_state;
pub mod views;
pub mod watch;

use std::io::stdout;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    cursor,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::cmd::Runtime;
use crate::tui::app::{App, Tone};
use crate::tui::fetch::{Executor, Inbound};
use crate::tui::input::{Input, Reader};
use crate::tui::ui_state::UiState;
use crate::tui::views::HandOff;

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen);
}

fn take_terminal() -> Result<()> {
    enable_raw_mode().context("cannot enter raw mode (need a TTY)")?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)
        .context("cannot enter alternate screen")
}

/// Steps aside for `work`: pauses the key reader, gives the terminal back
/// in its normal state, runs the work, then takes the terminal again. A
/// failure is left on screen until Enter, then summed up in the status bar.
async fn hand_off(rt: &Runtime, reader: &Reader, work: HandOff) -> Result<(Tone, String)> {
    let dir = std::env::current_dir().context("cannot read the current folder")?;
    reader.pause()?;
    restore_terminal();
    execute!(stdout(), cursor::Show).ok();
    let outcome = match work {
        HandOff::FixRun { project, job, run } => {
            let request = crate::cmd::job_fix::FixRequest {
                job: job.to_string(),
                project,
                run,
                dir,
                rerun: Default::default(),
            };
            crate::cmd::job_fix::fix(rt, &request).await
        }
    };
    let summary = match &outcome {
        Ok(()) => (Tone::Ok, "Back from gz code".to_string()),
        Err(err) => {
            eprintln!("gz: {err:#}");
            crate::cmd::prompt_line("Press Enter to return to gz tui ").ok();
            (Tone::Error, format!("{err:#}"))
        }
    };
    take_terminal()?;
    reader.resume();
    Ok(summary)
}

pub async fn run(rt: &Runtime) -> Result<()> {
    take_terminal()?;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous_hook(info);
    }));

    let result = event_loop(rt).await;
    restore_terminal();
    result
}

async fn event_loop(rt: &Runtime) -> Result<()> {
    let mut terminal =
        Terminal::new(CrosstermBackend::new(stdout())).context("cannot open terminal")?;
    terminal.clear().ok();

    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let reader = Reader::spawn(input_tx);
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::unbounded_channel::<Inbound>();
    let mut executor = Executor::new(rt.http.clone(), inbound_tx);
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let mut app = App::new(rt.http.resolved().clone());
    app.use_saved_views(saved_views::SavedViews::open());
    let state_path = UiState::path().ok();
    app.restore(
        state_path
            .as_deref()
            .map(UiState::load_from)
            .unwrap_or_default(),
    );
    session::set_admin(session::admin_in(rt.http.resolved()));

    while !app.quit {
        for request in app.outbox.drain(..) {
            executor.run(request);
        }
        terminal
            .draw(|frame| ui::render(&mut app, frame))
            .context("cannot draw")?;
        tokio::select! {
            Some(input) = input_rx.recv() => match input {
                Input::Key(key) => app.on_key(key),
                Input::Mouse(mouse) => app.on_mouse(mouse),
                Input::Resize => {}
            },
            Some(inbound) = inbound_rx.recv() => match inbound {
                Inbound::Fetched { id, target, result } => app.fetched(id, target, result),
                Inbound::Chat { tag, update } => app.chat_update(tag, update),
                Inbound::Streamed { id, view, item } => app.view_streamed(view, id, item),
            },
            _ = ticker.tick() => app.on_tick(crate::tui::clock::Now::system()),
        }
        if let Some(work) = app.hand_off.take() {
            let (tone, text) = hand_off(rt, &reader, work).await?;
            terminal.clear().ok();
            app.set_status(tone, text);
        }
    }
    if let Some(path) = state_path {
        // Losing the layout is harmless; say so without failing the exit.
        if let Err(err) = app.snapshot().save_to(&path) {
            eprintln!("gz: couldn't remember the TUI layout: {err:#}");
        }
    }
    Ok(())
}
