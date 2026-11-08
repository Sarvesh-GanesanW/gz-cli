//! Requests the TUI sends in the background and how their answers land.

use reqwest::Method;
use serde_json::Value;

use crate::tui::errors::{self, Subject};
use crate::tui::fetch::{FetchReq, FetchTarget, Request};
use crate::tui::nav::{ResourceId, Source};
use crate::tui::template::Scope;

use super::{App, Modal, Tone};

impl App {
    pub(super) fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Sends `source` filled with positional `ids`; None (and a message)
    /// when the template can't be filled.
    pub(super) fn fetch(
        &mut self,
        target: FetchTarget,
        source: &Source,
        ids: &[String],
    ) -> Option<u64> {
        match source.resolve(&Scope::with_ids(ids)) {
            Ok((path, body)) => {
                Some(self.fetch_resolved(target, source.service, source.verb.method(), path, body))
            }
            Err(err) => {
                self.set_status(Tone::Error, format!("Can't load this: {err}"));
                None
            }
        }
    }

    pub(super) fn fetch_resolved(
        &mut self,
        target: FetchTarget,
        service: &str,
        method: Method,
        path: String,
        body: Option<Value>,
    ) -> u64 {
        let id = self.next_id();
        self.outbox.push(Request::Fetch(FetchReq {
            id,
            target,
            service: service.to_string(),
            method,
            path,
            body,
        }));
        id
    }

    pub(super) fn fetch_get(&mut self, target: FetchTarget, service: &str, path: String) -> u64 {
        self.fetch_resolved(target, service, Method::GET, path, None)
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
            FetchTarget::View(view) => self.view_fetched(view, id, result),
            FetchTarget::Action(done) => self.action_finished(done, result),
            FetchTarget::FormChoices => self.form_choices_fetched(id, result),
            FetchTarget::Watch(feed) => self.watch_fetched(feed, id, result),
            FetchTarget::Background => {
                if let Err(err) = result {
                    self.set_status(Tone::Error, self.explain(&err, "this"));
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
        let spec = resource.grid_spec();
        let screen = spec.and_then(|s| s.screen_at(view.depth()));
        match result {
            Ok(value) => {
                view.loaded = true;
                view.error = None;
                let filter = view.grid().filter.clone();
                let selected = view.grid().selected;
                let sorted = view.grid().sorted;
                let grid = view.grid_mut();
                grid.load(&value, screen, self.now.epoch_ms);
                if let Some((col, descending)) = sorted {
                    grid.sort_by_column(col, descending);
                }
                grid.set_filter(&filter);
                // A poll or refresh keeps the user's place.
                grid.selected = selected.min(grid.visible_len().saturating_sub(1));
                let rows = grid.visible_len();
                if resource == self.active {
                    self.set_status(Tone::Ok, format!("Loaded {rows} row(s)"));
                }
                self.apply_pending(resource);
            }
            Err(err) => {
                let what = screen.map_or_else(
                    || resource.resource().label.to_lowercase(),
                    |s| format!("these {}s", s.noun),
                );
                let service = spec.map_or("", |s| s.source.service);
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
