//! Files: storage connections, then one browser per connection with the
//! folders first and the files after, at any depth.
//!
//! Verified contracts (TUI research, "Files › Storage" and "File actions"):
//! - `POST data /storage {"ConnectionName":"","prefix":""}` → `{success, data: [{Id, Name, type, …}]}`
//!   (AWSS3 / AZURE_BLOB the caller may describe; DP app.py:767-780)
//! - `POST data /storage/listFolders|listFiles {ConnectionName, prefix}` → `{folders: [name]}` /
//!   `{files: [{fullPath, name, lastModified, size}]}` (DP app.py:865-908, AWSS3.py:100-154).
//!   A key starting with `/` shows as a `""` folder at the root; its prefix is `/`.
//! - download `POST files /api/storage/getFile {ConnectionName, fileNameWithPrefix}` → bytes;
//!   upload `POST files /api/storage/uploadFile` multipart (`data` JSON part + `file`), which
//!   silently overwrites (FM blueprints/storage.py:20-85) — both through `gz fs`'s client calls
//! - `POST data /storage/createFolder {ConnectionName, folderNameWithPrefix}`,
//!   `/storage/deleteFiles {ConnectionName, fileNamesWithPrefix: [..]}`,
//!   `/storage/deleteFolders {ConnectionName, folderNamesWithPrefix: [..]}` — folder delete is
//!   **recursive** and fails above 1000 objects (DP app.py:910-1085, AWSS3.py:276-308)

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::http::ApiClient;
use crate::tui::compose::{Combined, Compose, Parts};
use crate::tui::errors::{self, Subject};
use crate::tui::format::{text_of, Format};
use crate::tui::nav::{drill, Drill, GridSpec, Module, Resource, Source, View};
use crate::tui::screen::{
    ActionKind, ActionSpec, After, Column, Cond, DetailField, DetailSpec, FieldKind, FormField,
    FormSpec, Goto, Identity, KeyHelp, LinkSpec, RequestSpec, ScreenSpec, Section, SectionBody,
    Select, Sort, Target,
};
use crate::tui::views::Ctx;

/// Downloads are buffered in memory (by the file manager and by gz), so
/// bigger files go through `gz fs get` instead.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

const FOLDER: Cond = Cond {
    field: &["kind"],
    any_of: &["folder"],
};
const FILE: Cond = Cond {
    field: &["kind"],
    any_of: &["file"],
};

// ---------- connections ----------

/// A bucket is the root folder of its connection, so one drill (and one set
/// of templates) serves every level.
fn bucket_as_folder(bucket: &mut Value, _now_ms: i64) {
    bucket["kind"] = json!("folder");
    bucket["path"] = json!("");
    bucket["ConnectionName"] = bucket["Name"].clone();
}

pub static BUCKETS: ScreenSpec = ScreenSpec {
    noun: "bucket",
    columns: &[
        Column::new("Bucket", &["Name"], Format::Text),
        Column::new(
            "Type",
            &["type"],
            Format::Label(crate::tui::specs::connections::TYPE_LABELS),
        ),
        Column::new("Updated", &["modified_on"], Format::RelTime).width(10),
    ],
    sort: Some(Sort {
        field: &["Name"],
        descending: false,
    }),
    identity: Identity {
        id: &["Name"],
        title: &["Name"],
    },
    links: &[LinkSpec {
        key: 'c',
        label: "connection",
        goto: Goto {
            target: Target {
                module: "Connections",
                resource: "Connections",
            },
            select: Select::Row {
                field: &["Id"],
                from: &["Id"],
            },
        },
        when: None,
    }],
    empty: "No buckets to show",
    enrich: Some(bucket_as_folder),
    ..ScreenSpec::BLANK
};

// ---------- folder contents ----------

/// Folders (bare names) then files, each carrying its connection and the
/// folder it was listed in, so actions and deeper drills can name them.
fn folder_contents(parts: &Parts) -> Combined {
    let connection = parts.parent["ConnectionName"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let dir = parts.parent["path"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut rows: Vec<Value> = parts
        .rows(0)
        .iter()
        .map(|folder| {
            let name = text_of(folder);
            json!({
                "kind": "folder",
                "label": format!("{name}/"),
                "name": name,
                "path": format!("{dir}{name}/"),
                "dir": dir,
                "ConnectionName": connection,
            })
        })
        .collect();
    rows.extend(parts.rows(1).into_iter().map(|mut file| {
        file["kind"] = json!("file");
        file["label"] = file["name"].clone();
        file["dir"] = json!(dir);
        file["ConnectionName"] = json!(connection);
        file
    }));
    let failures = parts
        .answers
        .iter()
        .zip(["folders", "files"])
        .filter_map(|(answer, what)| {
            let err = answer.as_ref().err()?;
            Some(errors::explain(
                err,
                Subject {
                    what: &format!("this folder's {what}"),
                    service: "data",
                },
            ))
        })
        .collect();
    Combined { rows, failures }
}

static CONTENTS: Compose = Compose {
    with: &[Source::post(
        "data",
        "/storage/listFiles",
        r#"{"ConnectionName":"{ConnectionName}","prefix":"{path}"}"#,
    )],
    expand: None,
    combine: folder_contents,
};

const DOWNLOAD_FORM: FormSpec = FormSpec {
    title: "Download",
    submit: "Download",
    fields: &[FormField::new("dest", "Save as", FieldKind::Text)
        .required()
        .default("./{name}")
        .hint("An existing file is never overwritten")],
};

const DOWNLOAD: ActionSpec = ActionSpec::new(
    'w',
    "Download",
    ActionKind::FormCustom(&DOWNLOAD_FORM, download),
)
.when(FILE);

fn download(row: &Value, values: &Value, ctx: &mut Ctx) {
    let size = row["size"].as_u64().unwrap_or(0);
    if size > MAX_DOWNLOAD_BYTES {
        return ctx.toast(
            crate::tui::app::Tone::Error,
            format!(
                "This file is too big to download here; use `gz fs get --connection {} --path {}`",
                text_of(&row["ConnectionName"]),
                text_of(&row["fullPath"])
            ),
        );
    }
    let dest = PathBuf::from(values["dest"].as_str().unwrap_or_default());
    let body = json!({
        "ConnectionName": row["ConnectionName"],
        "fileNameWithPrefix": row["fullPath"],
    });
    ctx.run_task("download a file", move |client: ApiClient| {
        let (dest, body) = (dest.clone(), body.clone());
        async move { download_to(client, body, &dest).await }
    });
}

async fn download_to(client: ApiClient, body: Value, dest: &Path) -> Result<Value, String> {
    if dest.exists() {
        return Err(format!(
            "{} already exists; pick another name",
            dest.display()
        ));
    }
    let bytes = client
        .download("files", "/api/storage/getFile", Some(body), dest)
        .await
        .map_err(|e| format!("Download failed: {e:#}"))?;
    let size = crate::tui::format::show(Some(&json!(bytes)), Format::Bytes, 0).text;
    Ok(json!({"message": format!("Saved {} ({size})", dest.display())}))
}

const UPLOAD_FORM: FormSpec = FormSpec {
    title: "Upload a file here",
    submit: "Upload",
    fields: &[FormField::new("file", "Local file", FieldKind::Text)
        .required()
        .hint("A file with the same name in this folder is replaced")],
};

const UPLOAD: ActionSpec = ActionSpec::new(
    'u',
    "Upload file here",
    ActionKind::FormCustom(&UPLOAD_FORM, upload),
);

fn upload(row: &Value, values: &Value, ctx: &mut Ctx) {
    let file = PathBuf::from(values["file"].as_str().unwrap_or_default());
    if !file.is_file() {
        return ctx.toast(
            crate::tui::app::Tone::Error,
            format!("{} isn't a readable file", file.display()),
        );
    }
    // The file manager stores the upload at `<filePath>/<file name>`.
    let dir = row["dir"].as_str().unwrap_or_default().trim_matches('/');
    let meta = json!({"ConnectionName": row["ConnectionName"], "filePath": dir});
    ctx.run_task("upload a file", move |client: ApiClient| {
        let (meta, file) = (meta.clone(), file.clone());
        async move {
            client
                .upload_file(
                    "files",
                    "/api/storage/uploadFile",
                    &meta,
                    &file,
                    "file",
                    Some("data"),
                )
                .await
                .map_err(|e| format!("Upload failed: {e:#}"))?;
            Ok(json!({"message": "1 file uploaded successfully"}))
        }
    });
}

const NEW_FOLDER_FORM: FormSpec = FormSpec {
    title: "New folder here",
    submit: "Create",
    fields: &[FormField::new("folder", "Name", FieldKind::Text)
        .required()
        .pattern(
            r"[^/\s]([^/]*[^/\s])?",
            "No slashes, and no spaces at either end",
        )],
};

const NEW_FOLDER: ActionSpec = ActionSpec::new(
    'n',
    "New folder",
    ActionKind::Form(
        &NEW_FOLDER_FORM,
        RequestSpec {
            source: Source::post(
                "data",
                "/storage/createFolder",
                r#"{"ConnectionName":"{ConnectionName}","folderNameWithPrefix":"{dir}{folder}"}"#,
            ),
            success: "Folder created successfully",
            after: After::Refresh,
        },
    ),
);

const DELETE_FILE: ActionSpec = ActionSpec::new(
    'd',
    "Delete file",
    ActionKind::Request(RequestSpec {
        source: Source::post(
            "data",
            "/storage/deleteFiles",
            r#"{"ConnectionName":"{ConnectionName}","fileNamesWithPrefix":["{fullPath}"]}"#,
        ),
        success: "File deleted successfully",
        after: After::RefreshAndClose,
    }),
)
.destructive("Delete {fullPath} from {ConnectionName}? This can't be undone.")
.when(FILE);

const DELETE_FOLDER: ActionSpec = ActionSpec::new(
    'x',
    "Delete folder and everything in it",
    ActionKind::Request(RequestSpec {
        source: Source::post(
            "data",
            "/storage/deleteFolders",
            r#"{"ConnectionName":"{ConnectionName}","folderNamesWithPrefix":["{path}"]}"#,
        ),
        success: "Folder deleted successfully",
        after: After::Refresh,
    }),
)
.destructive(
    "Delete folder {path} and EVERYTHING inside it, recursively? This can't be undone. (Folders holding over 1000 objects fail to delete.)",
)
.when(FOLDER);

pub static CONTENTS_SCREEN: ScreenSpec = ScreenSpec {
    noun: "item",
    columns: &[
        Column::new("Name", &["label"], Format::Text),
        Column::new("Size", &["size"], Format::Bytes).width(9),
        Column::new("Modified", &["lastModified"], Format::RelTime).width(10),
    ],
    // Folders first (rows arrive that way), then the user's own sort.
    sort: None,
    identity: Identity {
        id: &["path", "fullPath"],
        title: &["label"],
    },
    detail: Some(DetailSpec {
        subtitle: &[(&["ConnectionName"], Format::Text)],
        sections: &[Section {
            title: "File",
            body: SectionBody::Fields(&[
                DetailField::new("Connection", &["ConnectionName"], Format::Text),
                DetailField::new("Path", &["fullPath", "path"], Format::Text),
                DetailField::new("Size", &["size"], Format::Bytes),
                DetailField::new("Bytes", &["size"], Format::Number),
                DetailField::new("Modified", &["lastModified"], Format::RelTime),
                DetailField::new("Modified at", &["lastModified"], Format::DateTime),
            ]),
        }],
        ..DetailSpec::BLANK
    }),
    actions: &[DOWNLOAD, UPLOAD, NEW_FOLDER, DELETE_FILE, DELETE_FOLDER],
    keys: &[KeyHelp {
        keys: "⏎",
        what: "open a folder / a file's details",
    }],
    compose: Some(&CONTENTS),
    empty: "This folder is empty",
    ..ScreenSpec::BLANK
};

/// Every folder (a bucket is the root one) opens the same way, at any depth.
static STORAGE_DRILLS: &[Drill] = &[Drill {
    screen: Some(&CONTENTS_SCREEN),
    when: Some(FOLDER),
    repeats: true,
    ..drill(
        "folder",
        &["path", "Name"],
        Source::post(
            "data",
            "/storage/listFolders",
            r#"{"ConnectionName":"{ConnectionName}","prefix":"{path}"}"#,
        ),
    )
}];

pub const MODULE: Module = Module {
    title: "Files",
    icon: "▤",
    resources: &[Resource {
        label: "Storage",
        view: View::Grid(GridSpec {
            source: Source::post("data", "/storage", r#"{"ConnectionName":"","prefix":""}"#),
            drills: STORAGE_DRILLS,
            detail: None,
            screen: Some(&BUCKETS),
        }),
    }],
};

#[cfg(test)]
mod tests;
