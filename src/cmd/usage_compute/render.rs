//! `gz usage compute` as a panel, and as JSON with everything gathered.

use serde_json::{json, Value};

use super::{BillMonth, Report, Section, Spender, TopRuns};
use crate::cmd::status::render::{compute_rows, section_json};
use crate::cmd::status::sessions::SQL_SESSIONS_SCOPE;
use crate::code_ui::panel::{Panel, Row};
use crate::tui::format::date_time;

/// The bill's compute line (site manager `service` table, type `vCPU`).
const BILL_COMPUTE_LINE: &str = "vCPU";

const COST_METHOD: &str = "cost is Kubecost's allocation price for the CPU and memory the pods were given, in USD; a session's idle time between runs isn't counted";
const NO_SESSION_USAGE: &str = "the lakehouse and notebook services record no usage, so SQL sessions' and notebooks' resource-hours and cost can't be shown";
const PARTIAL_MARK: &str = "+";

pub fn panel(report: &Report) -> Panel {
    let mut panel = Panel::new("Compute").aside(aside(report));
    panel = panel.section(Some("Job runs"), summary_rows(&report.jobs));
    if let Some(datasets) = &report.datasets {
        panel = panel.section(Some("Dataset refreshes"), summary_rows(datasets));
    }
    let top = report.scope.top;
    for (title, list) in [
        ("Top projects", &report.top_projects),
        ("Top jobs", &report.top_jobs),
        ("Top datasets", &report.top_datasets),
    ] {
        if let Some(list) = list {
            panel = panel.section(Some(title), spender_rows(list, top));
        }
    }
    let (heading, rows) = top_run_rows(&report.top_runs, top);
    panel = panel.section(Some(&heading), rows);
    if let Some(bill) = &report.bill {
        panel = panel.section(
            Some("Platform bill · compute line, whole site"),
            bill_rows(bill),
        );
    }
    panel
        .section(
            Some("Left running · costs money while it runs"),
            running_rows(report),
        )
        .footer(format!(
            "{COST_METHOD}. {PARTIAL_MARK} marks a cost missing some sessions' data."
        ))
}

fn aside(report: &Report) -> String {
    let scope = &report.scope;
    let mut parts: Vec<String> = report.site.iter().cloned().collect();
    parts.push(format!("since {}", date_time(scope.from_ms)));
    if let Some((id, name)) = &scope.job {
        parts.push(format!("job {name} ({id})"));
    } else if let Some(project) = scope.project {
        parts.push(format!("project {project}"));
    }
    parts.join(" · ")
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64()
}

fn usd(amount: f64) -> String {
    if amount > 0.0 && amount < 0.01 {
        "<$0.01".into()
    } else {
        format!("${amount:.2}")
    }
}

fn hours(amount: f64) -> String {
    if amount >= 100.0 {
        format!("{amount:.0}")
    } else {
        format!("{amount:.2}")
    }
}

/// The window's totals: complete figures when the server has them, else
/// what was observed and how much of the window that covers.
fn summary_rows(summary: &Section<Value>) -> Vec<Row> {
    let summary = match summary {
        Ok(summary) => summary,
        Err(err) => return vec![Row::note(format!("could not load: {err}"))],
    };
    let runs = summary["executionCount"].as_u64().unwrap_or(0);
    if runs == 0 {
        return vec![Row::note("no runs in this window")];
    }
    let mut rows = vec![Row::field(
        "runs",
        format!(
            "{runs} in {} sessions",
            summary["sessionCount"].as_u64().unwrap_or(0)
        ),
    )];
    rows.push(cost_row(summary));
    for (label, total, observed, unit) in [
        (
            "CPU",
            "totalCpuCoreHours",
            "observedCpuCoreHours",
            "core-hours",
        ),
        (
            "memory",
            "totalMemoryGiBHours",
            "observedMemoryGiBHours",
            "GiB-hours",
        ),
    ] {
        let value = match (number(&summary[total]), number(&summary[observed])) {
            (Some(total), _) => format!("{} {unit}", hours(total)),
            (None, Some(seen)) => format!(
                "{} {unit} observed (some sessions have no data)",
                hours(seen)
            ),
            (None, None) => "not recorded".into(),
        };
        rows.push(Row::field(label, value));
    }
    rows
}

fn cost_row(summary: &Value) -> Row {
    if let Some(total) = number(&summary["totalCostUsd"]) {
        return Row::field("cost", usd(total));
    }
    let Some(observed) = number(&summary["observedCostUsd"]) else {
        return Row::field(
            "cost",
            "not recorded for these runs; see CPU and memory hours",
        );
    };
    let mut reasons = incomplete_reasons(summary);
    if reasons.is_empty() {
        reasons.push("the server marks the window incomplete".into());
    }
    Row::field(
        "cost",
        format!(
            "{} observed; the full total isn't shown because {}",
            usd(observed),
            reasons.join(", ")
        ),
    )
}

/// Why the server withheld `totalCostUsd`: the conditions of
/// fn_r_execution_observability_summary (site migration
/// 0002_consumption_rollup.sql), read from its `coverage`.
fn incomplete_reasons(summary: &Value) -> Vec<String> {
    let coverage = &summary["coverage"];
    let count = |key: &str| coverage[key].as_u64().unwrap_or(0);
    let sessions = count("aggregationUnitCount");
    let mut reasons = Vec::new();
    if count("missingSourceUnitCount") > 0 {
        reasons.push(format!(
            "{} of {sessions} sessions without metrics",
            count("missingSourceUnitCount")
        ));
    }
    if count("incompleteSourceUnitCount") > 0 {
        reasons.push(format!(
            "{} of {sessions} not fully collected (still running, or collection stopped short)",
            count("incompleteSourceUnitCount")
        ));
    }
    let with_cost = count("costCompleteUnitCount");
    if with_cost < sessions {
        reasons.push(format!(
            "{} of {sessions} without cost data",
            sessions - with_cost
        ));
    }
    let covered = number(&coverage["costCoveredWindowSeconds"]).unwrap_or(0.0);
    let window = number(&coverage["selectedWindowSeconds"]).unwrap_or(0.0);
    if window > 0.0 && covered < window * 0.99 {
        reasons.push(format!(
            "cost covers {:.0}% of the run time",
            covered / window * 100.0
        ));
    }
    if summary["crossGroupOverlap"].as_bool() == Some(true) {
        reasons.push("some runs share sessions with runs outside this filter".into());
    }
    reasons
}

fn spender_text(spender: &Spender) -> String {
    let mut parts = vec![match spender.cost_usd {
        Some(cost) if spender.cost_complete => usd(cost),
        Some(cost) => format!("{}{PARTIAL_MARK}", usd(cost)),
        None => "cost not recorded".into(),
    }];
    if let Some(core) = spender.core_hours {
        parts.push(format!("{} core-h", hours(core)));
    }
    if let Some(gib) = spender.gib_hours {
        parts.push(format!("{} GiB-h", hours(gib)));
    }
    if spender.runs > 1 {
        parts.push(format!("{} runs", spender.runs));
    }
    if let Some(context) = &spender.context {
        parts.push(context.clone());
    }
    parts.join(" · ")
}

fn spender_rows(list: &Section<Vec<Spender>>, top: usize) -> Vec<Row> {
    match list {
        Ok(list) if list.is_empty() => vec![Row::note("none in this window")],
        Ok(list) => list
            .iter()
            .take(top)
            .map(|s| Row::entry(&s.name, spender_text(s)))
            .collect(),
        Err(err) => vec![Row::note(format!("could not load: {err}"))],
    }
}

fn top_run_rows(top_runs: &Section<TopRuns>, top: usize) -> (String, Vec<Row>) {
    let runs = match top_runs {
        Ok(runs) => runs,
        Err(err) => {
            return (
                "Top runs".into(),
                vec![Row::note(format!("could not load: {err}"))],
            )
        }
    };
    let heading = if (runs.read as u64) < runs.total {
        format!(
            "Top runs · the costliest of the newest {} of {} runs",
            runs.read, runs.total
        )
    } else {
        format!("Top runs · of {}", runs.total)
    };
    let list: Section<Vec<Spender>> = Ok(runs.runs.clone());
    (heading, spender_rows(&list, top))
}

fn bill_rows(bill: &Section<Vec<BillMonth>>) -> Vec<Row> {
    let months = match bill {
        Ok(months) => months,
        Err(err) => return vec![Row::note(format!("could not load: {err}"))],
    };
    months.iter().map(bill_row).collect()
}

fn bill_row(month: &BillMonth) -> Row {
    let label = format!("{}-{:02}", month.year, month.month);
    let line = month.reply["bill"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|line| line["name"].as_str() == Some(BILL_COMPUTE_LINE));
    let Some(line) = line else {
        return Row::field(&label, "no compute billed");
    };
    let text = |key: &str| line[key].as_str().unwrap_or("?").to_string();
    Row::field(
        &label,
        format!(
            "{} {} of pod memory · ${}",
            text("consumption"),
            text("unit"),
            text("cost")
        ),
    )
}

fn running_rows(report: &Report) -> Vec<Row> {
    let sql = report
        .sql_session
        .clone()
        .map(|kept| kept.map(|(session, _)| session));
    let mut rows = compute_rows(&sql, &report.notebooks, report.now_ms);
    if let Ok(Some((_, busy))) = &report.sql_session {
        rows.insert(
            1,
            Row::note(match busy {
                Ok(true) => "  a statement is running in it".to_string(),
                Ok(false) => "  idle: no statement of yours is running in it".to_string(),
                Err(err) => format!("  whether it is idle could not be checked: {err}"),
            }),
        );
    }
    rows.push(Row::note(NO_SESSION_USAGE));
    rows
}

/// Everything gathered, each section as data or `{"error": "…"}`.
pub fn to_json(report: &Report) -> Value {
    let scope = &report.scope;
    json!({
        "site": report.site,
        "from": crate::tui::format::iso8601_utc(scope.from_ms),
        "to": crate::tui::format::iso8601_utc(scope.to_ms),
        "projectId": scope.project,
        "job": scope.job.as_ref().map(|(id, name)| json!({"id": id, "name": name})),
        "costMethod": COST_METHOD,
        "jobRuns": section_json(&report.jobs, Value::clone),
        "datasetRefreshes": report.datasets.as_ref().map(|s| section_json(s, Value::clone)),
        "topProjects": report.top_projects.as_ref().map(spenders_json),
        "topJobs": report.top_jobs.as_ref().map(spenders_json),
        "topDatasets": report.top_datasets.as_ref().map(spenders_json),
        "topRuns": section_json(&report.top_runs, |runs| json!({
            "read": runs.read,
            "total": runs.total,
            "runs": runs.runs.iter().map(spender_json).collect::<Vec<_>>(),
        })),
        "bill": report.bill.as_ref().map(|bill| section_json(bill, |months| json!(
            months.iter().map(|m| json!({"year": m.year, "month": m.month, "bill": m.reply})).collect::<Vec<_>>()
        ))),
        "sqlSession": section_json(&report.sql_session, |kept| json!({
            "kept": kept.as_ref().map(|(session, busy)| json!({
                "session": session,
                "busy": section_json(busy, |b| json!(b)),
            })),
            "scope": SQL_SESSIONS_SCOPE,
        })),
        "notebooks": section_json(&report.notebooks, |scan| json!(scan)),
        "sessionUsage": NO_SESSION_USAGE,
    })
}

fn spenders_json(list: &Section<Vec<Spender>>) -> Value {
    section_json(list, |list| {
        json!(list.iter().map(spender_json).collect::<Vec<_>>())
    })
}

fn spender_json(spender: &Spender) -> Value {
    json!({
        "name": spender.name,
        "context": spender.context,
        "costUsd": spender.cost_usd,
        "costComplete": spender.cost_complete,
        "cpuCoreHours": spender.core_hours,
        "memoryGiBHours": spender.gib_hours,
        "runs": spender.runs,
        "record": spender.row,
    })
}

#[cfg(test)]
mod tests;
