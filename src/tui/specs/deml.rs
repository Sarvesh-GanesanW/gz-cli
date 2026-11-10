//! DE/ML: ETL projects and jobs, job runs, published jobs, notebooks,
//! models and experiments.
//!
//! Verified contracts (TUI research notes, "DE/ML"):
//! - projects `GET etlprojects /etl/etlProjects` → `[{Id, Name, owner, modified_on, modified_by}]`;
//!   `GET /etl/etlProjects/{id}` always answers 500, so projects have no detail load
//! - jobs `GET etlprojects /etl/etlJobs/{projectId}/jobs`; one job
//!   `GET /etl/etlJobs/jobs/{projectId}/{jobId}` → `{config, description, fileContent, modified_*}`
//! - runs come from `GET etl /etl/consumption-metrics` (newest first). The job-runs
//!   route `/etl/etlJobs/jobs/{id}/runs` is shadowed and never answers, so it isn't used
//! - per-job aggregates `?groupBy=job&projectId=` (projectId is required there)
//! - run `POST etl /etl/job` (body from `cmd::jobs::run_spec`), stop
//!   `{"action":"Abort", sessionId, jobIdentifier}`, logs `POST etl /etl/logs`
//! - `GET /etl/jobs/{id}/current-run` and `/etl/logs` can mark a stale run Failed, so
//!   they are only ever called for the one run being viewed (`views::run_log`), and
//!   `F` (fix with gz code, `cmd::job_fix`) reads logs only of runs that are over
//! - mlops lists: `{success, notebooks|models|experiments: [...]}`; notebook writes on
//!   `/jupyter/notebook`, model delete `DELETE /mlflow/projects/{projectId}/models?name=`
//!
//! Left out on purpose: project delete (the web lists jobs first, which a spec can't),
//! import/export of jobs and notebooks (multi-step conflict dialogs), starting
//! notebook or MLflow pods (they bill until stopped), and published-notebook deletes
//! (no permission check on a cross-site catalogue).

use serde_json::Value;

use super::rules::{NAME_MAX, NAME_MESSAGE, NAME_PATTERN};
use crate::tui::fields;
use crate::tui::format::{epoch_ms, Format, StatusMap, TimeUnit};
use crate::tui::nav::{Drill, GridSpec, Module, Resource, Source, Verb, View};
use crate::tui::screen::{
    ActionKind, ActionSpec, After, Column, Cond, DetailField, DetailSpec, FieldKind, FormField,
    FormSpec, Goto, Identity, KeyHelp, LinkSpec, RelatedSpec, RequestSpec, ScreenSpec, Section,
    SectionBody, Select, Sort, Target,
};
use crate::tui::theme::BadgeTone;
use crate::tui::views::run_job::RunJob;
use crate::tui::views::run_log::{follow, RunLog, RunTarget};
use crate::tui::views::{Ctx, Effect, HandOff, RowContext};

const ID: &[&str] = &["Id", "id"];
const NAME: &[&str] = &["Name", "name"];
const UPDATED: &[&str] = &["modified_on"];

/// Run statuses as consumption metrics report them, plus the derived
/// `stuck` (see [`enrich_run`]). The web labels `cancelled` "Aborted".
pub static RUN_STATUS: StatusMap = StatusMap {
    entries: &[
        ("queued", "Queued", BadgeTone::Muted),
        ("running", "Running", BadgeTone::Info),
        ("succeeded", "Succeeded", BadgeTone::Good),
        ("failed", "Failed", BadgeTone::Bad),
        ("cancelled", "Aborted", BadgeTone::Warn),
        (STUCK, "May be stuck", BadgeTone::Warn),
    ],
    fallback: BadgeTone::Muted,
};

const STUCK: &str = "stuck";
/// The derived run state field [`enrich_run`] adds.
pub const RUN_STATE: &[&str] = &["_state"];

/// How far a run's usage was collected: the consumption rollup's
/// `collectionStatus` (`not_collected` when nothing was gathered), the ETL
/// provider's `complete`/`partial`/`failed`/`pending`, and `redacted` when
/// the session's usage is hidden from this user.
pub const COLLECTION_LABELS: &[(&str, &str)] = &[
    ("complete", "Complete"),
    ("partial", "Partial"),
    ("failed", "Failed"),
    ("pending", "Pending"),
    ("not_collected", "Not collected"),
    ("redacted", "Hidden"),
];
const ACTIVE_RUN: Cond = Cond {
    field: &["status"],
    any_of: &["queued", "running"],
};

/// Flags active runs that may be stuck: running far too long, or still
/// "running" with an end time (stage has ~185 such rows, months old).
pub fn enrich_run(row: &mut Value, now_ms: i64) {
    let status = row["status"].as_str().unwrap_or_default().to_string();
    let active = ACTIVE_RUN.holds(row);
    let elapsed = row["startedAt"]
        .as_str()
        .and_then(|s| epoch_ms(&Value::String(s.into())))
        .map(|start| now_ms - start);
    let has_end = row["completedAt"].as_str().is_some_and(|s| !s.is_empty());
    let stuck = active && elapsed.is_some_and(|e| follow::may_be_stuck(e, None, has_end));
    row["_state"] = Value::String(if stuck { STUCK.to_string() } else { status });
}

fn open_run_log(row: &Value, ctx: &mut Ctx) {
    ctx.push(Box::new(RunLog::new(RunTarget::from_row(row))));
}

const RUN_COLUMNS: &[Column] = &[
    Column::new("Status", RUN_STATE, Format::Status(&RUN_STATUS)),
    Column::new("Job", &["jobName"], Format::Text),
    Column::new("Project", &["projectName"], Format::Text),
    Column::new("Started", &["startedAt"], Format::RelTime),
    Column::new(
        "Duration",
        &["durationSeconds"],
        Format::Duration(TimeUnit::Seconds),
    ),
    Column::new("Cost", &["usage.cost.totalUsd"], Format::Usd).width(9),
    Column::new("Started by", &["startedBy"], Format::Text),
];

/// Recent runs inside a job or project detail (no project column).
const RELATED_RUN_COLUMNS: &[Column] = &[
    Column::new("Status", &["status"], Format::Status(&RUN_STATUS)),
    Column::new("Job", &["jobName"], Format::Text),
    Column::new("Started", &["startedAt"], Format::RelTime),
    Column::new(
        "Duration",
        &["durationSeconds"],
        Format::Duration(TimeUnit::Seconds),
    ),
    Column::new("Cost", &["usage.cost.totalUsd"], Format::Usd),
    Column::new("Started by", &["startedBy"], Format::Text),
];

/// Per-job aggregates over the last 30 days (`groupBy=job`).
const JOB_USAGE_COLUMNS: &[Column] = &[
    Column::new("Job", &["jobName"], Format::Text),
    Column::new(
        "Last run",
        &["latestExecution.status"],
        Format::Status(&RUN_STATUS),
    ),
    Column::new("Last run at", &["latestStartedAt"], Format::RelTime),
    Column::new("Runs", &["summary.executionCount"], Format::Number),
    Column::new("Success %", &["summary.successRate"], Format::Number),
    Column::new(
        "Avg run",
        &["statistics.durationSeconds.average"],
        Format::Duration(TimeUnit::Seconds),
    ),
    Column::new("Cost", &["statistics.costUsd.total"], Format::Usd),
];

const STOP_RUN: ActionSpec = ActionSpec::new(
    'x',
    "Stop run",
    ActionKind::Request(RequestSpec {
        source: Source::new(
            "etl",
            Verb::Post,
            "/etl/job",
            Some(r#"{"action":"Abort","sessionId":"{sessionId}","jobIdentifier":"{executionId}"}"#),
        ),
        success: "Job aborted successfully",
        after: After::Refresh,
    }),
)
.destructive("Stop this run of {jobName}? Its whole session stops now, sub-jobs included.")
.when(ACTIVE_RUN);

const RUN_LOGS: ActionSpec = ActionSpec::new('L', "View logs", ActionKind::Custom(open_run_log));

const FAILED_RUN: Cond = Cond {
    field: &["status"],
    any_of: &["failed"],
};

/// `gz jobs fix` on a failed run: only runs that are over, so the fix never
/// reads the logs of a live one.
fn fix_run(row: &Value, ctx: &mut Ctx) {
    let target = RunTarget::from_row(row);
    let (Some(job), Some(run)) = (target.job_id, target.execution_id) else {
        return ctx.toast(
            crate::tui::app::Tone::Error,
            "This run has no job or run id",
        );
    };
    ctx.effect(Effect::HandOff(HandOff::FixRun {
        project: target.project_id,
        job,
        run: Some(run),
    }));
}

const FIX_RUN: ActionSpec =
    ActionSpec::new('F', "Fix with gz code", ActionKind::Custom(fix_run)).when(FAILED_RUN);

const RUN_TO_PROJECT: LinkSpec = LinkSpec {
    key: 'p',
    label: "project",
    goto: Goto {
        target: Target {
            module: "DE/ML",
            resource: "Projects",
        },
        select: Select::Drill {
            field: ID,
            from: &["projectId"],
        },
    },
    when: None,
};

/// Every job run on the site, newest first.
pub static RUNS: ScreenSpec = ScreenSpec {
    noun: "run",
    columns: RUN_COLUMNS,
    identity: Identity {
        id: &["executionId"],
        title: &["jobName", "executionId"],
    },
    detail: Some(DetailSpec {
        load: Some(Source::get("etl", "/etl/consumption-metrics/{executionId}")),
        missing: "Run not found. It may have been removed.",
        badge: Some((RUN_STATE, &RUN_STATUS)),
        subtitle: &[
            (&["projectName"], Format::Text),
            (&["provider"], Format::Text),
        ],
        sections: &[
            Section {
                title: "Run",
                body: SectionBody::Fields(&[
                    DetailField::new("Started", &["startedAt"], Format::DateTime),
                    DetailField::new(
                        "Ended",
                        &["completedAt", "identity.completedAt"],
                        Format::DateTime,
                    )
                    .optional(),
                    DetailField::new(
                        "Duration",
                        &["durationSeconds"],
                        Format::Duration(TimeUnit::Seconds),
                    ),
                    DetailField::new("Started by", &["startedBy"], Format::Text).optional(),
                    DetailField::new("Engine", &["runtime.engineVersion"], Format::Text).optional(),
                    DetailField::new("Run id", &["executionId"], Format::Text),
                ]),
            },
            Section {
                title: "Data",
                body: SectionBody::Fields(&[
                    DetailField::new("Rows in", &["runtime.summary.inputRows"], Format::Number)
                        .optional(),
                    DetailField::new("Rows out", &["runtime.summary.outputRows"], Format::Number)
                        .optional(),
                    DetailField::new("Bytes in", &["runtime.summary.inputBytes"], Format::Bytes)
                        .optional(),
                    DetailField::new("Bytes out", &["runtime.summary.outputBytes"], Format::Bytes)
                        .optional(),
                ]),
            },
            Section {
                title: "Cost and resources",
                body: SectionBody::Fields(&[
                    DetailField::new("Cost", &["usage.cost.totalUsd"], Format::Usd),
                    DetailField::new("Per hour", &["usage.cost.averageUsdPerHour"], Format::Usd)
                        .optional(),
                    DetailField::new("Avg CPU cores", &["usage.cpu.averageCores"], Format::Number)
                        .optional(),
                    DetailField::new("Peak memory GiB", &["usage.memory.peakGiB"], Format::Number)
                        .optional(),
                    DetailField::new(
                        "Collection",
                        &["collectionStatus", "sessionUsage.collection.status"],
                        Format::Label(COLLECTION_LABELS),
                    )
                    .optional(),
                ]),
            },
        ],
        ..DetailSpec::BLANK
    }),
    actions: &[RUN_LOGS, STOP_RUN, FIX_RUN],
    links: &[RUN_TO_PROJECT],
    empty: "No job runs in the last 30 days",
    enrich: Some(enrich_run),
    ..ScreenSpec::BLANK
};

// ---------- projects ----------

static NEW_PROJECT: FormSpec = FormSpec {
    title: "New project",
    submit: "Create",
    fields: &[FormField::new("name", "Project name", FieldKind::Text)
        .required()
        .pattern(NAME_PATTERN, NAME_MESSAGE)
        .max_len(NAME_MAX)],
};

static RENAME: FormSpec = FormSpec {
    title: "Rename",
    submit: "Save",
    fields: &[FormField::new("name", "New name", FieldKind::Text)
        .required()
        .default("{Name}")
        .pattern(NAME_PATTERN, NAME_MESSAGE)
        .max_len(NAME_MAX)],
};

static NEW_JOB: FormSpec = FormSpec {
    title: "New job",
    submit: "Create",
    fields: &[FormField::new("name", "Job name", FieldKind::Text)
        .required()
        .pattern(NAME_PATTERN, NAME_MESSAGE)
        .max_len(NAME_MAX)
        .hint("Starts empty; add code and a connection in the web job editor")],
};

static CHANGE_OWNER: FormSpec = FormSpec {
    title: "Change owner",
    submit: "Change",
    fields: &[
        FormField::new("owner", "New owner (username)", FieldKind::Text)
            .required()
            .default("{owner}")
            .pattern(
                NAME_PATTERN,
                "The backend only accepts usernames made of letters, numbers and hyphens",
            ),
    ],
};

const PROJECT_ACTIONS: &[ActionSpec] = &[
    ActionSpec::new(
        'n',
        "New project",
        ActionKind::Form(
            &NEW_PROJECT,
            RequestSpec {
                source: Source::post("etlprojects", "/etl/etlProjects", r#"{"Name":"{name}"}"#),
                success: "Project added",
                after: After::Refresh,
            },
        ),
    ),
    ActionSpec::new(
        'N',
        "New job in this project",
        ActionKind::Form(
            &NEW_JOB,
            RequestSpec {
                source: Source::post(
                    "etlprojects",
                    "/etl/etlJobs/{Id}",
                    r#"{"Name":"{name}","projectId":{Id}}"#,
                ),
                success: "Job added",
                after: After::Refresh,
            },
        ),
    ),
    ActionSpec::new(
        'e',
        "Rename",
        ActionKind::Form(
            &RENAME,
            RequestSpec {
                source: Source::new(
                    "etlprojects",
                    Verb::Patch,
                    "/etl/etlProjects/{Id}",
                    Some(r#"{"Name":"{name}"}"#),
                ),
                success: "Project renamed",
                after: After::Refresh,
            },
        ),
    ),
    // `checkAdminAccess` guards the route; the web shows it to admins only.
    ActionSpec::new(
        'W',
        "Change owner",
        ActionKind::Form(
            &CHANGE_OWNER,
            RequestSpec {
                source: Source::new(
                    "etlprojects",
                    Verb::Put,
                    "/etl/etlProjects/{Id}/owner",
                    Some(r#"{"owner":"{owner}"}"#),
                ),
                success: "Owner changed",
                after: After::Refresh,
            },
        ),
    )
    .admin_only(),
];

static PROJECTS: ScreenSpec = ScreenSpec {
    noun: "project",
    columns: &[
        Column::new("Name", NAME, Format::Text),
        Column::new("Owner", &["owner"], Format::Text),
        Column::new("Updated by", &["modified_by"], Format::Text),
        Column::new("Updated", UPDATED, Format::RelTime).width(10),
    ],
    sort: Some(Sort {
        field: UPDATED,
        descending: true,
    }),
    identity: Identity {
        id: ID,
        title: NAME,
    },
    detail: Some(DetailSpec {
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new("Owner", &["owner"], Format::Text),
                DetailField::new("Updated", UPDATED, Format::RelTime),
                DetailField::new("Updated at", UPDATED, Format::DateTime),
                DetailField::new("Updated by", &["modified_by"], Format::Text).optional(),
                DetailField::new("Id", ID, Format::Text),
            ]),
        }],
        related: &[
            RelatedSpec {
                title: "Jobs that ran (last 30 days)",
                source: Source::get(
                    "etl",
                    "/etl/consumption-metrics?groupBy=job&projectId={Id}&limit=100",
                ),
                columns: JOB_USAGE_COLUMNS,
                when: None,
                empty: "No job ran in the last 30 days",
                limit: 20,
            },
            RelatedSpec {
                title: "Recent runs",
                source: Source::get(
                    "etl",
                    "/etl/consumption-metrics?projectId={Id}&workloadType=job&limit=10",
                ),
                columns: RELATED_RUN_COLUMNS,
                when: None,
                empty: "No runs yet",
                limit: 10,
            },
        ],
        ..DetailSpec::BLANK
    }),
    actions: PROJECT_ACTIONS,
    links: &[LinkSpec {
        key: 'u',
        label: "project usage",
        goto: Goto {
            target: Target {
                module: "Catalog",
                resource: "Project usage",
            },
            select: Select::Row {
                field: &["projectId"],
                from: ID,
            },
        },
        when: None,
    }],
    keys: &[KeyHelp {
        keys: "⏎",
        what: "open the project's jobs",
    }],
    empty: "No projects to show",
    ..ScreenSpec::BLANK
};

// ---------- jobs ----------

fn run_job(row: &RowContext, ctx: &mut Ctx) {
    ctx.push(Box::new(RunJob::new(row)));
}

fn job_logs(row: &RowContext, ctx: &mut Ctx) {
    let id = |v: &Value| fields::lookup_any(v, ID).and_then(Value::as_i64);
    let Some(job) = id(&row.row) else {
        return ctx.toast(crate::tui::app::Tone::Error, "This job has no id");
    };
    let project = row.parents.first().and_then(id);
    ctx.push(Box::new(RunLog::latest_of(project, job, row.title.clone())));
}

/// `gz jobs fix` on the job's newest failed run; it stops by itself when
/// the latest run is still going.
fn fix_job(row: &RowContext, ctx: &mut Ctx) {
    let id = |v: &Value| fields::lookup_any(v, ID).and_then(Value::as_i64);
    let Some(job) = id(&row.row) else {
        return ctx.toast(crate::tui::app::Tone::Error, "This job has no id");
    };
    ctx.effect(Effect::HandOff(HandOff::FixRun {
        project: row.parents.first().and_then(id),
        job,
        run: None,
    }));
}

fn new_schedule(row: &RowContext, ctx: &mut Ctx) {
    ctx.push(Box::new(
        crate::tui::views::schedule_form::ScheduleForm::create(row),
    ));
}

/// `config.selectedJobs` names, for multi-job jobs.
fn enrich_job(row: &mut Value, _now_ms: i64) {
    let names: Vec<String> = row["config"]["selectedJobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|j| j["Name"].as_str().map(str::to_string))
        .collect();
    if !names.is_empty() {
        row["_subJobs"] = Value::String(names.join(" → "));
    }
}

const JOB_ACTIONS: &[ActionSpec] = &[
    ActionSpec::new('R', "Run now…", ActionKind::CustomRow(run_job)),
    ActionSpec::new(
        'L',
        "View logs of the latest run",
        ActionKind::CustomRow(job_logs),
    ),
    ActionSpec::new(
        'S',
        "Schedule this job…",
        ActionKind::CustomRow(new_schedule),
    ),
    ActionSpec::new(
        'F',
        "Fix the latest failed run with gz code",
        ActionKind::CustomRow(fix_job),
    ),
    ActionSpec::new(
        'e',
        "Rename",
        ActionKind::Form(
            &RENAME,
            RequestSpec {
                source: Source::new(
                    "etlprojects",
                    Verb::Patch,
                    "/etl/etlJobs/jobs/{Id}",
                    Some(r#"{"Name":"{name}"}"#),
                ),
                success: "Job renamed",
                after: After::Refresh,
            },
        ),
    ),
    ActionSpec::new(
        'd',
        "Delete",
        ActionKind::Request(RequestSpec {
            source: Source::new("etlprojects", Verb::Delete, "/etl/etlJobs/jobs/{Id}", None),
            success: "Job deleted",
            after: After::RefreshAndClose,
        }),
    )
    .destructive("Delete job {Name}? Its code is removed. Delete schedules that run it first."),
];

static JOBS: ScreenSpec = ScreenSpec {
    noun: "job",
    columns: &[
        Column::new("Name", NAME, Format::Text),
        Column::new("Connections", &["config.connectionNames"], Format::Text),
        Column::new("Sub-jobs", &["config.selectedJobs"], Format::Count).width(8),
        Column::new("Workers", &["config.minimumWorkers"], Format::Number).width(7),
        Column::new("Updated by", &["modified_by"], Format::Text),
        Column::new("Updated", UPDATED, Format::RelTime).width(10),
    ],
    sort: Some(Sort {
        field: UPDATED,
        descending: true,
    }),
    identity: Identity {
        id: ID,
        title: NAME,
    },
    detail: Some(DetailSpec {
        // {0} is the project from the drill trail.
        load: Some(Source::get("etlprojects", "/etl/etlJobs/jobs/{0}/{Id}")),
        missing: "Job not found. It may have been deleted.",
        sections: &[Section {
            title: "Definition",
            body: SectionBody::Fields(&[
                DetailField::new("Connections", &["config.connectionNames"], Format::Text),
                DetailField::new(
                    "Minimum workers",
                    &["config.minimumWorkers"],
                    Format::Number,
                ),
                DetailField::new("GPU", &["config.gpuSize"], Format::Number).optional(),
                DetailField::new("Models", &["config.modelNames"], Format::Text).optional(),
                DetailField::new("Runs these jobs", &["_subJobs"], Format::Text).optional(),
                DetailField::new("Retries", &["config.maxRetries"], Format::Number).optional(),
                DetailField::new(
                    "Time limit (min)",
                    &["config.timeoutMinutes"],
                    Format::Number,
                )
                .optional(),
                DetailField::new("Updated by", &["modified_by"], Format::Text),
                DetailField::new("Updated", UPDATED, Format::DateTime),
                DetailField::new("Id", ID, Format::Text),
            ]),
        }],
        related: &[
            RelatedSpec {
                title: "Usage (last 30 days)",
                source: Source::get(
                    "etl",
                    "/etl/consumption-metrics?groupBy=job&projectId={0}&jobId={Id}&limit=1",
                ),
                columns: &[
                    Column::new(
                        "Last run",
                        &["latestExecution.status"],
                        Format::Status(&RUN_STATUS),
                    ),
                    Column::new("Last run at", &["latestStartedAt"], Format::RelTime),
                    Column::new("Runs", &["summary.executionCount"], Format::Number),
                    Column::new("Success %", &["summary.successRate"], Format::Number),
                    Column::new(
                        "Avg run",
                        &["statistics.durationSeconds.average"],
                        Format::Duration(TimeUnit::Seconds),
                    ),
                    Column::new(
                        "Avg $/h",
                        &["statistics.costUsd.averageRatePerHour"],
                        Format::Usd,
                    ),
                ],
                when: None,
                empty: "No runs in the last 30 days",
                limit: 1,
            },
            RelatedSpec {
                title: "Recent runs",
                source: Source::get(
                    "etl",
                    "/etl/consumption-metrics?jobId={Id}&workloadType=job&limit=10",
                ),
                columns: RELATED_RUN_COLUMNS,
                when: None,
                empty: "No runs yet. Press R to run this job.",
                limit: 10,
            },
        ],
        ..DetailSpec::BLANK
    }),
    actions: JOB_ACTIONS,
    links: &[LinkSpec {
        key: 's',
        label: "schedules",
        goto: Goto {
            target: Target {
                module: "Schedules",
                resource: "Schedules",
            },
            select: Select::Filter { from: NAME },
        },
        when: None,
    }],
    empty: "No jobs to show. Press N on the project to add one.",
    enrich: Some(enrich_job),
    ..ScreenSpec::BLANK
};

// ---------- published jobs ----------

static PUBLISHED_JOBS: ScreenSpec = ScreenSpec {
    noun: "published job",
    columns: &[
        Column::new("Name", &["name"], Format::Text),
        Column::new("Source site", &["siteName"], Format::Text),
        Column::new("Published by", &["publishedBy"], Format::Text),
        Column::new("Published", &["publishedOn"], Format::RelTime).width(10),
    ],
    sort: Some(Sort {
        field: &["publishedOn"],
        descending: true,
    }),
    identity: Identity {
        id: &["id"],
        title: &["name"],
    },
    detail: Some(DetailSpec {
        load: Some(Source::get("etlprojects", "/etl/etlJobs/publish/{name}")),
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new("Source site", &["siteName"], Format::Text),
                DetailField::new("Published by", &["publishedBy"], Format::Text),
                DetailField::new("Published", &["publishedOn"], Format::DateTime),
                DetailField::new("Connections", &["config.connectionNames"], Format::Text)
                    .optional(),
                DetailField::new(
                    "Minimum workers",
                    &["config.minimumWorkers"],
                    Format::Number,
                )
                .optional(),
            ]),
        }],
        ..DetailSpec::BLANK
    }),
    // The publish routes have no permission check and the catalogue spans
    // every site of the client; the web offers delete in its admin console only.
    actions: &[ActionSpec::new(
        'd',
        "Delete from the catalogue",
        ActionKind::Request(RequestSpec {
            source: Source::new(
                "etlprojects",
                Verb::Delete,
                "/etl/etlJobs/publish/{id}/{name}",
                None,
            ),
            success: "Published job deleted",
            after: After::RefreshAndClose,
        }),
    )
    .destructive("Delete published job {name}? Every site of this client loses it.")
    .admin_only()],
    empty: "No published jobs. Publish one from a project's job in the web editor.",
    ..ScreenSpec::BLANK
};

// ---------- notebooks, models, experiments ----------

static NEW_NOTEBOOK: FormSpec = FormSpec {
    title: "New notebook",
    submit: "Create",
    fields: &[FormField::new("name", "Notebook name", FieldKind::Text)
        .required()
        .pattern(NAME_PATTERN, NAME_MESSAGE)
        .max_len(NAME_MAX)],
};

/// The Notebooks resource's first level: pick a project (or add a notebook to it).
static NOTEBOOK_PROJECTS: ScreenSpec = ScreenSpec {
    noun: "project",
    columns: &[
        Column::new("Project", NAME, Format::Text),
        Column::new("Owner", &["owner"], Format::Text),
        Column::new("Updated", UPDATED, Format::RelTime).width(10),
    ],
    sort: Some(Sort {
        field: UPDATED,
        descending: true,
    }),
    identity: Identity {
        id: ID,
        title: NAME,
    },
    // mlops checks no name; the web's own rule is enforced here instead.
    actions: &[ActionSpec::new(
        'n',
        "New notebook",
        ActionKind::Form(
            &NEW_NOTEBOOK,
            RequestSpec {
                source: Source::post(
                    "mlops",
                    "/jupyter/notebook",
                    r#"{"notebookName":"{name}","projectId":{Id}}"#,
                ),
                success: "Notebook added",
                after: After::Refresh,
            },
        ),
    )],
    keys: &[KeyHelp {
        keys: "⏎",
        what: "open the project's notebooks",
    }],
    empty: "No projects to show",
    ..ScreenSpec::BLANK
};

static NOTEBOOKS: ScreenSpec = ScreenSpec {
    noun: "notebook",
    columns: &[
        Column::new("Name", &["name"], Format::Text),
        Column::new("Updated by", &["modified_by"], Format::Text),
        Column::new("Updated", UPDATED, Format::RelTime).width(10),
    ],
    sort: Some(Sort {
        field: UPDATED,
        descending: true,
    }),
    identity: Identity {
        id: &["id"],
        title: &["name"],
    },
    detail: Some(DetailSpec {
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new("Updated by", &["modified_by"], Format::Text),
                DetailField::new("Updated", UPDATED, Format::DateTime),
                DetailField::new("Id", &["id"], Format::Text),
            ]),
        }],
        ..DetailSpec::BLANK
    }),
    actions: &[
        ActionSpec::new(
            'e',
            "Rename",
            ActionKind::Form(
                &RENAME,
                RequestSpec {
                    source: Source::new(
                        "mlops",
                        Verb::Patch,
                        "/jupyter/notebook/{id}",
                        Some(r#"{"name":"{name}"}"#),
                    ),
                    success: "Notebook renamed",
                    after: After::Refresh,
                },
            ),
        ),
        ActionSpec::new(
            'd',
            "Delete",
            ActionKind::Request(RequestSpec {
                source: Source::new("mlops", Verb::Delete, "/jupyter/notebook/{0}/{id}", None),
                success: "Notebook deleted",
                after: After::RefreshAndClose,
            }),
        )
        .destructive("Delete notebook {name}? Stop its session in the web first if it is running."),
    ],
    empty: "No notebooks in this project. Go back and press n to add one.",
    ..ScreenSpec::BLANK
};

static MODELS: ScreenSpec = ScreenSpec {
    noun: "model",
    columns: &[
        Column::new("Model", &["name"], Format::Text),
        Column::new("Project", &["parentName"], Format::Text),
    ],
    sort: Some(Sort {
        field: &["name"],
        descending: false,
    }),
    identity: Identity {
        id: &["id"],
        title: &["name"],
    },
    detail: Some(DetailSpec {
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new("Project", &["parentName"], Format::Text),
                DetailField::new("MLflow name", &["name"], Format::Text),
                DetailField::new("Id", &["id"], Format::Text),
            ]),
        }],
        ..DetailSpec::BLANK
    }),
    actions: &[ActionSpec::new(
        'd',
        "Delete model",
        ActionKind::Request(RequestSpec {
            source: Source::new(
                "mlops",
                Verb::Delete,
                "/mlflow/projects/{parentId}/models?name={name}",
                None,
            ),
            success: "Model deleted. Training runs, artifacts and exported copies were kept.",
            after: After::RefreshAndClose,
        }),
    )
    .destructive(
        "Permanently delete {name} and all its registered versions from MLflow and this project?",
    )],
    links: &[LinkSpec {
        key: 'p',
        label: "project",
        goto: Goto {
            target: Target {
                module: "DE/ML",
                resource: "Projects",
            },
            select: Select::Row {
                field: ID,
                from: &["parentId"],
            },
        },
        when: None,
    }],
    empty: "No models yet. They appear when a notebook or job registers one in MLflow.",
    ..ScreenSpec::BLANK
};

static EXPERIMENTS: ScreenSpec = ScreenSpec {
    noun: "experiment",
    columns: &[
        Column::new("Experiment", &["name"], Format::Text),
        Column::new("Project", &["parentName"], Format::Text),
        Column::new("#", &["id"], Format::Number).width(5),
    ],
    sort: Some(Sort {
        field: &["id"],
        descending: true,
    }),
    identity: Identity {
        id: &["id"],
        title: &["name"],
    },
    detail: Some(DetailSpec {
        sections: &[Section {
            title: "Summary",
            body: SectionBody::Fields(&[
                DetailField::new("Project", &["parentName"], Format::Text),
                DetailField::new("MLflow experiment id", &["id"], Format::Text),
            ]),
        }],
        ..DetailSpec::BLANK
    }),
    links: &[LinkSpec {
        key: 'p',
        label: "project",
        goto: Goto {
            target: Target {
                module: "DE/ML",
                resource: "Projects",
            },
            select: Select::Row {
                field: ID,
                from: &["parentId"],
            },
        },
        when: None,
    }],
    empty: "No experiments yet. Runs, params and metrics live in the MLflow UI.",
    ..ScreenSpec::BLANK
};

const fn grid_screen(label: &'static str, source: Source, spec: &'static ScreenSpec) -> Resource {
    crate::tui::nav::screen(label, source, spec)
}

static JOB_DRILLS: [Drill; 1] = [Drill {
    noun: "project",
    id_keys: ID,
    source: Source::get("etlprojects", "/etl/etlJobs/{Id}/jobs"),
    screen: Some(&JOBS),
    when: None,
    repeats: false,
}];

static NOTEBOOK_DRILLS: [Drill; 1] = [Drill {
    noun: "project",
    id_keys: ID,
    source: Source::get("mlops", "/jupyter/{Id}/notebook"),
    screen: Some(&NOTEBOOKS),
    when: None,
    repeats: false,
}];

pub const MODULE: Module = Module {
    title: "DE/ML",
    icon: "◈",
    resources: &[
        Resource {
            label: "Projects",
            view: View::Grid(GridSpec {
                source: Source::get("etlprojects", "/etl/etlProjects"),
                drills: &JOB_DRILLS,
                detail: None,
                screen: Some(&PROJECTS),
            }),
        },
        grid_screen(
            "Job runs",
            Source::get("etl", "/etl/consumption-metrics?workloadType=job&limit=100"),
            &RUNS,
        ),
        grid_screen(
            "Published jobs",
            Source::get("etlprojects", "/etl/etlJobs/publish"),
            &PUBLISHED_JOBS,
        ),
        Resource {
            label: "Notebooks",
            view: View::Grid(GridSpec {
                source: Source::get("etlprojects", "/etl/etlProjects"),
                drills: &NOTEBOOK_DRILLS,
                detail: None,
                screen: Some(&NOTEBOOK_PROJECTS),
            }),
        },
        grid_screen(
            "Models",
            Source::get("mlops", "/mlflow/listAllModels"),
            &MODELS,
        ),
        grid_screen(
            "Experiments",
            Source::get("mlops", "/mlflow/listAllExperiments"),
            &EXPERIMENTS,
        ),
    ],
};

#[cfg(test)]
mod tests;
