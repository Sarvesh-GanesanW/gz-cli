//! Workspaces and their contents.

use crate::tui::nav::{drill, GridSpec, Module, Resource, Source, View};

pub const MODULE: Module = Module {
    title: "Workspaces",
    icon: "▣",
    resources: &[Resource {
        label: "Workspaces",
        view: View::Grid(GridSpec {
            source: Source::get("workspaces", "/workspace"),
            drills: &[drill(
                "workspace",
                &["Id", "id", "workspaceId"],
                Source::get("workspaces", "/workspace/{0}/contents"),
            )],
            detail: None,
            screen: None,
        }),
    }],
};
