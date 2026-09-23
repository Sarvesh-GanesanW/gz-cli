use anyhow::Result;
use reqwest::Method;
use serde_json::json;

use crate::cli::{
    BackupAction, GovernAction, SavedQueryAction, SqlAction, TableAction, TagsAction,
    WarehouseAction,
};
use crate::cmd::{object_body, read_text_arg, Runtime};

const SVC: &str = "lakehouse";

pub async fn warehouse(rt: &Runtime, action: &WarehouseAction) -> Result<()> {
    match action {
        WarehouseAction::List => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/lakehouse/warehouse", &[], None)
                .await?;
            rt.show(value).await
        }
        WarehouseAction::Create { name, body } => {
            rt.call(
                SVC,
                Method::POST,
                "/lakehouse/warehouse",
                body,
                Some(object_body(vec![("warehouseName", json!(name))])),
            )
            .await
        }
        WarehouseAction::Delete { id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/lakehouse/warehouse/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        WarehouseAction::Describe { id, description } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::PATCH,
                    &format!("/lakehouse/warehouse/{id}/description"),
                    &[],
                    Some(json!({"description": description})),
                )
                .await?;
            rt.show(value).await
        }
        WarehouseAction::Namespaces { warehouse } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/warehouse/{warehouse}/namespaces"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        WarehouseAction::Client => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/lakehouse/getClientName", &[], None)
                .await?;
            rt.show(value).await
        }
        WarehouseAction::Account => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/lakehouse/getAccountId", &[], None)
                .await?;
            rt.show(value).await
        }
    }
}

pub async fn sql(rt: &Runtime, action: &SqlAction) -> Result<()> {
    match action {
        SqlAction::Run { sql, wait, body } => {
            let mut pairs = vec![("waitForOutput", json!(wait))];
            if let Some(raw) = sql {
                pairs.push(("query", json!(read_text_arg(raw)?)));
            }
            rt.call(
                SVC,
                Method::POST,
                "/lakehouse/runquery",
                body,
                Some(object_body(pairs)),
            )
            .await
        }
        SqlAction::Submit { sql, body } => {
            let extra = sql
                .as_deref()
                .map(read_text_arg)
                .transpose()?
                .map(|q| object_body(vec![("query", json!(q))]));
            rt.call(SVC, Method::POST, "/lakehouse/submitquery", body, extra)
                .await
        }
        SqlAction::QueryTime {
            session_id,
            execution_id,
        } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/querytime/{session_id}/{execution_id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        SqlAction::StartSession { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/startsession", body, None)
                .await
        }
        SqlAction::StopSession { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/stopsession", body, None)
                .await
        }
        SqlAction::SessionStatus { session_id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/sessionstatus/{session_id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        SqlAction::Shutdown { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/shutdown", body, None)
                .await
        }
        SqlAction::Authorize { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/authorize", body, None)
                .await
        }
        SqlAction::Statement { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/v1/statements", body, None)
                .await
        }
        SqlAction::Saved { action } => saved(rt, action).await,
        SqlAction::TestConnection { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/testconnection", body, None)
                .await
        }
    }
}

async fn saved(rt: &Runtime, action: &SavedQueryAction) -> Result<()> {
    match action {
        SavedQueryAction::List => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/lakehouse/query", &[], None)
                .await?;
            rt.show(value).await
        }
        SavedQueryAction::Get { id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/query/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        SavedQueryAction::Create { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/query", body, None)
                .await
        }
        SavedQueryAction::Update { id, body } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("/lakehouse/query/{id}"),
                body,
                None,
            )
            .await
        }
        SavedQueryAction::Delete { id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/lakehouse/query/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        SavedQueryAction::Rename { id, name } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("/lakehouse/query/rename/{id}"),
                &crate::cli::BodyArgs {
                    data: None,
                    query: vec![],
                },
                Some(json!({"name": name})),
            )
            .await
        }
        SavedQueryAction::ByWarehouse { id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/query/warehouse/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
    }
}

pub async fn table(rt: &Runtime, action: &TableAction) -> Result<()> {
    match action {
        TableAction::Schemas { warehouse } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/schema/{warehouse}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TableAction::Namespace {
            warehouse,
            namespace,
        } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/schema/{warehouse}/{namespace}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TableAction::Describe {
            warehouse,
            namespace,
            table,
        } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/schema/{warehouse}/{namespace}/{table}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TableAction::Props {
            warehouse,
            namespace,
            table,
        } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/tableproperties/{warehouse}/{namespace}/{table}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TableAction::Sample {
            warehouse,
            namespace,
            table,
        } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/samplerecords/{warehouse}/{namespace}/{table}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TableAction::Count {
            warehouse,
            namespace,
        } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/tablecount/{warehouse}/{namespace}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TableAction::DbSize { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/dbsize", body, None)
                .await
        }
        TableAction::ExportSchema { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/exportschema", body, None)
                .await
        }
        TableAction::ImportSchema { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/importschema", body, None)
                .await
        }
        TableAction::ConnectionSchema { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/connectionschema", body, None)
                .await
        }
        TableAction::SetDescription { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/tabledescription", body, None)
                .await
        }
        TableAction::Settings { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/settings", body, None)
                .await
        }
        TableAction::Compact { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/compaction", body, None)
                .await
        }
        TableAction::Optimize { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/lakehouse/storageoptimization",
                body,
                None,
            )
            .await
        }
        TableAction::Snapshots { body } => {
            rt.call("iceberg", Method::POST, "/list_snapshots", body, None)
                .await
        }
        TableAction::Rollback { body } => {
            rt.call("iceberg", Method::POST, "/rollback", body, None)
                .await
        }
        TableAction::ExpireSnapshots { body } => {
            rt.call(SVC, Method::POST, "/iceberg/expire-snapshots", body, None)
                .await
        }
        TableAction::RemoveOrphans { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/iceberg/remove-orphan-files",
                body,
                None,
            )
            .await
        }
        TableAction::Commit { body } => {
            rt.call(SVC, Method::POST, "/iceberg/commit", body, None)
                .await
        }
        TableAction::DeleteDb { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/deleteDB", body, None)
                .await
        }
        TableAction::AuditLogs { warehouse } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/auditlogs/{warehouse}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TableAction::MaintainAuditLogs { warehouse } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::POST,
                    &format!("/lakehouse/auditlogs/{warehouse}/maintain"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TableAction::Credentials { body } => {
            rt.call(SVC, Method::POST, "/iceberg/credentials", body, None)
                .await
        }
    }
}

pub async fn backup(rt: &Runtime, action: &BackupAction) -> Result<()> {
    match action {
        BackupAction::Create { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/dbbackup", body, None)
                .await
        }
        BackupAction::Restore { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/dbrestore", body, None)
                .await
        }
        BackupAction::Export { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/exportDataset", body, None)
                .await
        }
        BackupAction::List { warehouse } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/listbackups/{warehouse}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        BackupAction::Delete { name } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/lakehouse/deleteBackups/{name}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        BackupAction::Status { warehouse } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/backupStatus/{warehouse}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        BackupAction::Statuses { warehouse } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/backupStatuses/{warehouse}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        BackupAction::UpdateStatus { body } => {
            rt.call(SVC, Method::PATCH, "/lakehouse/backupStatus", body, None)
                .await
        }
        BackupAction::RecordStatus { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/backupStatus", body, None)
                .await
        }
        BackupAction::MarkStale { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/lakehouse/backupStatuses/markStale",
                body,
                None,
            )
            .await
        }
        BackupAction::Schedules {
            warehouse,
            database,
        } => {
            let path = match database {
                Some(db) => format!("/lakehouse/schedules/{warehouse}/{db}"),
                None => format!("/lakehouse/schedules/{warehouse}"),
            };
            let value = rt
                .http
                .request_json(SVC, Method::GET, &path, &[], None)
                .await?;
            rt.show(value).await
        }
        BackupAction::Schedule { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/scheduleBackup", body, None)
                .await
        }
        BackupAction::Unschedule { body } => {
            rt.call(SVC, Method::DELETE, "/lakehouse/scheduleBackup", body, None)
                .await
        }
        BackupAction::Pause { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/pauseSchedule", body, None)
                .await
        }
        BackupAction::Resume { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/resumeSchedule", body, None)
                .await
        }
        BackupAction::Start { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/startBackup", body, None)
                .await
        }
    }
}

pub async fn govern(rt: &Runtime, action: &GovernAction) -> Result<()> {
    match action {
        GovernAction::GetPolicy { body } => {
            rt.call(
                SVC,
                Method::GET,
                "/lakehouse/governance/table-policy",
                body,
                None,
            )
            .await
        }
        GovernAction::SetPolicy { body } => {
            rt.call(
                SVC,
                Method::PUT,
                "/lakehouse/governance/table-policy",
                body,
                None,
            )
            .await
        }
        GovernAction::DeletePolicy { body } => {
            rt.call(
                SVC,
                Method::DELETE,
                "/lakehouse/governance/table-policy",
                body,
                None,
            )
            .await
        }
        GovernAction::PreviewPolicy { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/lakehouse/governance/table-policy/preview",
                body,
                None,
            )
            .await
        }
        GovernAction::PolicyHistory { body } => {
            rt.call(
                SVC,
                Method::GET,
                "/lakehouse/governance/table-policy/history",
                body,
                None,
            )
            .await
        }
        GovernAction::RollbackPolicy { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/lakehouse/governance/table-policy/rollback",
                body,
                None,
            )
            .await
        }
        GovernAction::GetPermissions { body } => {
            rt.call(
                SVC,
                Method::GET,
                "/lakehouse/governance/table-permissions",
                body,
                None,
            )
            .await
        }
        GovernAction::SetPermissions { body } => {
            rt.call(
                SVC,
                Method::PUT,
                "/lakehouse/governance/table-permissions",
                body,
                None,
            )
            .await
        }
        GovernAction::PreviewPermissions { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/lakehouse/governance/table-permissions/preview",
                body,
                None,
            )
            .await
        }
        GovernAction::Lineage { body } => {
            rt.call(
                SVC,
                Method::GET,
                "/lakehouse/governance/lineage/table",
                body,
                None,
            )
            .await
        }
        GovernAction::IngestLineage { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/lakehouse/governance/lineage/events",
                body,
                None,
            )
            .await
        }
    }
}

pub async fn tags(rt: &Runtime, action: &TagsAction) -> Result<()> {
    match action {
        TagsAction::Inventory { body } => {
            rt.call(
                SVC,
                Method::GET,
                "/lakehouse/tags/columns/inventory",
                body,
                None,
            )
            .await
        }
        TagsAction::List { warehouse } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/tags/{warehouse}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TagsAction::Tag { warehouse, body } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("/lakehouse/tags/{warehouse}"),
                body,
                None,
            )
            .await
        }
        TagsAction::Untag { warehouse, key } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/lakehouse/tags/{warehouse}/{key}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TagsAction::TableList {
            warehouse,
            namespace,
            table,
        } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/lakehouse/tags/{warehouse}/tables/{namespace}/{table}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TagsAction::TableTag {
            warehouse,
            namespace,
            table,
            body,
        } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("/lakehouse/tags/{warehouse}/tables/{namespace}/{table}"),
                body,
                None,
            )
            .await
        }
        TagsAction::Reindex { warehouse } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::POST,
                    &format!("/lakehouse/tags/{warehouse}/reindex"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TagsAction::Search { warehouse, q, body } => {
            let mut query = body.query.clone();
            if let Some(term) = q {
                query.push(format!("q={term}"));
            }
            let owned = crate::cli::BodyArgs {
                data: body.data.clone(),
                query,
            };
            rt.call(
                SVC,
                Method::GET,
                &format!("/lakehouse/tags/{warehouse}/search"),
                &owned,
                None,
            )
            .await
        }
        TagsAction::Retention {
            warehouse,
            namespace,
            table,
        } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/iceberg/retention/{warehouse}/tables/{namespace}/{table}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        TagsAction::SetRetention {
            warehouse,
            namespace,
            table,
            body,
        } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("/iceberg/retention/{warehouse}/tables/{namespace}/{table}"),
                body,
                None,
            )
            .await
        }
        TagsAction::ClearRetention {
            warehouse,
            namespace,
            table,
        } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/iceberg/retention/{warehouse}/tables/{namespace}/{table}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
    }
}
