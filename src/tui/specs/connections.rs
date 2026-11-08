//! Connections.

use crate::tui::nav::{grid_with_detail, Module, Source};

pub const MODULE: Module = Module {
    title: "Connections",
    icon: "⇄",
    resources: &[grid_with_detail(
        "Connections",
        Source::get("connections", "/connection"),
        Source::get("connections", "/connection/{0}"),
    )],
};
