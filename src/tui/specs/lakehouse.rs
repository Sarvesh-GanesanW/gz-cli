//! Lakehouse: warehouses (→ namespaces → tables) and saved queries.

use crate::tui::nav::{drill, grid_with_detail, GridSpec, Module, Resource, Source, View};

pub const MODULE: Module = Module {
    title: "Lakehouse",
    icon: "≋",
    resources: &[
        Resource {
            label: "Warehouses",
            view: View::Grid(GridSpec {
                source: Source::get("lakehouse", "/lakehouse/warehouse"),
                drills: &[
                    drill(
                        "warehouse",
                        &["name", "warehouseName", "warehouse"],
                        Source::get("lakehouse", "/lakehouse/warehouse/{0}/namespaces"),
                    ),
                    drill(
                        "namespace",
                        &["name", "namespace", "namespaceName"],
                        Source::get("lakehouse", "/lakehouse/schema/{0}/{1}"),
                    ),
                ],
                detail: None,
                screen: None,
            }),
        },
        grid_with_detail(
            "Saved queries",
            Source::get("lakehouse", "/lakehouse/query"),
            Source::get("lakehouse", "/lakehouse/query/{0}"),
        ),
    ],
};
