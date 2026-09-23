use anyhow::Result;
use reqwest::Method;
use serde_json::json;
use std::path::Path;

use crate::cli::{DataAction, FsAction, SitesAction};
use crate::cmd::{merge_body, object_body, parse_query, Runtime};
use crate::output;

pub async fn fs(rt: &Runtime, action: &FsAction) -> Result<()> {
    match action {
        FsAction::Ls { connection, prefix } => {
            let body = object_body(vec![
                ("ConnectionName", json!(connection)),
                ("prefix", json!(prefix)),
            ]);
            let value = rt
                .http
                .request_json(
                    "data",
                    Method::POST,
                    "/storage/getFilesList",
                    &[],
                    Some(body),
                )
                .await?;
            rt.show(value).await
        }
        FsAction::Lsd { connection, prefix } => {
            let body = object_body(vec![
                ("ConnectionName", json!(connection)),
                ("prefix", json!(prefix)),
            ]);
            let value = rt
                .http
                .request_json(
                    "data",
                    Method::POST,
                    "/storage/listFolders",
                    &[],
                    Some(body),
                )
                .await?;
            rt.show(value).await
        }
        FsAction::List { connection, prefix } => {
            let body = object_body(vec![
                ("ConnectionName", json!(connection)),
                ("prefix", json!(prefix)),
            ]);
            let value = rt
                .http
                .request_json("data", Method::POST, "/storage/listFiles", &[], Some(body))
                .await?;
            rt.show(value).await
        }
        FsAction::Get {
            connection,
            path,
            out,
        } => {
            let body = object_body(vec![
                ("ConnectionName", json!(connection)),
                ("fileNameWithPrefix", json!(path)),
            ]);
            let dest = match out {
                Some(explicit) => Path::new(explicit).to_path_buf(),
                None => Path::new(path)
                    .file_name()
                    .map(Path::new)
                    .unwrap_or_else(|| Path::new("download.bin"))
                    .to_path_buf(),
            };
            let bytes = rt
                .http
                .download("files", "/api/storage/getFile", &[], Some(body), &dest)
                .await?;
            if rt.format == crate::output::OutputFormat::Table {
                output::success(&format!("saved {} ({} bytes)", dest.display(), bytes));
            } else {
                rt.show(json!({"path": dest.display().to_string(), "bytes": bytes}))
                    .await?;
            }
            Ok(())
        }
        FsAction::Put {
            connection,
            path,
            file,
        } => {
            let meta = object_body(vec![
                ("ConnectionName", json!(connection)),
                ("filePath", json!(path)),
            ]);
            let value = rt
                .http
                .upload_file(
                    "files",
                    "/api/storage/uploadFile",
                    &meta,
                    Path::new(file),
                    "file",
                    Some("data"),
                )
                .await?;
            rt.show(value).await
        }
        FsAction::Mkdir { connection, path } => {
            let body = object_body(vec![
                ("ConnectionName", json!(connection)),
                ("filePath", json!(path)),
            ]);
            let value = rt
                .http
                .request_json(
                    "data",
                    Method::POST,
                    "/storage/createFolder",
                    &[],
                    Some(body),
                )
                .await?;
            rt.show(value).await
        }
        FsAction::Rm {
            connection,
            keys,
            body,
        } => {
            let mut extra = object_body(vec![("ConnectionName", json!(connection))]);
            if !keys.is_empty() {
                extra["fileKeys"] = json!(keys);
            }
            let payload = merge_body(body.data.as_deref(), Some(extra))?;
            let query = parse_query(&body.query)?;
            let value = rt
                .http
                .request_json(
                    "data",
                    Method::POST,
                    "/storage/deleteFiles",
                    &query,
                    payload,
                )
                .await?;
            rt.show(value).await
        }
        FsAction::Rmdir {
            connection,
            keys,
            body,
        } => {
            let mut extra = object_body(vec![("ConnectionName", json!(connection))]);
            if !keys.is_empty() {
                extra["folderKeys"] = json!(keys);
            }
            let payload = merge_body(body.data.as_deref(), Some(extra))?;
            let query = parse_query(&body.query)?;
            let value = rt
                .http
                .request_json(
                    "data",
                    Method::POST,
                    "/storage/deleteFolders",
                    &query,
                    payload,
                )
                .await?;
            rt.show(value).await
        }
        FsAction::RteStatus => {
            let value = rt
                .http
                .request_json(
                    "files",
                    Method::GET,
                    "/api/customRte/getCustomRteStatus",
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        FsAction::RtePut { file, body } => {
            let meta = merge_body(body.data.as_deref(), None)?.unwrap_or_else(|| json!({}));
            let value = rt
                .http
                .upload_file(
                    "files",
                    "/api/customRte/uploadFile",
                    &meta,
                    Path::new(file),
                    "file",
                    Some("data"),
                )
                .await?;
            rt.show(value).await
        }
    }
}

pub async fn data(rt: &Runtime, action: &DataAction) -> Result<()> {
    match action {
        DataAction::Schema { body } => {
            rt.call("data", Method::POST, "/getSchema", body, None)
                .await
        }
        DataAction::IcebergSchema { body } => {
            rt.call("data", Method::POST, "/getSchema/iceberg", body, None)
                .await
        }
        DataAction::Test { body } => {
            rt.call("data", Method::POST, "/testConnection", body, None)
                .await
        }
        DataAction::Read { body } => rt.call("data", Method::POST, "/getdata", body, None).await,
        DataAction::Execution { body } => {
            rt.call("data", Method::POST, "/getdata/getExecution", body, None)
                .await
        }
        DataAction::Fingerprint { body } => {
            rt.call(
                "data",
                Method::POST,
                "/getdata/policyFingerprint",
                body,
                None,
            )
            .await
        }
        DataAction::OData { body } => {
            rt.call("data", Method::POST, "/Odata/getdata", body, None)
                .await
        }
        DataAction::ODataMeta { body } => {
            rt.call("data", Method::POST, "/Odata/metadata", body, None)
                .await
        }
    }
}

pub async fn sites(rt: &Runtime, action: &SitesAction) -> Result<()> {
    match action {
        SitesAction::Set { body } => {
            rt.call("files", Method::POST, "/api/siteSetting/", body, None)
                .await
        }
        SitesAction::Get { name } => {
            let value = rt
                .http
                .request_json(
                    "files",
                    Method::GET,
                    &format!("/api/siteSetting/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
    }
}
