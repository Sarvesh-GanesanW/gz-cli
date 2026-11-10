//! Usage screens shown under Catalog: AI usage, Agent usage, Job usage and
//! Project usage. They live here so the Catalog file only lists them.
//!
//! Verified contracts (TUI research notes §3, §8-§10):
//! - `GET chat /chat/metrics` → one object; rows in `modelMetrics`, costs as
//!   decimal strings; tenant-wide and including agent-builder spend
//!   (gz-chat-eks `api/routes/budgets.py:183-252`)
//! - `GET agents /v1/agents/metrics?limit&offset` → `{items}` per agent, all
//!   time when `from`/`to` are left out (agent builder `api.py:303-325`);
//!   `successRate` is 0-100, durations in ms, costs decimal strings
//! - `GET etl /etl/consumption-metrics?workloadType=job` → `{items}` one row
//!   per execution; durations in seconds, costs as floats or null
//!   (ETL `app.py:1458-1607`); detail `GET …/{executionId}` (`app.py:1619`)
//! - `GET etl /etl/consumption-metrics/aggregates/projects` → per project;
//!   `?groupBy=job&projectId=` lists a project's jobs, `?jobId=` a job's runs

use crate::tui::format::{Format, StatusMap, TimeUnit};
use crate::tui::nav::{Drill, GridSpec, Resource, Source, View};
use crate::tui::screen::{
    ActionKind, ActionSpec, Column, DetailField, DetailSpec, Goto, Identity, LinkSpec, ScreenSpec,
    Section, SectionBody, Select, Sort, Target,
};
use crate::tui::specs::chat::METRIC_SECTIONS;
use crate::tui::theme::BadgeTone;
use crate::tui::views::usage::Usage;

const USAGE_VIEW: ActionSpec = ActionSpec::new(
    'u',
    "Usage & cost by day",
    ActionKind::Custom(|_, ctx| ctx.push(Box::new(Usage::new()))),
);

const BY_COST_USD: Sort = Sort {
    field: &["totalCostUsd"],
    descending: true,
};

// ---------- AI usage ----------

pub static AI_USAGE: ScreenSpec = ScreenSpec {
    noun: "model",
    columns: &[
        Column::new("Model", &["modelId"], Format::Model),
        Column::new("Requests", &["requestCount"], Format::Number),
        Column::new("Tokens", &["totalTokens"], Format::Compact),
        Column::new("Tok/s", &["avgOutputTokensPerSecond"], Format::Number),
        Column::new(
            "First token",
            &["medianTimeToFirstTokenMs"],
            Format::Duration(TimeUnit::Millis),
        ),
        Column::new("Cost", &["totalCostUsd"], Format::Usd),
    ],
    sort: Some(BY_COST_USD),
    identity: Identity {
        id: &["modelId"],
        title: &["modelId"],
    },
    detail: Some(DetailSpec {
        subtitle: &[(&["modelId"], Format::Model)],
        sections: METRIC_SECTIONS,
        ..DetailSpec::BLANK
    }),
    actions: &[USAGE_VIEW],
    links: &[LinkSpec {
        key: 'M',
        label: "agent model",
        goto: Goto {
            target: Target {
                module: "Agents",
                resource: "Models",
            },
            select: Select::Row {
                field: &["id"],
                from: &["modelId"],
            },
        },
        when: None,
    }],
    empty: "No model usage in the last 30 days",
    rows: Some(&["modelMetrics"]),
    ..ScreenSpec::BLANK
};

// ---------- Agent usage ----------

pub static AGENT_USAGE: ScreenSpec = ScreenSpec {
    noun: "agent",
    columns: &[
        Column::new("Agent", &["agentName"], Format::Text),
        Column::new("Runs", &["runCount"], Format::Number),
        Column::new("Running", &["runningCount"], Format::Number),
        Column::new("Success", &["successRate"], Format::Percent),
        Column::new(
            "Avg time",
            &["averageDurationMs"],
            Format::Duration(TimeUnit::Millis),
        ),
        Column::new("Tokens", &["totalTokens"], Format::Compact),
        Column::new("Cost", &["estimatedModelCostUsd"], Format::Usd),
    ],
    sort: Some(Sort {
        field: &["estimatedModelCostUsd"],
        descending: true,
    }),
    identity: Identity {
        id: &["agentId"],
        title: &["agentName"],
    },
    detail: Some(DetailSpec {
        subtitle: &[
            (&["configuredModelId"], Format::Model),
            (&["agentVersion"], Format::Version),
        ],
        sections: &[
            Section {
                title: "Outcomes (all time)",
                body: SectionBody::Fields(&[
                    DetailField::new("Runs", &["runCount"], Format::Number),
                    DetailField::new("Completed", &["completedCount"], Format::Number),
                    DetailField::new("Failed", &["failedCount"], Format::Number),
                    DetailField::new("Cancelled", &["cancelledCount"], Format::Number),
                    DetailField::new("Awaiting approval", &["interruptedCount"], Format::Number),
                    DetailField::new("Running", &["runningCount"], Format::Number),
                    DetailField::new("Draft tests", &["previewRunCount"], Format::Number),
                    DetailField::new("Invocations", &["invokeRunCount"], Format::Number),
                    DetailField::new("Users", &["uniqueUsers"], Format::Number),
                    DetailField::new("Success", &["successRate"], Format::Percent),
                ]),
            },
            Section {
                title: "Cost",
                body: SectionBody::Fields(&[
                    DetailField::new("Input", &["inputCostUsd"], Format::Usd),
                    DetailField::new("Output", &["outputCostUsd"], Format::Usd),
                    DetailField::new("Cache read", &["cacheReadCostUsd"], Format::Usd),
                    DetailField::new("Cache write", &["cacheWriteCostUsd"], Format::Usd),
                    DetailField::new("Total", &["estimatedModelCostUsd"], Format::Usd),
                    DetailField::new("Unpriced runs", &["unpricedRunCount"], Format::Number),
                ]),
            },
            Section {
                title: "Tokens",
                body: SectionBody::Fields(&[
                    DetailField::new("Input", &["inputTokens"], Format::Number),
                    DetailField::new("Output", &["outputTokens"], Format::Number),
                    DetailField::new("Cache read", &["cacheReadTokens"], Format::Number),
                    DetailField::new("Cache write", &["cacheWriteTokens"], Format::Number),
                    DetailField::new("Total", &["totalTokens"], Format::Number),
                ]),
            },
            Section {
                title: "Latency",
                body: SectionBody::Fields(&[
                    DetailField::new(
                        "Average run",
                        &["averageDurationMs"],
                        Format::Duration(TimeUnit::Millis),
                    ),
                    DetailField::new(
                        "p95 run",
                        &["p95DurationMs"],
                        Format::Duration(TimeUnit::Millis),
                    ),
                    DetailField::new(
                        "First token (avg)",
                        &["observabilityMetrics.averageTimeToFirstTokenMs"],
                        Format::Duration(TimeUnit::Millis),
                    ),
                    DetailField::new(
                        "First token (p95)",
                        &["observabilityMetrics.p95TimeToFirstTokenMs"],
                        Format::Duration(TimeUnit::Millis),
                    ),
                    DetailField::new("Steps per run", &["averageCycleCount"], Format::Number),
                ]),
            },
            Section {
                title: "Quality",
                body: SectionBody::Fields(&[
                    DetailField::new(
                        "Evaluations",
                        &["qualityMetrics.evaluationCount"],
                        Format::Number,
                    ),
                    DetailField::new("Pass rate", &["qualityMetrics.passRate"], Format::Percent),
                    DetailField::new(
                        "Feedback",
                        &["qualityMetrics.feedbackCount"],
                        Format::Number,
                    ),
                    DetailField::new(
                        "Feedback score (1-5)",
                        &["qualityMetrics.averageFeedbackScore"],
                        Format::Number,
                    ),
                ]),
            },
            Section {
                title: "Approvals",
                body: SectionBody::Fields(&[
                    DetailField::new(
                        "Requests",
                        &["approvalMetrics.requestCount"],
                        Format::Number,
                    ),
                    DetailField::new("Waiting", &["approvalMetrics.waitingCount"], Format::Number),
                    DetailField::new(
                        "Approved",
                        &["approvalMetrics.approvedCount"],
                        Format::Number,
                    ),
                    DetailField::new(
                        "Changes requested",
                        &["approvalMetrics.rejectedCount"],
                        Format::Number,
                    ),
                ]),
            },
        ],
        ..DetailSpec::BLANK
    }),
    actions: &[USAGE_VIEW],
    links: &[LinkSpec {
        key: 'A',
        label: "agent",
        goto: Goto {
            target: Target {
                module: "Agents",
                resource: "Agents",
            },
            select: Select::Row {
                field: &["id"],
                from: &["agentId"],
            },
        },
        when: None,
    }],
    empty: "No agent runs yet",
    ..ScreenSpec::BLANK
};

// ---------- Job and project usage ----------

static EXECUTION_STATUS: StatusMap = StatusMap {
    entries: &[
        ("queued", "Queued", BadgeTone::Muted),
        ("running", "Running", BadgeTone::Info),
        ("succeeded", "Succeeded", BadgeTone::Good),
        ("failed", "Failed", BadgeTone::Bad),
        ("cancelled", "Cancelled", BadgeTone::Muted),
    ],
    fallback: BadgeTone::Info,
};

const TO_PROJECT_USAGE: LinkSpec = LinkSpec {
    key: 'P',
    label: "project usage",
    goto: Goto {
        target: Target {
            module: "Catalog",
            resource: "Project usage",
        },
        select: Select::Row {
            field: &["projectId"],
            from: &["projectId"],
        },
    },
    when: None,
};

pub static JOB_USAGE: ScreenSpec = ScreenSpec {
    noun: "execution",
    columns: &[
        Column::new("Job", &["jobName", "datasetName"], Format::Text),
        Column::new("Project", &["projectName"], Format::Text),
        Column::new("Status", &["status"], Format::Status(&EXECUTION_STATUS)),
        Column::new("Started", &["startedAt"], Format::RelTime).width(10),
        Column::new(
            "Duration",
            &["durationSeconds"],
            Format::Duration(TimeUnit::Seconds),
        ),
        Column::new("CPU core-h", &["usage.cpu.coreHours"], Format::Number),
        Column::new("Cost", &["usage.cost.totalUsd"], Format::Usd),
    ],
    sort: Some(Sort {
        field: &["startedAt"],
        descending: true,
    }),
    identity: Identity {
        id: &["executionId"],
        title: &["jobName", "datasetName", "executionId"],
    },
    detail: Some(DetailSpec {
        load: Some(Source::get("etl", "/etl/consumption-metrics/{executionId}")),
        missing: "Execution not found.",
        badge: Some((&["status"], &EXECUTION_STATUS)),
        subtitle: &[
            (&["projectName"], Format::Text),
            (&["provider"], Format::Text),
            (&["startedBy"], Format::Text),
        ],
        sections: &[
            Section {
                title: "Run",
                body: SectionBody::Fields(&[
                    DetailField::new("Started", &["startedAt"], Format::DateTime),
                    DetailField::new("Completed", &["completedAt"], Format::DateTime),
                    DetailField::new(
                        "Duration",
                        &["durationSeconds"],
                        Format::Duration(TimeUnit::Seconds),
                    ),
                    DetailField::new("Stages", &["stageCount"], Format::Number),
                    DetailField::new(
                        "Telemetry",
                        &["collectionStatus"],
                        Format::Label(super::deml::COLLECTION_LABELS),
                    ),
                    DetailField::new("Execution id", &["executionId"], Format::Text),
                ]),
            },
            Section {
                title: "Cost",
                body: SectionBody::Fields(&[
                    DetailField::new("Total", &["usage.cost.totalUsd"], Format::Usd),
                    DetailField::new("CPU", &["usage.cost.cpuUsd"], Format::Usd),
                    DetailField::new("Memory", &["usage.cost.memoryUsd"], Format::Usd),
                    DetailField::new("GPU", &["usage.cost.gpuUsd"], Format::Usd).optional(),
                    DetailField::new(
                        "Per hour (avg)",
                        &["usage.cost.averageUsdPerHour"],
                        Format::Usd,
                    ),
                    DetailField::new("Fidelity", &["usage.reliability.fidelity"], Format::Text),
                ]),
            },
            Section {
                title: "CPU and memory",
                body: SectionBody::Fields(&[
                    DetailField::new("Core-hours", &["usage.cpu.coreHours"], Format::Number),
                    DetailField::new("Peak cores", &["usage.cpu.peakCores"], Format::Number),
                    DetailField::new(
                        "CPU utilization %",
                        &["usage.cpu.utilizationPercent"],
                        Format::Number,
                    ),
                    DetailField::new("GiB-hours", &["usage.memory.gibHours"], Format::Number),
                    DetailField::new("Peak GiB", &["usage.memory.peakGiB"], Format::Number),
                ]),
            },
            Section {
                title: "I/O",
                body: SectionBody::Fields(&[
                    DetailField::new("Read", &["usage.storage.readBytes"], Format::Bytes),
                    DetailField::new("Written", &["usage.storage.writeBytes"], Format::Bytes),
                    DetailField::new("Received", &["usage.network.receiveBytes"], Format::Bytes),
                    DetailField::new("Sent", &["usage.network.transmitBytes"], Format::Bytes),
                ]),
            },
        ],
        ..DetailSpec::BLANK
    }),
    links: &[TO_PROJECT_USAGE],
    empty: "No job executions in the last 30 days.",
    ..ScreenSpec::BLANK
};

/// Project and job groups share a shape (`summary`, `latestStartedAt`).
const GROUP_COLUMNS: &[Column] = &[
    Column::new("Runs", &["summary.executionCount"], Format::Number),
    Column::new("Success", &["summary.successRate"], Format::Percent),
    Column::new(
        "Avg duration",
        &["summary.averageDurationSeconds"],
        Format::Duration(TimeUnit::Seconds),
    ),
    // The web prefers the fully covered total, else what was observed.
    Column::new(
        "Cost",
        &["summary.totalCostUsd", "summary.observedCostUsd"],
        Format::Usd,
    ),
    Column::new("Last run", &["latestStartedAt"], Format::RelTime),
];

const LAST_RUN_FIRST: Sort = Sort {
    field: &["latestStartedAt"],
    descending: true,
};

pub static PROJECT_USAGE: ScreenSpec = ScreenSpec {
    noun: "project",
    columns: &[
        Column::new("Project", &["projectName", "projectId"], Format::Text),
        Column::new("Jobs", &["jobCount"], Format::Number),
        GROUP_COLUMNS[0],
        GROUP_COLUMNS[1],
        GROUP_COLUMNS[2],
        GROUP_COLUMNS[3],
        GROUP_COLUMNS[4],
    ],
    sort: Some(LAST_RUN_FIRST),
    identity: Identity {
        id: &["projectId"],
        title: &["projectName", "projectId"],
    },
    empty: "No project activity in the last 30 days.",
    ..ScreenSpec::BLANK
};

pub static JOB_GROUPS: ScreenSpec = ScreenSpec {
    noun: "job",
    columns: &[
        Column::new("Job", &["jobName", "jobId"], Format::Text),
        GROUP_COLUMNS[0],
        GROUP_COLUMNS[1],
        GROUP_COLUMNS[2],
        GROUP_COLUMNS[3],
        GROUP_COLUMNS[4],
    ],
    sort: Some(LAST_RUN_FIRST),
    identity: Identity {
        id: &["jobId"],
        title: &["jobName", "jobId"],
    },
    empty: "No job runs in this project in the last 30 days.",
    ..ScreenSpec::BLANK
};

/// Project → its jobs → a job's executions.
static PROJECT_DRILLS: [Drill; 2] = [
    Drill {
        noun: "project",
        id_keys: &["projectId"],
        source: Source::get(
            "etl",
            "/etl/consumption-metrics?groupBy=job&projectId={projectId}",
        ),
        screen: Some(&JOB_GROUPS),
        when: None,
        repeats: false,
    },
    Drill {
        noun: "job",
        id_keys: &["jobId"],
        source: Source::get("etl", "/etl/consumption-metrics?jobId={jobId}"),
        screen: Some(&JOB_USAGE),
        when: None,
        repeats: false,
    },
];

pub const AI_USAGE_RESOURCE: Resource =
    crate::tui::nav::screen("AI usage", Source::get("chat", "/chat/metrics"), &AI_USAGE);

pub const AGENT_USAGE_RESOURCE: Resource = crate::tui::nav::screen(
    "Agent usage",
    Source::get("agents", "/v1/agents/metrics?limit=100&offset=0"),
    &AGENT_USAGE,
);

pub const JOB_USAGE_RESOURCE: Resource = crate::tui::nav::screen(
    "Job usage",
    Source::get("etl", "/etl/consumption-metrics?workloadType=job"),
    &JOB_USAGE,
);

pub const PROJECT_USAGE_RESOURCE: Resource = Resource {
    label: "Project usage",
    view: View::Grid(GridSpec {
        source: Source::get("etl", "/etl/consumption-metrics/aggregates/projects"),
        drills: &PROJECT_DRILLS,
        detail: None,
        screen: Some(&PROJECT_USAGE),
    }),
};

#[cfg(test)]
mod tests;
