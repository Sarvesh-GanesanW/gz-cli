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
//! History runs SQL over the results tables on the session `gz sql` uses,
//! under its scan limit; without `--results` it finds them through the
//! lineage links the runtime records ([`links`]). `check` runs the checks
//! as a temporary job ([`check`]).

mod args;
mod check;
mod history;
mod links;
mod verdict;

pub use args::{HistoryArgs, QualityAction, QualityArgs};

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::ShellArgs;
use crate::cmd::lakehouse_table::{route_missing, LakehouseTable};
use crate::cmd::Runtime;
use crate::output::{self, OutputFormat};
use crate::table_history::TableName;
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
        QualityAction::Check(args) => check::run(rt, args).await,
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
    let (warehouse, results) = match &args.results {
        Some(results) => {
            let results = LakehouseTable::parse(results)
                .context("write --results as warehouse.database.table")?;
            (results.warehouse, vec![results.name])
        }
        None => (
            checked.warehouse.clone(),
            linked_results_tables(rt, &checked).await?,
        ),
    };
    let sql = HistoryQuery {
        results: &results,
        checked: &checked.name,
        limit: rt.limit.unwrap_or(DEFAULT_HISTORY_ROWS),
        failed_only: args.failed,
    }
    .sql()?;
    let shell = ShellArgs {
        warehouse: Some(warehouse),
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

/// The results tables jobs linked to `checked` that can still be read,
/// saying which were found and which were skipped.
async fn linked_results_tables(rt: &Runtime, checked: &LakehouseTable) -> Result<Vec<TableName>> {
    let linked = links::linked_results(rt, checked).await?;
    let mut readable = Vec::new();
    for table in linked.tables {
        match links::unreadable_reason(rt, &checked.warehouse, &table).await {
            None => readable.push(table),
            Some(reason) => output::warn(&format!(
                "skipping {}.{table}, linked as a results table of {checked}: {reason}",
                checked.warehouse
            )),
        }
    }
    if readable.is_empty() {
        bail!("{}", no_results_message(checked, linked.hidden));
    }
    if !rt.quiet {
        let names: Vec<String> = readable
            .iter()
            .map(|t| format!("{}.{t}", checked.warehouse))
            .collect();
        let pick = if names.len() > 1 {
            " (one of them: --results)"
        } else {
            ""
        };
        output::info(&format!(
            "results of {checked} from {}{pick}",
            names.join(", ")
        ));
        if linked.hidden {
            output::info("more results tables are linked that you may not read");
        }
    }
    Ok(readable)
}

fn no_results_message(checked: &LakehouseTable, hidden: bool) -> String {
    let why = if hidden {
        "the results tables linked to it are ones you may not read"
    } else {
        "no job has linked a results table to it. A Spark job links one when it saves \
         results (runTableDataQualityChecks resultsTable=, saveDataQualityResults) \
         to a table of the same warehouse; runs before that runtime update linked none"
    };
    format!(
        "no results found for {checked}: {why}. Name the results table with \
         --results WAREHOUSE.DATABASE.TABLE"
    )
}

/// A table whose quality gate refuses a branch, and why.
#[derive(Debug, Clone)]
pub struct Blocked {
    pub table: LakehouseTable,
    pub reason: String,
}

/// The tables whose quality gate refuses `branch`; empty when every table
/// may be published or the provider can't say yet.
pub async fn publish_blockers(
    rt: &Runtime,
    tables: &[&LakehouseTable],
    branch: &str,
) -> Result<Vec<Blocked>> {
    let mut blocked = Vec::new();
    for table in tables {
        let Some(reply) = fetch(rt, table).await? else {
            continue;
        };
        let quality = TableQuality::parse(&reply["response"])
            .with_context(|| format!("cannot read the quality gate of {table}"))?;
        blocked.extend(quality.blocker(table, branch).map(|reason| Blocked {
            table: (*table).clone(),
            reason,
        }));
    }
    Ok(blocked)
}
