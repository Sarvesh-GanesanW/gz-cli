//! Catalog › Data catalog: one list of datasets, ETL jobs and lakehouse
//! warehouses, as the web catalog shows them.
//!
//! Verified contracts (TUI research, "Catalog › Data catalog"; web
//! apiCatalog/catalog/useCatalogResources.js:49-77, apiCatalogUtils.js:640-720):
//! - `GET datasets /dataset` → `[{Id, Name, modified_by, modified_on, tags}]`
//! - `GET lakehouse /lakehouse/warehouse` → `{response: [{id, name, owner, bucket, created_on,
//!   description, tags}]}`
//! - `GET etlprojects /etl/etlProjects` → `[{Id, Name, owner, …}]`, then per project
//!   `GET etlprojects /etl/etlJobs/{Id}/jobs` → `[{Id, Name, modified_by, modified_on}]`
//!
//! Each source fails on its own: the rest still show, and the status line
//! names what didn't load, as the web's per-source toasts do.

use serde_json::{json, Value};

use crate::tui::compose::{Answer, Combined, Compose, Expand, Parts};
use crate::tui::errors::{self, Subject};
use crate::tui::format::{text_of, Format, StatusMap};
use crate::tui::nav::{screen, Resource, Source, Verb};
use crate::tui::screen::{
    ActionKind, ActionSpec, After, Column, Cond, DetailField, DetailSpec, FieldKind, FormField,
    FormSpec, Goto, Identity, LinkSpec, RelatedSpec, RequestSpec, ScreenSpec, Section, SectionBody,
    Select, Target,
};
use crate::tui::shape;
use crate::tui::theme::BadgeTone;

/// Projects whose jobs are listed at most (one request each).
const MAX_PROJECTS: usize = 100;

const DATASET: &str = "dataset";
const JOB: &str = "job";
const WAREHOUSE: &str = "warehouse";

static KINDS: StatusMap = StatusMap {
    entries: &[
        (DATASET, "Dataset", BadgeTone::Info),
        (JOB, "ETL Job", BadgeTone::Warn),
        (WAREHOUSE, "Warehouse", BadgeTone::Good),
    ],
    fallback: BadgeTone::Muted,
};

/// Tag labels from plain lists (`["pii"]`, `[{key, value}]`) and the
/// lakehouse's grouped form (`[{warehouse: [{k: v}], others: [{k: v}]}]`).
fn tag_labels(tags: &Value) -> Vec<String> {
    let mut labels = Vec::new();
    for tag in tags.as_array().into_iter().flatten() {
        match tag {
            Value::String(s) if !s.trim().is_empty() => labels.push(s.clone()),
            Value::Object(map) if map.contains_key("key") => {
                let key = text_of(&map["key"]);
                match map
                    .get("value")
                    .map(text_of)
                    .filter(|v| !v.is_empty() && v != "—")
                {
                    Some(value) => labels.push(format!("{key}={value}")),
                    None => labels.push(key),
                }
            }
            Value::Object(groups) => {
                for entry in groups.values().filter_map(Value::as_array).flatten() {
                    for (key, value) in entry.as_object().into_iter().flatten() {
                        labels.push(format!("{key}={}", text_of(value)));
                    }
                }
            }
            _ => {}
        }
    }
    labels.sort();
    labels.dedup();
    labels
}

fn entry(kind: &str, order: u8, name: &Value, location: String) -> Value {
    json!({"kind": kind, "kindOrder": order, "name": name, "location": location})
}

fn datasets(rows: Vec<Value>) -> Vec<Value> {
    rows.into_iter()
        .map(|d| {
            let mut row = entry(DATASET, 0, &d["Name"], "Designer › Datasets".into());
            row["sourceId"] = d["Id"].clone();
            row["owner"] = d["modified_by"].clone();
            row["updatedAt"] = d["modified_on"].clone();
            row["tags"] = json!(tag_labels(&d["tags"]));
            row
        })
        .collect()
}

fn jobs(expanded: &[(Value, Answer)]) -> Vec<Value> {
    expanded
        .iter()
        .filter_map(|(project, answer)| Some((project, answer.as_ref().ok()?)))
        .flat_map(|(project, jobs)| {
            let project_name = text_of(&project["Name"]);
            shape::rows_of(jobs).into_iter().map(move |j| {
                let mut row = entry(JOB, 1, &j["Name"], format!("DE/ML › {project_name}"));
                row["sourceId"] = j["Id"].clone();
                row["projectId"] = project["Id"].clone();
                row["projectName"] = json!(project_name);
                row["owner"] = j["modified_by"].clone();
                row["updatedAt"] = j["modified_on"].clone();
                row["tags"] = json!(Vec::<String>::new());
                row
            })
        })
        .collect()
}

fn warehouses(rows: Vec<Value>) -> Vec<Value> {
    rows.into_iter()
        .map(|w| {
            let mut row = entry(WAREHOUSE, 2, &w["name"], "Lakehouse › Warehouses".into());
            row["sourceId"] = w["id"].clone();
            row["warehouseName"] = w["name"].clone();
            row["owner"] = w["owner"].clone();
            row["updatedAt"] = w["created_on"].clone();
            row["bucket"] = w["bucket"].clone();
            row["description"] = w["description"].clone();
            row["tags"] = json!(tag_labels(&w["tags"]));
            row
        })
        .collect()
}

fn catalog(parts: &Parts) -> Combined {
    let mut rows = datasets(parts.rows(0));
    rows.extend(jobs(parts.expanded));
    rows.extend(warehouses(parts.rows(1)));
    // Kind order, then name, as the web sorts (useCatalogResources.js:26-28).
    rows.sort_by_key(|r| {
        (
            r["kindOrder"].as_u64().unwrap_or(9),
            text_of(&r["name"]).to_lowercase(),
        )
    });
    let sources = [
        ("Datasets", "datasets"),
        ("Warehouses", "lakehouse"),
        ("Projects", "etlprojects"),
    ];
    let mut failures: Vec<String> = parts
        .answers
        .iter()
        .zip(sources)
        .filter_map(|(answer, (what, service))| {
            let err = answer.as_ref().err()?;
            let lower = what.to_lowercase();
            let why = errors::explain(
                err,
                Subject {
                    what: &lower,
                    service,
                },
            );
            Some(format!("{what} failed to load: {why}"))
        })
        .collect();
    let failed_jobs = parts.expanded.iter().filter(|(_, a)| a.is_err()).count();
    if failed_jobs > 0 {
        failures.push(format!("Jobs of {failed_jobs} project(s) failed to load"));
    }
    Combined { rows, failures }
}

static SOURCES: Compose = Compose {
    with: &[
        Source::get("lakehouse", "/lakehouse/warehouse"),
        Source::get("etlprojects", "/etl/etlProjects"),
    ],
    expand: Some(Expand {
        part: 2,
        source: Source::get("etlprojects", "/etl/etlJobs/{Id}/jobs"),
        max: MAX_PROJECTS,
    }),
    combine: catalog,
};

const IS_WAREHOUSE: Cond = Cond {
    field: &["kind"],
    any_of: &[WAREHOUSE],
};
const IS_DATASET: Cond = Cond {
    field: &["kind"],
    any_of: &[DATASET],
};
const IS_JOB: Cond = Cond {
    field: &["kind"],
    any_of: &[JOB],
};

const DESCRIPTION_FORM: FormSpec = FormSpec {
    title: "Edit description",
    submit: "Save",
    fields: &[
        FormField::new("description", "Description", FieldKind::Multiline)
            .default("{description}")
            .max_len(1000),
    ],
};

const DATABASE_COLUMNS: &[Column] = &[
    Column::new("Database", &["namespace"], Format::Text),
    Column::new("Rows", &["records"], Format::Number),
    Column::new("Size", &["size"], Format::Bytes),
];

pub static CATALOG: ScreenSpec = ScreenSpec {
    noun: "catalog entry",
    columns: &[
        Column::new("Kind", &["kind"], Format::Status(&KINDS)).width(9),
        Column::new("Name", &["name"], Format::Text),
        Column::new("Location", &["location"], Format::Text),
        Column::new("Owner", &["owner"], Format::Text),
        Column::new("Tags", &["tags"], Format::Count).width(4),
        Column::new("Updated", &["updatedAt"], Format::RelTime).width(10),
    ],
    sort: None,
    identity: Identity {
        id: &["sourceId"],
        title: &["name"],
    },
    detail: Some(DetailSpec {
        badge: Some((&["kind"], &KINDS)),
        subtitle: &[(&["location"], Format::Text)],
        sections: &[Section {
            title: "Overview",
            body: SectionBody::Fields(&[
                DetailField::new("Owner", &["owner"], Format::Text),
                DetailField::new("Location", &["location"], Format::Text),
                DetailField::new("Project", &["projectName"], Format::Text).optional(),
                DetailField::new("Bucket", &["bucket"], Format::Text).optional(),
                DetailField::new("Resource id", &["sourceId"], Format::Text),
                DetailField::new("Updated", &["updatedAt"], Format::RelTime),
                DetailField::new("Description", &["description"], Format::Text).optional(),
                DetailField::new("Tags", &["tags"], Format::Text).optional(),
            ]),
        }],
        related: &[RelatedSpec {
            title: "Databases",
            source: Source::get(
                "lakehouse",
                "/lakehouse/warehouse/{warehouseName}/namespaces",
            ),
            columns: DATABASE_COLUMNS,
            when: Some(IS_WAREHOUSE),
            empty: "No databases found in this warehouse yet.",
            limit: 50,
        }],
        ..DetailSpec::BLANK
    }),
    actions: &[ActionSpec::new(
        'e',
        "Edit description",
        ActionKind::Form(
            &DESCRIPTION_FORM,
            RequestSpec {
                source: Source::new(
                    "lakehouse",
                    Verb::Patch,
                    "/lakehouse/warehouse/{sourceId}/description",
                    Some(r#"{"description":"{description}"}"#),
                ),
                success: "Description saved",
                after: After::Refresh,
            },
        ),
    )
    .when(IS_WAREHOUSE)],
    links: &[
        LinkSpec {
            key: 'w',
            label: "warehouse",
            goto: Goto {
                target: Target {
                    module: "Lakehouse",
                    resource: "Warehouses",
                },
                select: Select::Drill {
                    field: &["name"],
                    from: &["warehouseName"],
                },
            },
            when: Some(IS_WAREHOUSE),
        },
        LinkSpec {
            key: 'D',
            label: "dataset",
            goto: Goto {
                target: Target {
                    module: "Designer",
                    resource: "Datasets",
                },
                select: Select::Row {
                    field: &["Id"],
                    from: &["sourceId"],
                },
            },
            when: Some(IS_DATASET),
        },
        LinkSpec {
            key: 'p',
            label: "project's jobs",
            goto: Goto {
                target: Target {
                    module: "DE/ML",
                    resource: "Projects",
                },
                select: Select::Drill {
                    field: &["Id"],
                    from: &["projectId"],
                },
            },
            when: Some(IS_JOB),
        },
    ],
    compose: Some(&SOURCES),
    empty: "Nothing in the catalog yet",
    ..ScreenSpec::BLANK
};

pub const RESOURCE: Resource = screen(
    "Data catalog",
    Source::get("datasets", "/dataset"),
    &CATALOG,
);

#[cfg(test)]
mod tests;
