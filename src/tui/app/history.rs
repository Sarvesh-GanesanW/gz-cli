//! Where the user has been and how to get back: places (a resource, the
//! rows drilled through, the row selected, its filter and sort), the Back
//! stack, and the Recent list.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::tui::fields;
use crate::tui::model::Grid;
use crate::tui::nav::{self, GridSpec, ResourceId};
use crate::tui::palette::{Action, Entry};

use super::{App, GridView, Pending, Tone};

/// Places Back can return to; older ones are dropped.
const HISTORY_LIMIT: usize = 50;
/// Screens and items the Recent list keeps.
pub(super) const RECENT_LIMIT: usize = 20;

/// One row to find on the way to a place: the first of `field` (dotted
/// paths, tried in order) whose text is `value`. `drill` opens the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub field: Vec<String>,
    pub value: String,
    pub drill: bool,
}

impl Step {
    pub fn select(field: &[&str], value: impl Into<String>) -> Self {
        Self {
            field: field.iter().map(|f| f.to_string()).collect(),
            value: value.into(),
            drill: false,
        }
    }

    pub fn drill(field: &[&str], value: impl Into<String>) -> Self {
        Self {
            drill: true,
            ..Self::select(field, value)
        }
    }

    /// The raw index of the first row this step points at.
    fn find(&self, grid: &Grid) -> Option<usize> {
        let field: Vec<&str> = self.field.iter().map(String::as_str).collect();
        grid.raw.iter().position(|row| {
            fields::lookup_any(row, &field)
                .and_then(fields::id_text)
                .is_some_and(|text| text == self.value)
        })
    }
}

/// A table sort by column header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortBy {
    pub column: String,
    pub descending: bool,
}

/// A place to reopen: a resource, then the steps to the rows shown and
/// selected, then the filter and sort to apply there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub resource: ResourceId,
    pub steps: Vec<Step>,
    pub filter: String,
    pub sort: Option<SortBy>,
}

impl Place {
    pub fn at(resource: ResourceId) -> Self {
        Self {
            resource,
            steps: Vec::new(),
            filter: String::new(),
            sort: None,
        }
    }

    fn drill_ids(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|s| s.drill)
            .map(|s| s.value.as_str())
            .collect()
    }
}

/// What a followed link, search hit or saved view still has to do once
/// rows arrive.
#[derive(Debug, Clone)]
pub struct Trail {
    steps: VecDeque<Step>,
    filter: String,
    sort: Option<SortBy>,
}

impl Trail {
    fn new(place: &Place) -> Self {
        Self {
            steps: place.steps.iter().cloned().collect(),
            filter: place.filter.clone(),
            sort: place.sort.clone(),
        }
    }
}

/// Fields that hold the id of rows shown at `depth`: the screen's
/// identity, else the drill's id keys, else the common ones. The empty
/// path matches rows that are bare strings (folder lists).
fn id_fields(spec: &GridSpec, depth: usize) -> Vec<&'static str> {
    let keys = match (spec.screen_at(depth), spec.drills.get(depth)) {
        (Some(screen), _) => screen.identity.id,
        (None, Some(drill)) => drill.id_keys,
        (None, None) => nav::ID_KEYS,
    };
    keys.iter().copied().chain([""]).collect()
}

impl App {
    /// Where the user is now, to come back to or reopen later.
    pub(super) fn place_here(&self) -> Place {
        let mut place = Place::at(self.active);
        let (Some(spec), Some(view)) = (self.spec(), self.view()) else {
            return place;
        };
        for (depth, level) in view.levels[1..].iter().enumerate() {
            place
                .steps
                .push(Step::drill(&id_fields(&spec, depth), level.id.clone()));
        }
        let grid = view.grid();
        let fields = id_fields(&spec, view.depth());
        if let Some(id) = grid
            .selected_raw()
            .and_then(|row| nav::row_id(row, &fields))
        {
            place.steps.push(Step::select(&fields, id));
        }
        place.filter = grid.filter.clone();
        place.sort = grid.sorted.and_then(|(col, descending)| {
            Some(SortBy {
                column: grid.cols.get(col)?.clone(),
                descending,
            })
        });
        place
    }

    /// Goes to `place`, so that Back returns here.
    pub(crate) fn go_to(&mut self, place: &Place) {
        self.push_history();
        self.open_place(place);
    }

    /// Notes the current place on the Back stack.
    pub(super) fn push_history(&mut self) {
        let here = self.place_here();
        if self.history.last() != Some(&here) {
            self.history.push(here);
        }
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
    }

    /// Reopens `place` from its resource's root with fresh rows, then walks
    /// its steps as the rows arrive.
    fn open_place(&mut self, place: &Place) {
        self.views.clear();
        let resource = place.resource;
        if resource.grid_spec().is_none() {
            return self.open_and_focus(resource);
        }
        let view = self
            .grids
            .entry(resource)
            .or_insert_with(|| GridView::new(resource.resource().label));
        let was_loaded = view.loaded;
        view.levels.truncate(1);
        view.awaiting = None;
        view.error = None;
        view.grid_mut().set_filter("");
        view.pending = Some(Pending::Trail(Trail::new(place)));
        self.open_and_focus(resource);
        if was_loaded {
            self.reload_resource(resource);
        }
    }

    /// Back past a screen's root: the previous place on the stack. A place
    /// whose rows are still on hand is shown as it was, without reloading.
    pub(super) fn back_in_history(&mut self) -> bool {
        let Some(place) = self.history.pop() else {
            return false;
        };
        if self.is_still_open(&place) {
            self.open_and_focus(place.resource);
            let last = place.steps.last().filter(|s| !s.drill).cloned();
            if let Some(view) = self.grids.get_mut(&place.resource) {
                let rest = Place {
                    steps: last.into_iter().collect(),
                    ..place.clone()
                };
                view.pending = Some(Pending::Trail(Trail::new(&rest)));
            }
            self.apply_pending(place.resource);
        } else {
            self.open_place(&place);
        }
        self.set_status(Tone::Info, self.breadcrumb().join(" › "));
        true
    }

    /// The resource still shows the rows `place` drilled through.
    fn is_still_open(&self, place: &Place) -> bool {
        let Some(view) = self.grids.get(&place.resource) else {
            return place.resource.grid_spec().is_none();
        };
        let ids: Vec<&str> = view.levels[1..].iter().map(|l| l.id.as_str()).collect();
        view.loaded && !view.is_loading() && ids == place.drill_ids()
    }

    /// Plain navigation (the sidebar) starts a fresh trail.
    pub(super) fn forget_history(&mut self) {
        self.history.clear();
    }

    /// Takes the trail's next step on freshly loaded rows: select the row,
    /// drill into it, or (at the end) apply the filter and sort.
    pub(super) fn walk_trail(&mut self, resource: ResourceId, mut trail: Trail) {
        let Some(view) = self.grids.get_mut(&resource) else {
            return;
        };
        let grid = view.grid_mut();
        let Some(step) = trail.steps.pop_front() else {
            return finish(grid, &trail);
        };
        if !step.drill && trail.steps.is_empty() {
            finish(grid, &trail);
        } else {
            grid.set_filter("");
        }
        let found = step.find(grid).is_some_and(|i| grid.select_raw(i));
        if !found {
            let title = resource.title();
            let value = &step.value;
            return self.set_status(Tone::Error, format!("{value} isn't in {title} any more"));
        }
        if step.drill {
            view.pending = Some(Pending::Trail(trail));
            if resource == self.active {
                self.open_row();
            }
        }
    }

    /// Puts `entry` at the top of the Recent list.
    pub(super) fn remember(&mut self, entry: Entry) {
        self.recent.retain(|e| e.action != entry.action);
        self.recent.insert(0, entry);
        self.recent.truncate(RECENT_LIMIT);
    }

    /// Notes the place on screen in Recent, named by its breadcrumb.
    pub(super) fn remember_here(&mut self, item: Option<String>) {
        let mut place = self.place_here();
        if item.is_none() {
            place.steps.retain(|s| s.drill);
        }
        place.filter.clear();
        place.sort = None;
        let mut crumbs = self.breadcrumb();
        crumbs.extend(item);
        self.remember(Entry {
            label: crumbs.join(" › "),
            hint: "recent",
            action: Action::Go(place),
        });
    }

    /// Notes a resource opened from the sidebar or palette in Recent.
    pub(super) fn remember_resource(&mut self, resource: ResourceId) {
        self.remember(Entry {
            label: resource.title(),
            hint: "recent",
            action: Action::Open(resource),
        });
    }
}

/// The trail's end: the saved filter and sort, on top of the rows.
fn finish(grid: &mut Grid, trail: &Trail) {
    if let Some(sort) = &trail.sort {
        if let Some(col) = grid.cols.iter().position(|c| *c == sort.column) {
            grid.sort_by_column(col, sort.descending);
        }
    }
    grid.set_filter(&trail.filter);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;
    use crate::tui::app::Focus;

    use crossterm::event::KeyCode;
    use serde_json::json;

    fn warehouses() -> ResourceId {
        resource("Lakehouse", "Warehouses")
    }

    fn sales_place() -> Place {
        Place {
            steps: vec![
                Step::drill(&["name"], "wh2"),
                Step::select(&["name"], "sales"),
            ],
            ..Place::at(warehouses())
        }
    }

    fn selected_name(app: &App) -> String {
        app.view().unwrap().grid().selected_raw().unwrap()["name"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn going_to_a_place_drills_and_selects_as_rows_arrive() {
        let mut app = app();
        app.go_to(&sales_place());
        assert_eq!(app.active, warehouses());
        assert_eq!(last_fetch(&app).path, "/lakehouse/warehouse");
        answer(&mut app, json!([{"name": "wh1"}, {"name": "wh2"}]));
        assert_eq!(last_fetch(&app).path, "/lakehouse/warehouse/wh2/namespaces");
        answer(&mut app, json!([{"name": "hr"}, {"name": "sales"}]));
        assert_eq!(selected_name(&app), "sales");
        assert_eq!(app.breadcrumb(), vec!["Lakehouse", "Warehouses", "wh2"]);
    }

    #[test]
    fn a_missing_row_stops_the_trail_with_a_message() {
        let mut app = app();
        app.go_to(&sales_place());
        answer(&mut app, json!([{"name": "wh1"}]));
        assert_eq!(app.view().unwrap().depth(), 0);
        assert!(app
            .status
            .text
            .contains("wh2 isn't in Lakehouse › Warehouses"));
    }

    #[test]
    fn a_place_reapplies_its_filter_and_sort() {
        let mut app = app();
        let place = Place {
            filter: "a".into(),
            sort: Some(SortBy {
                column: "name".into(),
                descending: true,
            }),
            ..Place::at(resource("Designer", "Datasets"))
        };
        app.go_to(&place);
        answer(
            &mut app,
            json!([{"name": "alpha"}, {"name": "beta"}, {"name": "zed"}]),
        );
        let grid = app.view().unwrap().grid();
        assert_eq!(grid.filter, "a");
        assert_eq!(grid.visible_len(), 2);
        assert_eq!(grid.sort_label().unwrap(), "name ▼");
        assert_eq!(
            app.place_here(),
            Place {
                steps: vec![Step::select(&id_fields_of(&app), "alpha")],
                ..place
            }
        );
    }

    fn id_fields_of(app: &App) -> Vec<&'static str> {
        id_fields(&app.spec().unwrap(), app.view().unwrap().depth())
    }

    #[test]
    fn back_returns_to_a_drilled_place_without_refetching() {
        let mut app = app();
        app.open_and_focus(warehouses());
        answer(&mut app, json!([{"name": "wh1"}, {"name": "wh2"}]));
        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
        answer(&mut app, json!([{"name": "hr"}, {"name": "sales"}]));
        press(&mut app, &[KeyCode::Down]);
        app.go_to(&Place::at(resource("Designer", "Datasets")));
        answer(&mut app, json!([{"n": 1}]));
        let fetches = app.outbox.len();
        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(app.active, warehouses());
        assert_eq!(app.outbox.len(), fetches, "rows still on hand");
        assert_eq!(app.breadcrumb(), vec!["Lakehouse", "Warehouses", "wh2"]);
        assert_eq!(selected_name(&app), "sales");
        press(&mut app, &[KeyCode::Esc, KeyCode::Esc]);
        assert_eq!(app.focus, Focus::Sidebar, "history used up");
    }

    #[test]
    fn back_rebuilds_a_place_a_jump_replaced() {
        let mut app = app();
        app.go_to(&sales_place());
        answer(&mut app, json!([{"name": "wh1"}, {"name": "wh2"}]));
        answer(&mut app, json!([{"name": "hr"}, {"name": "sales"}]));
        app.go_to(&Place::at(warehouses()));
        answer(&mut app, json!([{"name": "wh1"}, {"name": "wh2"}]));
        assert_eq!(app.view().unwrap().depth(), 0);
        press(&mut app, &[KeyCode::Backspace]);
        answer(&mut app, json!([{"name": "wh1"}, {"name": "wh2"}]));
        answer(&mut app, json!([{"name": "hr"}, {"name": "sales"}]));
        assert_eq!(app.breadcrumb(), vec!["Lakehouse", "Warehouses", "wh2"]);
        assert_eq!(selected_name(&app), "sales");
    }

    #[test]
    fn recent_lists_drills_and_details_and_reopens_them() {
        let mut app = app();
        app.open_and_focus(warehouses());
        answer(&mut app, json!([{"name": "wh1"}, {"name": "wh2"}]));
        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
        answer(&mut app, json!([{"name": "sales"}]));
        let top = &app.recent[0];
        assert_eq!(top.label, "Lakehouse › Warehouses › wh2");
        let Action::Go(place) = top.action.clone() else {
            panic!("a drill is remembered as a place")
        };
        app.forget_history();
        app.open_and_focus(resource("Designer", "Datasets"));
        app.run_action(Action::Go(place));
        answer(&mut app, json!([{"name": "wh1"}, {"name": "wh2"}]));
        assert_eq!(last_fetch(&app).path, "/lakehouse/warehouse/wh2/namespaces");
    }

    #[test]
    fn recent_keeps_the_latest_twenty_without_repeats() {
        let mut app = app();
        let ids: Vec<ResourceId> = nav::all_resources().collect();
        for id in ids.iter().chain(ids.iter()) {
            app.remember_resource(*id);
        }
        assert_eq!(app.recent.len(), RECENT_LIMIT.min(ids.len()));
        assert_eq!(app.recent[0].label, ids.last().unwrap().title());
    }
}
