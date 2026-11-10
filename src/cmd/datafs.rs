use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::cli::{DataAction, FsAction, SitesAction};
use crate::cmd::{read_data_arg, seg, Runtime};
use crate::output::{self, OutputFormat};

pub async fn fs(rt: &Runtime, action: &FsAction) -> Result<()> {
    match action {
        FsAction::Connections => {
            let body = json!({"ConnectionName": "", "prefix": ""});
            rt.send("data", Method::POST, "/storage", body).await
        }
        FsAction::Ls { connection, prefix } => {
            let body = json!({"ConnectionName": connection, "prefix": prefix});
            rt.send("data", Method::POST, "/storage/listFiles", body)
                .await
        }
        FsAction::Lsd { connection, prefix } => {
            let body = json!({"ConnectionName": connection, "prefix": prefix});
            rt.send("data", Method::POST, "/storage/listFolders", body)
                .await
        }
        FsAction::ListAll { connection } => {
            let body = json!({"ConnectionName": connection});
            rt.send("data", Method::POST, "/storage/getFilesList", body)
                .await
        }
        FsAction::Get {
            connection,
            path,
            out,
        } => download(rt, connection, path, out.as_deref()).await,
        FsAction::Put {
            connection,
            dir,
            file,
        } => {
            // The file manager stores the upload at `<filePath>/<file name>`.
            let meta = json!({"ConnectionName": connection, "filePath": dir.trim_matches('/')});
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
            let body = json!({"ConnectionName": connection, "folderNameWithPrefix": path});
            rt.send("data", Method::POST, "/storage/createFolder", body)
                .await
        }
        FsAction::Rm { connection, keys } => {
            let body = json!({"ConnectionName": connection, "fileNamesWithPrefix": keys});
            rt.send("data", Method::POST, "/storage/deleteFiles", body)
                .await
        }
        FsAction::Rmdir { connection, keys } => {
            let body = json!({"ConnectionName": connection, "folderNamesWithPrefix": keys});
            rt.send("data", Method::POST, "/storage/deleteFolders", body)
                .await
        }
        FsAction::RteStatus => {
            rt.get("files", "/api/customRte/getCustomRteStatus", &[])
                .await
        }
        FsAction::RtePut {
            file,
            base_repo,
            tag,
        } => {
            let meta = json!({"baseRepo": base_repo, "tagName": tag});
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

async fn download(rt: &Runtime, connection: &str, key: &str, out: Option<&str>) -> Result<()> {
    let dest = match out {
        Some(explicit) => PathBuf::from(explicit),
        None => Path::new(key)
            .file_name()
            .map(PathBuf::from)
            .context("--path has no file name; pass --out")?,
    };
    let body = json!({"ConnectionName": connection, "fileNameWithPrefix": key});
    let bytes = rt
        .http
        .download("files", "/api/storage/getFile", Some(body), &dest)
        .await?;
    if rt.format == OutputFormat::Table {
        output::success(&format!(
            "saved {} ({})",
            dest.display(),
            human_bytes(bytes)
        ));
        return Ok(());
    }
    rt.show(json!({"path": dest.display().to_string(), "bytes": bytes}))
        .await
}

pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

pub async fn data(rt: &Runtime, action: &DataAction) -> Result<()> {
    let (path, body) = match action {
        DataAction::Schema { body } => ("/getSchema", body),
        DataAction::IcebergSchema { body } => ("/getSchema/iceberg", body),
        DataAction::Test { body } => ("/testConnection", body),
        DataAction::Read { body } => ("/getdata", body),
        DataAction::Execution { body } => ("/getdata/getExecution", body),
        DataAction::Fingerprint { body } => ("/getdata/policyFingerprint", body),
        DataAction::OData { body } => ("/Odata/getdata", body),
        DataAction::ODataMeta { body } => ("/Odata/metadata", body),
    };
    rt.call("data", Method::POST, path, body, None).await
}

/// Site settings (theme and other JSON values) live in the site manager,
/// behind the admin gateway.
pub async fn sites(rt: &Runtime, action: &SitesAction) -> Result<()> {
    match action {
        SitesAction::Set { name, value } => {
            let setting = read_data_arg(Some(value))?.unwrap_or_else(|| json!({}));
            if !setting.is_object() {
                anyhow::bail!("--value must be a JSON object");
            }
            let body = json!({"settingName": name, "settingValue": setting});
            rt.send("admin", Method::POST, "/admin/siteSetting", body)
                .await
        }
        SitesAction::Get { name } => {
            rt.get("admin", &format!("/admin/siteSetting/{}", seg(name)), &[])
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
