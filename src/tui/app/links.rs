//! Following links between screens: open the target, select (or drill
//! into, or filter for) the matching row once it loads, and come back with
//! Back.

use crate::tui::fields;
use crate::tui::fields::FieldPath;
use crate::tui::format::text_of;
use crate::tui::nav::{self, ResourceId};
use crate::tui::screen::{Goto, Select};
use crate::tui::views::RowContext;

use super::{App, Tone};

/// What a followed link does once the target's rows are in.
#[derive(Debug, Clone)]
pub enum Pending {
    Select {
        field: FieldPath,
        value: String,
        drill: bool,
    },
    Filter(String),
}

impl App {
    pub(super) fn follow(&mut self, goto: Goto, row: &RowContext) {
        let Some(target) = nav::find_resource(goto.target.module, goto.target.resource) else {
            self.set_status(
                Tone::Error,
                format!(
                    "{} › {} no longer exists",
                    goto.target.module, goto.target.resource
                ),
            );
            return;
        };
        let value_of = |from: FieldPath| fields::lookup_any(&row.row, from).map(text_of);
        let pending = match goto.select {
            Select::Open => None,
            Select::Row { field, from } | Select::Drill { field, from } => match value_of(from) {
                Some(value) => Some(Pending::Select {
                    field,
                    value,
                    drill: matches!(goto.select, Select::Drill { .. }),
                }),
                None => {
                    return self
                        .set_status(Tone::Error, format!("This row has no {}", from.join("/")))
                }
            },
            Select::Filter { from } => match value_of(from) {
                Some(value) => Some(Pending::Filter(value)),
                None => {
                    return self
                        .set_status(Tone::Error, format!("This row has no {}", from.join("/")))
                }
            },
        };
        if target != self.active {
            self.link_back.push(self.active);
        }
        self.views.clear();
        self.open_and_focus(target);
        let Some(view) = self.grids.get_mut(&target) else {
            return;
        };
        view.levels.truncate(1);
        view.pending = pending;
        if view.loaded && !view.is_loading() {
            self.apply_pending(target);
        }
    }

    /// Carries out a followed link's selection on freshly loaded rows.
    pub(super) fn apply_pending(&mut self, resource: ResourceId) {
        let Some(view) = self.grids.get_mut(&resource) else {
            return;
        };
        let Some(pending) = view.pending.take() else {
            return;
        };
        match pending {
            Pending::Filter(text) => view.grid_mut().set_filter(&text),
            Pending::Select {
                field,
                value,
                drill,
            } => {
                let grid = view.grid_mut();
                let found = grid
                    .find_raw(field, &value)
                    .is_some_and(|i| grid.select_raw(i));
                if !found {
                    let title = resource.title();
                    return self.set_status(Tone::Error, format!("{value} isn't in {title}"));
                }
                if drill && resource == self.active {
                    self.open_row();
                }
            }
        }
    }

    /// Back at a screen's root: return to where a link was followed from.
    pub(super) fn back_from_link(&mut self) -> bool {
        let Some(previous) = self.link_back.pop() else {
            return false;
        };
        self.open_and_focus(previous);
        self.set_status(Tone::Info, self.breadcrumb().join(" › "));
        true
    }

    /// Plain navigation forgets the link trail.
    pub(super) fn forget_links(&mut self) {
        self.link_back.clear();
    }
}
