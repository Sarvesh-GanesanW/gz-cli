//! Catalog › Data lineage: the connections that have lineage, their assets
//! (a page at a time), and each asset's graph.
//!
//! Verified contracts (TUI research, "Catalog › Data lineage"; groundzero-etl-provider-api):
//! - `GET etl /etl/lineage/connections` → `{connections: [{connectionId, assetCount, assetTypes:
//!   [{assetType, count}]}]}`, joined with `GET connections /connection` by `connectionId == Id`
//!   for names, as the web does (app.py:2463-2477; web lineageApi.js:183-190,
//!   useLineageQueries.js:43-62)
//! - `GET etl /etl/lineage/assets?connectionId=&limit=&offset=` → `{assets, total, hasMore,
//!   nextOffset}`, rows keyed by `assetId` (app.py:2227-2287; limit 1..100)
//! - `GET etl /etl/lineage/assets/{assetId}?depth=1&…` → `{graph}` (app.py:2290-2331)
//! - `GET etl /etl/lineage/assets/{assetId}/impact?depth=3` → `{assets, jobs, …}` (app.py:2363-2427)

use serde_json::{json, Value};

use crate::tui::compose::{Combined, Compose, Parts};
use crate::tui::errors::{self, Subject};
use crate::tui::format::{text_of, Format};
use crate::tui::nav::{drill, Drill, GridSpec, Resource, Source, View};
use crate::tui::screen::{
    ActionKind, ActionSpec, Column, Cond, CopySpec, DetailField, DetailSpec, Goto, Identity,
    KeyHelp, LinkSpec, RelatedSpec, ScreenSpec, Section, SectionBody, Select, Sort, Target,
};
use crate::tui::shape::Paging;
use crate::tui::specs::connections::TYPE_LABELS;
use crate::tui::views::lineage_graph::{lakehouse_table, LineageGraph};
use crate::tui::views::Ctx;

const CONNECTION_LINK: Goto = Goto {
    target: Target {
        module: "Connections",
        resource: "Connections",
    },
    select: Select::Row {
        field: &["Id"],
        from: &["connectionId", "bindings.0.connectionId"],
    },
};

/// Lineage connections named from the connection list. A connection the
/// list doesn't show (no access, or deleted) keeps its id as its name.
fn join_names(parts: &Parts) -> Combined {
    let names = parts.rows(1);
    let rows = parts
        .rows(0)
        .into_iter()
        .map(|mut lineage| {
            let id = text_of(&lineage["connectionId"]);
            let connection = names.iter().find(|c| text_of(&c["Id"]) == id);
            lineage["Name"] = connection
                .map(|c| c["Name"].clone())
                .unwrap_or_else(|| json!(format!("Connection {id}")));
            lineage["type"] = connection.map_or(Value::Null, |c| c["type"].clone());
            lineage["kinds"] = json!(kinds_text(&lineage["assetTypes"]));
            lineage
        })
        .collect();
    let failures = match &parts.answers.get(1) {
        Some(Err(err)) => vec![format!(
            "Connection names didn't load: {}",
            errors::explain(
                err,
                Subject {
                    what: "the connections",
                    service: "connections",
                },
            )
        )],
        _ => Vec::new(),
    };
    Combined { rows, failures }
}

/// "TABLE 51 · OBJECT 2".
fn kinds_text(types: &Value) -> String {
    types
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| format!("{} {}", text_of(&t["assetType"]), text_of(&t["count"])))
        .collect::<Vec<_>>()
        .join(" · ")
}

static WITH_NAMES: Compose = Compose {
    with: &[Source::get("connections", "/connection")],
    expand: None,
    combine: join_names,
};

pub static CONNECTIONS: ScreenSpec = ScreenSpec {
    noun: "connection",
    columns: &[
        Column::new("Connection", &["Name"], Format::Text),
        Column::new("Type", &["type"], Format::Label(TYPE_LABELS)),
        Column::new("Assets", &["assetCount"], Format::Number).width(7),
        Column::new("Kinds", &["kinds"], Format::Text),
    ],
    sort: Some(Sort {
        field: &["Name"],
        descending: false,
    }),
    identity: Identity {
        id: &["connectionId"],
        title: &["Name"],
    },
    links: &[LinkSpec {
        key: 'c',
        label: "connection",
        goto: CONNECTION_LINK,
        when: None,
    }],
    compose: Some(&WITH_NAMES),
    empty: "No lineage recorded yet. It appears once jobs or datasets run on a connection.",
    ..ScreenSpec::BLANK
};

// ---------- assets ----------

const LAKEHOUSE_ASSET: Cond = Cond {
    field: &["bindings.0.connectionType"],
    any_of: &["GZLAKEHOUSE"],
};

fn asset_title(row: &Value) -> String {
    ["displayName", "qualifiedName"]
        .iter()
        .find_map(|k| row[k].as_str())
        .map_or_else(|| text_of(&row["assetId"]), str::to_string)
}

/// Enter on an asset: its graph.
fn open_graph(row: &Value, ctx: &mut Ctx) {
    match row["assetId"].as_i64() {
        Some(id) => ctx.push(Box::new(LineageGraph::new(id, asset_title(row)))),
        None => ctx.toast(crate::tui::app::Tone::Error, "This asset has no id"),
    }
}

fn open_table(row: &Value, ctx: &mut Ctx) {
    match lakehouse_table(row) {
        Some(preview) => ctx.push(Box::new(preview)),
        None => ctx.toast(
            crate::tui::app::Tone::Info,
            "This asset isn't a lakehouse table",
        ),
    }
}

const IMPACT_COLUMNS: &[Column] = &[
    Column::new("Asset", &["displayName"], Format::Text),
    Column::new("Type", &["assetType"], Format::Text),
    Column::new("Hops", &["depth"], Format::Number),
    Column::new("Deleted", &["isDeleted"], Format::Bool),
];

pub static ASSETS: ScreenSpec = ScreenSpec {
    noun: "asset",
    columns: &[
        Column::new("Asset", &["displayName", "qualifiedName"], Format::Text),
        Column::new("Type", &["assetType"], Format::Text).width(7),
        Column::new("Platform", &["platform"], Format::Text),
        Column::new("Columns", &["latestVersion.columnCount"], Format::Number).width(7),
        Column::new("Observed", &["latestVersion.observedOn"], Format::RelTime).width(10),
    ],
    // The server orders by name; keep its order across pages.
    sort: None,
    identity: Identity {
        id: &["assetId"],
        title: &["displayName", "qualifiedName"],
    },
    detail: Some(DetailSpec {
        missing: "Lineage asset not found. It may have been removed.",
        subtitle: &[
            (&["bindings.0.connectionName"], Format::Text),
            (&["qualifiedName"], Format::Text),
            (&["assetType"], Format::Text),
        ],
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new("Platform", &["platform"], Format::Text),
                DetailField::new("Connection", &["bindings.0.connectionName"], Format::Text),
                DetailField::new(
                    "Connection type",
                    &["bindings.0.connectionType"],
                    Format::Label(TYPE_LABELS),
                ),
                DetailField::new("External key", &["bindings.0.externalKey"], Format::Text),
                DetailField::new(
                    "Schema version",
                    &["latestVersion.versionNumber"],
                    Format::Number,
                ),
                DetailField::new("Columns", &["latestVersion.columnCount"], Format::Number),
                DetailField::new("Observed", &["latestVersion.observedOn"], Format::RelTime),
                DetailField::new(
                    "Observed at",
                    &["latestVersion.observedOn"],
                    Format::DateTime,
                ),
                DetailField::new("Tags", &["tags"], Format::Text).optional(),
            ]),
        }],
        related: &[RelatedSpec {
            title: "Downstream impact (3 hops)",
            source: Source::get(
                "etl",
                "/etl/lineage/assets/{assetId}/impact?depth=3&assetLimit=100",
            ),
            columns: IMPACT_COLUMNS,
            when: None,
            empty: "Nothing downstream depends on this asset",
            limit: 20,
        }],
        copy: Some(CopySpec {
            label: "qualified name",
            template: "{qualifiedName}",
        }),
        ..DetailSpec::BLANK
    }),
    actions: &[
        ActionSpec::new('v', "View graph", ActionKind::Custom(open_graph)),
        ActionSpec::new('t', "Open lakehouse table", ActionKind::Custom(open_table))
            .when(LAKEHOUSE_ASSET),
    ],
    links: &[LinkSpec {
        key: 'c',
        label: "connection",
        goto: CONNECTION_LINK,
        when: None,
    }],
    keys: &[KeyHelp {
        keys: "⏎",
        what: "open the asset's lineage graph",
    }],
    paging: Some(Paging {
        size: 100,
        max: 100,
    }),
    open: Some(open_graph),
    empty: "No matching tables.",
    ..ScreenSpec::BLANK
};

static DRILLS: &[Drill] = &[Drill {
    screen: Some(&ASSETS),
    ..drill(
        "connection",
        &["connectionId"],
        Source::get("etl", "/etl/lineage/assets?connectionId={connectionId}"),
    )
}];

pub const RESOURCE: Resource = Resource {
    label: "Data lineage",
    view: View::Grid(GridSpec {
        source: Source::get("etl", "/etl/lineage/connections"),
        drills: DRILLS,
        detail: None,
        screen: Some(&CONNECTIONS),
    }),
};

#[cfg(test)]
mod tests;
