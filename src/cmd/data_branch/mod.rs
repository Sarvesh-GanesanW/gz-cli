//! `gz jobs try`, `gz jobs publish` and `gz jobs discard`: run a changed job
//! against branches of the real tables it writes, look at what it changed,
//! then move the tables' main branch to its output or throw it away.
//!
//! Endpoints (each checked against its provider route; the web app calls
//! none of the new ones yet):
//! - `GET /etl/job/options` (etl-provider app.py `etlJobOptionsHandler`):
//!   whether runs take `writeBranch`. An older provider has no such route.
//! - `PUT /lakehouse/tables/{wh}/{db}/{t}/write-branches/{name}` and
//!   `POST …/write-branches/{name}/publish` (iceberg-provider
//!   app_routes/table_versions.py `create_table_write_branch`,
//!   `publish_table_write_branch`).
//! - `DELETE …/branches/{name}` (table_versions.py `delete_table_branch`; the
//!   web app's removeTableRef, lakehouseTableUtils.js:45).
//! - `GET …/versions` through [`History`] (table_versions.py
//!   `list_table_versions`; the web app's getTableVersions,
//!   lakehouseTableUtils.js:26).
//! - `GET /etl/lineage/jobs/{id}` (etl-provider app.py:2121; the web app's
//!   lineageApi.js:133) and `GET /connection/{id}` (the web app's
//!   fetchGetConnectionById, connectionUtils.js:236) find the tables.
//! - `POST /etl/job` submits the run with the web app's body (etlUtlis.js
//!   `submitAJob`) plus `writeBranch`, `writeBranchTables` and `code`.

mod args;
mod tables;

pub use args::{BranchJobArgs, DiscardArgs, PublishArgs, TryArgs};

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use crate::cmd::job_fix::resolve_job;
use crate::cmd::jobs::{self, JobRef};
use crate::cmd::table_history::report::{self, Subject};
use crate::cmd::{confirm, devloop, seg, Runtime};
use crate::output::{self, OutputFormat};
use crate::table_history::diff::Comparison;
use crate::table_history::{History, TableName};
use tables::BranchTable;

const ETL: &str = "etl";
const LAKEHOUSE: &str = "lakehouse";
const CONNECTIONS: &str = "connections";
const WRITE_BRANCH_OPTION: &str = "writeBranch";
const SUCCESS: &str = "Success";
const FINAL_STATUSES: [&str; 3] = ["Success", "Failed", "Aborted"];
const STATUS_POLL: Duration = Duration::from_secs(5);
const NOT_DEPLOYED: &str = "it needs the lakehouse and ETL provider updates deployed";

/// The job and the lakehouse tables it writes.
struct Target {
    project: i64,
    project_name: String,
    job: JobRef,
    tables: Vec<BranchTable>,
    /// The `--table` flags, repeated in the commands gz suggests.
    extra: Vec<String>,
}

impl Target {
    /// `1324 --project 1138 --branch try-x --table wh.db.t`.
    fn command_args(&self, branch: &str) -> String {
        let mut text = format!(
            "{} --project {} --branch {branch}",
            self.job.id, self.project
        );
        for table in &self.extra {
            text.push_str(&format!(" --table {table}"));
        }
        text
    }

    fn next_steps(&self, branch: &str) -> Vec<String> {
        let args = self.command_args(branch);
        vec![
            format!("gz jobs publish {args}"),
            format!("gz jobs discard {args}"),
        ]
    }
}

/// A route the provider doesn't have yet: Flask's own 404 page, not one of
/// its JSON replies (those say which table, warehouse or branch is missing).
fn route_missing(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_lowercase();
    text.contains("http 404")
        && (text.contains("requested url was not found") || text.contains("<!doctype html"))
}

fn table_path(table: &BranchTable, rest: &str) -> String {
    format!(
        "/lakehouse/tables/{}/{}/{}/{rest}",
        seg(&table.warehouse),
        seg(&table.name.database),
        seg(&table.name.table)
    )
}

/// Tables in first-seen order, each once whatever its case.
fn unique(tables: impl IntoIterator<Item = BranchTable>) -> Vec<BranchTable> {
    let mut seen = BTreeSet::new();
    tables
        .into_iter()
        .filter(|table| seen.insert(table.key()))
        .collect()
}

/// The warehouse a lakehouse connection points at (its `config.warehouseName`).
async fn connection_warehouse(rt: &Runtime, connection: &str) -> Result<String> {
    let reply = rt
        .http
        .request_json(
            CONNECTIONS,
            Method::GET,
            &format!("/connection/{}", seg(connection)),
            &[],
            None,
        )
        .await
        .with_context(|| format!("cannot read connection {connection}"))?;
    let row = if reply.is_array() { &reply[0] } else { &reply };
    row["config"]["warehouseName"]
        .as_str()
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .with_context(|| {
            format!("connection {connection} names no warehouse; pass the table with --table")
        })
}

async fn target(rt: &Runtime, args: &BranchJobArgs, dir: &Path) -> Result<Target> {
    let named = args
        .tables
        .iter()
        .map(|text| BranchTable::parse(text))
        .collect::<Result<Vec<_>>>()?;
    let project = args.project.or_else(|| devloop::synced_project(dir));
    let (project, job) = resolve_job(rt, &args.job, project).await?;
    let project_name = jobs::project_name(rt, project).await?;
    let lineage = rt
        .http
        .request_json(
            ETL,
            Method::GET,
            &format!("/etl/lineage/jobs/{}", job.id),
            &[],
            None,
        )
        .await
        .with_context(|| format!("cannot read the lineage of job {}", job.name))?;
    let mut warehouses: BTreeMap<String, String> = BTreeMap::new();
    let mut found = Vec::new();
    for written in tables::written_tables(&lineage) {
        let warehouse = match warehouses.get(&written.connection) {
            Some(warehouse) => warehouse.clone(),
            None => {
                let warehouse = connection_warehouse(rt, &written.connection).await?;
                warehouses.insert(written.connection.clone(), warehouse.clone());
                warehouse
            }
        };
        found.push(BranchTable {
            warehouse,
            name: TableName {
                database: written.database,
                table: written.table,
            },
        });
    }
    Ok(Target {
        project,
        project_name,
        job,
        tables: unique(named.into_iter().chain(found)),
        extra: args.tables.clone(),
    })
}

/// Refuse early, before anything is made, when the ETL provider can't run a
/// job on a branch.
async fn require_branch_runs(rt: &Runtime) -> Result<()> {
    let options = match rt
        .http
        .request_json(ETL, Method::GET, "/etl/job/options", &[], None)
        .await
    {
        Ok(options) => options,
        Err(err) if route_missing(&err) => {
            bail!("the ETL provider doesn't support writeBranch yet: {NOT_DEPLOYED}")
        }
        Err(err) => return Err(err),
    };
    if options.get(WRITE_BRANCH_OPTION).is_none() {
        bail!("the ETL provider doesn't support writeBranch yet: {NOT_DEPLOYED}");
    }
    Ok(())
}

async fn make_write_branch(rt: &Runtime, table: &BranchTable, branch: &str) -> Result<()> {
    let path = table_path(table, &format!("write-branches/{}", seg(branch)));
    match rt
        .http
        .request_json(LAKEHOUSE, Method::PUT, &path, &[], None)
        .await
    {
        Ok(_) => Ok(()),
        Err(err) if route_missing(&err) => {
            bail!("the lakehouse provider doesn't support write branches yet: {NOT_DEPLOYED}")
        }
        Err(err) => Err(err).with_context(|| format!("cannot make branch {branch} of {table}")),
    }
}

async fn drop_branch(rt: &Runtime, table: &BranchTable, branch: &str) -> Result<()> {
    let path = table_path(table, &format!("branches/{}", seg(branch)));
    rt.http
        .request_json(LAKEHOUSE, Method::DELETE, &path, &[], None)
        .await
        .map(|_| ())
        .with_context(|| format!("cannot drop branch {branch} of {table}"))
}

/// Undo the branches made so far when the try can't go on.
async fn drop_made(rt: &Runtime, made: &[BranchTable], branch: &str) {
    for table in made {
        if let Err(err) = drop_branch(rt, table, branch).await {
            output::warn(&format!(
                "{err:#}; drop it with `gz table versions` and the web app"
            ));
        }
    }
}

/// The run's status from its own record (`/etl/jobStatus`), which reading
/// does not change, until it is final.
async fn wait_for_run(rt: &Runtime, execution: &str) -> Result<String> {
    loop {
        let reply = rt
            .http
            .request_json(
                ETL,
                Method::POST,
                "/etl/jobStatus",
                &[],
                Some(json!({"jobIdentifier": execution})),
            )
            .await?;
        let status = reply["data"][0]["jobstatus"].as_str().unwrap_or_default();
        if FINAL_STATUSES.contains(&status) {
            return Ok(status.to_string());
        }
        tokio::time::sleep(STATUS_POLL).await;
    }
}

/// What the run did to one table, read from its history.
struct TableChange {
    text: String,
    json: Value,
}

fn branch_change(table: &BranchTable, history: &History, branch: &str) -> Result<TableChange> {
    let head = history
        .branches
        .iter()
        .find(|named| named.name == branch)
        .with_context(|| format!("{table} has no branch {branch} any more"))?;
    if history.current.as_deref() == Some(head.snapshot.as_str()) {
        return Ok(TableChange {
            text: format!("{table}\n  unchanged: the run wrote nothing to it\n"),
            json: json!({"table": table.to_string(), "changed": false}),
        });
    }
    let comparison = Comparison::between(history, "main", branch)?;
    let subject = Subject {
        table: &table.name,
        warehouse: &table.warehouse,
        history,
    };
    let mut diff = report::diff_json(&subject, &comparison, None);
    diff["changed"] = json!(true);
    Ok(TableChange {
        text: report::diff_summary(&subject, &comparison),
        json: diff,
    })
}

/// Why main can't be fast-forwarded to the branch, if it can't.
fn publish_blocker(table: &BranchTable, history: &History, branch: &str) -> Option<String> {
    let head = history.branches.iter().find(|named| named.name == branch)?;
    let main = history.current.as_deref()?;
    let head = history.snapshot(&head.snapshot)?;
    let on_line = history.line(head).iter().any(|s| s.id == main);
    (!on_line).then(|| format!("{table}: main has moved on (now {main}) since the branch was made"))
}

/// The tables that have the branch, with their histories.
async fn tables_on_branch(
    rt: &Runtime,
    target: &Target,
    branch: &str,
) -> Result<Vec<(BranchTable, History)>> {
    let mut found = Vec::new();
    for table in &target.tables {
        let history = History::load(&rt.http, &table.warehouse, &table.name).await?;
        if history.branches.iter().any(|named| named.name == branch) {
            found.push((table.clone(), history));
        }
    }
    if found.is_empty() {
        bail!(
            "no table job {} writes has a branch named {branch}; `gz table versions <database.table>` lists a table's branches, and --table adds a table its lineage doesn't show",
            target.job.name
        );
    }
    Ok(found)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

pub async fn try_job(rt: &Runtime, args: &TryArgs) -> Result<()> {
    let target = target(rt, &args.target, &args.dir).await?;
    let branch = args
        .branch
        .clone()
        .unwrap_or_else(|| tables::default_branch(&target.job.name, now_secs()));
    tables::check_branch(&branch)?;
    if target.tables.is_empty() {
        bail!(
            "the lineage of job {} shows no lakehouse table it writes; pass each one with --table warehouse.database.table",
            target.job.name
        );
    }
    let local = devloop::synced_job_code(&args.dir, target.project, &target.job.name)?;
    require_branch_runs(rt).await?;
    let saved = devloop::saved_job(rt, target.project, target.job.id).await?;
    let mut spec = jobs::run_spec(
        &target.project_name,
        target.project,
        target.job.id,
        &target.job.name,
        &saved,
        &args.choice,
    );
    spec["writeBranch"] = json!(branch);
    spec["writeBranchTables"] = target
        .tables
        .iter()
        .map(|t| {
            json!({
                "warehouseName": t.warehouse,
                "databaseName": t.name.database,
                "tableName": t.name.table,
            })
        })
        .collect();
    let code_from = match &local {
        Some((file, code)) => {
            spec["code"] = json!(code);
            file.clone()
        }
        None => "the saved code".to_string(),
    };
    let tables_text: Vec<String> = target.tables.iter().map(|t| t.to_string()).collect();
    if !rt.quiet {
        output::info(&format!(
            "Trying job {} ({code_from}) on branch {branch} of {}",
            target.job.name,
            tables_text.join(", ")
        ));
    }

    let mut made = Vec::new();
    for table in &target.tables {
        if let Err(err) = make_write_branch(rt, table, &branch).await {
            drop_made(rt, &made, &branch).await;
            return Err(err);
        }
        made.push(table.clone());
    }
    let started = match rt
        .http
        .request_json(ETL, Method::POST, "/etl/job", &[], Some(spec))
        .await
    {
        Ok(started) => started,
        Err(err) => {
            drop_made(rt, &made, &branch).await;
            return Err(err.context(format!(
                "the run was not started, and gz removed branch {branch} again"
            )));
        }
    };
    let execution = started["jobIdentifier"]
        .as_str()
        .context("the ETL provider started a run but sent no run id")?
        .to_string();
    if started.get(WRITE_BRANCH_OPTION).is_none() {
        output::warn("the ETL provider didn't confirm the branch; check main before publishing");
    }
    let status = if rt.format == OutputFormat::Table {
        output::info(&format!("run {execution} started; following its logs"));
        jobs::follow_logs(rt, &execution).await?.unwrap_or_default()
    } else {
        wait_for_run(rt, &execution).await?
    };

    let mut changes = Vec::new();
    for table in &target.tables {
        let history = History::load(&rt.http, &table.warehouse, &table.name).await?;
        changes.push(branch_change(table, &history, &branch)?);
    }
    let next = target.next_steps(&branch);
    if rt.format != OutputFormat::Table {
        rt.show(json!({
            "job": {"id": target.job.id, "name": target.job.name, "project": target.project},
            "branch": branch,
            "code": code_from,
            "run": {"jobIdentifier": execution, "status": status},
            "tables": changes.iter().map(|c| c.json.clone()).collect::<Vec<_>>(),
            "next": next,
        }))
        .await?;
    } else {
        println!();
        for change in &changes {
            println!("{}", change.text);
        }
        println!("Next:");
        for step in &next {
            println!("  {step}");
        }
    }
    if status != SUCCESS {
        bail!(
            "run {execution} ended with status {}; main is unchanged and the branch is kept to look at (discard it with `{}`)",
            if status.is_empty() { "unknown" } else { &status },
            next[1]
        );
    }
    Ok(())
}

pub async fn publish(rt: &Runtime, args: &PublishArgs) -> Result<()> {
    tables::check_branch(&args.branch)?;
    let target = target(rt, &args.target, Path::new(".")).await?;
    let found = tables_on_branch(rt, &target, &args.branch).await?;
    let blocked: Vec<String> = found
        .iter()
        .filter_map(|(table, history)| publish_blocker(table, history, &args.branch))
        .collect();
    if !blocked.is_empty() {
        bail!(
            "nothing was published: {}. Publishing would drop those commits; discard the branch (`gz jobs discard {}`) and try the job again",
            blocked.join("; "),
            target.command_args(&args.branch)
        );
    }
    let names: Vec<String> = found.iter().map(|(t, _)| t.to_string()).collect();
    confirm(
        args.yes,
        &format!(
            "Publish branch {} to main of {}?",
            args.branch,
            names.join(", ")
        ),
    )?;
    let mut published = Vec::new();
    for (table, _) in &found {
        let path = table_path(
            table,
            &format!("write-branches/{}/publish", seg(&args.branch)),
        );
        let reply = match rt
            .http
            .request_json(LAKEHOUSE, Method::POST, &path, &[], None)
            .await
        {
            Ok(reply) => reply,
            Err(err) => {
                let done: Vec<String> = published
                    .iter()
                    .map(|(t, _): &(BranchTable, Value)| t.to_string())
                    .collect();
                let cause = if route_missing(&err) {
                    format!("the lakehouse provider doesn't support publishing yet: {NOT_DEPLOYED}")
                } else {
                    format!("{err:#}")
                };
                bail!(
                    "{table} was not published: {cause}. Published already: {}",
                    if done.is_empty() {
                        "none".to_string()
                    } else {
                        done.join(", ")
                    }
                );
            }
        };
        published.push((table.clone(), reply["response"].clone()));
    }
    if rt.format != OutputFormat::Table {
        return rt
            .show(json!({
                "branch": args.branch,
                "tables": published.iter().map(|(t, r)| {
                    let mut row = r.clone();
                    row["table"] = json!(t.to_string());
                    row
                }).collect::<Vec<_>>(),
            }))
            .await;
    }
    for (table, reply) in &published {
        if reply["published"] == json!(true) {
            output::success(&format!(
                "{table}: main moved from {} to {} ({} commit(s))",
                reply["previousSnapshotId"].as_str().unwrap_or("?"),
                reply["currentSnapshotId"].as_str().unwrap_or("?"),
                reply["snapshots"].as_array().map_or(0, Vec::len)
            ));
        } else {
            output::success(&format!(
                "{table}: the branch had nothing new; main is unchanged"
            ));
        }
    }
    Ok(())
}

pub async fn discard(rt: &Runtime, args: &DiscardArgs) -> Result<()> {
    tables::check_branch(&args.branch)?;
    let target = target(rt, &args.target, Path::new(".")).await?;
    let found = tables_on_branch(rt, &target, &args.branch).await?;
    let mut dropped = Vec::new();
    for (table, _) in &found {
        drop_branch(rt, table, &args.branch).await?;
        dropped.push(table.to_string());
    }
    if rt.format != OutputFormat::Table {
        return rt
            .show(json!({"branch": args.branch, "dropped": dropped}))
            .await;
    }
    for table in &dropped {
        output::success(&format!("{table}: branch {} dropped", args.branch));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table_history::test_support::snapshot;

    fn history(main: u64, branch_head: u64) -> History {
        History::parse(&json!({
            "currentSnapshotId": main.to_string(),
            "snapshots": [
                snapshot(13, Some(11), "2026-11-11T09:03:00+00:00", json!({"added-records": "5", "total-records": "15"})),
                snapshot(12, Some(11), "2026-11-11T09:02:00+00:00", json!({"added-records": "1", "total-records": "11"})),
                snapshot(11, None, "2026-11-11T09:01:00+00:00", json!({"added-records": "10", "total-records": "10"})),
            ],
            "branches": [
                {"name": "main", "snapshotId": main.to_string()},
                {"name": "try-1", "snapshotId": branch_head.to_string()},
            ],
            "tags": [],
        }))
        .unwrap()
    }

    fn orders() -> BranchTable {
        BranchTable::parse("Lineage.sales.orders").unwrap()
    }

    #[test]
    fn a_branch_built_on_main_can_be_published() {
        assert_eq!(publish_blocker(&orders(), &history(11, 13), "try-1"), None);
    }

    #[test]
    fn a_main_with_its_own_commits_blocks_publishing() {
        assert_eq!(
            publish_blocker(&orders(), &history(12, 13), "try-1").as_deref(),
            Some("Lineage.sales.orders: main has moved on (now 12) since the branch was made")
        );
    }

    #[test]
    fn an_untouched_branch_reports_no_change() {
        let change = branch_change(&orders(), &history(11, 11), "try-1").unwrap();
        assert_eq!(change.json["changed"], json!(false));
        assert!(change.text.contains("unchanged"), "{}", change.text);
    }

    #[test]
    fn a_written_branch_reports_the_diff_from_main() {
        let change = branch_change(&orders(), &history(11, 13), "try-1").unwrap();
        assert_eq!(change.json["changed"], json!(true));
        assert_eq!(change.json["from"]["snapshotId"], json!("11"));
        assert_eq!(change.json["to"]["snapshotId"], json!("13"));
        assert!(
            change.text.starts_with("sales.orders · Lineage"),
            "{}",
            change.text
        );
    }

    #[test]
    fn only_flask_404_pages_mean_a_missing_route() {
        let page = anyhow::anyhow!(
            "PUT u → HTTP 404 Not Found: <!doctype html><title>404</title><p>The requested URL was not found on the server."
        );
        let table = anyhow::anyhow!("PUT u → HTTP 404 Not Found: Table not found");
        assert!(route_missing(&page));
        assert!(!route_missing(&table));
    }

    #[test]
    fn tables_are_kept_once_whatever_their_case() {
        let tables = unique([
            BranchTable::parse("Lineage.sales.orders").unwrap(),
            BranchTable::parse("lineage.SALES.orders").unwrap(),
            BranchTable::parse("lineage.sales.returns").unwrap(),
        ]);
        let names: Vec<String> = tables.iter().map(|t| t.to_string()).collect();
        assert_eq!(names, ["Lineage.sales.orders", "lineage.sales.returns"]);
    }
}
