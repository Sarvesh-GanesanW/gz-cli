//! `gz usage compute`: compute used and money spent by jobs, dataset
//! refreshes and their runs over a window, the top spenders, and compute
//! left running (SQL sessions, notebooks).
//!
//! What the platform records, and where:
//! - Per run: `GET /etl/consumption-metrics` (etl provider app.py:1524,
//!   utils/execution_observability.py). Each job or dataset run carries
//!   `usage.cpu.coreHours`, `usage.memory.gibHours` and `usage.cost`, which
//!   is Kubecost's allocation price (node hourly CPU and memory rates times
//!   what the run's pods were allocated, execution_observability.py:1089),
//!   in USD. Sessions shared by overlapping runs are deduplicated by the
//!   server. Runs whose metrics were never collected, or only in part,
//!   have no cost, so the server sends `totalCostUsd` only when every
//!   session is complete and `observedCostUsd` otherwise.
//! - Grouped: the same route with `groupBy=project|job|dataset` (job groups
//!   need `projectId`), each with its own `summary`.
//! - The platform bill: `POST /admin/client/bill {year, month, siteName}`
//!   (site manager client.controller.mjs:23, fn_r_monthlyBilling; web app
//!   Utils/billingUtils.js:11). Its `vCPU` line is what the site is billed
//!   for compute: pod hours times pod memory GiB at the service's unit
//!   price (groundzero-infra default-db.sql:303). Whole-site and monthly.
//! - SQL sessions and notebooks: neither service records usage, so only
//!   what is running now is shown (see `status::sessions`).

mod render;

use anyhow::Result;
use clap::Args;
use reqwest::Method;
use serde_json::{json, Value};

use crate::cmd::job_fix::{now_ms, resolve_job};
use crate::cmd::status::sessions::{self, NotebookScan, SqlSession};
use crate::cmd::status::{section, Section};
use crate::cmd::{window, Runtime};
use crate::http::ApiClient;
use crate::impact::lookup::parallel;
use crate::output::OutputFormat;
use crate::tui::format::iso8601_utc;

const ETL: &str = "etl";
const CONSUMPTION: &str = "/etl/consumption-metrics";
/// The server's page limit.
const PAGE: usize = 100;
/// Runs read to find the costliest: 20 pages, the newest 2,000.
const MAX_RUN_PAGES: usize = 20;
const DEFAULT_SINCE: &str = "30d";
const DEFAULT_TOP: usize = 10;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Args, Debug)]
pub struct ComputeArgs {
    #[arg(
        long,
        default_value = DEFAULT_SINCE,
        help = "Window start: an age (30m, 6h, 7d, 2w) or a UTC date or time"
    )]
    pub since: String,
    #[arg(long, help = "Only this project's jobs (project id)")]
    pub project: Option<i64>,
    #[arg(long, help = "Only this job (id or name) and its runs")]
    pub job: Option<String>,
    #[arg(long, default_value_t = DEFAULT_TOP, help = "How many top spenders to list")]
    pub top: usize,
}

/// What the report covers.
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub from_ms: i64,
    pub to_ms: i64,
    pub project: Option<i64>,
    pub job: Option<(i64, String)>,
    pub top: usize,
}

impl Scope {
    /// Narrowed to a project or job: datasets and the bill are left out.
    fn is_narrowed(&self) -> bool {
        self.project.is_some() || self.job.is_some()
    }

    fn query(&self, extra: &[(&str, String)]) -> Vec<(String, String)> {
        let mut query = vec![
            ("from".to_string(), iso8601_utc(self.from_ms)),
            ("to".to_string(), iso8601_utc(self.to_ms)),
        ];
        if let Some(project) = self.project {
            query.push(("projectId".into(), project.to_string()));
        }
        if let Some((job, _)) = &self.job {
            query.push(("jobId".into(), job.to_string()));
        }
        query.extend(extra.iter().map(|(k, v)| (k.to_string(), v.clone())));
        query
    }
}

/// One project, job, dataset or run, with what it used.
#[derive(Debug, Clone, PartialEq)]
pub struct Spender {
    pub name: String,
    pub context: Option<String>,
    /// Observed cost in USD; `None` when nothing in it has cost data.
    pub cost_usd: Option<f64>,
    /// Every session in it has full cost data.
    pub cost_complete: bool,
    pub core_hours: Option<f64>,
    pub gib_hours: Option<f64>,
    pub runs: u64,
    pub row: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TopRuns {
    pub runs: Vec<Spender>,
    /// Runs read, and how many the window holds.
    pub read: usize,
    pub total: u64,
}

/// One month's compute line from the platform bill.
#[derive(Debug, Clone, PartialEq)]
pub struct BillMonth {
    pub year: i32,
    pub month: u32,
    pub reply: Value,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub site: Option<String>,
    pub scope: Scope,
    pub now_ms: i64,
    /// Summaries of job runs, and (site-wide) of dataset refreshes.
    pub jobs: Section<Value>,
    pub datasets: Option<Section<Value>>,
    pub top_projects: Option<Section<Vec<Spender>>>,
    pub top_jobs: Option<Section<Vec<Spender>>>,
    pub top_datasets: Option<Section<Vec<Spender>>>,
    pub top_runs: Section<TopRuns>,
    pub bill: Option<Section<Vec<BillMonth>>>,
    pub sql_session: Section<Option<(SqlSession, Section<bool>)>>,
    pub notebooks: Section<NotebookScan>,
}

pub async fn run(rt: &Runtime, args: &ComputeArgs) -> Result<()> {
    let now = now_ms();
    let from_ms = window::parse_since(&args.since, now)?;
    let (project, job) = match &args.job {
        Some(wanted) => {
            let (project, job) = resolve_job(rt, wanted, args.project).await?;
            (Some(project), Some((job.id, job.name)))
        }
        None => (args.project, None),
    };
    let scope = Scope {
        from_ms,
        to_ms: now,
        project,
        job,
        top: args.top.max(1),
    };
    let report = gather(&rt.http, scope).await;
    if rt.format == OutputFormat::Table {
        for line in render::panel(&report).plain_lines() {
            println!("{line}");
        }
        return Ok(());
    }
    rt.show(render::to_json(&report)).await
}

pub async fn gather(http: &ApiClient, scope: Scope) -> Report {
    let narrowed = scope.is_narrowed();
    let (
        jobs,
        datasets,
        top_projects,
        top_jobs,
        top_datasets,
        top_runs,
        bill,
        sql_session,
        notebooks,
    ) = tokio::join!(
        summary(http, &scope, "job"),
        maybe(!narrowed, summary(http, &scope, "dataset")),
        maybe(!narrowed, groups(http, &scope, "project", None)),
        maybe(scope.job.is_none(), top_jobs(http, &scope)),
        maybe(!narrowed, groups(http, &scope, "dataset", None)),
        top_runs(http, &scope),
        maybe(!narrowed, bill(http, &scope)),
        kept_session(http),
        sessions::running_notebooks(http),
    );
    Report {
        site: http.resolved().site(),
        now_ms: scope.to_ms,
        scope,
        jobs: section(jobs),
        datasets: datasets.map(section),
        top_projects: top_projects.map(section),
        top_jobs: top_jobs.map(section),
        top_datasets: top_datasets.map(section),
        top_runs: section(top_runs),
        bill: bill.map(section),
        sql_session: section(sql_session),
        notebooks: section(notebooks),
    }
}

/// Runs `future` only when `wanted`, so every branch of the join has the
/// same shape.
async fn maybe<T>(wanted: bool, future: impl std::future::Future<Output = T>) -> Option<T> {
    if wanted {
        Some(future.await)
    } else {
        None
    }
}

async fn consumption(http: &ApiClient, query: &[(String, String)]) -> Result<Value> {
    http.request_json(ETL, Method::GET, CONSUMPTION, query, None)
        .await
}

/// The window's `summary` for one workload type.
async fn summary(http: &ApiClient, scope: &Scope, workload: &str) -> Result<Value> {
    let query = scope.query(&[("workloadType", workload.into()), ("limit", "1".into())]);
    Ok(consumption(http, &query).await?["summary"].clone())
}

/// Every group of one kind, costliest first.
async fn groups(
    http: &ApiClient,
    scope: &Scope,
    group_by: &str,
    project: Option<i64>,
) -> Result<Vec<Spender>> {
    let mut extra = vec![
        ("groupBy", group_by.to_string()),
        ("limit", PAGE.to_string()),
    ];
    if let Some(project) = project.filter(|_| scope.project.is_none()) {
        extra.push(("projectId", project.to_string()));
    }
    let answer = consumption(http, &scope.query(&extra)).await?;
    let mut spenders: Vec<Spender> = answer["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| group_spender(item, group_by))
        .collect();
    rank(&mut spenders);
    Ok(spenders)
}

/// Job groups need a project, so without one each project's jobs are
/// read and merged.
async fn top_jobs(http: &ApiClient, scope: &Scope) -> Result<Vec<Spender>> {
    if scope.project.is_some() {
        return groups(http, scope, "job", None).await;
    }
    let projects = groups(http, scope, "project", None).await?;
    let asks: Vec<_> = projects
        .iter()
        .filter_map(|p| p.row["projectId"].as_i64())
        .map(|project| {
            let http = http.clone();
            let scope = scope.clone();
            async move { groups(&http, &scope, "job", Some(project)).await }
        })
        .collect();
    let mut jobs = Vec::new();
    for answer in parallel(asks).await? {
        jobs.extend(answer?);
    }
    rank(&mut jobs);
    Ok(jobs)
}

/// Every run in the window, newest first, up to [`MAX_RUN_PAGES`] pages;
/// the costliest are picked from what was read.
async fn top_runs(http: &ApiClient, scope: &Scope) -> Result<TopRuns> {
    let first = consumption(http, &scope.query(&[("limit", PAGE.to_string())])).await?;
    let total = first["pagination"]["total"].as_u64().unwrap_or(0);
    let pages = (total as usize).div_ceil(PAGE).clamp(1, MAX_RUN_PAGES);
    let rest: Vec<_> = (1..pages)
        .map(|page| {
            let http = http.clone();
            let query = scope.query(&[
                ("limit", PAGE.to_string()),
                ("offset", (page * PAGE).to_string()),
            ]);
            async move { consumption(&http, &query).await }
        })
        .collect();
    let mut rows: Vec<Value> = items(&first);
    for page in parallel(rest).await? {
        rows.extend(items(&page?));
    }
    let mut runs: Vec<Spender> = rows.iter().map(run_spender).collect();
    rank(&mut runs);
    Ok(TopRuns {
        read: rows.len(),
        runs,
        total,
    })
}

fn items(answer: &Value) -> Vec<Value> {
    answer["items"].as_array().cloned().unwrap_or_default()
}

/// The platform bill for each calendar month the window touches.
async fn bill(http: &ApiClient, scope: &Scope) -> Result<Vec<BillMonth>> {
    let site = http.resolved().site();
    let asks: Vec<_> = months(scope.from_ms, scope.to_ms)
        .into_iter()
        .map(|(year, month)| {
            let http = http.clone();
            let mut body = json!({"year": year, "month": month});
            if let Some(site) = &site {
                body["siteName"] = json!(site);
            }
            async move {
                let reply = http
                    .request_json("admin", Method::POST, "/admin/client/bill", &[], Some(body))
                    .await?;
                anyhow::Ok(BillMonth { year, month, reply })
            }
        })
        .collect();
    parallel(asks).await?.into_iter().collect()
}

/// (year, month) for every UTC month from `from_ms` to `to_ms`.
fn months(from_ms: i64, to_ms: i64) -> Vec<(i32, u32)> {
    let month_of = |ms: i64| {
        let (year, month, _) = crate::tui::format::civil_from_days(ms.div_euclid(DAY_MS));
        (year as i32, month)
    };
    let (mut year, mut month) = month_of(from_ms);
    let last = month_of(to_ms);
    let mut all = vec![(year, month)];
    while (year, month) < last {
        (year, month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        all.push((year, month));
    }
    all
}

async fn kept_session(http: &ApiClient) -> Result<Option<(SqlSession, Section<bool>)>> {
    let Some(session) = sessions::kept_sql_session(http).await? else {
        return Ok(None);
    };
    let busy = section(sessions::is_busy(http, &session.session_id).await);
    Ok(Some((session, busy)))
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A group's own summary: observed figures, and `totalCostUsd` only when
/// every session in it has cost data.
fn group_spender(item: &Value, group_by: &str) -> Spender {
    let summary = &item["summary"];
    let (name, context) = match group_by {
        "project" => (text(&item["projectName"]), None),
        "job" => (text(&item["jobName"]), text(&item["projectName"])),
        _ => (text(&item["datasetName"]), None),
    };
    let id_key = format!("{group_by}Id");
    Spender {
        name: name
            .or_else(|| text(&item[&id_key]).map(|id| format!("{group_by} {id}")))
            .unwrap_or_else(|| format!("unnamed {group_by}")),
        context,
        cost_usd: number(&summary["observedCostUsd"]),
        cost_complete: number(&summary["totalCostUsd"]).is_some(),
        core_hours: number(&summary["observedCpuCoreHours"]),
        gib_hours: number(&summary["observedMemoryGiBHours"]),
        runs: summary["executionCount"].as_u64().unwrap_or(0),
        row: item.clone(),
    }
}

fn run_spender(row: &Value) -> Spender {
    let usage = &row["usage"];
    let complete = row["collectionStatus"].as_str() == Some("complete");
    let cost = number(&usage["cost"]["totalUsd"]);
    Spender {
        name: text(&row["jobName"])
            .or_else(|| text(&row["datasetName"]))
            .or_else(|| text(&row["jobId"]).map(|id| format!("job {id}")))
            .or_else(|| text(&row["datasetId"]).map(|id| format!("dataset {id}")))
            .unwrap_or_else(|| "unnamed run".into()),
        context: Some(format!(
            "run {}",
            row["executionId"].as_str().unwrap_or_default()
        )),
        cost_usd: cost,
        cost_complete: complete && cost.is_some(),
        core_hours: number(&usage["cpu"]["coreHours"]),
        gib_hours: number(&usage["memory"]["gibHours"]),
        runs: 1,
        row: row.clone(),
    }
}

/// Costliest first; those without cost after, by memory-hours.
fn rank(spenders: &mut [Spender]) {
    spenders.sort_by(|a, b| {
        let key = |s: &Spender| (s.cost_usd.unwrap_or(-1.0), s.gib_hours.unwrap_or(0.0));
        let (ka, kb) = (key(a), key(b));
        kb.0.total_cmp(&ka.0).then(kb.1.total_cmp(&ka.1))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_cover_the_window() {
        let at = |s: &str| crate::tui::format::epoch_ms(&json!(s)).unwrap();
        assert_eq!(
            months(at("2026-10-19T00:00:00Z"), at("2026-11-18T10:00:00Z")),
            vec![(2026, 10), (2026, 11)]
        );
        assert_eq!(
            months(at("2025-12-30T00:00:00Z"), at("2026-01-02T00:00:00Z")),
            vec![(2025, 12), (2026, 1)]
        );
        assert_eq!(
            months(at("2026-11-02T00:00:00Z"), at("2026-11-18T00:00:00Z")).len(),
            1
        );
    }

    #[test]
    fn groups_read_their_own_summary_and_runs_their_usage() {
        let group = json!({"groupType": "job", "jobId": 3805, "jobName": "RCM-Investigator",
            "projectName": "Home-Health-Rcm", "summary": {"executionCount": 14,
            "observedCostUsd": 0.0138, "observedCpuCoreHours": 0.0087,
            "observedMemoryGiBHours": 0.1219, "totalMemoryGiBHours": 0.1219}});
        let job = group_spender(&group, "job");
        assert_eq!(job.name, "RCM-Investigator");
        assert_eq!(job.context.as_deref(), Some("Home-Health-Rcm"));
        assert_eq!((job.cost_usd, job.runs), (Some(0.0138), 14));
        assert!(
            !job.cost_complete,
            "no totalCostUsd: some sessions lack cost"
        );

        let run = run_spender(&json!({"executionId": "e1", "jobName": "load",
            "collectionStatus": "complete", "usage": {"cost": {"totalUsd": 0.031},
            "cpu": {"coreHours": 0.006}, "memory": {"gibHours": 0.32}}}));
        assert_eq!(run.cost_usd, Some(0.031));
        assert!(run.cost_complete);
        assert_eq!(run.context.as_deref(), Some("run e1"));
        let nameless = run_spender(&json!({"executionId": "e2", "jobId": 2716, "usage": {}}));
        assert_eq!(nameless.name, "job 2716");
        assert_eq!(nameless.cost_usd, None);
    }

    #[test]
    fn ranks_by_cost_then_memory_hours() {
        let spender = |name: &str, cost: Option<f64>, gib: f64| Spender {
            name: name.into(),
            context: None,
            cost_usd: cost,
            cost_complete: false,
            core_hours: None,
            gib_hours: Some(gib),
            runs: 1,
            row: Value::Null,
        };
        let mut list = vec![
            spender("no-cost-big", None, 9.0),
            spender("cheap", Some(0.01), 1.0),
            spender("dear", Some(2.0), 1.0),
            spender("no-cost-small", None, 1.0),
        ];
        rank(&mut list);
        let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["dear", "cheap", "no-cost-big", "no-cost-small"]);
    }

    #[test]
    fn a_job_scope_filters_every_query() {
        let scope = Scope {
            from_ms: 0,
            to_ms: DAY_MS,
            project: Some(2098),
            job: Some((3805, "x".into())),
            top: 5,
        };
        let query = scope.query(&[("limit", "1".into())]);
        assert!(query.contains(&("projectId".into(), "2098".into())));
        assert!(query.contains(&("jobId".into(), "3805".into())));
        assert!(scope.is_narrowed());
    }
}
