//! Lakehouse: warehouses (→ databases → tables → table preview) and saved
//! queries.
//!
//! Verified contracts (TUI research, "Lakehouse"; groundzero-iceberg-provider-api):
//! - `GET lakehouse /lakehouse/warehouse` → `{status, response: [{id, name, description,
//!   owner, bucket, created_on}]}` (warehouses.py:129-225)
//! - `GET …/warehouse/{name}/namespaces` → `[{namespace, records, size: "4MB"}]` (warehouses.py:228-255)
//! - `GET /lakehouse/schema/{wh}/{ns}` → one row **per column** (`tableName`, `columnName`,
//!   `dataType`, `totalBytes`, `updatedAt` epoch ms); grouped by `tableName` here (schemas.py:156-173)
//! - `POST /lakehouse/dbsize {warehouseName, databaseName}` → `{tableCount, totalBytes, totalFiles}`
//!   (schemas.py:535-575; web etlUtlis.js:554-573)
//! - `PATCH /lakehouse/warehouse/{id}/description {description}` (≤1000 chars, warehouses.py:94-126)
//! - `DELETE /lakehouse/warehouse/{id}`: 409 while it still holds data (warehouses.py:63-91)
//! - saved queries: `GET /lakehouse/query` (the caller's own, bare array), `GET /lakehouse/query/{Id}`
//!   → `{query}`, `POST /lakehouse/query/rename/{Id} {name}`, `DELETE /lakehouse/query/{Id}`
//!   (saved_queries.py:70-182)

use serde_json::Value;

use crate::tui::format::{text_of, Format};
use crate::tui::nav::{drill, Drill, GridSpec, Module, Resource, Source, Verb, View};
use crate::tui::screen::{
    ActionKind, ActionSpec, After, Column, CopySpec, DetailField, DetailSpec, FieldKind, FormField,
    FormSpec, Goto, Identity, KeyHelp, LinkSpec, RelatedSpec, RequestSpec, ScreenSpec, Section,
    SectionBody, Select, Sort, Target,
};
use crate::tui::shape;
use crate::tui::views::sql::SqlView;
use crate::tui::views::table_preview::{TablePreview, TableRef};
use crate::tui::views::Ctx;

// ---------- warehouses ----------

const EDIT_DESCRIPTION_FORM: FormSpec = FormSpec {
    title: "Edit description",
    submit: "Save",
    fields: &[
        FormField::new("description", "Description", FieldKind::Multiline)
            .default("{description}")
            .max_len(1000)
            .hint("Shown in the data catalog"),
    ],
};

const EDIT_DESCRIPTION: ActionSpec = ActionSpec::new(
    'e',
    "Edit description",
    ActionKind::Form(
        &EDIT_DESCRIPTION_FORM,
        RequestSpec {
            source: Source::new(
                "lakehouse",
                Verb::Patch,
                "/lakehouse/warehouse/{id}/description",
                Some(r#"{"description":"{description}"}"#),
            ),
            success: "Description saved",
            after: After::Refresh,
        },
    ),
);

const DELETE_WAREHOUSE: ActionSpec = ActionSpec::new(
    'D',
    "Delete warehouse",
    ActionKind::Request(RequestSpec {
        source: Source::new("lakehouse", Verb::Delete, "/lakehouse/warehouse/{id}", None),
        success: "Deleted successfully.",
        after: After::RefreshAndClose,
    }),
)
.destructive(
    "Delete warehouse {name}? The lakehouse refuses while it still has databases or tables.",
);

const OPEN_SQL: ActionSpec = ActionSpec::new(
    's',
    "Query with SQL",
    ActionKind::Custom(open_warehouse_sql),
);

fn open_warehouse_sql(row: &Value, ctx: &mut Ctx) {
    let warehouse = text_of(&row["name"]);
    ctx.push(Box::new(SqlView::new(warehouse, "SHOW DATABASES")));
}

const NAMESPACE_COLUMNS: &[Column] = &[
    Column::new("Database", &["namespace"], Format::Text),
    Column::new("Rows", &["records"], Format::Number),
    Column::new("Size", &["size"], Format::Bytes),
];

const GOVERNED_COLUMNS: &[Column] = &[
    Column::new("Database", &["namespace"], Format::Text),
    Column::new("Table", &["tableName"], Format::Text),
    Column::new("Why", &["reasons"], Format::Text),
];

pub static WAREHOUSES: ScreenSpec = ScreenSpec {
    noun: "warehouse",
    columns: &[
        Column::new("Warehouse", &["name"], Format::Text),
        Column::new("Description", &["description"], Format::Text),
        Column::new("Created by", &["owner"], Format::Text),
        Column::new("Bucket", &["bucket"], Format::Text),
        Column::new("Created", &["created_on"], Format::RelTime).width(10),
    ],
    sort: Some(Sort {
        field: &["name"],
        descending: false,
    }),
    // Drills, links and search all go by name; actions use the numeric `id`.
    identity: Identity {
        id: &["name"],
        title: &["name"],
    },
    detail: Some(DetailSpec {
        subtitle: &[(&["bucket"], Format::Text)],
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new("Description", &["description"], Format::Text).optional(),
                DetailField::new("Created by", &["owner"], Format::Text),
                DetailField::new("Bucket", &["bucket"], Format::Text),
                DetailField::new("Created", &["created_on"], Format::RelTime),
                DetailField::new("Created at", &["created_on"], Format::DateTime),
            ]),
        }],
        related: &[
            RelatedSpec {
                title: "Databases",
                source: Source::get("lakehouse", "/lakehouse/warehouse/{name}/namespaces"),
                columns: NAMESPACE_COLUMNS,
                when: None,
                empty: "No databases in this warehouse yet",
                limit: 50,
            },
            RelatedSpec {
                title: "Governed tables",
                source: Source::get("lakehouse", "/lakehouse/governance/governed-tables/{name}"),
                columns: GOVERNED_COLUMNS,
                when: None,
                empty: "No tables have data policies",
                limit: 50,
            },
        ],
        copy: Some(CopySpec {
            label: "bucket",
            template: "{bucket}",
        }),
        ..DetailSpec::BLANK
    }),
    actions: &[OPEN_SQL, EDIT_DESCRIPTION, DELETE_WAREHOUSE],
    links: &[LinkSpec {
        key: 'Q',
        label: "saved queries",
        goto: Goto {
            target: Target {
                module: "Lakehouse",
                resource: "Saved queries",
            },
            select: Select::Filter { from: &["name"] },
        },
        when: None,
    }],
    empty: "No warehouses yet",
    ..ScreenSpec::BLANK
};

// ---------- databases ----------

pub static DATABASES: ScreenSpec = ScreenSpec {
    noun: "database",
    columns: NAMESPACE_COLUMNS,
    sort: Some(Sort {
        field: &["namespace"],
        descending: false,
    }),
    identity: Identity {
        id: &["namespace", "name"],
        title: &["namespace", "name"],
    },
    detail: Some(DetailSpec {
        // Reads table metadata only, so it is quick. Positional ids: a
        // namespace row's own fields could shadow the warehouse's name.
        load: Some(Source::post(
            "lakehouse",
            "/lakehouse/dbsize",
            r#"{"warehouseName":"{0}","databaseName":"{1}"}"#,
        )),
        sections: &[Section {
            title: "Properties",
            body: SectionBody::Fields(&[
                DetailField::new("Tables", &["tableCount"], Format::Number),
                DetailField::new("Size", &["totalBytes", "size"], Format::Bytes),
                DetailField::new("Files", &["totalFiles"], Format::Number),
                DetailField::new("Rows", &["records"], Format::Number),
            ]),
        }],
        ..DetailSpec::BLANK
    }),
    empty: "No databases in this warehouse yet",
    ..ScreenSpec::BLANK
};

// ---------- tables ----------

/// One row per table from the per-column schema rows.
fn group_tables(rows: Vec<Value>, _now_ms: i64) -> Vec<Value> {
    shape::group_by(rows, &["tableName"], "columns")
}

fn table_ref(row: &Value) -> TableRef {
    TableRef {
        warehouse: text_of(&row["catalog"]),
        database: text_of(&row["databaseName"]),
        table: text_of(&row["tableName"]),
    }
}

/// Enter on a table: its preview, with SQL and lineage one key away.
fn open_table(row: &Value, ctx: &mut Ctx) {
    ctx.push(Box::new(TablePreview::new(table_ref(row))));
}

fn open_table_sql(table: &TableRef, ctx: &mut Ctx) {
    ctx.push(Box::new(SqlView::new(
        table.warehouse.clone(),
        table.preview_sql(),
    )));
}

pub static TABLES: ScreenSpec = ScreenSpec {
    noun: "table",
    columns: &[
        Column::new("Table", &["tableName"], Format::Text),
        Column::new("Columns", &["columns"], Format::Count).width(7),
        Column::new("Size", &["totalBytes"], Format::Bytes).width(9),
        Column::new("Updated", &["updatedAt"], Format::RelTime).width(10),
        Column::new("Created by", &["createdBy"], Format::Text),
    ],
    sort: Some(Sort {
        field: &["tableName"],
        descending: false,
    }),
    identity: Identity {
        id: &["tableName"],
        title: &["tableName"],
    },
    detail: Some(DetailSpec {
        subtitle: &[(&["databaseName"], Format::Text)],
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new("Columns", &["columns"], Format::Count),
                DetailField::new("Size", &["totalBytes"], Format::Bytes),
                DetailField::new("Updated", &["updatedAt"], Format::RelTime),
                DetailField::new("Created", &["createdAt"], Format::DateTime),
                DetailField::new("Created by", &["createdBy"], Format::Text).optional(),
            ]),
        }],
        copy: Some(CopySpec {
            label: "database.table",
            template: "{databaseName}.{tableName}",
        }),
        ..DetailSpec::BLANK
    }),
    actions: &[ActionSpec::new(
        's',
        "Query with SQL",
        ActionKind::Custom(|row, ctx| open_table_sql(&table_ref(row), ctx)),
    )],
    keys: &[KeyHelp {
        keys: "⏎",
        what: "preview the table (schema, sample rows, snapshots)",
    }],
    shape: Some(group_tables),
    open: Some(open_table),
    empty: "No tables in this database",
    ..ScreenSpec::BLANK
};

// ---------- saved queries ----------

const RENAME_FORM: FormSpec = FormSpec {
    title: "Rename",
    submit: "Rename",
    fields: &[FormField::new("name", "Name", FieldKind::Text)
        .required()
        .default("{Name}")
        .max_len(100)],
};

const RUN_SAVED: ActionSpec =
    ActionSpec::new('s', "Run in SQL", ActionKind::Custom(run_saved_query));

/// The query text is in the detail (`query`); from the table it is fetched
/// first by the SQL view's caller, so offer it once the text is loaded.
fn run_saved_query(row: &Value, ctx: &mut Ctx) {
    let Some(query) = row["query"].as_str() else {
        return ctx.toast(
            crate::tui::app::Tone::Info,
            "Open the query (⏎) to load its SQL first",
        );
    };
    ctx.push(Box::new(SqlView::new(
        text_of(&row["warehouseName"]),
        query,
    )));
}

pub static SAVED_QUERIES: ScreenSpec = ScreenSpec {
    noun: "saved query",
    columns: &[
        Column::new("Name", &["Name"], Format::Text),
        Column::new("Warehouse", &["warehouseName"], Format::Text),
        Column::new("Modified by", &["modified_by"], Format::Text),
        Column::new("Modified", &["modified_on"], Format::RelTime).width(10),
    ],
    sort: Some(Sort {
        field: &["modified_on"],
        descending: true,
    }),
    identity: Identity {
        id: &["Id"],
        title: &["Name"],
    },
    detail: Some(DetailSpec {
        load: Some(Source::get("lakehouse", "/lakehouse/query/{Id}")),
        missing: "The query text could not be read (it may have been deleted).",
        subtitle: &[(&["warehouseName"], Format::Text)],
        sections: &[
            Section {
                title: "Summary",
                body: SectionBody::Fields(&[
                    DetailField::new("Warehouse", &["warehouseName"], Format::Text),
                    DetailField::new("Modified by", &["modified_by"], Format::Text),
                    DetailField::new("Modified", &["modified_on"], Format::RelTime),
                ]),
            },
            Section {
                title: "SQL",
                body: SectionBody::Text(&["query"]),
            },
        ],
        copy: Some(CopySpec {
            label: "SQL",
            template: "{query}",
        }),
        ..DetailSpec::BLANK
    }),
    actions: &[
        RUN_SAVED,
        ActionSpec::new(
            'R',
            "Rename",
            ActionKind::Form(
                &RENAME_FORM,
                RequestSpec {
                    source: Source::post(
                        "lakehouse",
                        "/lakehouse/query/rename/{Id}",
                        r#"{"name":"{name}"}"#,
                    ),
                    success: "Query renamed successfully",
                    after: After::Refresh,
                },
            ),
        ),
        ActionSpec::new(
            'D',
            "Delete",
            ActionKind::Request(RequestSpec {
                source: Source::new("lakehouse", Verb::Delete, "/lakehouse/query/{Id}", None),
                success: "Query deleted successfully",
                after: After::RefreshAndClose,
            }),
        )
        .destructive("Delete saved query {Name}? Its SQL text is removed too."),
    ],
    links: &[LinkSpec {
        key: 'W',
        label: "warehouse",
        goto: Goto {
            target: Target {
                module: "Lakehouse",
                resource: "Warehouses",
            },
            select: Select::Row {
                field: &["name"],
                from: &["warehouseName"],
            },
        },
        when: None,
    }],
    empty: "No saved queries. Only queries you created are listed.",
    ..ScreenSpec::BLANK
};

static WAREHOUSE_DRILLS: &[Drill] = &[
    Drill {
        screen: Some(&DATABASES),
        ..drill(
            "warehouse",
            &["name"],
            Source::get("lakehouse", "/lakehouse/warehouse/{0}/namespaces"),
        )
    },
    Drill {
        screen: Some(&TABLES),
        ..drill(
            "database",
            &["namespace"],
            Source::get("lakehouse", "/lakehouse/schema/{0}/{1}"),
        )
    },
];

pub const MODULE: Module = Module {
    title: "Lakehouse",
    icon: "≋",
    resources: &[
        Resource {
            label: "Warehouses",
            view: View::Grid(GridSpec {
                source: Source::get("lakehouse", "/lakehouse/warehouse"),
                drills: WAREHOUSE_DRILLS,
                detail: None,
                screen: Some(&WAREHOUSES),
            }),
        },
        crate::tui::nav::screen(
            "Saved queries",
            Source::get("lakehouse", "/lakehouse/query"),
            &SAVED_QUERIES,
        ),
    ],
};

#[cfg(test)]
mod tests;
