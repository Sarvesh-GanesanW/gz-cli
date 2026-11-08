//! Schedules: timed ETL job runs, their latest and recent runs.
//!
//! Verified contracts (TUI research notes, "Schedules"):
//! - `GET schedules /schedule/executions` → one row per schedule with its latest run:
//!   `{scheduleId, scheduleName, isActive, status, startTime, stopTime, executionId,
//!   sessionId, jobId, jobName, projectId, projectName, computeId, rteName, scheduleType,
//!   modifiedBy, modifiedOn}`. The web's Scheduler page lists exactly this.
//! - `GET /schedule/{id}` → `{cron, enabled, startAt, stopAt, config, …}` (no names)
//! - `GET /schedule/{id}/runs` → the last 10 runs; `GET /schedule/{id}/email`
//! - pause/resume `PUT /schedule/stop|start/{id}/{name}` (no body); delete
//!   `DELETE /schedule/{id}/{name}`; run now `PUT /schedule/run/{id}/{name}`
//!   (`views::schedule_run`); edit `PUT /schedule/{id}/{name}` with the whole
//!   record (`views::schedule_form`)
//! - crons are EventBridge Scheduler expressions in UTC, Sunday = 1 (`views::cron`)
//! - a schedule run is an ordinary ETL run: its `executionId` works with
//!   `/etl/logs` and `/etl/job {action: Abort}`

use serde_json::Value;

use crate::tui::format::{self, epoch_ms, Format, StatusMap, TimeUnit};
use crate::tui::nav::{Drill, GridSpec, Module, Resource, Source, Verb, View};
use crate::tui::screen::{
    ActionKind, ActionSpec, After, Column, Cond, DetailField, DetailSpec, Goto, Identity, LinkSpec,
    RelatedSpec, RequestSpec, ScreenSpec, Section, SectionBody, Select, Sort, Target,
};
use crate::tui::theme::BadgeTone;
use crate::tui::views::cron;
use crate::tui::views::run_log::{follow, RunLog, RunTarget};
use crate::tui::views::schedule_form::ScheduleForm;
use crate::tui::views::schedule_run::ScheduleRun;
use crate::tui::views::Ctx;

const ID: &[&str] = &["scheduleId", "Id"];
const NAME: &[&str] = &["scheduleName", "name"];
const STATE: &[&str] = &["_state"];
const STUCK: &str = "stuck";
const NEXT_RUNS: usize = 3;

/// `exec_status` values, the computed `Scheduled`, and the derived `stuck`.
static RUN_STATUS: StatusMap = StatusMap {
    entries: &[
        ("Scheduled", "Scheduled", BadgeTone::Info),
        ("Queued", "Queued", BadgeTone::Muted),
        ("Running", "Running", BadgeTone::Info),
        ("Success", "Succeeded", BadgeTone::Good),
        ("Failed", "Failed", BadgeTone::Bad),
        ("Aborted", "Aborted", BadgeTone::Warn),
        (STUCK, "May be stuck", BadgeTone::Warn),
    ],
    fallback: BadgeTone::Muted,
};

static ACTIVE: StatusMap = StatusMap {
    entries: &[
        ("true", "● Active", BadgeTone::Good),
        ("false", "○ Paused", BadgeTone::Muted),
    ],
    fallback: BadgeTone::Muted,
};

const TYPES: &[(&str, &str)] = &[("BATCH", "Batch"), ("STREAMING", "Streaming")];

const IS_ACTIVE: Cond = Cond {
    field: &["isActive", "enabled"],
    any_of: &["true"],
};
const IS_PAUSED: Cond = Cond {
    field: &["isActive", "enabled"],
    any_of: &["false"],
};
/// The latest run has started and not stopped (`status` can't say: an
/// enabled schedule with a running run reports `Scheduled`).
const LATEST_ACTIVE: Cond = Cond {
    field: &["_latestActive"],
    any_of: &["true"],
};
const RUN_ACTIVE: Cond = Cond {
    field: &["status"],
    any_of: &["Running", "Queued"],
};

/// A run row's state: its status, or `stuck` when an active run is too old.
fn run_state(row: &Value, now_ms: i64) -> String {
    let status = row["status"].as_str().unwrap_or_default().to_string();
    let active = row["stopTime"].is_null() && row["executionId"].is_string();
    let elapsed = row["startTime"]
        .as_str()
        .and_then(|s| epoch_ms(&Value::String(s.into())));
    let running = RUN_ACTIVE.holds(row) || (active && status == "Scheduled");
    match elapsed {
        Some(start) if running && follow::may_be_stuck(now_ms - start, None, false) => STUCK.into(),
        _ => status,
    }
}

/// Adds the job path, latest-run state, and (once the detail has the cron)
/// the timing in plain English with the next runs.
fn enrich_schedule(row: &mut Value, now_ms: i64) {
    let job = format!(
        "{} › {}",
        format::text_of(&row["projectName"]),
        format::text_of(&row["jobName"])
    );
    row["_job"] = Value::String(job);
    let latest_active = row["executionId"].is_string() && row["stopTime"].is_null();
    row["_latestActive"] = Value::Bool(latest_active);
    row["_state"] = Value::String(run_state(row, now_ms));
    let Some(text) = row["cron"].as_str().map(str::to_string) else {
        return;
    };
    let Ok(expression) = cron::parse(&text) else {
        row["_runs"] = Value::String(format!("{text} (not understood)"));
        return;
    };
    row["_runs"] = Value::String(cron::describe(&expression));
    let paused = IS_PAUSED.holds(row);
    let start = row["startAt"]
        .as_str()
        .and_then(|s| epoch_ms(&Value::String(s.into())));
    let end = row["stopAt"]
        .as_str()
        .and_then(|s| epoch_ms(&Value::String(s.into())));
    let from = start.map_or(now_ms, |s| s.max(now_ms) - 1);
    let next = cron::next_runs(&expression, from, end, NEXT_RUNS);
    row["_next"] = Value::String(if paused {
        "paused".into()
    } else if next.is_empty() {
        "no upcoming runs".into()
    } else {
        next.iter()
            .map(|t| format::date_time(*t))
            .collect::<Vec<_>>()
            .join(" · ")
    });
}

fn enrich_run(row: &mut Value, now_ms: i64) {
    let state = run_state(row, now_ms);
    row["_state"] = Value::String(state);
}

fn edit_timing(row: &Value, ctx: &mut Ctx) {
    ctx.push(Box::new(ScheduleForm::edit(row)));
}

fn run_now(row: &Value, ctx: &mut Ctx) {
    ctx.push(Box::new(ScheduleRun::new(row)));
}

fn latest_logs(row: &Value, ctx: &mut Ctx) {
    if !row["executionId"].is_string() {
        return ctx.toast(crate::tui::app::Tone::Info, "This schedule hasn't run yet");
    }
    ctx.push(Box::new(RunLog::new(RunTarget::from_row(row))));
}

const ABORT_BODY: &str =
    r#"{"action":"Abort","sessionId":"{sessionId}","jobIdentifier":"{executionId}"}"#;

const SCHEDULE_ACTIONS: &[ActionSpec] = &[
    ActionSpec::new('R', "Run now…", ActionKind::Custom(run_now)),
    ActionSpec::new('e', "Change timing…", ActionKind::Custom(edit_timing)),
    ActionSpec::new(
        'p',
        "Pause",
        ActionKind::Request(RequestSpec {
            source: Source::new(
                "schedules",
                Verb::Put,
                "/schedule/stop/{scheduleId}/{scheduleName}",
                None,
            ),
            success: "Schedule paused",
            after: After::Refresh,
        }),
    )
    .confirm("Pause {scheduleName}? It won't run until resumed.")
    .when(IS_ACTIVE),
    ActionSpec::new(
        's',
        "Resume",
        ActionKind::Request(RequestSpec {
            source: Source::new(
                "schedules",
                Verb::Put,
                "/schedule/start/{scheduleId}/{scheduleName}",
                None,
            ),
            success: "Schedule resumed",
            after: After::Refresh,
        }),
    )
    .when(IS_PAUSED),
    ActionSpec::new(
        'L',
        "Logs of the latest run",
        ActionKind::Custom(latest_logs),
    ),
    ActionSpec::new(
        'x',
        "Stop the latest run",
        ActionKind::Request(RequestSpec {
            source: Source::new("etl", Verb::Post, "/etl/job", Some(ABORT_BODY)),
            success: "Job aborted successfully",
            after: After::Refresh,
        }),
    )
    .destructive(
        "Stop the running {jobName} started by {scheduleName}? Its whole session stops now.",
    )
    .when(LATEST_ACTIVE),
    ActionSpec::new(
        'd',
        "Delete",
        ActionKind::Request(RequestSpec {
            source: Source::new(
                "schedules",
                Verb::Delete,
                "/schedule/{scheduleId}/{scheduleName}",
                None,
            ),
            success: "Schedule deleted",
            after: After::RefreshAndClose,
        }),
    )
    .destructive("Delete schedule {scheduleName}? Its email notifications go too; past runs stay."),
];

const TO_JOB: LinkSpec = LinkSpec {
    key: 'P',
    label: "job's project",
    goto: Goto {
        target: Target {
            module: "DE/ML",
            resource: "Projects",
        },
        select: Select::Drill {
            field: &["Id"],
            from: &["projectId"],
        },
    },
    when: None,
};

const RUN_COLUMNS: &[Column] = &[
    Column::new("Status", STATE, Format::Status(&RUN_STATUS)),
    Column::new("Started", &["startTime"], Format::RelTime),
    Column::new("Stopped", &["stopTime"], Format::RelTime),
    Column::new("Job", &["jobName"], Format::Text),
    Column::new("Runtime", &["rteName"], Format::Text),
];

pub static SCHEDULES: ScreenSpec = ScreenSpec {
    noun: "schedule",
    columns: &[
        Column::new("Schedule", NAME, Format::Text),
        Column::new("State", &["isActive"], Format::Status(&ACTIVE)).width(9),
        Column::new("Job", &["_job"], Format::Text),
        Column::new("Last run", STATE, Format::Status(&RUN_STATUS)),
        Column::new("Last run at", &["startTime"], Format::RelTime).width(11),
        Column::new("Type", &["scheduleType"], Format::Label(TYPES)).width(9),
    ],
    sort: Some(Sort {
        field: &["projectName"],
        descending: false,
    }),
    identity: Identity {
        id: ID,
        title: NAME,
    },
    detail: Some(DetailSpec {
        load: Some(Source::get("schedules", "/schedule/{scheduleId}")),
        missing: "Schedule not found. It may have been deleted.",
        badge: Some((&["isActive", "enabled"], &ACTIVE)),
        subtitle: &[(&["_job"], Format::Text)],
        sections: &[
            Section {
                title: "Timing (UTC)",
                body: SectionBody::Fields(&[
                    DetailField::new("Runs", &["_runs"], Format::Text),
                    DetailField::new("Next", &["_next"], Format::Text),
                    DetailField::new("Cron", &["cron"], Format::Text),
                    DetailField::new("Starts", &["startAt"], Format::DateTime),
                    DetailField::new("Ends", &["stopAt"], Format::DateTime).optional(),
                    DetailField::new("Type", &["scheduleType"], Format::Label(TYPES)),
                ]),
            },
            Section {
                title: "Job",
                body: SectionBody::Fields(&[
                    DetailField::new("Job", &["_job"], Format::Text),
                    DetailField::new("Runtime", &["rteName"], Format::Text),
                    DetailField::new("Compute id", &["computeId"], Format::Text),
                    DetailField::new("Description", &["description"], Format::Text).optional(),
                    DetailField::new("Changed by", &["modifiedBy"], Format::Text).optional(),
                    DetailField::new("Changed", &["modifiedOn"], Format::RelTime).optional(),
                ]),
            },
        ],
        related: &[
            RelatedSpec {
                title: "Recent runs",
                source: Source::get("schedules", "/schedule/{scheduleId}/runs"),
                columns: &[
                    Column::new("Status", &["status"], Format::Status(&RUN_STATUS)),
                    Column::new("Started", &["startTime"], Format::RelTime),
                    Column::new("Stopped", &["stopTime"], Format::RelTime),
                    Column::new("Runtime", &["rteName"], Format::Text),
                ],
                when: None,
                empty: "No runs yet. Press R to run it now.",
                limit: 10,
            },
            RelatedSpec {
                title: "Email notifications",
                source: Source::get("schedules", "/schedule/{scheduleId}/email"),
                columns: &[
                    Column::new("When", &["type"], Format::Text),
                    Column::new("To", &["toEmail"], Format::Text),
                    Column::new("Subject", &["subject"], Format::Text),
                ],
                when: None,
                empty: "No email notifications",
                limit: 5,
            },
        ],
        ..DetailSpec::BLANK
    }),
    actions: SCHEDULE_ACTIONS,
    links: &[TO_JOB],
    empty: "No schedules yet. Schedule a job from DE/ML › Projects (S on a job).",
    enrich: Some(enrich_schedule),
    ..ScreenSpec::BLANK
};

/// One schedule's runs (an execution each).
static RUNS: ScreenSpec = ScreenSpec {
    noun: "run",
    columns: RUN_COLUMNS,
    sort: Some(Sort {
        field: &["startTime"],
        descending: true,
    }),
    identity: Identity {
        id: &["executionId"],
        title: &["executionId"],
    },
    detail: Some(DetailSpec {
        badge: Some((STATE, &RUN_STATUS)),
        subtitle: &[
            (&["scheduleName"], Format::Text),
            (&["jobName"], Format::Text),
        ],
        sections: &[Section {
            title: "Run",
            body: SectionBody::Fields(&[
                DetailField::new("Started", &["startTime"], Format::DateTime),
                DetailField::new("Stopped", &["stopTime"], Format::DateTime).optional(),
                DetailField::new("Job", &["projectName"], Format::Text),
                DetailField::new("Runtime", &["rteName"], Format::Text),
                DetailField::new("Run id", &["executionId"], Format::Text),
            ]),
        }],
        ..DetailSpec::BLANK
    }),
    actions: &[
        ActionSpec::new('L', "View logs", ActionKind::Custom(latest_logs)),
        ActionSpec::new(
            'x',
            "Stop run",
            ActionKind::Request(RequestSpec {
                source: Source::new("etl", Verb::Post, "/etl/job", Some(ABORT_BODY)),
                success: "Job aborted successfully",
                after: After::Refresh,
            }),
        )
        .destructive("Stop this run of {jobName}? Its whole session stops now.")
        .when(RUN_ACTIVE),
    ],
    links: &[
        TO_JOB,
        LinkSpec {
            key: 's',
            label: "schedule",
            goto: Goto {
                target: Target {
                    module: "Schedules",
                    resource: "Schedules",
                },
                select: Select::Row {
                    field: ID,
                    from: &["scheduleId"],
                },
            },
            when: None,
        },
    ],
    empty: "No runs yet. This schedule hasn't fired.",
    enrich: Some(enrich_run),
    ..ScreenSpec::BLANK
};

/// The Executions resource's first level: schedules to pick runs from.
static EXECUTION_SCHEDULES: ScreenSpec = ScreenSpec {
    noun: "schedule",
    columns: &[
        Column::new("Schedule", NAME, Format::Text),
        Column::new("Job", &["_job"], Format::Text),
        Column::new("Last run", STATE, Format::Status(&RUN_STATUS)),
        Column::new("Last run at", &["startTime"], Format::RelTime),
        Column::new("Took", &["_took"], Format::Duration(TimeUnit::Millis)),
    ],
    sort: Some(Sort {
        field: &["startTime"],
        descending: true,
    }),
    identity: Identity {
        id: ID,
        title: NAME,
    },
    actions: &[ActionSpec::new(
        'L',
        "Logs of the latest run",
        ActionKind::Custom(latest_logs),
    )],
    empty: "No schedules yet",
    enrich: Some(enrich_execution),
    ..ScreenSpec::BLANK
};

fn enrich_execution(row: &mut Value, now_ms: i64) {
    enrich_schedule(row, now_ms);
    let time = |key: &str| {
        row[key]
            .as_str()
            .and_then(|s| epoch_ms(&Value::String(s.into())))
    };
    if let (Some(start), Some(stop)) = (time("startTime"), time("stopTime")) {
        row["_took"] = Value::from(stop - start);
    }
}

static RUN_DRILLS: [Drill; 1] = [Drill {
    noun: "schedule",
    id_keys: ID,
    source: Source::get("schedules", "/schedule/{scheduleId}/runs"),
    screen: Some(&RUNS),
}];

pub const MODULE: Module = Module {
    title: "Schedules",
    icon: "◷",
    resources: &[
        crate::tui::nav::screen(
            "Schedules",
            Source::get("schedules", "/schedule/executions"),
            &SCHEDULES,
        ),
        Resource {
            label: "Executions",
            view: View::Grid(GridSpec {
                source: Source::get("schedules", "/schedule/executions"),
                drills: &RUN_DRILLS,
                detail: None,
                screen: Some(&EXECUTION_SCHEDULES),
            }),
        },
    ],
};

#[cfg(test)]
mod tests;
