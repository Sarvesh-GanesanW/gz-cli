//! Catalog: data catalog, usage, lineage, streams, ontology and log exports.

use crate::tui::nav::{drill, grid, grid_with_detail, GridSpec, Module, Resource, Source, View};

pub const MODULE: Module = Module {
    title: "Catalog",
    icon: "▦",
    resources: &[
        Resource {
            label: "Data catalog",
            view: View::Grid(GridSpec {
                source: Source::get("lakehouse", "/lakehouse/warehouse"),
                drills: &[
                    drill(
                        "warehouse",
                        &["name", "warehouseName"],
                        Source::get("lakehouse", "/lakehouse/warehouse/{0}/namespaces"),
                    ),
                    drill(
                        "database",
                        &["name", "databaseName", "namespace"],
                        Source::get("lakehouse", "/lakehouse/schema/{0}/{1}"),
                    ),
                ],
                detail: None,
                screen: None,
            }),
        },
        grid("AI usage", Source::get("chat", "/chat/metrics")),
        grid("Agent usage", Source::get("agents", "/v1/agents/metrics")),
        grid("Job usage", Source::get("etl", "/etl/consumption-metrics")),
        grid(
            "Project usage",
            Source::get("etl", "/etl/consumption-metrics/aggregates/projects"),
        ),
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
