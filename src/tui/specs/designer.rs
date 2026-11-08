//! Designer: datasets, dashboards, visualizations and filters.
//!
//! Verified contracts (TUI research notes, "Designer"): bare-array lists
//! `GET /dataset|/dashboard|/visualization|/filter` with `{Id, Name, modified_by,
//! modified_on}` (+ `chartType`, `created_by` for charts; `FilterType`, `displayName`
//! for filters), unsorted; details `GET /<thing>/{Id}`, which carry no id, name or
//! timestamps, so the list row stays underneath. Create `POST {"Name"}`, rename
//! `PATCH /{Id} {"Name"}`, delete `DELETE /{Id}` (200, empty body). Missing ids
//! answer differently per screen: dataset 500, chart 404, dashboard and filter
//! `null`.
//!
//! Left out on purpose: config edits (full overwrites made by the web editors),
//! dashboard publish (the web builds the snapshot in the browser), and
//! unpublish (no permission check on the route).

use super::rules::{NAME_MAX, NAME_MESSAGE, NAME_PATTERN};
use crate::tui::format::Format;
use crate::tui::nav::{screen, Module, Source, Verb};
use crate::tui::screen::{
    ActionKind, ActionSpec, After, Column, DetailField, DetailSpec, FieldKind, FormField, FormSpec,
    Goto, Identity, LinkSpec, RequestSpec, ScreenSpec, Section, SectionBody, Select, Sort, Target,
};

const ID: &[&str] = &["Id", "id"];
const NAME: &[&str] = &["Name", "name"];
const UPDATED: &[&str] = &["modified_on"];
const UPDATED_BY: &[&str] = &["modified_by"];

/// `chartType` → the web's label (`utils/chartTypeLabels.js`).
const CHART_TYPES: &[(&str, &str)] = &[
    ("BarChart", "Bar"),
    ("ColumnChart", "Vertical bar"),
    ("StackedBarChart", "Stacked bar"),
    ("StackedVerticalBarChart", "Stacked vertical bar"),
    ("ComboChart", "Combo"),
    ("LineChart", "Line"),
    ("PieChart", "Pie"),
    ("HalfDonutChart", "Half donut"),
    ("DonutChart", "Donut"),
    ("GuageChart", "Gauge"),
    ("BubbleChart", "Bubble"),
    ("ScatterChart", "Scatter"),
    ("CandlestickChart", "Candle"),
    ("TreeMap", "Tree map"),
    ("GeoChart", "Geo"),
    ("FunnelChart", "Funnel"),
    ("FlatChart", "Table"),
];

/// `FilterType` → the web's label (`constants/index.js`).
const FILTER_TYPES: &[(&str, &str)] = &[
    ("list", "Single select list"),
    ("range", "Range"),
    ("singleValue", "Single value"),
    ("multiSelect", "Multi select list"),
    ("slider", "One-way slider"),
    ("slider_two_way", "Two-way slider"),
    ("toggle", "Toggle"),
];

const DATA_TYPES: &[(&str, &str)] = &[
    ("text", "Plain text"),
    ("number", "Number"),
    ("datetime-local", "Date time"),
];

const SOURCES: &[(&str, &str)] = &[("custom", "Custom values"), ("datasets", "From a dataset")];

static NEW_NAME: FormSpec = FormSpec {
    title: "New",
    submit: "Create",
    fields: &[FormField::new("name", "Name", FieldKind::Text)
        .required()
        .pattern(NAME_PATTERN, NAME_MESSAGE)
        .max_len(NAME_MAX)
        .hint("Starts empty; set it up in the web designer")],
};

static RENAME: FormSpec = FormSpec {
    title: "Rename",
    submit: "Save",
    fields: &[FormField::new("name", "New name", FieldKind::Text)
        .required()
        .default("{Name}")
        .pattern(NAME_PATTERN, NAME_MESSAGE)
        .max_len(NAME_MAX)],
};

/// Datasets refer to filters by name in `{{name.prop}}`, and nothing
/// rewrites those queries on rename.
static RENAME_FILTER: FormSpec = FormSpec {
    title: "Rename filter",
    submit: "Save",
    fields: &[FormField::new("name", "New name", FieldKind::Text)
        .required()
        .default("{Name}")
        .pattern(NAME_PATTERN, NAME_MESSAGE)
        .max_len(NAME_MAX)
        .hint("Dataset queries that use {{oldName.…}} will break")],
};

/// The create, rename and delete every Designer list offers.
macro_rules! designer_actions {
    ($service:literal, $path:literal, $noun:literal, $rename:expr, $warning:literal) => {
        &[
            ActionSpec::new(
                'n',
                concat!("New ", $noun),
                ActionKind::Form(
                    &NEW_NAME,
                    RequestSpec {
                        source: Source::post($service, $path, r#"{"Name":"{name}"}"#),
                        success: concat!("Added the ", $noun),
                        after: After::Refresh,
                    },
                ),
            ),
            ActionSpec::new(
                'e',
                "Rename",
                ActionKind::Form(
                    $rename,
                    RequestSpec {
                        source: Source::new(
                            $service,
                            Verb::Patch,
                            concat!($path, "/{Id}"),
                            Some(r#"{"Name":"{name}"}"#),
                        ),
                        success: concat!("Renamed the ", $noun),
                        after: After::Refresh,
                    },
                ),
            ),
            ActionSpec::new(
                'd',
                "Delete",
                ActionKind::Request(RequestSpec {
                    source: Source::new($service, Verb::Delete, concat!($path, "/{Id}"), None),
                    success: concat!("Deleted the ", $noun),
                    after: After::RefreshAndClose,
                }),
            )
            .destructive(concat!("Delete ", $noun, " {Name}? ", $warning)),
        ]
    };
}

const SORT: Option<Sort> = Some(Sort {
    field: UPDATED,
    descending: true,
});

const IDENTITY: Identity = Identity {
    id: ID,
    title: NAME,
};

const fn to_dataset(from: &'static [&'static str]) -> LinkSpec {
    LinkSpec {
        key: 'D',
        label: "dataset",
        goto: Goto {
            target: Target {
                module: "Designer",
                resource: "Datasets",
            },
            select: Select::Row { field: ID, from },
        },
        when: None,
    }
}

static DATASETS: ScreenSpec = ScreenSpec {
    noun: "dataset",
    columns: &[
        Column::new("Name", NAME, Format::Text),
        Column::new("Updated by", UPDATED_BY, Format::Text),
        Column::new("Updated", UPDATED, Format::RelTime).width(10),
    ],
    sort: SORT,
    identity: IDENTITY,
    detail: Some(DetailSpec {
        load: Some(Source::get("datasets", "/dataset/{Id}")),
        sections: &[
            Section {
                title: "Summary",
                body: SectionBody::Fields(&[
                    DetailField::new("Connections", &["ConnectionNames"], Format::Text),
                    DetailField::new("Source", &["Config.ConnectionType"], Format::Text).optional(),
                    DetailField::new("Runtime", &["Config.rteName"], Format::Text).optional(),
                    DetailField::new("Workers", &["Config.minimumWorkers"], Format::Number)
                        .optional(),
                    DetailField::new("Models", &["Config.modelNames"], Format::Text).optional(),
                    DetailField::new("Filters", &["filters"], Format::Count),
                    DetailField::new("Updated by", UPDATED_BY, Format::Text),
                    DetailField::new("Updated", UPDATED, Format::DateTime),
                    DetailField::new("Id", ID, Format::Text),
                ]),
            },
            Section {
                title: "Query",
                body: SectionBody::Text(&["DataSourceQuery"]),
            },
            Section {
                title: "Transformation",
                body: SectionBody::Text(&["TransformationQuery"]),
            },
        ],
        ..DetailSpec::BLANK
    }),
    actions: designer_actions!(
        "datasets",
        "/dataset",
        "dataset",
        &RENAME,
        "Visualizations and filters that use it will break; nothing checks."
    ),
    empty: "No datasets to show",
    ..ScreenSpec::BLANK
};

static DASHBOARDS: ScreenSpec = ScreenSpec {
    noun: "dashboard",
    columns: &[
        Column::new("Name", NAME, Format::Text),
        Column::new("Updated by", UPDATED_BY, Format::Text),
        Column::new("Updated", UPDATED, Format::RelTime).width(10),
    ],
    sort: SORT,
    identity: IDENTITY,
    detail: Some(DetailSpec {
        load: Some(Source::get("dashboards", "/dashboard/{Id}")),
        missing: "Dashboard not found. It may have been deleted.",
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new("Description", &["description"], Format::Text).optional(),
                DetailField::new("Tabs", &["config.tabs"], Format::Count),
                DetailField::new("Charts", &["config.visualizationDetails"], Format::Count),
                DetailField::new("Updated by", UPDATED_BY, Format::Text),
                DetailField::new("Updated", UPDATED, Format::DateTime),
                DetailField::new("Id", ID, Format::Text),
            ]),
        }],
        ..DetailSpec::BLANK
    }),
    actions: designer_actions!(
        "dashboards",
        "/dashboard",
        "dashboard",
        &RENAME,
        "Its published copy and workspace copies stay."
    ),
    empty: "No dashboards to show",
    ..ScreenSpec::BLANK
};

static VISUALIZATIONS: ScreenSpec = ScreenSpec {
    noun: "visualization",
    columns: &[
        Column::new("Name", NAME, Format::Text),
        Column::new("Chart", &["chartType"], Format::Label(CHART_TYPES)),
        Column::new("Updated by", UPDATED_BY, Format::Text),
        Column::new("Updated", UPDATED, Format::RelTime).width(10),
        Column::new("Created by", &["created_by"], Format::Text),
    ],
    sort: SORT,
    identity: IDENTITY,
    detail: Some(DetailSpec {
        load: Some(Source::get("visualizations", "/visualization/{Id}")),
        missing: "Visualization not found. It may have been deleted.",
        subtitle: &[(&["chartType"], Format::Label(CHART_TYPES))],
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                // `datasetName` holds the dataset's id, despite its name.
                DetailField::new(
                    "Dataset id",
                    &[
                        "config.chartConfig.datasetName",
                        "config.datasetId",
                        "config.datasetName",
                    ],
                    Format::Text,
                ),
                DetailField::new("Title", &["config.chartConfig.title"], Format::Text).optional(),
                DetailField::new("Filters", &["config.filters"], Format::Count),
                DetailField::new("Created by", &["created_by"], Format::Text),
                DetailField::new("Updated by", UPDATED_BY, Format::Text),
                DetailField::new("Updated", UPDATED, Format::DateTime),
            ]),
        }],
        ..DetailSpec::BLANK
    }),
    actions: designer_actions!(
        "visualizations",
        "/visualization",
        "visualization",
        &RENAME,
        "Dashboards that place it keep an empty tile."
    ),
    links: &[to_dataset(&[
        "config.chartConfig.datasetName",
        "config.datasetId",
        "config.datasetName",
    ])],
    empty: "No visualizations to show",
    ..ScreenSpec::BLANK
};

static FILTERS: ScreenSpec = ScreenSpec {
    noun: "filter",
    columns: &[
        Column::new("Name", NAME, Format::Text),
        Column::new("Display name", &["displayName"], Format::Text),
        Column::new(
            "Type",
            &["FilterType", "filterType"],
            Format::Label(FILTER_TYPES),
        ),
        Column::new("Updated by", UPDATED_BY, Format::Text),
        Column::new("Updated", UPDATED, Format::RelTime).width(10),
    ],
    sort: SORT,
    identity: IDENTITY,
    detail: Some(DetailSpec {
        load: Some(Source::get("filters", "/filter/{Id}")),
        missing: "Filter not found. It may have been deleted.",
        subtitle: &[(&["FilterType", "filterType"], Format::Label(FILTER_TYPES))],
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new(
                    "Display name",
                    &["displayName", "config.displayName"],
                    Format::Text,
                )
                .optional(),
                DetailField::new("Data type", &["dataType"], Format::Label(DATA_TYPES)).optional(),
                DetailField::new("Values from", &["source"], Format::Label(SOURCES)).optional(),
                DetailField::new("Dataset id", &["config.datasetId"], Format::Text).optional(),
                DetailField::new("Column", &["config.columnName"], Format::Text).optional(),
                DetailField::new("Choices", &["config.allItems"], Format::Count).optional(),
                DetailField::new("Updated by", UPDATED_BY, Format::Text),
                DetailField::new("Updated", UPDATED, Format::DateTime),
                DetailField::new("Id", ID, Format::Text),
            ]),
        }],
        ..DetailSpec::BLANK
    }),
    actions: designer_actions!(
        "filters",
        "/filter",
        "filter",
        &RENAME_FILTER,
        "Datasets and dashboards that use it keep a dangling reference."
    ),
    links: &[to_dataset(&["config.datasetId"])],
    empty: "No filters to show",
    ..ScreenSpec::BLANK
};

pub const MODULE: Module = Module {
    title: "Designer",
    icon: "◧",
    resources: &[
        screen("Datasets", Source::get("datasets", "/dataset"), &DATASETS),
        screen(
            "Dashboards",
            Source::get("dashboards", "/dashboard"),
            &DASHBOARDS,
        ),
        screen(
            "Visualizations",
            Source::get("visualizations", "/visualization"),
            &VISUALIZATIONS,
        ),
        screen("Filters", Source::get("filters", "/filter"), &FILTERS),
    ],
};

#[cfg(test)]
mod tests;
