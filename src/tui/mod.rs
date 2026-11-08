pub mod app;
pub mod chat;
pub mod fetch;
pub mod markdown;
pub mod model;
pub mod nav;
pub mod palette;
pub mod specs;
pub mod theme;
pub mod ui;

use std::io::stdout;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::cmd::Runtime;
use crate::tui::app::App;
use crate::tui::fetch::{Executor, Inbound};

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen);
}

pub async fn run(rt: &Runtime) -> Result<()> {
    enable_raw_mode().context("cannot enter raw mode (need a TTY)")?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)
        .context("cannot enter alternate screen")?;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous_hook(info);
    }));

    let result = event_loop(rt).await;
    restore_terminal();
    result
}

/// Terminal input the app reacts to.
enum Input {
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Resize,
}

async fn event_loop(rt: &Runtime) -> Result<()> {
    let mut terminal =
        Terminal::new(CrosstermBackend::new(stdout())).context("cannot open terminal")?;
    terminal.clear().ok();

    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            let input = match event {
                // Windows reports key releases too; act on presses only.
                Event::Key(key) if key.kind != KeyEventKind::Release => Input::Key(key),
                Event::Mouse(mouse) => Input::Mouse(mouse),
                Event::Resize(..) => Input::Resize,
                _ => continue,
            };
            if input_tx.send(input).is_err() {
                break;
            }
        }
    });
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::unbounded_channel::<Inbound>();
    let mut executor = Executor::new(rt.http.clone(), inbound_tx);
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let mut app = App::new(rt.http.resolved().clone());

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
            },
            _ = ticker.tick() => app.on_tick(),
        }
    }
    Ok(())
}
