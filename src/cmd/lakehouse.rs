use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::cli::{
    BackupAction, GovernAction, SavedQueryAction, ScanArgs, SqlAction, TableAction, TableRef,
    TagsAction, WarehouseAction,
};
use crate::cmd::{merge_body, object_body, read_text_arg, seg, Runtime};
use crate::output::{self, OutputFormat};
use crate::sql::scan_limit::ScanLimit;

const SVC: &str = "lakehouse";
const EXPORT_POLL: Duration = Duration::from_secs(2);
/// A full-result export rarely takes this long; past it something is stuck.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const TERMINAL_QUERY_STATES: &[&str] = &["SUCCEEDED", "FAILED", "CANCELLED"];

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

pub async fn sql(rt: &Runtime, action: &SqlAction, scan: &ScanArgs) -> Result<()> {
    let max_scan = scan.max_scan.as_deref();
    match action {
        SqlAction::Shell(options) => {
            crate::sql::run(rt, options, crate::sql::Source::Interactive, None).await
        }
        SqlAction::RunFile { file, shell } => {
            let source = crate::sql::Source::File(std::path::PathBuf::from(file));
            crate::sql::run(rt, shell, source, max_scan).await
        }
        SqlAction::Run { sql, wait, body } => {
            let mut extra = json!({"waitForOutput": wait});
            if let Some(raw) = sql {
                extra["query"] = json!(read_text_arg(raw)?);
            }
            // Merged once: `--data @-` can only be read from stdin once.
            let payload = merge_body(body.data.as_deref(), Some(extra))?.unwrap_or_default();
            let warehouse = payload["connectionConfig"]["config"]["warehouseName"].as_str();
            let query = payload["query"].as_str().unwrap_or_default();
            check_scan(rt, warehouse, query, max_scan).await?;
            let query_pairs = crate::cmd::parse_query(&body.query)?;
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::POST,
                    "/lakehouse/runquery",
                    &query_pairs,
                    Some(payload),
                )
                .await?;
            rt.show(value).await
        }
        SqlAction::Submit {
            sql,
            session,
            warehouse,
            page_size,
            background,
        } => {
            let query = read_text_arg(sql)?;
            check_scan(rt, Some(warehouse), &query, max_scan).await?;
            let mut body = json!({
                "query": query,
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
        SqlAction::Export {
            session,
            execution,
            warehouse,
            format,
            out,
        } => {
            let target = QueryTarget {
                session,
                execution,
                warehouse,
            };
            export(rt, &target, format, out.as_deref()).await
        }
        SqlAction::Split { sql } => {
            let body = json!({"query": read_text_arg(sql)?});
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::POST,
                    "/lakehouse/splitstatements",
                    &[],
                    Some(body),
                )
                .await?;
            rt.show_columns(
                &value["response"]["statements"],
                &["startLine", "endLine", "start", "end"],
            )
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
        SqlAction::Stop { session } => stop_kept_session(rt, session.as_deref()).await,
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
        SqlAction::Estimate { sql, warehouse } => {
            estimate_scan(rt, &read_text_arg(sql)?, warehouse.as_deref()).await
        }
        SqlAction::MaxScan { size } => max_scan_setting(rt, size.as_deref()),
    }
}

/// The warehouse a query runs on: the one named, else the one `gz sql`
/// remembers.
fn query_warehouse(rt: &Runtime, named: Option<&str>) -> Result<String> {
    named
        .map(str::to_string)
        .or_else(|| rt.http.resolved().profile.sql_warehouse.clone())
        .context("pass --warehouse NAME (`gz warehouse list` shows them)")
}

/// Refuses `query` when its estimated scan is over the limit in force.
async fn check_scan(
    rt: &Runtime,
    warehouse: Option<&str>,
    query: &str,
    max_scan: Option<&str>,
) -> Result<()> {
    let saved = rt.http.resolved().profile.sql_max_scan.clone();
    let limit = ScanLimit::resolve(max_scan, saved.as_deref())?;
    if limit == ScanLimit::Off {
        return Ok(());
    }
    let warehouse = query_warehouse(rt, warehouse)?;
    let checked = crate::sql::scan_limit::enforce(&rt.http, &warehouse, query, limit).await?;
    if let (Some(estimate), false) = (checked, rt.quiet) {
        output::info(&format!("scan check: {}", estimate.headline()));
    }
    Ok(())
}

/// `gz sql estimate`: the scan preview, as lines or JSON.
async fn estimate_scan(rt: &Runtime, query: &str, warehouse: Option<&str>) -> Result<()> {
    let warehouse = query_warehouse(rt, warehouse)?;
    let estimate = crate::sql::estimate::estimate(&rt.http, &warehouse, query).await?;
    match rt.format {
        OutputFormat::Table => {
            for line in estimate.lines() {
                println!("{line}");
            }
            Ok(())
        }
        format => output::print_value(&estimate.to_json(), format, None),
    }
}

/// `gz sql max-scan [SIZE|off]`: shows or saves the profile's limit.
fn max_scan_setting(rt: &Runtime, size: Option<&str>) -> Result<()> {
    let profile = &rt.http.resolved().profile_name;
    let Some(size) = size else {
        let saved = rt.http.resolved().profile.sql_max_scan.clone();
        match saved {
            Some(limit) => output::info(&format!(
                "gz sql refuses queries estimated to scan more than {limit} (profile {profile})"
            )),
            None => output::info(&format!(
                "No scan limit in profile {profile}; set one with `gz sql max-scan 10GB`"
            )),
        }
        return Ok(());
    };
    let limit = ScanLimit::parse(size, false)?;
    let saved = match limit {
        ScanLimit::Off => None,
        ScanLimit::Bytes { bytes, .. } => Some(crate::sql::estimate::exact_size(bytes)),
    };
    crate::sql::remember_in_profile(profile, |p| p.sql_max_scan = saved.clone())?;
    match saved {
        Some(limit) => output::success(&format!(
            "gz sql now refuses queries estimated to scan more than {limit}"
        )),
        None => output::success("gz sql no longer limits how much a query scans"),
    }
    Ok(())
}

/// Stops `session`, or the one `gz sql --keep-session` remembered, and
/// forgets it so the next `gz sql` starts fresh.
async fn stop_kept_session(rt: &Runtime, session: Option<&str>) -> Result<()> {
    let remembered = rt.http.resolved().profile.sql_session.clone();
    let Some(session) = session.map(str::to_string).or(remembered.clone()) else {
        output::info("No gz sql session is running for this profile.");
        return Ok(());
    };
    rt.http
        .request_json(
            SVC,
            Method::POST,
            "/lakehouse/stopsession",
            &[],
            Some(json!({"sessionId": session})),
        )
        .await?;
    if remembered.as_deref() == Some(session.as_str()) {
        let mut config = crate::config::load_config()?;
        let profile = config.profile_mut(&rt.http.resolved().profile_name);
        profile.sql_session = None;
        profile.sql_session_warehouse = None;
        crate::config::save_config(&config)?;
    }
    output::success(&format!("Session {session} stopped"));
    Ok(())
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
        TableAction::Versions(args) => crate::cmd::table_history::versions(rt, args).await,
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
            cursor,
            lookback_days,
        } => {
            let mut query = Vec::new();
            push_opt(&mut query, "limit", &rt.limit);
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
        BackupAction::Properties {
            warehouse,
            database,
            backup,
        } => {
            rt.get(
                SVC,
                &path(
                    "/lakehouse/backupProperties",
                    &[warehouse, database, backup],
                ),
                &[],
            )
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
        GovernAction::PolicyHistory { table } => {
            let mut query = table_query(table);
            push_opt(&mut query, "limit", &rt.limit);
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
        } => {
            let mut query = table_query(table);
            query.push(("direction".to_string(), direction.clone()));
            push_opt(&mut query, "depth", depth);
            push_opt(&mut query, "limit", &rt.limit);
            rt.get(SVC, "/lakehouse/governance/lineage/table", &query)
                .await
        }
        GovernAction::GovernedTables { warehouse } => {
            let url = path("/lakehouse/governance/governed-tables", &[warehouse]);
            rt.get(SVC, &url, &[]).await
        }
        GovernAction::ModelAccess(args) => super::model_access::run(rt, args).await,
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
        TagsAction::DbList {
            warehouse,
            database,
        } => rt.get(SVC, &db_tags_path(warehouse, database), &[]).await,
        TagsAction::DbTag {
            warehouse,
            database,
            tags,
        } => {
            let body = json!({"tags": parse_tags(tags)?});
            rt.send(SVC, Method::POST, &db_tags_path(warehouse, database), body)
                .await
        }
        TagsAction::DbUntag {
            warehouse,
            database,
            key,
        } => {
            let url = format!("{}/{}", db_tags_path(warehouse, database), seg(key));
            rt.send_empty(SVC, Method::DELETE, &url).await
        }
        TagsAction::ColumnList {
            warehouse,
            namespace,
            table,
            column_id,
        } => {
            rt.get(
                SVC,
                &column_tags_path(warehouse, namespace, table, *column_id),
                &[],
            )
            .await
        }
        TagsAction::ColumnSet {
            warehouse,
            namespace,
            table,
            column_id,
            tags,
            version,
        } => {
            let url = column_tags_path(warehouse, namespace, table, *column_id);
            set_column_tags(rt, &url, parse_tags(tags)?, *version).await
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
            offset,
        } => {
            let mut query = vec![("tagKey".to_string(), tag_key.clone())];
            push_opt(&mut query, "tagValue", tag_value);
            push_opt(&mut query, "namespace", namespace);
            push_opt(&mut query, "limit", &rt.limit);
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

fn db_tags_path(warehouse: &str, database: &str) -> String {
    path("/lakehouse/tags", &[warehouse, "databases", database])
}

fn column_tags_path(warehouse: &str, namespace: &str, table: &str, column_id: u64) -> String {
    format!(
        "{}/columns/{column_id}/tags",
        table_tags_path(warehouse, namespace, table)
    )
}

/// `KEY=VALUE` flags → a tag object.
fn parse_tags(pairs: &[String]) -> Result<Map<String, Value>> {
    pairs
        .iter()
        .map(|pair| {
            let (key, value) = pair
                .split_once('=')
                .with_context(|| format!("--tag must be KEY=VALUE, got '{pair}'"))?;
            if key.trim().is_empty() {
                anyhow::bail!("--tag needs a key, got '{pair}'");
            }
            Ok((key.trim().to_string(), Value::String(value.to_string())))
        })
        .collect()
}

/// Column tags are replaced as one document guarded by its version
/// (`If-Match: "<version>"`), so a concurrent edit fails instead of being lost.
async fn set_column_tags(
    rt: &Runtime,
    url: &str,
    tags: Map<String, Value>,
    version: Option<u64>,
) -> Result<()> {
    let version = match version {
        Some(version) => version,
        None => {
            let current = rt
                .http
                .request_json(SVC, Method::GET, url, &[], None)
                .await?;
            current["version"]
                .as_u64()
                .context("the column's tag version was not returned; pass --version")?
        }
    };
    let value = rt
        .http
        .request_json_with(
            SVC,
            Method::PUT,
            url,
            &[],
            Some(json!({"tags": tags})),
            &[("If-Match", if_match(version))],
        )
        .await?;
    rt.show(value).await
}

fn if_match(version: u64) -> String {
    format!("\"{version}\"")
}

/// One finished query on a session.
struct QueryTarget<'a> {
    session: &'a str,
    execution: &'a str,
    warehouse: &'a str,
}

/// The web app's export: write the full result as files, wait for that job,
/// then ask for presigned links (`useResultExport.js`, `lakehouse_queries.py`).
async fn export(
    rt: &Runtime,
    target: &QueryTarget<'_>,
    format: &str,
    out: Option<&str>,
) -> Result<()> {
    let chunks = path(
        "/lakehouse/queryresultchunks",
        &[target.session, target.execution],
    );
    let body = json!({"catalog": target.warehouse, "purpose": "export", "format": format});
    let job = rt
        .http
        .request_json(SVC, Method::POST, &chunks, &[], Some(body))
        .await?;
    let job = &job["response"];
    if job["status"] == "RUNNING" {
        let job_id = job["queryExecutionId"]
            .as_str()
            .context("the export job has no id to wait on")?;
        wait_for_query(rt, target.session, job_id).await?;
    }
    let query = vec![
        ("catalog".to_string(), target.warehouse.to_string()),
        ("format".to_string(), format.to_string()),
    ];
    let links_path = path(
        "/lakehouse/queryexport",
        &[target.session, target.execution],
    );
    let links = rt
        .http
        .request_json(SVC, Method::GET, &links_path, &query, None)
        .await?;
    match out {
        Some(dir) => download_export(rt, &links["response"], Path::new(dir)).await,
        None => rt.show_columns(
            &links["response"]["files"],
            &["part", "name", "rows", "url"],
        ),
    }
}

async fn wait_for_query(rt: &Runtime, session: &str, execution: &str) -> Result<()> {
    let started = Instant::now();
    let status_path = path("/lakehouse/querystatus", &[session, execution]);
    loop {
        let reply = rt
            .http
            .request_json(SVC, Method::GET, &status_path, &[], None)
            .await?;
        let state = reply["response"]["status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if state == "SUCCEEDED" {
            return Ok(());
        }
        if TERMINAL_QUERY_STATES.contains(&state.as_str()) {
            let reason = reply["response"]["error"]
                .as_str()
                .unwrap_or("no reason given");
            anyhow::bail!("the export {} ({reason})", state.to_lowercase());
        }
        if started.elapsed() > EXPORT_TIMEOUT {
            anyhow::bail!("the export is still running after 30 minutes; check `gz sql status {session} {execution}`");
        }
        tokio::time::sleep(EXPORT_POLL).await;
    }
}

/// Presigned links carry their own authorization, so no gz headers are sent.
async fn download_export(rt: &Runtime, listing: &Value, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let files = listing["files"].as_array().cloned().unwrap_or_default();
    for file in &files {
        let (Some(name), Some(url)) = (file["name"].as_str(), file["url"].as_str()) else {
            continue;
        };
        let file_name = Path::new(name)
            .file_name()
            .with_context(|| format!("unsafe export file name '{name}'"))?;
        let response = rt
            .http
            .raw()
            .get(url)
            .send()
            .await
            .context("download failed")?;
        if !response.status().is_success() {
            anyhow::bail!("downloading {name} failed: HTTP {}", response.status());
        }
        let bytes = response.bytes().await.context("download interrupted")?;
        let dest = dir.join(file_name);
        std::fs::write(&dest, &bytes)
            .with_context(|| format!("cannot write {}", dest.display()))?;
    }
    output::success(&format!(
        "saved {} file(s), {} rows, to {}",
        files.len(),
        listing["totalRecords"],
        dir.display()
    ));
    Ok(())
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
    fn parses_tag_flags() {
        let tags = parse_tags(&["owner=data team".to_string(), "pii=".to_string()]).unwrap();
        assert_eq!(tags["owner"], "data team");
        assert_eq!(tags["pii"], "");
        assert!(parse_tags(&["nokey".to_string()]).is_err());
        assert!(parse_tags(&["=v".to_string()]).is_err());
    }

    #[test]
    fn builds_tag_paths_and_etags() {
        assert_eq!(
            db_tags_path("wh", "my db"),
            "/lakehouse/tags/wh/databases/my%20db"
        );
        assert_eq!(
            column_tags_path("wh", "ns", "t", 4),
            "/lakehouse/tags/wh/tables/ns/t/columns/4/tags"
        );
        assert_eq!(if_match(3), "\"3\"");
    }

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
