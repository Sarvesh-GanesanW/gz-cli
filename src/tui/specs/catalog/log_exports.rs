//! Catalog › Log exports (one row per export run) and Export schedules.
//!
//! Verified contracts (TUI research, "Log exports" / "Export schedules"; the
//! `logexports` gateway Lambda, LX = ground-zero/amplify/backend/function/LogExport/src):
//! - `GET /log-export/runs?limit=&offset=[&scheduleName=]` → `{runs, total, limit, offset}`,
//!   newest first, limit 1..500 (LX index.py:78-83, run_history.py:83-162). Rows key on
//!   `exportRunId`; the detail endpoint adds nothing, so details come from the row.
//! - `DELETE /log-export/runs/{exportRunId}`: Run Once only; the S3 file stays (index.py:97-115)
//! - `GET /log-export/schedules` → `{schedules}` keyed by `scheduleName` (index.py:126-129).
//!   An enabled schedule past its `scheduleEndDate` still says `active`; the web shows it as
//!   ended (LogExportUtility.js:165-168), and so does this screen.
//! - `POST /log-export/schedules/{name}/stop`, `DELETE /log-export/schedules/{name}`
//!   (index.py:159-168). Edit (PATCH) always re-enables a schedule, so it isn't offered here.

use std::time::Duration;

use serde_json::{json, Value};

use crate::tui::format::{epoch_ms, text_of, Format, StatusMap, TimeUnit};
use crate::tui::nav::{screen, Resource, Source, Verb};
use crate::tui::screen::{
    ActionKind, ActionSpec, After, Column, Cond, CopySpec, DetailField, DetailSpec, Goto, Identity,
    LinkSpec, RelatedSpec, RequestSpec, ScreenSpec, Section, SectionBody, Select, Target,
};
use crate::tui::shape::Paging;
use crate::tui::theme::BadgeTone;

/// The web polls both lists this often (LogExportUtility.js:300-338).
const POLL: Duration = Duration::from_secs(60);

/// Log kinds → the web's labels (LogExportUtility.js:63-70).
const LOG_LABELS: &[(&str, &str)] = &[
    ("etl", "ETL"),
    ("lakehouse", "Lakehouse"),
    ("chat", "Chat"),
    ("activity", "Activity Logs"),
    ("admin", "Admin Logs"),
    ("all", "All"),
];

static RUN_STATUS: StatusMap = StatusMap {
    entries: &[
        ("queued", "Queued", BadgeTone::Warn),
        ("running", "Running", BadgeTone::Info),
        ("success", "Success", BadgeTone::Good),
        ("failed", "Failed", BadgeTone::Bad),
    ],
    fallback: BadgeTone::Info,
};

const TRIGGER_LABELS: &[(&str, &str)] = &[("manual", "Run Once"), ("scheduled", "Scheduled")];

fn log_labels(kinds: &Value) -> String {
    kinds
        .as_array()
        .into_iter()
        .flatten()
        .map(|kind| {
            let code = text_of(kind);
            LOG_LABELS
                .iter()
                .find(|(c, _)| *c == code)
                .map_or(code, |(_, label)| label.to_string())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn first_present<'a>(row: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .map(|k| &row[*k])
        .find(|v| !v.is_null() && v.as_str() != Some(""))
}

/// Display name, start, size, duration and labels the list and detail show.
fn enrich_run(run: &mut Value, _now_ms: i64) {
    let name = first_present(run, &["exportName", "scheduleName"])
        .map(text_of)
        .unwrap_or_else(|| format!("Run Once · {}", text_of(&run["logWindowStartAt"])));
    let started = first_present(run, &["runStartedAt", "startedAt", "createdAt"]).cloned();
    let ended = first_present(run, &["runEndedAt", "completedAt"]).cloned();
    let duration = started
        .as_ref()
        .and_then(epoch_ms)
        .zip(ended.as_ref().and_then(epoch_ms))
        .map(|(from, to)| to - from);
    run["displayName"] = json!(name);
    run["started"] = started.unwrap_or(Value::Null);
    run["durationMs"] = json!(duration);
    run["outputBytes"] = run["outputObjects"][0]["outputBytes"].clone();
    run["eventCount"] = run["outputObjects"][0]["eventCount"].clone();
    run["outputFile"] = match (
        run["outputObjects"][0]["bucket"].as_str(),
        run["outputObjects"][0]["key"].as_str(),
    ) {
        (Some(bucket), Some(key)) => json!(format!("s3://{bucket}/{key}")),
        _ => Value::Null,
    };
    run["logs"] = json!(log_labels(&run["resourceTypes"]));
}

const MANUAL: Cond = Cond {
    field: &["triggerType"],
    any_of: &["manual"],
};
const SCHEDULED: Cond = Cond {
    field: &["triggerType"],
    any_of: &["scheduled"],
};

const RUN_COLUMNS: &[Column] = &[
    Column::new("Export", &["displayName"], Format::Text),
    Column::new("Status", &["status"], Format::Status(&RUN_STATUS)).width(8),
    Column::new("Type", &["triggerType"], Format::Label(TRIGGER_LABELS)).width(9),
    Column::new("Logs", &["logs"], Format::Text),
    Column::new("Started", &["started"], Format::RelTime).width(10),
    Column::new("Size", &["outputBytes"], Format::Bytes).width(9),
];

pub static RUNS: ScreenSpec = ScreenSpec {
    noun: "export run",
    columns: RUN_COLUMNS,
    // Newest first, as the server sends them; kept across pages.
    sort: None,
    identity: Identity {
        id: &["exportRunId"],
        title: &["displayName"],
    },
    detail: Some(DetailSpec {
        badge: Some((&["status"], &RUN_STATUS)),
        subtitle: &[(&["exportRunId"], Format::Text)],
        sections: &[
            Section {
                title: "Summary",
                body: SectionBody::Fields(&[
                    DetailField::new("Type", &["triggerType"], Format::Label(TRIGGER_LABELS)),
                    DetailField::new("Logs", &["logs"], Format::Text),
                    DetailField::new("Requested by", &["requestedBy"], Format::Text),
                    DetailField::new("Schedule", &["scheduleName"], Format::Text).optional(),
                    DetailField::new("Prefix", &["destinationPrefix"], Format::Text),
                ]),
            },
            Section {
                title: "Log window",
                body: SectionBody::Fields(&[
                    DetailField::new("From", &["logWindowStartAt"], Format::DateTime),
                    DetailField::new("To", &["logWindowEndAt"], Format::DateTime),
                ]),
            },
            Section {
                title: "Execution",
                body: SectionBody::Fields(&[
                    DetailField::new("Queued", &["createdAt"], Format::DateTime),
                    DetailField::new("Started", &["started"], Format::DateTime),
                    DetailField::new(
                        "Completed",
                        &["completedAt", "runEndedAt"],
                        Format::DateTime,
                    )
                    .optional(),
                    DetailField::new("Took", &["durationMs"], Format::Duration(TimeUnit::Millis))
                        .optional(),
                ]),
            },
            Section {
                title: "Output",
                body: SectionBody::Fields(&[
                    DetailField::new("File", &["outputFile"], Format::Text).optional(),
                    DetailField::new("Events", &["eventCount"], Format::Number).optional(),
                    DetailField::new("Size", &["outputBytes"], Format::Bytes).optional(),
                ]),
            },
            Section {
                title: "Events by log group",
                body: SectionBody::Object(&["outputObjects.0.groupEventCounts"]),
            },
            Section {
                title: "Error",
                body: SectionBody::Text(&["errorMessage"]),
            },
        ],
        copy: Some(CopySpec {
            label: "output file",
            template: "{outputFile}",
        }),
        ..DetailSpec::BLANK
    }),
    actions: &[ActionSpec::new(
        'd',
        "Delete",
        ActionKind::Request(RequestSpec {
            source: Source::new(
                "logexports",
                Verb::Delete,
                "/log-export/runs/{exportRunId}",
                None,
            ),
            success: "Run Once export deleted successfully",
            after: After::RefreshAndClose,
        }),
    )
    .confirm("Delete the Run Once export \"{displayName}\"? Its file in the bucket stays.")
    .when(MANUAL)],
    links: &[
        LinkSpec {
            key: 's',
            label: "schedule",
            goto: Goto {
                target: Target {
                    module: "Catalog",
                    resource: "Export schedules",
                },
                select: Select::Row {
                    field: &["scheduleName"],
                    from: &["scheduleName"],
                },
            },
            when: Some(SCHEDULED),
        },
        LinkSpec {
            key: 'c',
            label: "connection",
            goto: Goto {
                target: Target {
                    module: "Connections",
                    resource: "Connections",
                },
                select: Select::Row {
                    field: &["Id"],
                    from: &["connectionId"],
                },
            },
            when: None,
        },
    ],
    poll: Some(POLL),
    enrich: Some(enrich_run),
    paging: Some(Paging {
        size: 100,
        max: 500,
    }),
    empty: "No log exports found. New exports are made in the web app.",
    ..ScreenSpec::BLANK
};

pub const RUNS_RESOURCE: Resource = screen(
    "Log exports",
    Source::get("logexports", "/log-export/runs"),
    &RUNS,
);

// ---------- schedules ----------

static SCHEDULE_STATE: StatusMap = StatusMap {
    entries: &[
        ("active", "Active", BadgeTone::Good),
        ("stopped", "Stopped", BadgeTone::Muted),
        ("ended", "Ended", BadgeTone::Warn),
    ],
    fallback: BadgeTone::Info,
};

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// A five-field cron in words for the common shapes the web creates
/// ("Every 5 minute(s)", "Daily 04:30"); anything else stays as cron.
pub fn describe_cron(cron: &str) -> String {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    let [minute, hour, day, month, weekday] = fields[..] else {
        return cron.to_string();
    };
    let step = |f: &str| {
        f.strip_prefix("*/")
            .or_else(|| f.strip_prefix("0/"))
            .and_then(|n| n.parse::<u32>().ok())
    };
    let number = |f: &str| f.parse::<u32>().ok();
    let anything = |f: &str| f == "*" || f == "?";
    if !anything(month) {
        return cron.to_string();
    }
    match (
        step(minute),
        number(minute),
        step(hour),
        number(hour),
        anything(day),
        anything(weekday),
    ) {
        (Some(n), _, None, _, true, true) if anything(hour) => format!("Every {n} minute(s)"),
        (_, Some(m), Some(n), _, true, true) => format!("Every {n} hour(s) at :{m:02}"),
        (_, Some(m), None, None, true, true) if anything(hour) => format!("Hourly at :{m:02}"),
        (_, Some(m), _, Some(h), true, true) => format!("Daily {h:02}:{m:02}"),
        (_, Some(m), _, Some(h), true, false) => {
            match number(weekday).and_then(|d| DAYS.get(d as usize)) {
                Some(day) => format!("Weekly on {day} {h:02}:{m:02}"),
                None => cron.to_string(),
            }
        }
        (_, Some(m), _, Some(h), false, true) => match number(day) {
            Some(d) => format!("Monthly on day {d} at {h:02}:{m:02}"),
            None => cron.to_string(),
        },
        _ => cron.to_string(),
    }
}

/// "last 30 min", "last 1 h", "last 7 d".
fn window_text(minutes: Option<u64>) -> Value {
    let Some(minutes) = minutes else {
        return Value::Null;
    };
    let text = if minutes % 1440 == 0 {
        format!("last {} d", minutes / 1440)
    } else if minutes % 60 == 0 {
        format!("last {} h", minutes / 60)
    } else {
        format!("last {minutes} min")
    };
    json!(text)
}

/// The state the web shows (active past its end date is ended), the
/// recurrence in words, the window and log labels.
fn enrich_schedule(schedule: &mut Value, now_ms: i64) {
    let ended = schedule
        .get("scheduleEndDate")
        .and_then(epoch_ms)
        .is_some_and(|end| end <= now_ms);
    let state = match schedule["status"].as_str() {
        Some("active") if ended => "ended",
        Some(other) => other,
        None => "unknown",
    };
    schedule["state"] = json!(state);
    let timezone = schedule["timezone"].as_str().unwrap_or("UTC").to_string();
    let cron = schedule["cronExpression"].as_str().unwrap_or_default();
    schedule["repeats"] = json!(format!("{} {timezone}", describe_cron(cron)));
    schedule["window"] = window_text(schedule["exportWindowMinutes"].as_u64());
    schedule["logs"] = json!(log_labels(&schedule["resourceTypes"]));
}

const ACTIVE: Cond = Cond {
    field: &["state"],
    any_of: &["active"],
};

const RECENT_RUN_COLUMNS: &[Column] = &[
    Column::new("Status", &["status"], Format::Status(&RUN_STATUS)),
    Column::new("Started", &["startedAt", "createdAt"], Format::RelTime),
    Column::new("Events", &["outputObjects.0.eventCount"], Format::Number),
    Column::new("Size", &["outputObjects.0.outputBytes"], Format::Bytes),
];

pub static SCHEDULES: ScreenSpec = ScreenSpec {
    noun: "schedule",
    columns: &[
        Column::new("Schedule", &["scheduleName"], Format::Text),
        Column::new("Status", &["state"], Format::Status(&SCHEDULE_STATE)).width(8),
        Column::new("Repeats", &["repeats"], Format::Text),
        Column::new("Window", &["window"], Format::Text),
        Column::new("Logs", &["logs"], Format::Text),
        Column::new("Ends", &["scheduleEndDate"], Format::Date).width(10),
    ],
    sort: Some(crate::tui::screen::Sort {
        field: &["scheduleName"],
        descending: false,
    }),
    identity: Identity {
        id: &["scheduleName"],
        title: &["scheduleName"],
    },
    detail: Some(DetailSpec {
        badge: Some((&["state"], &SCHEDULE_STATE)),
        subtitle: &[(&["repeats"], Format::Text)],
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new("Repeats", &["repeats"], Format::Text),
                DetailField::new("Cron", &["cronExpression"], Format::Text),
                DetailField::new("Exports the", &["window"], Format::Text),
                DetailField::new("Logs", &["logs"], Format::Text),
                DetailField::new("Prefix", &["destination.prefix"], Format::Text),
                DetailField::new("Starts", &["scheduleStartDate"], Format::DateTime).optional(),
                DetailField::new("Ends", &["scheduleEndDate"], Format::DateTime).optional(),
                DetailField::new("Created by", &["createdBy"], Format::Text),
                DetailField::new("Modified by", &["modifiedBy"], Format::Text).optional(),
                DetailField::new("Modified", &["modifiedAt"], Format::RelTime),
            ]),
        }],
        related: &[RelatedSpec {
            title: "Last 10 runs",
            source: Source::get(
                "logexports",
                "/log-export/runs?scheduleName={scheduleName}&limit=10&offset=0",
            ),
            columns: RECENT_RUN_COLUMNS,
            when: None,
            empty: "No runs found for this schedule.",
            limit: 10,
        }],
        ..DetailSpec::BLANK
    }),
    actions: &[
        ActionSpec::new(
            'x',
            "Stop schedule",
            ActionKind::Request(RequestSpec {
                source: Source::post(
                    "logexports",
                    "/log-export/schedules/{scheduleName}/stop",
                    "{}",
                ),
                success: "Schedule stopped successfully",
                after: After::Refresh,
            }),
        )
        .confirm("Stop schedule {scheduleName}? Only editing it in the web app starts it again.")
        .when(ACTIVE),
        ActionSpec::new(
            'd',
            "Delete",
            ActionKind::Request(RequestSpec {
                source: Source::new(
                    "logexports",
                    Verb::Delete,
                    "/log-export/schedules/{scheduleName}",
                    None,
                ),
                success: "Schedule deleted successfully",
                after: After::RefreshAndClose,
            }),
        )
        .destructive("Delete the schedule {scheduleName}? Past runs and their files stay."),
    ],
    links: &[
        LinkSpec {
            key: 'R',
            label: "runs",
            goto: Goto {
                target: Target {
                    module: "Catalog",
                    resource: "Log exports",
                },
                select: Select::Filter {
                    from: &["scheduleName"],
                },
            },
            when: None,
        },
        LinkSpec {
            key: 'c',
            label: "connection",
            goto: Goto {
                target: Target {
                    module: "Connections",
                    resource: "Connections",
                },
                select: Select::Row {
                    field: &["Id"],
                    from: &["destination.connectionId"],
                },
            },
            when: None,
        },
    ],
    poll: Some(POLL),
    enrich: Some(enrich_schedule),
    empty: "No export schedules. They are created in the web app.",
    ..ScreenSpec::BLANK
};

pub const SCHEDULES_RESOURCE: Resource = screen(
    "Export schedules",
    Source::get("logexports", "/log-export/schedules"),
    &SCHEDULES,
);

#[cfg(test)]
mod tests;
