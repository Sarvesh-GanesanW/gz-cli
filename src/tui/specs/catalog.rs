//! Catalog: data catalog, usage, lineage, streams, ontology and log exports.
//! The data screens live in `catalog/`, one file each.

mod data_catalog;

use crate::tui::nav::{drill, grid, grid_with_detail, GridSpec, Module, Resource, Source, View};
use crate::tui::specs::usage;

pub const MODULE: Module = Module {
    title: "Catalog",
    icon: "▦",
    resources: &[
        data_catalog::RESOURCE,
        usage::AI_USAGE_RESOURCE,
        usage::AGENT_USAGE_RESOURCE,
        usage::JOB_USAGE_RESOURCE,
        usage::PROJECT_USAGE_RESOURCE,
        Resource {
            label: "Data lineage",
            view: View::Grid(GridSpec {
                source: Source::get("connections", "/connection"),
                drills: &[drill(
                    "connection",
                    &["Id", "id"],
                    Source::get("etl", "/etl/lineage/assets?connectionId={0}"),
                )],
                detail: Some(Source::get("etl", "/etl/lineage/assets/{1}")),
                screen: None,
            }),
        },
        grid(
            "Live streams",
            Source::post("etl", "/etl/dataset-stream/list", "{}"),
        ),
        grid_with_detail(
            "Ontology models",
            Source::get("etl", "/etl/ontology/models"),
            Source::get("etl", "/etl/ontology/models/{0}"),
        ),
        grid_with_detail(
            "Log exports",
            Source::get("logexports", "/log-export/runs"),
            Source::get("logexports", "/log-export/runs/{0}"),
        ),
        grid_with_detail(
            "Export schedules",
            Source::get("logexports", "/log-export/schedules"),
            Source::get("logexports", "/log-export/schedules/{0}"),
        ),
    ],
};
