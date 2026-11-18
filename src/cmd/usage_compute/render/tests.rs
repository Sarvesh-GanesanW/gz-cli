use super::*;
use crate::cmd::status::sessions::{NotebookScan, SqlSession};
use crate::cmd::usage_compute::Scope;

/// 2026-11-18T10:00:00Z.
const NOW: i64 = 1_794_996_000_000;
const DAY: i64 = 24 * 60 * 60 * 1000;

fn spender(name: &str, cost: Option<f64>, complete: bool) -> Spender {
    Spender {
        name: name.into(),
        context: Some("Home-Health-Rcm".into()),
        cost_usd: cost,
        cost_complete: complete,
        core_hours: Some(0.0087),
        gib_hours: Some(0.1219),
        runs: 14,
        row: json!({"jobId": 3805}),
    }
}

fn report() -> Report {
    Report {
        site: Some("preview".into()),
        scope: Scope {
            from_ms: NOW - 30 * DAY,
            to_ms: NOW,
            project: None,
            job: None,
            top: 2,
        },
        now_ms: NOW,
        jobs: Ok(json!({"executionCount": 629, "sessionCount": 471,
            "observedCostUsd": 1.0298, "observedCpuCoreHours": 4.574, "observedMemoryGiBHours": 14.97,
            "totalCostUsd": null, "totalCpuCoreHours": null, "totalMemoryGiBHours": null,
            "coverage": {"costCompleteUnitCount": 288, "aggregationUnitCount": 561,
                "missingSourceUnitCount": 100, "incompleteSourceUnitCount": 153,
                "costCoveredWindowSeconds": 35983.45, "selectedWindowSeconds": 175170365.137}})),
        datasets: Some(Ok(json!({"executionCount": 22, "sessionCount": 22,
            "totalCostUsd": 0.5, "totalCpuCoreHours": 1.0, "totalMemoryGiBHours": 2.0}))),
        top_projects: Some(Ok(vec![spender("DataQuality", Some(0.058), false)])),
        top_jobs: Some(Ok(vec![
            spender("RCM-Investigator", Some(0.0138), true),
            spender("no-data", None, false),
            spender("third", Some(0.001), true),
        ])),
        top_datasets: Some(Ok(vec![])),
        top_runs: Ok(TopRuns {
            runs: vec![spender("nightly", Some(0.031), true)],
            read: 2000,
            total: 2629,
        }),
        bill: Some(Ok(vec![BillMonth {
            year: 2026,
            month: 11,
            reply: json!({"bill": [{"name": "vCPU", "unit": "GB-Hour", "consumption": "180.67", "cost": "45.17"}]}),
        }])),
        sql_session: Ok(Some((
            SqlSession {
                session_id: "s1".into(),
                warehouse: Some("Lineage".into()),
                compute: Some("small".into()),
                state: "Running".into(),
                started_ms: Some(NOW - 3 * 60 * 60 * 1000),
            },
            Ok(false),
        ))),
        notebooks: Ok(NotebookScan::default()),
    }
}

fn text(report: &Report) -> String {
    panel(report).plain_lines().join("\n")
}

#[test]
fn shows_totals_top_spenders_bill_and_running_compute() {
    let text = text(&report());
    for part in [
        "Compute · preview · since 2026-10-19 10:00 UTC",
        "629 in 471 sessions",
        "$1.03 observed; the full total isn't shown because 100 of 561 sessions without metrics, 153 of 561 not fully collected (still running, or collection stopped short), 273 of 561 without cost data, cost covers 0% of the run time",
        "4.57 core-hours observed (some sessions have no data)",
        "Dataset refreshes",
        "$0.50",
        "DataQuality",
        "$0.06+ · 0.01 core-h · 0.12 GiB-h · 14 runs · Home-Health-Rcm",
        "RCM-Investigator",
        "cost not recorded",
        "Top datasets",
        "none in this window",
        "Top runs · the costliest of the newest 2000 of 2629 runs",
        "180.67 GB-Hour of pod memory · $45.17",
        "SQL session s1",
        "up 3h 00m",
        "idle: no statement of yours is running in it",
        "SQL sessions' and notebooks' resource-hours and cost can't be shown",
        "Kubecost's allocation price",
    ] {
        assert!(text.contains(part), "missing {part:?} in\n{text}");
    }
    assert!(!text.contains("third"), "--top 2 lists two: {text}");
}

#[test]
fn an_incomplete_window_names_its_one_gap() {
    let summary = json!({"executionCount": 4, "sessionCount": 4, "observedCostUsd": 0.17,
        "coverage": {"aggregationUnitCount": 4, "costCompleteUnitCount": 4, "incompleteSourceUnitCount": 1,
            "missingSourceUnitCount": 0, "costCoveredWindowSeconds": 8046.7, "selectedWindowSeconds": 8046.7}});
    let text = Panel::new("t")
        .section(None, summary_rows(&Ok(summary)))
        .plain_lines()
        .join("\n");
    assert!(
        text.contains(
            "$0.17 observed; the full total isn't shown because 1 of 4 not fully collected"
        ),
        "{text}"
    );
    assert!(!text.contains("no cost data"), "{text}");
}

#[test]
fn a_failed_section_says_so_and_the_rest_still_show() {
    let mut report = report();
    report.jobs = Err("HTTP 500: Unable to list execution observability".into());
    report.bill = Some(Err("HTTP 403: Forbidden".into()));
    report.sql_session = Ok(Some((
        match &report.sql_session {
            Ok(Some((session, _))) => session.clone(),
            _ => unreachable!(),
        },
        Err("HTTP 503".into()),
    )));
    let text = text(&report);
    for part in [
        "could not load: HTTP 500: Unable to list execution observability",
        "could not load: HTTP 403: Forbidden",
        "whether it is idle could not be checked: HTTP 503",
        "DataQuality",
    ] {
        assert!(text.contains(part), "missing {part:?} in\n{text}");
    }
}

#[test]
fn a_narrowed_report_leaves_out_site_wide_sections() {
    let mut report = report();
    report.scope.job = Some((3805, "RCM-Investigator".into()));
    report.datasets = None;
    report.top_projects = None;
    report.top_jobs = None;
    report.top_datasets = None;
    report.bill = None;
    let text = text(&report);
    assert!(text.contains("job RCM-Investigator (3805)"), "{text}");
    for gone in [
        "Dataset refreshes",
        "Top projects",
        "Top jobs",
        "Platform bill",
    ] {
        assert!(!text.contains(gone), "{gone} in\n{text}");
    }
}

#[test]
fn json_keeps_records_and_errors() {
    let mut report = report();
    report.notebooks = Err("cannot list your projects".into());
    let value = to_json(&report);
    assert_eq!(value["jobRuns"]["executionCount"], 629);
    assert_eq!(value["topJobs"][0]["costComplete"], true);
    assert_eq!(value["topJobs"][0]["record"]["jobId"], 3805);
    assert_eq!(value["topRuns"]["total"], 2629);
    assert_eq!(value["bill"][0]["bill"]["bill"][0]["cost"], "45.17");
    assert_eq!(value["sqlSession"]["kept"]["busy"], false);
    assert_eq!(value["notebooks"]["error"], "cannot list your projects");
    assert!(value["job"].is_null());
}
