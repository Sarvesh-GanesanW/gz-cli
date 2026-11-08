//! Moving around: the sidebar, opening resources, drilling down and back,
//! and the palette's actions.

use crate::tui::chat::Conversation;
use crate::tui::fetch::FetchTarget;
use crate::tui::model::Grid;
use crate::tui::nav::{self, ResourceId, View};
use crate::tui::palette::Action;
use crate::tui::template::Scope;
use crate::tui::views::detail::DetailView;
use crate::tui::views::search::SearchView;

use super::{App, Focus, GridView, Level, Modal, SidebarRow, Tone, CHAT};

impl App {
    /// Shows a resource, loading it the first time.
    pub(super) fn open(&mut self, id: ResourceId) {
        self.active = id;
        self.editing_filter = false;
        match id.resource().view {
            View::Chat => {
                if !self.chat.list_loaded && self.awaiting_chat_list.is_none() {
                    self.load_chat_list();
                }
            }
            View::Grid(_) => {
                let view = self
                    .grids
                    .entry(id)
                    .or_insert_with(|| GridView::new(id.resource().label));
                if !view.loaded && !view.is_loading() {
                    self.reload();
                }
            }
        }
    }

    pub(crate) fn open_and_focus(&mut self, id: ResourceId) {
        self.expanded[id.module] = true;
        if let Some(row) = self.row_of(id) {
            self.cursor = row;
        }
        self.open(id);
        self.focus = Focus::Content;
    }

    /// Refetches what the current view shows.
    pub(super) fn reload(&mut self) {
        if self.is_chat() {
            self.load_chat_list();
            if let Some(open) = self.chat.open.clone() {
                self.load_transcript(&open.id);
            }
            return;
        }
        self.reload_resource(self.active);
    }

    /// Refetches the rows a grid resource shows at its current depth.
    pub(super) fn reload_resource(&mut self, resource: ResourceId) {
        let Some(spec) = resource.grid_spec() else {
            return;
        };
        let Some(view) = self.grids.get(&resource) else {
            return;
        };
        let source = spec.source_at(view.depth());
        let ids = view.trail_ids();
        let rows = view.trail_rows();
        let site = self.resolved.site();
        let mut scope = Scope::with_ids(&ids);
        for row in rows.iter().rev() {
            scope = scope.nearest(row);
        }
        if let Some(site) = site {
            scope = scope.var("site", site);
        }
        let resolved = source.resolve(&scope);
        let (path, body) = match resolved {
            Ok(resolved) => resolved,
            Err(err) => {
                let message = format!("Can't load this view: {err}");
                if let Some(view) = self.grids.get_mut(&resource) {
                    view.error = Some(message.clone());
                }
                return self.set_status(Tone::Error, message);
            }
        };
        let id = self.fetch_resolved(
            FetchTarget::Grid(resource),
            source.service,
            source.verb.method(),
            path,
            body,
        );
        let now = self.now.instant;
        if let Some(view) = self.grids.get_mut(&resource) {
            view.awaiting = Some(id);
            view.error = None;
            if let Some(poller) = view.poller.as_mut() {
                poller.restart(now);
            }
        }
        if resource == self.active {
            self.set_status(Tone::Info, "Loading…");
        }
    }

    pub(super) fn move_cursor(&mut self, delta: isize) {
        let rows = self.sidebar_rows();
        let last = rows.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
        // Resources preview as the cursor lands on them.
        if let SidebarRow::Resource(id) = rows[self.cursor] {
            self.forget_history();
            self.open(id);
        }
    }

    pub(super) fn sidebar_enter(&mut self) {
        self.forget_history();
        match self.sidebar_rows()[self.cursor] {
            SidebarRow::Module(m) if !self.expanded[m] => self.expanded[m] = true,
            SidebarRow::Module(m) => {
                self.cursor += 1;
                self.open_and_focus(ResourceId {
                    module: m,
                    resource: 0,
                });
            }
            SidebarRow::Resource(id) => {
                self.open_and_focus(id);
                self.remember_resource(id);
            }
        }
    }

    pub(super) fn sidebar_left(&mut self) {
        match self.sidebar_rows()[self.cursor] {
            SidebarRow::Module(m) => self.expanded[m] = false,
            SidebarRow::Resource(id) => {
                if let Some(row) = self
                    .sidebar_rows()
                    .iter()
                    .position(|r| *r == SidebarRow::Module(id.module))
                {
                    self.cursor = row;
                }
            }
        }
    }

    pub(super) fn toggle_module(&mut self) {
        if let SidebarRow::Module(m) = self.sidebar_rows()[self.cursor] {
            self.expanded[m] = !self.expanded[m];
        }
    }

    /// Enter on a grid row: drill a level deeper, or open its details.
    pub(super) fn open_row(&mut self) {
        let Some(spec) = self.spec() else { return };
        let Some(view) = self.view() else { return };
        let depth = view.depth();
        let Some(row) = view.grid().selected_raw().cloned() else {
            self.set_status(Tone::Error, "Nothing selected");
            return;
        };
        if let Some(drill) = spec.drills.get(depth) {
            let screen = spec.screen_at(depth);
            let id = match screen {
                Some(screen) => screen.identity.id_of(&row),
                None => nav::row_id(&row, drill.id_keys),
            };
            let Some(id) = id else {
                self.set_status(Tone::Error, format!("Select a {} first", drill.noun));
                return;
            };
            let label = match screen.and_then(|s| s.identity.title_of(&row)) {
                Some(title) => title,
                None if id.is_empty() => "(root)".to_string(),
                None => id.clone(),
            };
            if let Some(view) = self.view_mut() {
                view.levels.push(Level {
                    label,
                    id,
                    row,
                    grid: Grid::default(),
                });
            }
            self.remember_here(None);
            self.reload();
            return;
        }
        self.open_details();
    }

    /// The selected row's details: the spec's detail view, the resource's
    /// detail endpoint as JSON, or the raw row.
    pub(super) fn open_details(&mut self) {
        let Some(row) = self.row_context() else {
            self.set_status(Tone::Error, "Nothing selected");
            return;
        };
        let item = row
            .title
            .clone()
            .or_else(|| nav::row_id(&row.row, nav::ID_KEYS));
        self.remember_here(item);
        if let Some(view) = row.spec.and_then(|spec| DetailView::new(spec, row.clone())) {
            self.push_view(Box::new(view));
            return;
        }
        let Some(spec) = self.spec() else { return };
        let label = self.active.resource().label;
        match (spec.detail, nav::row_id(&row.row, nav::ID_KEYS)) {
            (Some(detail), Some(id)) => {
                let title = format!("{label} · {id}");
                if let Some(req) = self.fetch(FetchTarget::Detail { title }, &detail, &row.ids) {
                    self.awaiting_modal = Some(req);
                    self.set_status(Tone::Info, "Loading details…");
                }
            }
            _ => self.modal = Some(Modal::json(format!("{label} · row"), &row.row)),
        }
    }

    /// One level up; at the root, back to the sidebar.
    pub(super) fn back(&mut self) {
        let popped = self
            .view_mut()
            .filter(|v| v.depth() > 0)
            .map(|v| v.levels.pop())
            .is_some();
        if popped {
            if let Some(view) = self.view_mut() {
                view.awaiting = None;
                view.error = None;
            }
            self.set_status(Tone::Info, self.breadcrumb().join(" › "));
        } else if !self.back_in_history() {
            self.focus = Focus::Sidebar;
        }
    }

    /// Global search over everything, in front of what is open.
    pub(super) fn open_search(&mut self, query: &str) {
        self.push_view(Box::new(SearchView::new(query)));
    }

    pub(crate) fn run_action(&mut self, action: Action) {
        match action {
            Action::Open(id) => {
                self.push_history();
                self.views.clear();
                self.open_and_focus(id);
                self.remember_resource(id);
            }
            Action::Go(place) => self.go_to(&place),
            Action::Search(query) => self.open_search(&query),
            Action::OpenConversation { id, title } => {
                self.open_and_focus(CHAT);
                self.open_conversation(Conversation { id, title });
            }
            Action::NewChat => self.new_chat(),
            Action::Reload => self.reload(),
            Action::Help => self.help = true,
            Action::SignOutHint => self.set_status(
                Tone::Info,
                "To sign out, quit and run `gz auth logout` (then `gz auth login` to switch users).",
            ),
            Action::Quit => self.quit = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    use crossterm::event::KeyCode;

    use crate::tui::nav::MODULES;

    use serde_json::json;

    #[test]
    fn sidebar_expands_collapses_and_previews() {
        let mut app = app();
        let rows_before = app.sidebar_rows().len();
        // Down past Chat's resources to the Lakehouse module, then expand it.
        while app.sidebar_rows()[app.cursor] != SidebarRow::Module(1) {
            press(&mut app, &[KeyCode::Down]);
        }
        press(&mut app, &[KeyCode::Right]);
        assert!(app.expanded[1]);
        assert_eq!(
            app.sidebar_rows().len(),
            rows_before + MODULES[1].resources.len()
        );
        press(&mut app, &[KeyCode::Down]);
        assert_eq!(app.active, resource("Lakehouse", "Warehouses"));
        assert_eq!(last_fetch(&app).path, "/lakehouse/warehouse");
        press(&mut app, &[KeyCode::Left, KeyCode::Left]);
        assert!(!app.expanded[1]);
    }

    #[test]
    fn drills_down_and_back_without_refetching() {
        let mut app = app();
        app.open_and_focus(resource("Lakehouse", "Warehouses"));
        answer(
            &mut app,
            json!({"status": 200, "response": [{"name": "wh1"}, {"name": "wh2"}]}),
        );
        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
        assert_eq!(last_fetch(&app).path, "/lakehouse/warehouse/wh2/namespaces");
        answer(&mut app, json!([{"name": "sales"}]));
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(last_fetch(&app).path, "/lakehouse/schema/wh2/sales");
        assert_eq!(
            app.breadcrumb(),
            vec!["Lakehouse", "Warehouses", "wh2", "sales"]
        );
        let fetches = app.outbox.len();
        press(&mut app, &[KeyCode::Backspace, KeyCode::Backspace]);
        assert_eq!(app.outbox.len(), fetches);
        assert_eq!(app.view().unwrap().grid().selected, 1);
        press(&mut app, &[KeyCode::Backspace]);
        assert_eq!(app.focus, Focus::Sidebar);
    }
}
