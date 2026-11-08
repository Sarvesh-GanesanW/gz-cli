//! Workspaces and their contents (folders and imported dashboard copies).
//!
//! Verified contracts (TUI research notes, "Workspaces"):
//! - `GET workspaces /workspace` → `[{Id, Name, owner, modifiedBy (always null), modifiedOn}]`
//! - `GET /workspace/{id}/contents` → `[{id, Name, isDashboard, owner, modifiedBy, modifiedOn}]`
//!   (lowercase `id`; an unknown folder answers `[]`)
//! - `GET /workspace/dashboard/{parentId}/{id}` → the dashboard snapshot (about 250 KB,
//!   so only on open); its `Id` is the Designer dashboard it was copied from
//! - create `POST /workspace {"Name"}`, rename `PATCH /workspace/{id} {"Name"}`, delete a
//!   workspace `DELETE /workspace/{Id}`, a child `DELETE /workspace/{parent}/{child}`,
//!   owner `PUT /workspace/{id}/owner` (admins only, 403 otherwise)
//! - `GET /workspace/{id}` always answers empty, so workspaces have no detail load
//!
//! Folders nest without limit but drills are fixed-depth, so the TUI shows one
//! level of contents; deeper folders open in the web.

use super::rules::{NAME_MAX, NAME_MESSAGE, NAME_PATTERN};
use crate::tui::format::Format;
use crate::tui::nav::{Drill, GridSpec, Module, Resource, Source, Verb, View};
use crate::tui::screen::{
    ActionKind, ActionSpec, After, Column, Cond, DetailField, DetailSpec, FieldKind, FormField,
    FormSpec, Goto, Identity, KeyHelp, LinkSpec, RequestSpec, ScreenSpec, Section, SectionBody,
    Select, Sort, Target,
};

const NAME: &[&str] = &["Name", "name"];
const UPDATED: &[&str] = &["modifiedOn"];
const KINDS: &[(&str, &str)] = &[("true", "▦ dashboard"), ("false", "▸ folder")];
const IS_DASHBOARD: Cond = Cond {
    field: &["isDashboard"],
    any_of: &["true"],
};

static NEW_WORKSPACE: FormSpec = FormSpec {
    title: "New workspace",
    submit: "Create",
    fields: &[FormField::new("name", "Workspace name", FieldKind::Text)
        .required()
        .pattern(NAME_PATTERN, NAME_MESSAGE)
        .max_len(NAME_MAX)],
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

static CHANGE_OWNER: FormSpec = FormSpec {
    title: "Change owner",
    submit: "Change",
    fields: &[
        FormField::new("owner", "New owner (username)", FieldKind::Text)
            .required()
            .default("{owner}")
            .pattern(
                NAME_PATTERN,
                "The backend only accepts usernames made of letters, numbers and hyphens",
            ),
    ],
};

static WORKSPACES: ScreenSpec = ScreenSpec {
    noun: "workspace",
    columns: &[
        Column::new("Name", NAME, Format::Text),
        Column::new("Owner", &["owner"], Format::Text),
        Column::new("Updated", UPDATED, Format::RelTime).width(10),
    ],
    sort: Some(Sort {
        field: UPDATED,
        descending: true,
    }),
    identity: Identity {
        id: &["Id", "id"],
        title: NAME,
    },
    actions: &[
        ActionSpec::new(
            'n',
            "New workspace",
            ActionKind::Form(
                &NEW_WORKSPACE,
                RequestSpec {
                    source: Source::post("workspaces", "/workspace", r#"{"Name":"{name}"}"#),
                    success: "Workspace added",
                    after: After::Refresh,
                },
            ),
        ),
        ActionSpec::new(
            'e',
            "Rename",
            ActionKind::Form(
                &RENAME,
                RequestSpec {
                    source: Source::new(
                        "workspaces",
                        Verb::Patch,
                        "/workspace/{Id}",
                        Some(r#"{"Name":"{name}"}"#),
                    ),
                    success: "Workspace renamed",
                    after: After::Refresh,
                },
            ),
        ),
        ActionSpec::new(
            'd',
            "Delete",
            ActionKind::Request(RequestSpec {
                source: Source::new("workspaces", Verb::Delete, "/workspace/{Id}", None),
                success: "Workspace deleted",
                after: After::Refresh,
            }),
        )
        .destructive(
            "Delete workspace {Name}? Its folders and dashboard copies are not deleted with it.",
        ),
        ActionSpec::new(
            'W',
            "Change owner",
            ActionKind::Form(
                &CHANGE_OWNER,
                RequestSpec {
                    source: Source::new(
                        "workspaces",
                        Verb::Put,
                        "/workspace/{Id}/owner",
                        Some(r#"{"owner":"{owner}"}"#),
                    ),
                    success: "Owner changed",
                    after: After::Refresh,
                },
            ),
        )
        .admin_only(),
    ],
    keys: &[KeyHelp {
        keys: "⏎",
        what: "open the workspace",
    }],
    empty: "No workspaces to show",
    ..ScreenSpec::BLANK
};

static CONTENTS: ScreenSpec = ScreenSpec {
    noun: "item",
    columns: &[
        Column::new("Kind", &["isDashboard"], Format::Label(KINDS)).width(11),
        Column::new("Name", NAME, Format::Text),
        Column::new("Owner", &["owner"], Format::Text),
        Column::new("Updated by", &["modifiedBy"], Format::Text),
        Column::new("Updated", UPDATED, Format::RelTime).width(10),
    ],
    sort: Some(Sort {
        field: UPDATED,
        descending: true,
    }),
    identity: Identity {
        id: &["id"],
        title: NAME,
    },
    detail: Some(DetailSpec {
        // {0} is the workspace being viewed.
        load: Some(Source::get("workspaces", "/workspace/dashboard/{0}/{id}")),
        missing: "This dashboard copy is gone. It may have been deleted.",
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new("Kind", &["isDashboard"], Format::Label(KINDS)),
                DetailField::new("Copied from dashboard", &["Id"], Format::Text).optional(),
                DetailField::new("Tabs", &["tabs"], Format::Count).optional(),
                DetailField::new("Owner", &["owner"], Format::Text),
                DetailField::new("Updated by", &["modifiedBy"], Format::Text),
                DetailField::new("Updated", UPDATED, Format::DateTime),
            ]),
        }],
        ..DetailSpec::BLANK
    }),
    actions: &[
        ActionSpec::new(
            'e',
            "Rename",
            ActionKind::Form(
                &RENAME,
                RequestSpec {
                    source: Source::new(
                        "workspaces",
                        Verb::Patch,
                        "/workspace/{id}",
                        Some(r#"{"Name":"{name}"}"#),
                    ),
                    success: "Renamed",
                    after: After::Refresh,
                },
            ),
        ),
        ActionSpec::new(
            'd',
            "Delete",
            ActionKind::Request(RequestSpec {
                source: Source::new("workspaces", Verb::Delete, "/workspace/{0}/{id}", None),
                success: "Deleted",
                after: After::RefreshAndClose,
            }),
        )
        .destructive("Delete {Name} from this workspace?"),
    ],
    links: &[LinkSpec {
        key: 'D',
        label: "source dashboard",
        goto: Goto {
            target: Target {
                module: "Designer",
                resource: "Dashboards",
            },
            select: Select::Row {
                field: &["Id"],
                from: &["Id"],
            },
        },
        when: Some(IS_DASHBOARD),
    }],
    empty: "No dashboards to show",
    ..ScreenSpec::BLANK
};

static CONTENT_DRILLS: [Drill; 1] = [Drill {
    noun: "workspace",
    id_keys: &["Id", "id"],
    source: Source::get("workspaces", "/workspace/{Id}/contents"),
    screen: Some(&CONTENTS),
}];

pub const MODULE: Module = Module {
    title: "Workspaces",
    icon: "▣",
    resources: &[Resource {
        label: "Workspaces",
        view: View::Grid(GridSpec {
            source: Source::get("workspaces", "/workspace"),
            drills: &CONTENT_DRILLS,
            detail: None,
            screen: Some(&WORKSPACES),
        }),
    }],
};

#[cfg(test)]
mod tests;
