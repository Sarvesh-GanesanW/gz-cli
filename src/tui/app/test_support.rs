//! Helpers the app tests share.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use crate::config::Config;
use crate::tui::fetch::{FetchReq, Request};
use crate::tui::nav::{self, ResourceId};

use super::App;

pub(super) fn app() -> App {
    let config = Config::default();
    let resolved = crate::config::resolve(&config, Some("t"), None, None);
    App::new(resolved)
}

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(super) fn press(app: &mut App, codes: &[KeyCode]) {
    for code in codes {
        app.on_key(key(*code));
    }
}

pub(super) fn last_fetch(app: &App) -> FetchReq {
    app.outbox
        .iter()
        .rev()
        .find_map(|r| match r {
            Request::Fetch(f) => Some(f.clone()),
            _ => None,
        })
        .unwrap()
}

pub(super) fn answer(app: &mut App, value: Value) {
    let req = last_fetch(app);
    app.fetched(req.id, req.target, Ok(value));
}

pub(super) fn resource(module: &str, label: &str) -> ResourceId {
    nav::all_resources()
        .find(|id| id.module().title == module && id.resource().label == label)
        .unwrap()
}
