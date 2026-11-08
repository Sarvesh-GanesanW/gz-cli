//! Carrying the UI state across launches: snapshotting where the user is
//! and how each table is laid out, and putting it back on the next start.

use crate::tui::nav::{self, ResourceId};
use crate::tui::ui_state::{Place, ScreenState, SortState, Start, UiState};
use crate::tui::views::home::HOME_TITLE;

use super::App;

impl App {
    /// Reopens the remembered place, then the dashboard unless the user
    /// left from somewhere else last time.
    pub fn restore(&mut self, state: UiState) {
        let last = state
            .last
            .as_ref()
            .and_then(|place| nav::find_resource(&place.module, &place.resource));
        let start = state.start;
        self.remembered = state;
        if let Some(resource) = last {
            self.open_and_focus(resource);
        }
        if start == Start::Home {
            self.open_home();
        }
    }

    /// What to remember: the place on screen, whether the dashboard is in
    /// front, and every visited table's sort and column scroll (screens not
    /// visited this time keep what they had).
    pub fn snapshot(&self) -> UiState {
        let mut state = self.remembered.clone();
        state.last = Some(Place {
            module: self.active.module().title.to_string(),
            resource: self.active.resource().label.to_string(),
        });
        let on_home = self.views.titles().last().map(String::as_str) == Some(HOME_TITLE);
        state.start = if on_home { Start::Home } else { Start::Last };
        for (resource, view) in &self.grids {
            let Some(root) = view.levels.first().filter(|_| view.loaded) else {
                continue;
            };
            let grid = &root.grid;
            let screen = ScreenState {
                sort: grid.sorted.and_then(|(col, descending)| {
                    Some(SortState {
                        column: grid.cols.get(col)?.clone(),
                        descending,
                    })
                }),
                col_offset: grid.col_offset,
            };
            state.set_screen(resource.title(), screen);
        }
        state
    }

    /// Puts a table's remembered sort and column scroll back the first time
    /// its rows arrive.
    pub(super) fn restore_layout(&mut self, resource: ResourceId) {
        let Some(saved) = self.remembered.screens.get(&resource.title()).cloned() else {
            return;
        };
        let Some(view) = self.grids.get_mut(&resource) else {
            return;
        };
        let grid = &mut view.levels[0].grid;
        if let Some(sort) = saved.sort {
            if let Some(col) = grid.cols.iter().position(|c| *c == sort.column) {
                grid.sort_by_column(col, sort.descending);
            }
        }
        grid.col_offset = saved.col_offset.min(grid.cols.len().saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;
    use serde_json::json;

    use super::*;
    use crate::tui::app::test_support::*;

    fn warehouses() -> serde_json::Value {
        json!({"status": 200, "response": [{"name": "alpha"}, {"name": "gamma"}, {"name": "beta"}]})
    }

    #[test]
    fn a_fresh_start_opens_on_home() {
        let mut app = app();
        app.restore(UiState::default());
        assert_eq!(app.breadcrumb().last().unwrap(), HOME_TITLE);
    }

    #[test]
    fn place_dashboard_and_sort_come_back_next_launch() {
        let mut app = app();
        app.open_and_focus(resource("Lakehouse", "Warehouses"));
        answer(&mut app, warehouses());
        press(&mut app, &[KeyCode::Char('o'), KeyCode::Char('O')]);
        let saved = app.snapshot();
        assert_eq!(saved.start, Start::Last, "left from a table, not home");
        assert_eq!(
            saved.screens["Lakehouse › Warehouses"].sort,
            Some(SortState {
                column: "Warehouse".into(),
                descending: true
            })
        );

        let mut next = app_with(saved);
        assert_eq!(next.active, resource("Lakehouse", "Warehouses"));
        assert!(next.views.is_empty());
        answer(&mut next, warehouses());
        let first = &next.view().unwrap().grid().rows[0][0];
        assert_eq!(first, "gamma", "sorted descending as last time");
    }

    #[test]
    fn leaving_from_home_reopens_home() {
        let mut app = app();
        app.open_home();
        let saved = app.snapshot();
        assert_eq!(saved.start, Start::Home);
        let next = app_with(saved);
        assert_eq!(next.breadcrumb().last().unwrap(), HOME_TITLE);
    }

    #[test]
    fn places_that_no_longer_exist_are_ignored() {
        let state = UiState {
            last: Some(Place {
                module: "Gone".into(),
                resource: "Away".into(),
            }),
            start: Start::Last,
            ..UiState::default()
        };
        let app = app_with(state);
        assert!(app.is_chat(), "the usual first screen");
    }

    fn app_with(state: UiState) -> App {
        let mut next = app();
        next.restore(state);
        next
    }
}
