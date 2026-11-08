//! Agents: agents (→ runs), approval requests, tools and models.

use crate::tui::nav::{drill, grid, GridSpec, Module, Resource, Source, View};

pub const MODULE: Module = Module {
    title: "Agents",
    icon: "◉",
    resources: &[
        Resource {
            label: "Agents",
            view: View::Grid(GridSpec {
                source: Source::get("agents", "/v1/agents"),
                drills: &[drill(
                    "agent",
                    &["id"],
                    Source::get("agents", "/v1/agents/{0}/runs"),
                )],
                detail: Some(Source::get("agents", "/v1/agents/{0}/runs/{1}")),
                screen: None,
            }),
        },
        grid(
            "Approval requests",
            Source::get("agents", "/v1/approval-requests"),
        ),
        grid("Tools", Source::get("agents", "/v1/tools")),
        grid("Models", Source::get("agents", "/v1/models")),
    ],
};
