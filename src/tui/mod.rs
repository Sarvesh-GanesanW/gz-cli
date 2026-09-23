pub mod app;
pub mod fetch;
pub mod model;
pub mod ui;

use std::io::stdout;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::cmd::Runtime;
use crate::tui::app::App;

pub async fn run(rt: &Runtime) -> Result<()> {
    enable_raw_mode().context("cannot enter raw mode (need a TTY)")?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).context("cannot enter alternate screen")?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
        hook(info);
    }));

    let result = event_loop(rt).await;

    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen);
    let _ = std::panic::take_hook();
    result
}

async fn event_loop(rt: &Runtime) -> Result<()> {
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("cannot open terminal")?;
    terminal.clear().ok();

    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if let Event::Key(key) = event {
                if input_tx.send(key).is_err() {
                    break;
                }
            }
        }
    });
    let (fetch_tx, mut fetch_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::tui::fetch::FetchDone>();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));

    let mut app = App::new(rt.http.resolved().clone());
    let client = rt.http.clone_client();

    loop {
        terminal
            .draw(|frame| ui::render(&mut app, frame))
            .context("cannot draw")?;
        tokio::select! {
            Some(key) = input_rx.recv() => app.on_key(key),
            Some(done) = fetch_rx.recv() => app.fetched(done.id, done.target, done.label, done.result),
            _ = ticker.tick() => app.on_tick(),
        }
        for req in app.outbox.drain(..) {
            fetch::spawn(&client, req, fetch_tx.clone());
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}
