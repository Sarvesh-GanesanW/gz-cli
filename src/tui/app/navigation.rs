//! Moving around: the sidebar, opening resources, drilling down and back,
//! and the palette's actions.

use crate::tui::chat::Conversation;
use crate::tui::fetch::FetchTarget;
use crate::tui::model::Grid;
use crate::tui::nav::{self, ResourceId, View};
use crate::tui::palette::Action;

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

    pub(super) fn open_and_focus(&mut self, id: ResourceId) {
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
        let Some(spec) = self.spec() else { return };
        let resource = self.active;
        let Some(view) = self.grids.get(&resource) else {
            return;
        };
        let depth = view.depth();
        let ids = view.trail_ids();
        let source = if depth == 0 {
            spec.source
        } else {
            spec.drills[depth - 1].source
        };
        let id = self.fetch(FetchTarget::Grid(resource), &source, &ids);
        if let Some(view) = self.grids.get_mut(&resource) {
            view.awaiting = Some(id);
            view.error = None;
        }
        self.set_status(Tone::Info, "Loading…");
    }

    pub(super) fn move_cursor(&mut self, delta: isize) {
        let rows = self.sidebar_rows();
        let last = rows.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
        // Resources preview as the cursor lands on them.
        if let SidebarRow::Resource(id) = rows[self.cursor] {
            self.open(id);
        }
    }

    pub(super) fn sidebar_enter(&mut self) {
        match self.sidebar_rows()[self.cursor] {
            SidebarRow::Module(m) if !self.expanded[m] => self.expanded[m] = true,
            SidebarRow::Module(m) => {
                self.cursor += 1;
                self.open_and_focus(ResourceId {
                    module: m,
                    resource: 0,
                });
            }
            SidebarRow::Resource(id) => self.open_and_focus(id),
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
            let Some(id) = nav::row_id(&row, drill.id_keys) else {
                self.set_status(Tone::Error, format!("Select a {} first", drill.noun));
                return;
            };
            let label = if id.is_empty() {
                "(root)".to_string()
            } else {
                id.clone()
            };
            if let Some(view) = self.view_mut() {
                view.levels.push(Level {
                    label,
                    id,
                    grid: Grid::default(),
                });
            }
            self.reload();
            return;
        }
        let mut ids = view.trail_ids();
        match (spec.detail, nav::row_id(&row, nav::ID_KEYS)) {
            (Some(detail), Some(id)) => {
                let title = format!("{} · {id}", self.active.resource().label);
                ids.push(id);
                let req = self.fetch(FetchTarget::Detail { title }, &detail, &ids);
                self.awaiting_modal = Some(req);
                self.set_status(Tone::Info, "Loading details…");
            }
            _ => {
                let title = format!("{} · row", self.active.resource().label);
                self.modal = Some(Modal::json(title, &row));
            }
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
        } else {
            self.focus = Focus::Sidebar;
        }
    }

    pub(super) fn run_action(&mut self, action: Action) {
        match action {
            Action::Open(id) => self.open_and_focus(id),
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
