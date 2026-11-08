//! Helpers the app, screen and view tests share.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use ratatui::{backend::TestBackend, Terminal};

use crate::config::Config;
use crate::tui::clock::Now;
use crate::tui::fetch::{FetchReq, FetchTarget, Request};
use crate::tui::nav::{self, ResourceId};

use super::App;

/// 2026-11-08T12:00:00Z, so relative times in tests never drift.
pub(crate) const TEST_NOW_MS: i64 = 1_794_139_200_000;

pub(crate) fn app() -> App {
    let config = Config::default();
    let resolved = crate::config::resolve(&config, Some("t"), None, None);
    let mut app = App::new(resolved);
    app.now = Now {
        epoch_ms: TEST_NOW_MS,
        ..app.now
    };
    app
}

/// The screen as text, one line per row, trailing spaces trimmed.
pub(crate) fn draw(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| crate::tui::ui::render(app, f)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(crate) fn press(app: &mut App, codes: &[KeyCode]) {
    for code in codes {
        app.on_key(key(*code));
    }
}

/// The newest request the app queued, leaving out the background news
/// check (`watch.rs`), which ticks on its own schedule.
pub(crate) fn last_fetch(app: &App) -> FetchReq {
    app.outbox
        .iter()
        .rev()
        .find_map(|r| match r {
            Request::Fetch(f) if !matches!(f.target, FetchTarget::Watch(_)) => Some(f.clone()),
            _ => None,
        })
        .unwrap()
}

pub(crate) fn answer(app: &mut App, value: Value) {
    let req = last_fetch(app);
    app.fetched(req.id, req.target, Ok(value));
}

pub(crate) fn resource(module: &str, label: &str) -> ResourceId {
    nav::find_resource(module, label).unwrap()
}

/// Selects the first row of the screen in front whose `field` reads `value`.
pub(crate) fn select_row(app: &mut App, field: crate::tui::fields::FieldPath, value: &str) {
    let grid = app.view_mut().unwrap().grid_mut();
    let index = grid.find_raw(field, value).unwrap();
    assert!(grid.select_raw(index));
}

/// Answers the newest request for `path` (one of several sent together).
pub(crate) fn answer_path(app: &mut App, path: &str, value: Value) {
    let req = app
        .outbox
        .iter()
        .rev()
        .find_map(|r| match r {
            Request::Fetch(f) if f.path == path => Some(f.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("nothing asked for {path}"));
    app.fetched(req.id, req.target, Ok(value));
}

/// The newest task the app queued (nothing is run).
pub(crate) fn last_task(app: &App) -> crate::tui::fetch::Task {
    app.outbox
        .iter()
        .rev()
        .find_map(|r| match r {
            Request::Task(t) => Some(t.clone()),
            _ => None,
        })
        .expect("a task should be queued")
}
