use super::*;
use crate::cmd::status::sessions::RunningNotebook;
use crate::cmd::status::{ChatPending, FailedRun};
use crate::tui::format::epoch_ms;

/// 2026-11-18T10:00:00Z.
const NOW: i64 = 1_794_996_000_000;
const HOUR: i64 = 60 * 60 * 1000;

fn at(ms: i64) -> String {
    crate::tui::format::iso8601_utc(ms)
}

fn failed_run(job: &str, reason: Reason) -> FailedRun {
    let row = json!({"status": "failed", "jobName": job, "projectName": "Home-Health-Rcm",
        "jobId": 3805, "projectId": 2098, "executionId": "65b14e22", "startedBy": "srijith",
        "startedAt": at(NOW - 2 * HOUR), "completedAt": at(NOW - 2 * HOUR + 30_000)});
    FailedRun {
        run: tiles::job_run(&row),
        reason,
    }
}

fn report() -> Report {
    Report {
        site: Some("preview".into()),
        since: Since::LastLook(NOW - 3 * HOUR),
        now_ms: NOW,
        failed: Ok(FailedRuns {
            runs: vec![failed_run(
                "RCM-Exception-Investigator",
                Reason::Recorded("AnalysisException: table not found".into()),
            )],
            total: 1,
        }),
        open: Ok(tiles::runs(
            &json!({"items": [
                {"status": "running", "jobName": "nightly-load", "projectName": "ETL",
                 "executionId": "e1", "startedAt": at(NOW - HOUR)},
                {"status": "running", "jobName": "memTest", "startedAt": "2026-10-01T17:16:37Z"}
            ], "pagination": {"total": 2}}),
            NOW,
        )),
        agent_versions: Ok(tiles::approvals(&json!({"items": [{
            "id": 271, "agentVersion": 3, "requestedBy": "sarvesh",
            "requestedAt": at(NOW - HOUR), "definitionSnapshot": {"name": "rlo-pair-judge"}
        }]}))),
        paused_agent_runs: Ok(PausedRuns {
            items: vec![
                json!({"runId": "r9", "agentId": "a-1", "agentName": "invoice-bot",
                "startedBy": "robin", "requestedAt": at(NOW - HOUR),
                "interrupts": [{"id": "i1", "toolName": "write_table"}]}),
            ],
            total: 1,
        }),
        chats: Ok(ChatApprovals {
            checked: 5,
            pending: vec![ChatPending {
                conversation_id: "c1".into(),
                name: "Clean the orders table".into(),
                approval: json!({"runId": "run-7", "interruptId": "int-3", "interruptName": "delete_dataset"}),
            }],
        }),
        sql_session: Ok(Some(SqlSession {
            session_id: "s1".into(),
            warehouse: Some("Lineage".into()),
            compute: Some("small".into()),
            state: "Running".into(),
            started_ms: Some(NOW - 2 * HOUR - 5 * 60_000),
        })),
        notebooks: Ok(NotebookScan {
            running: vec![RunningNotebook {
                project_id: 2098,
                project: "Home-Health-Rcm".into(),
                notebook_id: Some("1177".into()),
                name: Some("RCM-Denial-Risk".into()),
                status: "Running".into(),
                runtime: Some("gz-rte-sklearn:1.0.0".into()),
            }],
            unchecked_projects: vec![],
        }),
        usage: Ok(
            json!({"username": "sagatest", "costUsd": {"total": "24.68"},
            "userMetrics": [{"username": "sagatest", "totalCostUsd": "6.86"}],
            "budget": {"usedCents": 2500, "limitCents": 10000, "usedUsd": "25.0", "windowDays": 30}}),
        ),
        latest_release: Ok("v99.0.0".into()),
    }
}

fn text(report: &Report) -> String {
    panel(report).plain_lines().join("\n")
}

#[test]
fn shows_every_section() {
    let text = text(&report());
    for part in [
        "Status · preview · since your last look",
        "Failed job runs · 1",
        "RCM-Exception-Investigator",
        "Home-Health-Rcm · 1h ago · by srijith · run 65b14e22",
        "reason: AnalysisException: table not found",
        "gz fix 3805 --project 2098 --run 65b14e22",
        "Job runs in progress · 1",
        "running for 1h 00m",
        "1 more say running but started over a day ago, probably stuck: memTest",
        "rlo-pair-judge v3",
        "gz agents decide 271 --data '{\"decision\":\"approved\"}'",
        "invoice-bot",
        "approve: gz agents resume a-1 r9",
        "chat waiting to delete_dataset · gz chat approve run-7 --interrupt int-3",
        "only your 5 newest are checked",
        "SQL session s1",
        "Running · warehouse Lineage · compute small · up 2h 05m",
        "sessions started in the web app aren't shown",
        "RCM-Denial-Risk",
        "records no start time",
        "$6.86",
        "$24.68",
        "25%",
        "v99.0.0 is out",
    ] {
        assert!(text.contains(part), "missing {part:?} in\n{text}");
    }
}

#[test]
fn a_failed_section_says_so_and_the_rest_still_show() {
    let mut report = report();
    report.failed = Err("HTTP 503: Unable to list execution observability".into());
    report.chats = Err("HTTP 401".into());
    report.notebooks = Err("cannot list your projects".into());
    report.latest_release = Err("cannot reach github".into());
    let text = text(&report);
    for part in [
        "could not load: HTTP 503: Unable to list execution observability",
        "paused chats could not load: HTTP 401",
        "notebooks could not load: cannot list your projects",
        "could not check for a newer release: cannot reach github",
        "rlo-pair-judge v3",
        "SQL session s1",
        "$6.86",
    ] {
        assert!(text.contains(part), "missing {part:?} in\n{text}");
    }
}

#[test]
fn quiet_sections_say_none() {
    let mut report = report();
    report.failed = Ok(FailedRuns {
        runs: vec![],
        total: 0,
    });
    report.open = Ok(Runs::default());
    report.agent_versions = Ok(vec![]);
    report.paused_agent_runs = Ok(PausedRuns {
        items: vec![],
        total: 0,
    });
    report.chats = Ok(ChatApprovals {
        checked: 5,
        pending: vec![],
    });
    report.sql_session = Ok(None);
    report.notebooks = Ok(NotebookScan::default());
    report.latest_release = Ok(format!("v{}", env!("CARGO_PKG_VERSION")));
    let text = text(&report);
    for part in [
        "Failed job runs · 0",
        "Job runs in progress · 0",
        "Waiting for you",
        "nothing",
        "no SQL session kept by gz",
        "no notebooks of yours running",
        "is the latest release",
    ] {
        assert!(text.contains(part), "missing {part:?} in\n{text}");
    }
}

#[test]
fn failures_without_a_reason_point_at_the_logs_and_big_counts_say_what_is_shown() {
    let mut report = report();
    let runs: Vec<FailedRun> = (0..12)
        .map(|i| {
            failed_run(
                &format!("job{i}"),
                if i == 0 {
                    Reason::NotRecorded
                } else {
                    Reason::NotAsked
                },
            )
        })
        .collect();
    report.failed = Ok(FailedRuns { runs, total: 68 });
    let text = text(&report);
    assert!(
        text.contains("Failed job runs · 68, newest 10 shown"),
        "{text}"
    );
    assert!(
        text.contains("no reason recorded; read the logs: gz jobs logs --job 65b14e22"),
        "{text}"
    );
    assert!(!text.contains("job11"), "{text}");
}

#[test]
fn json_has_every_section_and_its_errors() {
    let mut report = report();
    report.notebooks = Err("cannot list your projects".into());
    let value = to_json(&report);
    assert_eq!(value["sinceFrom"], "lastLook");
    assert_eq!(
        value["failedRuns"]["runs"][0]["reason"]["recorded"],
        "AnalysisException: table not found"
    );
    assert_eq!(
        value["runsInProgress"]["probablyStuck"][0]["jobName"],
        "memTest"
    );
    assert_eq!(value["agentVersionsToReview"][0]["id"], 271);
    assert_eq!(value["pausedAgentRuns"]["total"], 1);
    assert_eq!(value["pausedChats"]["pending"][0]["conversationId"], "c1");
    assert_eq!(value["sqlSession"]["kept"]["sessionId"], "s1");
    assert_eq!(value["notebooks"]["error"], "cannot list your projects");
    assert_eq!(value["aiUsage"]["costUsd"]["total"], "24.68");
    assert_eq!(value["release"]["newer"], true);
    assert!(epoch_ms(&value["since"]).is_some());
}
