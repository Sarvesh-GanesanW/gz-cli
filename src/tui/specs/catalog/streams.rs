//! Catalog › Live streams and Ontology models.
//!
//! Verified contracts (TUI research; groundzero-etl-provider-api):
//! - `POST etl /etl/dataset-stream/list {}` → `{success, streams}`, one row per stream
//!   **run**, keyed by `jobIdentifier` (fallback `sessionId`); read-only, and listing does
//!   not keep streams alive (app.py:5021-5034, dataset_streams/listing.py:161-202)
//! - `POST etl /etl/dataset-stream/stop {"datasetId"}` ends the run for every watcher
//!   (app.py:5037-5060; web LiveStreams.js:140-184)
//! - `GET etl /etl/ontology/models?limit=100&offset=0` → `{items, capabilities}`
//!   (app.py:2837-2898); detail `GET /etl/ontology/models/{id}` can be 100 KB+, so only its
//!   summary is shown; releases `GET …/{id}/releases` (app.py:4058)
//! - `DELETE etl /etl/ontology/models/{id}`: 409 while published releases exist (app.py:2943-2977)

use std::time::Duration;

use crate::tui::format::{Format, StatusMap};
use crate::tui::nav::{screen, Resource, Source, Verb};
use crate::tui::screen::{
    ActionKind, ActionSpec, After, Column, Cond, DetailField, DetailSpec, Goto, Identity, LinkSpec,
    RelatedSpec, RequestSpec, ScreenSpec, Section, SectionBody, Select, Target,
};
use crate::tui::theme::BadgeTone;

static STREAM_STATUS: StatusMap = StatusMap {
    entries: &[
        ("running", "Running", BadgeTone::Good),
        ("stopped", "Stopped", BadgeTone::Muted),
        ("failed", "Failed", BadgeTone::Bad),
        ("ended", "Ended", BadgeTone::Muted),
    ],
    fallback: BadgeTone::Info,
};

const RUNNING: Cond = Cond {
    field: &["status"],
    any_of: &["running"],
};

pub static STREAMS: ScreenSpec = ScreenSpec {
    noun: "stream",
    columns: &[
        Column::new("Dataset", &["datasetName", "datasetId"], Format::Text),
        Column::new("Status", &["status"], Format::Status(&STREAM_STATUS)).width(8),
        Column::new("Runtime", &["rteName"], Format::Text),
        Column::new("Started", &["startedAt"], Format::RelTime).width(10),
        Column::new("Records/s", &["stats.recordsPerSecond"], Format::Number).width(9),
        Column::new("Total", &["stats.totalRecords"], Format::Number),
    ],
    identity: Identity {
        id: &["jobIdentifier", "sessionId", "datasetId"],
        title: &["datasetName", "datasetId"],
    },
    detail: Some(DetailSpec {
        badge: Some((&["status"], &STREAM_STATUS)),
        subtitle: &[(&["rteName"], Format::Text)],
        sections: &[
            Section {
                title: "Run",
                body: SectionBody::Fields(&[
                    DetailField::new("Why it ended", &["endReason"], Format::Text).optional(),
                    DetailField::new("Started by", &["startedBy"], Format::Text),
                    DetailField::new("Started", &["startedAt"], Format::DateTime),
                    DetailField::new("Ended", &["endedAt"], Format::DateTime).optional(),
                    DetailField::new("Stopped by", &["stoppedBy"], Format::Text).optional(),
                    DetailField::new("Watched", &["watched"], Format::Bool),
                ]),
            },
            Section {
                title: "Stats",
                body: SectionBody::Object(&["stats"]),
            },
        ],
        ..DetailSpec::BLANK
    }),
    actions: &[ActionSpec::new(
        'x',
        "Stop stream",
        ActionKind::Request(RequestSpec {
            source: Source::post(
                "etl",
                "/etl/dataset-stream/stop",
                r#"{"datasetId":{datasetId}}"#,
            ),
            success: "Stream stopped",
            after: After::Refresh,
        }),
    )
    .destructive("Stop streaming {datasetName}? It stops for everyone watching it.")
    .when(RUNNING)],
    links: &[LinkSpec {
        key: 'D',
        label: "dataset",
        goto: Goto {
            target: Target {
                module: "Designer",
                resource: "Datasets",
            },
            select: Select::Row {
                field: &["Id"],
                from: &["datasetId"],
            },
        },
        when: None,
    }],
    // The web polls every 5 s while the tab is visible.
    poll: Some(Duration::from_secs(5)),
    empty: "No streams yet.",
    ..ScreenSpec::BLANK
};

pub const STREAMS_RESOURCE: Resource = screen(
    "Live streams",
    Source::post("etl", "/etl/dataset-stream/list", "{}"),
    &STREAMS,
);

// ---------- ontology ----------

static MODEL_STATUS: StatusMap = StatusMap {
    entries: &[
        ("published", "Published", BadgeTone::Good),
        ("draft", "Draft", BadgeTone::Warn),
    ],
    fallback: BadgeTone::Info,
};

const CAN_DELETE: Cond = Cond {
    field: &["capabilities.canDelete"],
    any_of: &["true"],
};

const RELEASE_COLUMNS: &[Column] = &[
    Column::new("Version", &["version"], Format::Text),
    Column::new("By", &["createdBy"], Format::Text),
    Column::new("Created", &["createdAt"], Format::RelTime),
    Column::new("Definitions", &["definitionCount"], Format::Number),
];

pub static MODELS: ScreenSpec = ScreenSpec {
    noun: "ontology model",
    columns: &[
        Column::new("Model", &["name"], Format::Text),
        Column::new("Domain", &["domain"], Format::Text),
        Column::new("Status", &["status"], Format::Status(&MODEL_STATUS)).width(9),
        Column::new("Definitions", &["definitionCount"], Format::Number).width(11),
        Column::new("Relationships", &["relationshipCount"], Format::Number).width(13),
        Column::new("Updated", &["updatedAt"], Format::RelTime).width(10),
    ],
    identity: Identity {
        id: &["id"],
        title: &["name"],
    },
    detail: Some(DetailSpec {
        badge: Some((&["status"], &MODEL_STATUS)),
        subtitle: &[(&["domain"], Format::Text)],
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new("Description", &["description"], Format::Text).optional(),
                DetailField::new("Draft version", &["version"], Format::Number),
                DetailField::new("Created by", &["createdBy"], Format::Text),
                DetailField::new("Updated", &["updatedAt"], Format::RelTime),
                DetailField::new("Definitions", &["definitionCount"], Format::Number).optional(),
                DetailField::new("Relationships", &["relationshipCount"], Format::Number)
                    .optional(),
            ]),
        }],
        related: &[RelatedSpec {
            title: "Releases",
            source: Source::get("etl", "/etl/ontology/models/{id}/releases?limit=20&offset=0"),
            columns: RELEASE_COLUMNS,
            when: None,
            empty: "Never published",
            limit: 20,
        }],
        ..DetailSpec::BLANK
    }),
    actions: &[ActionSpec::new(
        'd',
        "Delete model",
        ActionKind::Request(RequestSpec {
            source: Source::new("etl", Verb::Delete, "/etl/ontology/models/{id}", None),
            success: "Ontology model deleted",
            after: After::RefreshAndClose,
        }),
    )
    .destructive(
        "Delete ontology model {name} and its draft and review history, permanently? A model with published releases can't be deleted until they are.",
    )
    .when(CAN_DELETE)],
    empty: "No ontology model yet. Models are created in the web app.",
    ..ScreenSpec::BLANK
};

pub const MODELS_RESOURCE: Resource = screen(
    "Ontology models",
    Source::get("etl", "/etl/ontology/models?limit=100&offset=0"),
    &MODELS,
);
