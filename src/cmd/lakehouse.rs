use anyhow::Result;
use reqwest::Method;
use serde_json::json;

use crate::cli::{
    BackupAction, GovernAction, SavedQueryAction, SqlAction, TableAction, TableRef, TagsAction,
    WarehouseAction,
};
use crate::cmd::{object_body, read_text_arg, seg, Runtime};
use crate::output::{self, OutputFormat};

const SVC: &str = "lakehouse";

/// `/a/{x}/{y}` with every segment percent-encoded.
fn path(prefix: &str, segments: &[&str]) -> String {
    let mut out = prefix.to_string();
    for segment in segments {
        out.push('/');
        out.push_str(&seg(segment));
    }
    out
}

fn table_query(table: &TableRef) -> Vec<(String, String)> {
    vec![
        ("warehouseName".to_string(), table.warehouse.clone()),
        ("databaseName".to_string(), table.database.clone()),
        ("tableName".to_string(), table.table.clone()),
    ]
}

fn push_opt<T: ToString>(query: &mut Vec<(String, String)>, key: &str, value: &Option<T>) {
    if let Some(value) = value {
        query.push((key.to_string(), value.to_string()));
    }
}

pub async fn warehouse(rt: &Runtime, action: &WarehouseAction) -> Result<()> {
    match action {
        WarehouseAction::List => rt.get(SVC, "/lakehouse/warehouse", &[]).await,
        WarehouseAction::Create {
            name,
            bucket,
            description,
        } => {
            let body =
                json!({"warehouseName": name, "bucketName": bucket, "description": description});
            rt.send(SVC, Method::POST, "/lakehouse/warehouse", body)
                .await
        }
        WarehouseAction::Delete { id } => {
            rt.send_empty(SVC, Method::DELETE, &path("/lakehouse/warehouse", &[id]))
                .await
        }
        WarehouseAction::Describe { id, description } => {
            let url = format!("{}/description", path("/lakehouse/warehouse", &[id]));
            rt.send(
                SVC,
                Method::PATCH,
                &url,
                json!({"description": description}),
            )
            .await
        }
        WarehouseAction::Namespaces { warehouse } => {
            let url = format!("{}/namespaces", path("/lakehouse/warehouse", &[warehouse]));
            rt.get(SVC, &url, &[]).await
        }
        WarehouseAction::Client => rt.get(SVC, "/lakehouse/getClientName", &[]).await,
        WarehouseAction::Account => rt.get(SVC, "/lakehouse/getAccountId", &[]).await,
    }
}

pub async fn sql(rt: &Runtime, action: &SqlAction) -> Result<()> {
    match action {
        SqlAction::Run { sql, wait, body } => {
            let mut extra = json!({"waitForOutput": wait});
            if let Some(raw) = sql {
                extra["query"] = json!(read_text_arg(raw)?);
            }
            rt.call(SVC, Method::POST, "/lakehouse/runquery", body, Some(extra))
                .await
        }
        SqlAction::Submit {
            sql,
            session,
            warehouse,
            page_size,
            background,
        } => {
            let mut body = json!({
                "query": read_text_arg(sql)?,
                "sessionId": session,
                "catalog": warehouse,
                "asyncQuery": background,
            });
            if let Some(size) = page_size {
                body["pageSize"] = json!(size);
            }
            let value = rt
                .http
                .request_json(SVC, Method::POST, "/lakehouse/submitquery", &[], Some(body))
                .await?;
            let execution = value["queryExecutionId"].as_str().map(str::to_string);
            rt.show(value).await?;
            // Table output unwraps `response`, which would hide the id needed next.
            if let (Some(id), OutputFormat::Table) = (execution, rt.format) {
                output::info(&format!(
                    "execution {id} · `gz sql status {session} {id}` · `gz sql results {session} {id} --warehouse {warehouse}`"
                ));
            }
            Ok(())
        }
        SqlAction::Status { session, execution } => {
            rt.get(
                SVC,
                &path("/lakehouse/querystatus", &[session, execution]),
                &[],
            )
            .await
        }
        SqlAction::Cancel { session, execution } => {
            rt.send_empty(
                SVC,
                Method::POST,
                &path("/lakehouse/cancelquery", &[session, execution]),
            )
            .await
        }
        SqlAction::Results {
            session,
            execution,
            warehouse,
            page,
            page_size,
        } => {
            let query = vec![
                ("catalog".to_string(), warehouse.clone()),
                ("pageNumber".to_string(), page.to_string()),
                ("pageSize".to_string(), page_size.to_string()),
            ];
            rt.get(
                SVC,
                &path("/lakehouse/queryresults", &[session, execution]),
                &query,
            )
            .await
        }
        SqlAction::SessionState { session } => {
            rt.get(SVC, &path("/lakehouse/sessionstate", &[session]), &[])
                .await
        }
        SqlAction::QueryTime {
            session_id,
            execution_id,
        } => {
            rt.get(
                SVC,
                &path("/lakehouse/querytime", &[session_id, execution_id]),
                &[],
            )
            .await
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
            rt.get(SVC, &path("/lakehouse/sessionstatus", &[session_id]), &[])
                .await
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
        SavedQueryAction::List => rt.get(SVC, "/lakehouse/query", &[]).await,
        SavedQueryAction::Get { id } => rt.get(SVC, &format!("/lakehouse/query/{id}"), &[]).await,
        SavedQueryAction::Create { body } => {
            rt.call(SVC, Method::POST, "/lakehouse/query", body, None)
                .await
        }
        SavedQueryAction::Update { id, sql, body } => {
            let extra = object_body(vec![("query", json!(read_text_arg(sql)?))]);
            rt.call(
                SVC,
                Method::POST,
                &format!("/lakehouse/query/{id}"),
                body,
                Some(extra),
            )
            .await
        }
        SavedQueryAction::Delete { id } => {
            rt.send_empty(SVC, Method::DELETE, &format!("/lakehouse/query/{id}"))
                .await
        }
        SavedQueryAction::Rename { id, name } => {
            rt.send(
                SVC,
                Method::POST,
                &format!("/lakehouse/query/rename/{id}"),
                json!({"name": name}),
            )
            .await
        }
        SavedQueryAction::ByWarehouse { id } => {
            rt.get(SVC, &format!("/lakehouse/query/warehouse/{id}"), &[])
                .await
        }
    }
}

pub async fn table(rt: &Runtime, action: &TableAction) -> Result<()> {
    let post = |url: &'static str, body| rt.call(SVC, Method::POST, url, body, None);
    match action {
        TableAction::Schemas { warehouse } => {
            rt.get(SVC, &path("/lakehouse/schema", &[warehouse]), &[])
                .await
        }
        TableAction::Namespace {
            warehouse,
            namespace,
        } => {
            rt.get(
                SVC,
                &path("/lakehouse/schema", &[warehouse, namespace]),
                &[],
            )
            .await
        }
        TableAction::Describe {
            warehouse,
            namespace,
            table,
        } => {
            rt.get(
                SVC,
                &path("/lakehouse/schema", &[warehouse, namespace, table]),
                &[],
            )
            .await
        }
        TableAction::Props {
            warehouse,
            namespace,
            table,
        } => {
            let url = path("/lakehouse/tableproperties", &[warehouse, namespace, table]);
            rt.get(SVC, &url, &[]).await
        }
        TableAction::Sample {
            warehouse,
            namespace,
            table,
        } => {
            let url = path("/lakehouse/samplerecords", &[warehouse, namespace, table]);
            rt.get(SVC, &url, &[]).await
        }
        TableAction::Count {
            warehouse,
            namespace,
        } => {
            rt.get(
                SVC,
                &path("/lakehouse/tablecount", &[warehouse, namespace]),
                &[],
            )
            .await
        }
        TableAction::DbSize { body } => post("/lakehouse/dbsize", body).await,
        TableAction::ExportSchema { body } => post("/lakehouse/exportschema", body).await,
        TableAction::ImportSchema { body } => post("/lakehouse/importschema", body).await,
        TableAction::ConnectionSchema { body } => post("/lakehouse/connectionschema", body).await,
        TableAction::SetDescription { body } => post("/lakehouse/tabledescription", body).await,
        TableAction::Settings { body } => post("/lakehouse/settings", body).await,
        TableAction::Compact { body } => post("/lakehouse/compaction", body).await,
        TableAction::Optimize { body } => post("/lakehouse/storageoptimization", body).await,
        TableAction::ExpireSnapshots { body } => post("/iceberg/expire-snapshots", body).await,
        TableAction::Commit { body } => post("/iceberg/commit", body).await,
        TableAction::DeleteDb { body } => post("/lakehouse/deleteDB", body).await,
        TableAction::Credentials { body } => post("/iceberg/credentials", body).await,
        TableAction::AuditLogs {
            warehouse,
            limit,
            cursor,
            lookback_days,
        } => {
            let mut query = Vec::new();
            push_opt(&mut query, "limit", limit);
            push_opt(&mut query, "cursor", cursor);
            push_opt(&mut query, "lookbackDays", lookback_days);
            rt.get(SVC, &path("/lakehouse/auditlogs", &[warehouse]), &query)
                .await
        }
        TableAction::MaintainAuditLogs {
            warehouse,
            older_than_days,
            retain_last,
        } => {
            let mut body = json!({});
            if let Some(days) = older_than_days {
                body["olderThanDays"] = json!(days);
            }
            if let Some(keep) = retain_last {
                body["retainLast"] = json!(keep);
            }
            let url = format!("{}/maintain", path("/lakehouse/auditlogs", &[warehouse]));
            rt.send(SVC, Method::POST, &url, body).await
        }
    }
}

pub async fn backup(rt: &Runtime, action: &BackupAction) -> Result<()> {
    let call = |method: Method, url: &'static str, body| rt.call(SVC, method, url, body, None);
    match action {
        BackupAction::Create { body } => call(Method::POST, "/lakehouse/dbbackup", body).await,
        BackupAction::Restore { body } => call(Method::POST, "/lakehouse/dbrestore", body).await,
        BackupAction::Export { body } => call(Method::POST, "/lakehouse/exportDataset", body).await,
        BackupAction::List { warehouse } => {
            rt.get(SVC, &path("/lakehouse/listbackups", &[warehouse]), &[])
                .await
        }
        BackupAction::Delete { name } => {
            rt.send_empty(
                SVC,
                Method::DELETE,
                &path("/lakehouse/deleteBackups", &[name]),
            )
            .await
        }
        BackupAction::Status { warehouse } => {
            rt.get(SVC, &path("/lakehouse/backupStatus", &[warehouse]), &[])
                .await
        }
        BackupAction::Statuses { warehouse } => {
            rt.get(SVC, &path("/lakehouse/backupStatuses", &[warehouse]), &[])
                .await
        }
        BackupAction::StatusByName { backup_name } => {
            rt.send(
                SVC,
                Method::POST,
                "/lakehouse/backupStatus",
                json!({"backupName": backup_name}),
            )
            .await
        }
        BackupAction::UpdateStatus { body } => {
            call(Method::PATCH, "/lakehouse/backupStatus", body).await
        }
        BackupAction::MarkStale { body } => {
            call(Method::POST, "/lakehouse/backupStatuses/markStale", body).await
        }
        BackupAction::Schedules {
            warehouse,
            database,
            status,
        } => {
            let url = match database {
                Some(db) => path("/lakehouse/schedules", &[warehouse, db]),
                None => path("/lakehouse/schedules", &[warehouse]),
            };
            let mut query = Vec::new();
            push_opt(&mut query, "status", status);
            rt.get(SVC, &url, &query).await
        }
        BackupAction::Schedule { body } => {
            call(Method::POST, "/lakehouse/scheduleBackup", body).await
        }
        BackupAction::Unschedule { body } => {
            call(Method::DELETE, "/lakehouse/scheduleBackup", body).await
        }
        BackupAction::Pause { body } => call(Method::POST, "/lakehouse/pauseSchedule", body).await,
        BackupAction::Resume { body } => {
            call(Method::POST, "/lakehouse/resumeSchedule", body).await
        }
        BackupAction::Start { body } => call(Method::POST, "/lakehouse/startBackup", body).await,
    }
}

pub async fn govern(rt: &Runtime, action: &GovernAction) -> Result<()> {
    const POLICY: &str = "/lakehouse/governance/table-policy";
    const PERMISSIONS: &str = "/lakehouse/governance/table-permissions";
    let call = |method: Method, url: &str, body| {
        let url = url.to_string();
        async move { rt.call(SVC, method, &url, body, None).await }
    };
    match action {
        GovernAction::GetPolicy { table } => rt.get(SVC, POLICY, &table_query(table)).await,
        GovernAction::SetPolicy { body } => call(Method::PUT, POLICY, body).await,
        GovernAction::DeletePolicy { body } => call(Method::DELETE, POLICY, body).await,
        GovernAction::PreviewPolicy { body } => {
            call(Method::POST, &format!("{POLICY}/preview"), body).await
        }
        GovernAction::PolicyHistory { table, limit } => {
            let mut query = table_query(table);
            push_opt(&mut query, "limit", limit);
            rt.get(SVC, &format!("{POLICY}/history"), &query).await
        }
        GovernAction::RollbackPolicy { body } => {
            call(Method::POST, &format!("{POLICY}/rollback"), body).await
        }
        GovernAction::GetPermissions { table } => {
            rt.get(SVC, PERMISSIONS, &table_query(table)).await
        }
        GovernAction::SetPermissions { body } => call(Method::PUT, PERMISSIONS, body).await,
        GovernAction::PreviewPermissions { body } => {
            call(Method::POST, &format!("{PERMISSIONS}/preview"), body).await
        }
        GovernAction::Lineage {
            table,
            direction,
            depth,
            limit,
        } => {
            let mut query = table_query(table);
            query.push(("direction".to_string(), direction.clone()));
            push_opt(&mut query, "depth", depth);
            push_opt(&mut query, "limit", limit);
            rt.get(SVC, "/lakehouse/governance/lineage/table", &query)
                .await
        }
        GovernAction::GovernedTables { warehouse } => {
            let url = path("/lakehouse/governance/governed-tables", &[warehouse]);
            rt.get(SVC, &url, &[]).await
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
            rt.get(SVC, &path("/lakehouse/tags", &[warehouse]), &[])
                .await
        }
        TagsAction::Tag { warehouse, body } => {
            rt.call(
                SVC,
                Method::POST,
                &path("/lakehouse/tags", &[warehouse]),
                body,
                None,
            )
            .await
        }
        TagsAction::Untag { warehouse, key } => {
            rt.send_empty(
                SVC,
                Method::DELETE,
                &path("/lakehouse/tags", &[warehouse, key]),
            )
            .await
        }
        TagsAction::TableList {
            warehouse,
            namespace,
            table,
        } => {
            rt.get(SVC, &table_tags_path(warehouse, namespace, table), &[])
                .await
        }
        TagsAction::TableTag {
            warehouse,
            namespace,
            table,
            body,
        } => {
            let url = table_tags_path(warehouse, namespace, table);
            rt.call(SVC, Method::POST, &url, body, None).await
        }
        TagsAction::TableUntag {
            warehouse,
            namespace,
            table,
            key,
        } => {
            let url = format!(
                "{}/{}",
                table_tags_path(warehouse, namespace, table),
                seg(key)
            );
            rt.send_empty(SVC, Method::DELETE, &url).await
        }
        TagsAction::Reindex { warehouse } => {
            let url = format!("{}/reindex", path("/lakehouse/tags", &[warehouse]));
            rt.send_empty(SVC, Method::POST, &url).await
        }
        TagsAction::Search {
            warehouse,
            tag_key,
            tag_value,
            namespace,
            limit,
            offset,
        } => {
            let mut query = vec![("tagKey".to_string(), tag_key.clone())];
            push_opt(&mut query, "tagValue", tag_value);
            push_opt(&mut query, "namespace", namespace);
            push_opt(&mut query, "limit", limit);
            push_opt(&mut query, "offset", offset);
            let url = format!("{}/search", path("/lakehouse/tags", &[warehouse]));
            rt.get(SVC, &url, &query).await
        }
        TagsAction::Retention {
            warehouse,
            namespace,
            table,
        } => {
            rt.get(SVC, &retention_path(warehouse, namespace, table), &[])
                .await
        }
        TagsAction::SetRetention {
            warehouse,
            namespace,
            table,
            body,
        } => {
            let url = retention_path(warehouse, namespace, table);
            rt.call(SVC, Method::POST, &url, body, None).await
        }
        TagsAction::ClearRetention {
            warehouse,
            namespace,
            table,
        } => {
            rt.send_empty(
                SVC,
                Method::DELETE,
                &retention_path(warehouse, namespace, table),
            )
            .await
        }
    }
}

fn table_tags_path(warehouse: &str, namespace: &str, table: &str) -> String {
    format!(
        "{}/tables/{}",
        path("/lakehouse/tags", &[warehouse]),
        [seg(namespace), seg(table)].join("/")
    )
}

fn retention_path(warehouse: &str, namespace: &str, table: &str) -> String {
    format!(
        "{}/tables/{}",
        path("/iceberg/retention", &[warehouse]),
        [seg(namespace), seg(table)].join("/")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_encoded_paths() {
        assert_eq!(
            path("/lakehouse/schema", &["wh", "my db"]),
            "/lakehouse/schema/wh/my%20db"
        );
        assert_eq!(
            table_tags_path("wh", "ns", "t/1"),
            "/lakehouse/tags/wh/tables/ns/t%2F1"
        );
        assert_eq!(
            retention_path("wh", "ns", "t"),
            "/iceberg/retention/wh/tables/ns/t"
        );
    }
}
