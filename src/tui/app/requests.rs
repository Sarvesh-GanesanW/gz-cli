//! Requests the TUI sends in the background and how their answers land.

use reqwest::Method;
use serde_json::Value;

use crate::tui::errors::{self, Subject};
use crate::tui::fetch::{FetchReq, FetchTarget, Request};
use crate::tui::format::counted;
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
                    Ok(value) => {
                        // "Loading details…" is over once they show.
                        self.set_status(Tone::Info, "");
                        self.modal = Some(Modal::json(title, &value));
                    }
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
            FetchTarget::TaskDone { resource, view } => self.task_done(resource, view, result),
        }
    }

    /// A row action's task finished: say how, and show what changed.
    fn task_done(
        &mut self,
        resource: Option<ResourceId>,
        view: Option<crate::tui::views::ViewId>,
        result: Result<Value, String>,
    ) {
        let message = match result {
            Ok(answer) => answer["message"].as_str().unwrap_or("Done").to_string(),
            // Task code words its own failures.
            Err(err) => return self.set_status(Tone::Error, err),
        };
        if let Some(view) = view {
            self.with_view(view, |v, ctx| v.refresh(ctx));
        }
        if let Some(resource) = resource {
            self.reload_resource(resource);
        }
        self.set_status(Tone::Ok, message);
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
        if view.compose.is_some() {
            return self.composed_fetched(resource, id, result);
        }
        if view.awaiting != Some(id) {
            return;
        }
        view.awaiting = None;
        let page = view.page_load.take();
        match result {
            Ok(value) => {
                let value = match page {
                    Some(page) => self.paged(resource, page, value),
                    None => value,
                };
                self.grid_loaded(resource, &value, &[]);
            }
            Err(err) => self.grid_failed(resource, err),
        }
    }

    /// Shows freshly loaded rows, keeping the user's sort, filter and place.
    /// `failures` name sources of a composed level that didn't load.
    pub(super) fn grid_loaded(&mut self, resource: ResourceId, value: &Value, failures: &[String]) {
        let screen = resource
            .grid_spec()
            .zip(self.grids.get(&resource))
            .and_then(|(spec, view)| spec.screen_at(view.depth()));
        let now_ms = self.now.epoch_ms;
        let Some(view) = self.grids.get_mut(&resource) else {
            return;
        };
        let first_root_load = !view.loaded && view.depth() == 0;
        view.loaded = true;
        view.error = None;
        let filter = view.grid().filter.clone();
        let selected = view.grid().selected;
        let sorted = view.grid().sorted;
        let grid = view.grid_mut();
        grid.load(value, screen, now_ms);
        if let Some((col, descending)) = sorted {
            grid.sort_by_column(col, descending);
        }
        grid.set_filter(&filter);
        // A poll or refresh keeps the user's place.
        grid.selected = selected.min(grid.visible_len().saturating_sub(1));
        let rows = grid.visible_len();
        if first_root_load {
            self.restore_layout(resource);
        }
        if resource == self.active {
            let loaded = format!("Loaded {}", counted(rows, "row"));
            if !failures.is_empty() {
                let status = format!("{loaded} · {}", failures.join(" · "));
                self.set_status(Tone::Error, status);
            } else if self.views.is_empty() && self.overlay.is_none() {
                // Rows that arrive under a view or menu are not its news.
                self.set_table_status(Tone::Ok, loaded);
            }
        }
        self.apply_pending(resource);
    }

    pub(super) fn grid_failed(&mut self, resource: ResourceId, err: String) {
        let spec = resource.grid_spec();
        let Some(view) = self.grids.get_mut(&resource) else {
            return;
        };
        let screen = spec.and_then(|s| s.screen_at(view.depth()));
        let what = screen.map_or_else(
            || resource.resource().label.to_lowercase(),
            |s| format!("these {}s", s.noun),
        );
        let service = spec.map_or("", |s| s.source_at(view.depth()).service);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    use crossterm::event::KeyCode;

    use serde_json::json;

    use crate::tui::fetch::Request;

    fn footer(app: &mut App) -> String {
        let screen = draw(app, 150, 42);
        screen.lines().last().unwrap().to_string()
    }

    fn connections(rows: Value) -> App {
        let mut app = app();
        app.open_and_focus(resource("Connections", "Connections"));
        answer(&mut app, rows);
        app
    }

    #[test]
    fn the_loaded_status_counts_rows_in_plain_words() {
        let mut one = connections(json!([{"Id": 1, "Name": "GZS3", "type": "AWSS3"}]));
        assert_eq!(one.status.text, "Loaded 1 row");
        assert!(footer(&mut one).contains("✓ Loaded 1 row"));
        let two = connections(json!([{"Id": 1, "Name": "a"}, {"Id": 2, "Name": "b"}]));
        assert_eq!(two.status.text, "Loaded 2 rows");
    }

    #[test]
    fn the_loaded_status_goes_when_something_covers_the_table() {
        let rows = json!([{"Id": 1, "Name": "GZS3", "type": "AWSS3"}]);
        for cover in ['H', 'i', 'a'] {
            let mut app = connections(rows.clone());
            press(&mut app, &[KeyCode::Char(cover)]);
            assert!(
                !app.views.is_empty() || app.overlay.is_some(),
                "{cover} opens something over the table"
            );
            let footer = footer(&mut app);
            assert!(!footer.contains("Loaded"), "after {cover}: {footer}");
            assert!(!footer.contains('✓'), "no marker without text: {footer}");
        }
    }

    #[test]
    fn rows_arriving_under_a_view_dont_report_in_its_status_line() {
        let mut app = app();
        app.open_and_focus(resource("Connections", "Connections"));
        let list = last_fetch(&app);
        press(&mut app, &[KeyCode::Char('H')]);
        app.fetched(list.id, list.target, Ok(json!([{"Id": 1, "Name": "a"}])));
        assert_eq!(app.view().unwrap().grid().visible_len(), 1);
        assert!(!footer(&mut app).contains("Loaded"));
    }

    #[test]
    fn other_statuses_stay_when_a_view_opens() {
        let mut app = connections(json!([{"Id": 1, "Name": "a"}]));
        app.set_status(Tone::Error, "Delete failed");
        press(&mut app, &[KeyCode::Char('H')]);
        assert_eq!(app.status.text, "Delete failed");
    }

    #[test]
    fn post_sources_carry_their_bodies() {
        let mut app = app();
        app.open_and_focus(resource("Files", "Storage"));
        answer(&mut app, json!({"data": [{"Name": "GZS3"}]}));
        press(&mut app, &[KeyCode::Enter]);
        let folders = app
            .outbox
            .iter()
            .find_map(|r| match r {
                Request::Fetch(f) if f.path == "/storage/listFolders" => Some(f.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(folders.body.unwrap()["ConnectionName"], "GZS3");
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
