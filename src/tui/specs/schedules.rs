//! Schedules and their executions.

use crate::tui::nav::{grid, grid_with_detail, Module, Source};

pub const MODULE: Module = Module {
    title: "Schedules",
    icon: "◷",
    resources: &[
        grid_with_detail(
            "Schedules",
            Source::get("schedules", "/schedule"),
            Source::get("schedules", "/schedule/{0}"),
        ),
        grid(
            "Executions",
            Source::get("schedules", "/schedule/executions"),
        ),
    ],
};
