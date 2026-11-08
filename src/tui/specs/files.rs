//! Files: storage connections → folders → files.

use crate::tui::nav::{drill, GridSpec, Module, Resource, Source, View};

const NAME_IDS: &[&str] = &["name", "Name"];

pub const MODULE: Module = Module {
    title: "Files",
    icon: "▤",
    resources: &[Resource {
        label: "Storage",
        view: View::Grid(GridSpec {
            source: Source::post("data", "/storage", r#"{"ConnectionName":"","prefix":""}"#),
            drills: &[
                drill(
                    "connection",
                    NAME_IDS,
                    Source::post(
                        "data",
                        "/storage/listFolders",
                        r#"{"ConnectionName":"{0}","prefix":""}"#,
                    ),
                ),
                drill(
                    "folder",
                    &[],
                    Source::post(
                        "data",
                        "/storage/listFiles",
                        r#"{"ConnectionName":"{0}","prefix":"{1}"}"#,
                    ),
                ),
            ],
            detail: None,
        }),
    }],
};
