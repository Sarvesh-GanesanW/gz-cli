//! `gz quality`: the quality verdicts on a table's branches, the gate that
//! makes publishing a branch need a passing one, and past check results.
//!
//! Endpoints (iceberg-provider app_routes, per the qualipilot contract):
//! - `GET /lakehouse/tables/{w}/{d}/{t}/quality` →
//!   `{"status": 200, "response": {"required", "branches": [...]}}`.
//! - `PUT …/quality-gate` with `{"required": bool}` →
//!   `{"status": 200, "response": {"required": bool}}`.
//! - Publishing a write branch (`gz jobs publish`) is refused with HTTP 409
//!   and `reason: "quality"` when the gate is on and the verdict is missing,
//!   failed or for an older snapshot; [`publish_blockers`] says so first.
//!
//! History runs SQL over the results table on the session `gz sql` uses,
//! under its scan limit.

mod args;
mod history;
mod verdict;

pub use args::{HistoryArgs, QualityAction, QualityArgs};

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::ShellArgs;
use crate::cmd::lakehouse_table::{route_missing, LakehouseTable};
use crate::cmd::Runtime;
use crate::output::{self, OutputFormat};
use history::HistoryQuery;
use verdict::TableQuality;

const LAKEHOUSE: &str = "lakehouse";
const NOT_DEPLOYED: &str = "it needs the lakehouse provider update deployed";
const DEFAULT_HISTORY_ROWS: usize = 20;

pub async fn run(rt: &Runtime, action: &QualityAction) -> Result<()> {
    match action {
        QualityAction::Show { table } => show(rt, &LakehouseTable::parse(table)?).await,
        QualityAction::Require { table, off } => {
            require(rt, &LakehouseTable::parse(table)?, !off).await
        }
        QualityAction::History(args) => history(rt, args).await,
    }
}

/// The provider's `/quality` reply; `None` when the provider has no such
/// route yet.
async fn fetch(rt: &Runtime, table: &LakehouseTable) -> Result<Option<Value>> {
    match rt
        .http
        .request_json(LAKEHOUSE, Method::GET, &table.path("quality"), &[], None)
        .await
    {
        Ok(reply) => Ok(Some(reply)),
        Err(err) if route_missing(&err) => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read the quality gate of {table}")),
    }
}

fn not_deployed(what: &str) -> anyhow::Error {
    anyhow::anyhow!("the lakehouse provider doesn't support {what} yet: {NOT_DEPLOYED}")
}

async fn show(rt: &Runtime, table: &LakehouseTable) -> Result<()> {
    let reply = fetch(rt, table)
        .await?
        .ok_or_else(|| not_deployed("quality verdicts"))?;
    if rt.format != OutputFormat::Table {
        return rt.show(reply).await;
    }
    let quality = TableQuality::parse(&reply["response"])?;
    output::info(&gate_line(table, quality.required));
    if quality.branches.is_empty() {
        output::info(&format!(
            "{table} has no branches besides main. A branch gets a verdict when a `gz jobs try` run calls runTableDataQualityChecks on the table"
        ));
        return Ok(());
    }
    rt.show_columns(&quality.rows(), &verdict::SHOW_COLUMNS)
}

fn gate_line(table: &LakehouseTable, required: bool) -> String {
    if required {
        format!("{table}: publishing a branch needs a passing verdict on its current head")
    } else {
        format!(
            "{table}: verdicts are recorded but not required to publish (`gz quality require {table}` turns that on)"
        )
    }
}

async fn require(rt: &Runtime, table: &LakehouseTable, required: bool) -> Result<()> {
    let reply = match rt
        .http
        .request_json(
            LAKEHOUSE,
            Method::PUT,
            &table.path("quality-gate"),
            &[],
            Some(json!({"required": required})),
        )
        .await
    {
        Ok(reply) => reply,
        Err(err) if route_missing(&err) => return Err(not_deployed("the quality gate")),
        Err(err) => {
            return Err(err).with_context(|| format!("cannot change the quality gate of {table}"))
        }
    };
    if rt.format != OutputFormat::Table {
        return rt.show(reply).await;
    }
    let now_required = reply["response"]["required"].as_bool().unwrap_or(required);
    output::success(&gate_line(table, now_required));
    Ok(())
}

async fn history(rt: &Runtime, args: &HistoryArgs) -> Result<()> {
    let checked = LakehouseTable::parse(&args.table)?;
    let results = LakehouseTable::parse(&args.results)
        .context("write --results as warehouse.database.table")?;
    let sql = HistoryQuery {
        results: &results.name,
        checked: &checked.name,
        limit: rt.limit.unwrap_or(DEFAULT_HISTORY_ROWS),
        failed_only: args.failed,
    }
    .sql()?;
    let shell = ShellArgs {
        warehouse: Some(results.warehouse.clone()),
        compute: args.compute.clone(),
        keep_session: args.keep_session,
    };
    crate::sql::run(
        rt,
        &shell,
        crate::sql::Source::Text(sql),
        args.scan.max_scan.as_deref(),
    )
    .await
}

/// One line per table whose quality gate refuses `branch`; empty when every
/// table may be published or the provider can't say yet.
pub async fn publish_blockers(
    rt: &Runtime,
    tables: &[&LakehouseTable],
    branch: &str,
) -> Result<Vec<String>> {
    let mut blocked = Vec::new();
    for table in tables {
        let Some(reply) = fetch(rt, table).await? else {
            continue;
        };
        let quality = TableQuality::parse(&reply["response"])
            .with_context(|| format!("cannot read the quality gate of {table}"))?;
        blocked.extend(quality.blocker(table, branch));
    }
    Ok(blocked)
}
