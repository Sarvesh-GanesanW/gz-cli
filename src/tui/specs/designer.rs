//! Designer: datasets, dashboards, visualizations and filters.

use crate::tui::nav::{grid_with_detail, Module, Source};

pub const MODULE: Module = Module {
    title: "Designer",
    icon: "◧",
    resources: &[
        grid_with_detail(
            "Datasets",
            Source::get("datasets", "/dataset"),
            Source::get("datasets", "/dataset/{0}"),
        ),
        grid_with_detail(
            "Dashboards",
            Source::get("dashboards", "/dashboard"),
            Source::get("dashboards", "/dashboard/{0}"),
        ),
        grid_with_detail(
            "Visualizations",
            Source::get("visualizations", "/visualization"),
            Source::get("visualizations", "/visualization/{0}"),
        ),
        grid_with_detail(
            "Filters",
            Source::get("filters", "/filter"),
            Source::get("filters", "/filter/{0}"),
        ),
    ],
};
