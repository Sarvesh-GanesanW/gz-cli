//! Requests the TUI sends in the background and how their answers land.

use serde_json::Value;

use crate::tui::errors::{self, Subject};
use crate::tui::fetch::{FetchReq, FetchTarget, Request};
use crate::tui::nav::{ResourceId, Source};

use super::{App, Modal, Tone};

impl App {
    pub(super) fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub(super) fn fetch(&mut self, target: FetchTarget, source: &Source, ids: &[String]) -> u64 {
        let id = self.next_id();
        let (path, body) = source.resolve(ids);
        self.outbox.push(Request::Fetch(FetchReq {
            id,
            target,
            service: source.service.to_string(),
            method: source.verb.method(),
            path,
            body,
        }));
        id
    }

    pub(super) fn fetch_get(&mut self, target: FetchTarget, service: &str, path: String) -> u64 {
        let id = self.next_id();
        self.outbox.push(Request::Fetch(FetchReq {
            id,
            target,
            service: service.to_string(),
            method: reqwest::Method::GET,
            path,
            body: None,
        }));
        id
    }

    pub fn fetched(&mut self, id: u64, target: FetchTarget, result: Result<Value, String>) {
        match target {
            FetchTarget::Grid(resource) => self.grid_fetched(resource, id, result),
            FetchTarget::Detail { title } => {
                if self.awaiting_modal != Some(id) {
                    return;
                }
                self.awaiting_modal = None;
                match result {
                    Ok(value) => self.modal = Some(Modal::json(title, &value)),
                    Err(err) => {
                        let what = format!("this {}", self.active.resource().label.to_lowercase());
                        self.set_status(Tone::Error, self.explain(&err, &what))
                    }
                }
            }
            FetchTarget::ChatList => {
                if self.awaiting_chat_list != Some(id) {
                    return;
                }
                self.awaiting_chat_list = None;
                match result {
                    Ok(value) => {
                        self.chat.set_conversations(&value);
                        self.chat.error = None;
                    }
                    Err(err) => self.chat.error = Some(self.explain(&err, "your conversations")),
                }
            }
            FetchTarget::ChatTranscript { conversation_id } => {
                let still_open = self
                    .chat
                    .open
                    .as_ref()
                    .is_some_and(|c| c.id == conversation_id);
                if self.awaiting_transcript != Some(id) || !still_open {
                    return;
                }
                self.awaiting_transcript = None;
                match result {
                    Ok(value) => self.chat.set_transcript(&value),
                    Err(err) => self.chat.error = Some(self.explain(&err, "this conversation")),
                }
            }
        }
    }

    /// A failure in plain words, naming the active resource's service.
    fn explain(&self, err: &str, what: &str) -> String {
        let service = self.spec().map_or("chat", |s| s.source.service);
        errors::explain(err, Subject { what, service })
    }

    fn grid_fetched(&mut self, resource: ResourceId, id: u64, result: Result<Value, String>) {
        let Some(view) = self.grids.get_mut(&resource) else {
            return;
        };
        if view.awaiting != Some(id) {
            return;
        }
        view.awaiting = None;
        match result {
            Ok(value) => {
                view.loaded = true;
                view.error = None;
                let filter = view.grid().filter.clone();
                let grid = view.grid_mut();
                grid.replace(&value);
                grid.set_filter(&filter);
                let rows = grid.visible_len();
                if resource == self.active {
                    self.set_status(Tone::Ok, format!("Loaded {rows} row(s)"));
                }
            }
            Err(err) => {
                let what = resource.resource().label.to_lowercase();
                let service = match resource.resource().view {
                    crate::tui::nav::View::Grid(spec) => spec.source.service,
                    crate::tui::nav::View::Chat => "chat",
                };
                view.error = Some(errors::explain(
                    &err,
                    Subject {
                        what: &what,
                        service,
                    },
                ));
                if resource == self.active {
                    self.set_status(Tone::Error, "Request failed · r to retry");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    use crossterm::event::KeyCode;

    use serde_json::json;

    #[test]
    fn post_sources_carry_their_bodies() {
        let mut app = app();
        app.open_and_focus(resource("Files", "Storage"));
        answer(&mut app, json!({"data": [{"Name": "GZS3"}]}));
        press(&mut app, &[KeyCode::Enter]);
        let req = last_fetch(&app);
        assert_eq!(req.path, "/storage/listFolders");
        assert_eq!(req.body.unwrap()["ConnectionName"], "GZS3");
    }

    #[test]
    fn stale_answers_are_dropped() {
        let mut app = app();
        app.open_and_focus(resource("Designer", "Datasets"));
        let first = last_fetch(&app);
        press(&mut app, &[KeyCode::Char('r')]);
        app.fetched(first.id, first.target, Ok(json!([{"a": 1}])));
        assert!(app.view().unwrap().grid().is_empty());
        answer(&mut app, json!([{"a": 2}, {"a": 3}]));
        assert_eq!(app.view().unwrap().grid().visible_len(), 2);
    }

    #[test]
    fn errors_become_actionable_messages() {
        let mut app = app();
        app.open_and_focus(resource("Designer", "Datasets"));
        let req = last_fetch(&app);
        app.fetched(
            req.id,
            req.target,
            Err("GET x → HTTP 401 Unauthorized".into()),
        );
        assert!(app
            .view()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("gz auth login"));
        assert!(app.status.text.contains("retry"));
        assert_eq!(app.status.tone, Tone::Error);
    }
}
