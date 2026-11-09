//! Agents: agents (→ runs), approval requests, tools and models.
//!
//! Verified contracts (TUI research notes, "Agents"; agent builder
//! `src/gz_agent_builder/api.py`, web `pages/Utils/agentUtils.js`):
//! - list `GET /v1/agents?limit=100` → `{items, limit, offset}`; each row is
//!   the full definition (api.py:275-300, `_listed_agent_record`)
//! - detail `GET /v1/agents/{id}` → the same keys, `tags` as a dict (api.py:377)
//! - runs `GET /v1/agents/{id}/runs` → `{items}`, newest first; only the
//!   agent's creator or an Admin sees every run, others see their own
//!   (services/core.py:1505-1511)
//! - run detail is the list row plus the durable records the web inspector
//!   loads (`/observability`, `/governance`, `/metrics`, `/evaluations`,
//!   `/feedback`; api.py:1340-1420, 1897). `GET …/runs/{runId}` reads a
//!   24 h store and answers 404 for older runs, so it is never used.
//! - approvals `GET /v1/approval-requests` (waiting by default; only current
//!   versions the caller may approve) and `GET /v1/agents/{id}/approvals`
//! - decisions `POST /v1/approval-requests/{id}/decision {decision, comment}`;
//!   rejecting requires a comment (schemas.py:677-687)
//! - submit `POST /v1/agents/{id}/approval-requests {comment?}` (api.py:890)
//! - publish `POST /v1/agents/{id}/publish` needs an approved current version
//! - run cancel `POST …/runs/{runId}/cancel`, feedback `PUT …/feedback
//!   {score 1-5, comment?}`, evaluate `POST …/evaluations
//!   {evaluators:["quality-suite"]}` (runs an LLM judge, which costs money)
//!
//! - answering paused tool calls `POST …/runs/{id}/resume
//!   {interruptResponses:[{interruptId, decision}]}`, every pending interrupt
//!   exactly once (schemas.py `InterventionResume`, services/invocations.py)
//!
//! A missing permission answers 401 `AccessDenied`; `errors.rs` words that
//! as "You don't have access", not as an expired sign-in.

use std::time::Duration;

use reqwest::Method;
use serde_json::{json, Value};

use crate::cmd::seg;
use crate::http::ApiClient;
use crate::tui::app::Tone;
use crate::tui::format::{text_of, Format, StatusMap, TimeUnit};
use crate::tui::nav::{Drill, GridSpec, Module, Resource, Source, Verb, View};
use crate::tui::screen::{
    ActionKind, ActionSpec, After, Choices, Column, Cond, CopySpec, DetailField, DetailSpec,
    FieldKind, FormField, FormSpec, Goto, Identity, KeyHelp, LinkSpec, RelatedSpec, RequestSpec,
    ScreenSpec, Section, SectionBody, Select, Sort, Target,
};
use crate::tui::theme::BadgeTone;
use crate::tui::views::playground::Playground;
use crate::tui::views::Ctx;

const AGENTS: &str = "agents";
const ID: &[&str] = &["id"];
const NAME: &[&str] = &["name"];
const AGENT_ID: &[&str] = &["agentId"];

/// `approvalStatus` of the current version (web `AgentsList.js:88-93`).
pub static APPROVAL_STATUS: StatusMap = StatusMap {
    entries: &[
        ("draft", "Draft", BadgeTone::Muted),
        ("waiting_for_approval", "In review", BadgeTone::Warn),
        ("approved", "Approved", BadgeTone::Good),
        ("rejected", "Changes requested", BadgeTone::Bad),
    ],
    fallback: BadgeTone::Info,
};

/// `governanceState`: anything but active blocks invocation.
static GOVERNANCE_STATE: StatusMap = StatusMap {
    entries: &[
        ("active", "Active", BadgeTone::Good),
        ("suspended", "Suspended", BadgeTone::Bad),
        ("retired", "Retired", BadgeTone::Muted),
    ],
    fallback: BadgeTone::Info,
};

/// `agent_run.status` (`ck_agent_run_status`).
pub static RUN_STATUS: StatusMap = StatusMap {
    entries: &[
        ("running", "Running", BadgeTone::Info),
        ("completed", "Completed", BadgeTone::Good),
        ("failed", "Failed", BadgeTone::Bad),
        ("cancelled", "Cancelled", BadgeTone::Muted),
        ("interrupted", "Awaiting approval", BadgeTone::Warn),
    ],
    fallback: BadgeTone::Info,
};

/// The web shows `published` runs as "invoke" (`RecentRunsPanel.js`).
const RUN_MODES: &[(&str, &str)] = &[
    ("preview", "draft test"),
    ("invoke", "invoke"),
    ("published", "invoke"),
];

const TOOL_CATEGORIES: &[(&str, &str)] = &[
    ("ontology", "Ontology"),
    ("data_provider", "Data provider"),
    ("etl", "ETL"),
    ("datasets", "Datasets"),
    ("filter", "Filter"),
    ("visualization", "Visualization"),
    ("dashboard", "Dashboard"),
    ("documents", "Documents"),
    ("connections", "Connections"),
];

// ---------- agents ----------

const RUN_COLUMNS: &[Column] = &[
    Column::new("Status", &["status"], Format::Status(&RUN_STATUS)),
    Column::new("Started", &["startedAt"], Format::RelTime),
    Column::new("Mode", &["executionMode"], Format::Label(RUN_MODES)),
    Column::new("Version", &["agentVersion"], Format::Version),
    Column::new(
        "Duration",
        &["durationMs"],
        Format::Duration(TimeUnit::Millis),
    ),
    Column::new("Tokens", &["totalTokens"], Format::Compact),
    Column::new("Cost", &["totalCostUsd"], Format::Usd),
    Column::new("By", &["createdBy"], Format::Text),
];

const APPROVAL_COLUMNS: &[Column] = &[
    Column::new("Version", &["agentVersion"], Format::Version),
    Column::new("Status", &["status"], Format::Status(&APPROVAL_STATUS)),
    Column::new("Requested by", &["requestedBy"], Format::Text),
    Column::new("Requested", &["requestedAt"], Format::RelTime),
    Column::new("Reviewed by", &["reviewedBy"], Format::Text),
    Column::new(
        "Comment",
        &["reviewComment", "requestComment"],
        Format::Text,
    ),
];

const CAN_SUBMIT: Cond = Cond {
    field: &["approvalStatus"],
    any_of: &["draft", "rejected"],
};

const IS_APPROVED: Cond = Cond {
    field: &["approvalStatus"],
    any_of: &["approved"],
};

const IN_REVIEW: Cond = Cond {
    field: &["approvalStatus"],
    any_of: &["waiting_for_approval"],
};

/// Opens the playground on this agent; running a prompt there spends money,
/// so nothing runs until the user types one.
const PLAYGROUND: ActionSpec = ActionSpec::new(
    'p',
    "Playground (test run)",
    ActionKind::Custom(|row, ctx| ctx.push(Box::new(Playground::new(row)))),
);

static SUBMIT_FORM: FormSpec = FormSpec {
    title: "Submit for approval",
    submit: "Submit",
    fields: &[
        FormField::new("comment", "Note for the reviewer", FieldKind::Text)
            .default("Submitted from gz")
            .max_len(1000)
            .hint("What changed in this version"),
    ],
};

const SUBMIT: ActionSpec = ActionSpec::new(
    's',
    "Submit for approval",
    ActionKind::Form(
        &SUBMIT_FORM,
        RequestSpec {
            source: Source::new(
                AGENTS,
                Verb::Post,
                "/v1/agents/{id}/approval-requests",
                Some(r#"{"comment":"{comment}"}"#),
            ),
            success: "Agent sent for approval",
            after: After::Refresh,
        },
    ),
)
.when(CAN_SUBMIT);

const PUBLISH: ActionSpec = ActionSpec::new(
    'P',
    "Publish to the catalog",
    ActionKind::Request(RequestSpec {
        source: Source::new(AGENTS, Verb::Post, "/v1/agents/{id}/publish", None),
        success: "Agent published to the catalog",
        after: After::Refresh,
    }),
)
.confirm("Publish {name} v{approvedVersion} to the catalog? Other sites can install it.")
.when(IS_APPROVED);

const DELETE_AGENT: ActionSpec = ActionSpec::new(
    'D',
    "Delete",
    ActionKind::Request(RequestSpec {
        source: Source::new(AGENTS, Verb::Delete, "/v1/agents/{id}", None),
        success: "Agent deleted",
        after: After::RefreshAndClose,
    }),
)
.destructive("Delete agent {name}? Its sessions, runs and metrics go too. This can't be undone.");

const TO_USAGE: LinkSpec = LinkSpec {
    key: 'U',
    label: "usage",
    goto: Goto {
        target: Target {
            module: "Catalog",
            resource: "Agent usage",
        },
        select: Select::Row {
            field: AGENT_ID,
            from: ID,
        },
    },
    when: None,
};

const TO_REVIEW: LinkSpec = LinkSpec {
    key: 'R',
    label: "approval request",
    goto: Goto {
        target: Target {
            module: "Agents",
            resource: "Approval requests",
        },
        select: Select::Row {
            field: AGENT_ID,
            from: ID,
        },
    },
    when: Some(IN_REVIEW),
};

const TO_MODEL: LinkSpec = LinkSpec {
    key: 'M',
    label: "model",
    goto: Goto {
        target: Target {
            module: "Agents",
            resource: "Models",
        },
        select: Select::Row {
            field: ID,
            from: &["modelId"],
        },
    },
    when: None,
};

pub static AGENT_SCREEN: ScreenSpec = ScreenSpec {
    noun: "agent",
    columns: &[
        Column::new("Name", NAME, Format::Text),
        Column::new("Status", &["approvalStatus"], Format::Status(&APPROVAL_STATUS)),
        Column::new("Live", &["approvedVersion"], Format::Version).width(6),
        Column::new("Model", &["modelId"], Format::Model),
        Column::new("Owner", &["createdBy"], Format::Text),
        Column::new("Updated", &["updatedAt", "createdAt"], Format::RelTime).width(10),
    ],
    sort: Some(Sort {
        field: &["updatedAt", "createdAt"],
        descending: true,
    }),
    identity: Identity { id: ID, title: NAME },
    detail: Some(DetailSpec {
        load: Some(Source::get(AGENTS, "/v1/agents/{id}")),
        missing: "Agent not found. It may have been deleted.",
        badge: Some((&["approvalStatus"], &APPROVAL_STATUS)),
        subtitle: &[
            (&["description"], Format::Text),
            (&["modelId"], Format::Model),
            (&["governanceState"], Format::Status(&GOVERNANCE_STATE)),
        ],
        sections: &[
            Section {
                title: "Version",
                body: SectionBody::Fields(&[
                    DetailField::new("Current", &["version"], Format::Version),
                    DetailField::new("Approved (runs)", &["approvedVersion"], Format::Version),
                    DetailField::new("Published", &["publishedVersion"], Format::Version)
                        .optional(),
                    DetailField::new("Can be invoked", &["isInvokable"], Format::Bool),
                    DetailField::new(
                        "Project",
                        &["metadata.projectName", "tags.project-name"],
                        Format::Text,
                    )
                    .optional(),
                    DetailField::new("Owner", &["createdBy"], Format::Text),
                    DetailField::new("Created", &["createdAt"], Format::DateTime),
                    DetailField::new("Updated by", &["modifiedBy"], Format::Text),
                    DetailField::new("Updated", &["updatedAt"], Format::DateTime),
                    DetailField::new("Id", ID, Format::Text),
                ]),
            },
            Section {
                title: "Last review",
                body: SectionBody::Fields(&[
                    DetailField::new("Reviewed by", &["approvalReviewedBy"], Format::Text)
                        .optional(),
                    DetailField::new("Reviewed", &["approvalReviewedAt"], Format::DateTime)
                        .optional(),
                    DetailField::new("Comment", &["approvalReviewComment"], Format::Text)
                        .optional(),
                ]),
            },
            Section {
                title: "Configuration",
                body: SectionBody::Fields(&[
                    DetailField::new("Model id", &["modelId"], Format::Text),
                    DetailField::new("Temperature", &["temperature"], Format::Number),
                    DetailField::new("Top P", &["topP"], Format::Number),
                    DetailField::new("Max tokens", &["maxTokens"], Format::Number),
                    DetailField::new("Max turns", &["maxTurns"], Format::Number),
                    DetailField::new("Memory window", &["memoryWindow"], Format::Number),
                    DetailField::new("History connection", &["connectionName"], Format::Text)
                        .optional(),
                ]),
            },
            Section {
                title: "Tools",
                body: SectionBody::Fields(&[
                    DetailField::new("Tools", &["tools"], Format::Text),
                    DetailField::new(
                        "Need approval",
                        &["metadata.groundzeroRuntime.approvalRequiredTools"],
                        Format::Text,
                    )
                    .optional(),
                ]),
            },
            Section {
                title: "Delegation",
                body: SectionBody::Fields(&[
                    DetailField::new("Sub-agents", &["subAgents"], Format::NamesOf("alias"))
                        .optional(),
                    DetailField::new("Team mode", &["team.mode"], Format::Text).optional(),
                    DetailField::new("Team members", &["team.members"], Format::NamesOf("alias"))
                        .optional(),
                ]),
            },
            Section {
                title: "Safety",
                body: SectionBody::Fields(&[
                    DetailField::new(
                        "Content guardrails",
                        &["metadata.groundzeroRuntime.contentGuardrails.enabled"],
                        Format::Bool,
                    ),
                    DetailField::new(
                        "PII redaction",
                        &["metadata.groundzeroRuntime.piiRedaction.enabled"],
                        Format::Bool,
                    ),
                    DetailField::new(
                        "Response format",
                        &["metadata.groundzeroRuntime.outcomeSchema.title"],
                        Format::Text,
                    )
                    .optional(),
                    DetailField::new(
                        "Ontology",
                        &["metadata.groundzeroRuntime.ontologyMode"],
                        Format::Text,
                    )
                    .optional(),
                    DetailField::new(
                        "Context",
                        &["metadata.groundzeroRuntime.contextManagement"],
                        Format::Text,
                    )
                    .optional(),
                ]),
            },
            Section {
                title: "Governance",
                body: SectionBody::Fields(&[
                    DetailField::new(
                        "State",
                        &["governanceState"],
                        Format::Status(&GOVERNANCE_STATE),
                    ),
                    DetailField::new("Reason", &["governanceStateReason"], Format::Text)
                        .optional(),
                    DetailField::new("Risk tier", &["governanceStatus.riskTier"], Format::Text),
                    DetailField::new(
                        "Classification",
                        &["governanceStatus.classification"],
                        Format::Text,
                    ),
                ]),
            },
            Section {
                title: "Instructions",
                body: SectionBody::Text(&["instructions"]),
            },
        ],
        related: &[
            RelatedSpec {
                title: "Recent runs (all runs if you own the agent, else yours)",
                source: Source::get(AGENTS, "/v1/agents/{id}/runs?limit=10&offset=0"),
                columns: RUN_COLUMNS,
                when: None,
                empty: "No runs yet. Unless you created this agent or are an admin, only your own runs are listed.",
                limit: 10,
            },
            RelatedSpec {
                title: "Approval history",
                source: Source::get(AGENTS, "/v1/agents/{id}/approvals?limit=10&offset=0"),
                columns: APPROVAL_COLUMNS,
                when: None,
                empty: "Never submitted for approval",
                limit: 10,
            },
        ],
        copy: Some(CopySpec {
            label: "name",
            template: "{name}",
        }),
        ..DetailSpec::BLANK
    }),
    actions: &[PLAYGROUND, SUBMIT, PUBLISH, DELETE_AGENT],
    links: &[TO_REVIEW, TO_USAGE, TO_MODEL],
    empty: "No agents yet",
    keys: &[KeyHelp {
        keys: "⏎",
        what: "runs (yours unless you own the agent)",
    }],
    ..ScreenSpec::BLANK
};

// ---------- runs ----------

const RUNNING: Cond = Cond {
    field: &["status"],
    any_of: &["running"],
};

const FINISHED: Cond = Cond {
    field: &["status"],
    any_of: &["completed"],
};

const CANCEL_RUN: ActionSpec = ActionSpec::new(
    'x',
    "Stop run",
    ActionKind::Request(RequestSpec {
        source: Source::new(
            AGENTS,
            Verb::Post,
            "/v1/agents/{agentId}/runs/{id}/cancel",
            None,
        ),
        success: "Agent run stopped",
        after: After::Refresh,
    }),
)
.confirm("Stop this run? Work done so far is kept.")
.when(RUNNING);

static FEEDBACK_FORM: FormSpec = FormSpec {
    title: "Rate this run",
    submit: "Save",
    fields: &[
        FormField::new(
            "score",
            "Score (1-5)",
            FieldKind::Number {
                min: Some(1.0),
                max: Some(5.0),
            },
        )
        .required()
        .pattern("[1-5]", "A whole number from 1 to 5"),
        FormField::new("comment", "Comment", FieldKind::Multiline).max_len(2000),
    ],
};

const RATE_RUN: ActionSpec = ActionSpec::new(
    'f',
    "Rate run",
    ActionKind::Form(
        &FEEDBACK_FORM,
        RequestSpec {
            source: Source::new(
                AGENTS,
                Verb::Put,
                "/v1/agents/{agentId}/runs/{id}/feedback",
                Some(r#"{"score":{score},"comment":"{comment}"}"#),
            ),
            success: "Feedback saved",
            after: After::Refresh,
        },
    ),
);

const EVALUATE_RUN: ActionSpec = ActionSpec::new(
    'E',
    "Evaluate (quality suite)",
    ActionKind::Request(RequestSpec {
        source: Source::new(
            AGENTS,
            Verb::Post,
            "/v1/agents/{agentId}/runs/{id}/evaluations",
            Some(r#"{"evaluators":["quality-suite"]}"#),
        ),
        success: "Evaluation recorded",
        after: After::Refresh,
    }),
)
.confirm("Run the quality suite on this run? It calls an LLM judge, which costs money.")
.when(FINISHED);

const PAUSED: Cond = Cond {
    field: &["status"],
    any_of: &["interrupted"],
};

static ANSWER_FORM: FormSpec = FormSpec {
    title: "Answer the paused tool calls",
    submit: "Send",
    fields: &[FormField::new(
        "decision",
        "Decision (every paused tool call in this run)",
        FieldKind::Select(Choices::Static(&[
            ("approved", "Approve"),
            ("denied", "Deny"),
        ])),
    )
    .required()],
};

const ANSWER_RUN: ActionSpec = ActionSpec::new(
    'p',
    "Answer paused tool calls",
    ActionKind::FormCustom(&ANSWER_FORM, answer_paused_run),
)
.when(PAUSED);

fn answer_paused_run(row: &Value, values: &Value, ctx: &mut Ctx) {
    let decision = values["decision"].as_str().unwrap_or("denied");
    let responses = interrupt_responses(row, decision);
    if responses.is_empty() {
        return ctx.toast(Tone::Error, "This run has no pending tool calls to answer");
    }
    let path = format!(
        "/v1/agents/{}/runs/{}/resume",
        seg(&text_of(&row["agentId"])),
        seg(&text_of(&row["id"]))
    );
    ctx.run_task("answer the paused tool calls", move |client: ApiClient| {
        let (path, responses) = (path.clone(), responses.clone());
        async move {
            let body = json!({"interruptResponses": responses});
            client
                .request_json(AGENTS, Method::POST, &path, &[], Some(body))
                .await
                .map_err(|e| format!("Answer failed: {e:#}"))?;
            Ok(json!({"message": "Answered; the run continues"}))
        }
    });
}

/// One response per pending interrupt, as the resume endpoint requires.
fn interrupt_responses(row: &Value, decision: &str) -> Vec<Value> {
    row["interrupts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|interrupt| interrupt["id"].as_str().is_some_and(|id| !id.is_empty()))
        .map(|interrupt| json!({"interruptId": interrupt["id"], "decision": decision}))
        .collect()
}

const TO_AGENT: LinkSpec = LinkSpec {
    key: 'A',
    label: "agent",
    goto: Goto {
        target: Target {
            module: "Agents",
            resource: "Agents",
        },
        select: Select::Row {
            field: ID,
            from: AGENT_ID,
        },
    },
    when: None,
};

pub static RUN_SCREEN: ScreenSpec = ScreenSpec {
    noun: "run",
    columns: RUN_COLUMNS,
    sort: Some(Sort {
        field: &["startedAt"],
        descending: true,
    }),
    identity: Identity { id: ID, title: ID },
    detail: Some(DetailSpec {
        badge: Some((&["status"], &RUN_STATUS)),
        subtitle: &[
            (&["modelId"], Format::Model),
            (&["executionMode"], Format::Label(RUN_MODES)),
            (&["agentVersion"], Format::Version),
            (&["createdBy"], Format::Text),
        ],
        sections: &[
            Section {
                title: "Summary",
                body: SectionBody::Fields(&[
                    DetailField::new("Started", &["startedAt"], Format::DateTime),
                    DetailField::new("Finished", &["finishedAt"], Format::DateTime),
                    DetailField::new("Took", &["durationMs"], Format::Duration(TimeUnit::Millis)),
                    DetailField::new("Steps", &["cycleCount"], Format::Number),
                    DetailField::new("Stop reason", &["stopReason"], Format::Text).optional(),
                    DetailField::new("Run by", &["createdBy"], Format::Text),
                    DetailField::new("Session", &["sessionId"], Format::Text).optional(),
                    DetailField::new("Started by run", &["parentRunId"], Format::Text).optional(),
                    DetailField::new("Waiting on", &["interrupts"], Format::NamesOf("toolName"))
                        .optional(),
                    DetailField::new("Run id", ID, Format::Text),
                ]),
            },
            Section {
                title: "Error",
                body: SectionBody::Fields(&[
                    DetailField::new("Code", &["errorCode"], Format::Text).optional(),
                    DetailField::new("Message", &["errorMessage"], Format::Text).optional(),
                    DetailField::new("History", &["historyArchiveError"], Format::Text).optional(),
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
                title: "Cost",
                body: SectionBody::Fields(&[
                    DetailField::new("Input", &["inputCostUsd"], Format::Usd),
                    DetailField::new("Output", &["outputCostUsd"], Format::Usd),
                    DetailField::new("Cache read", &["cacheReadCostUsd"], Format::Usd),
                    DetailField::new("Cache write", &["cacheWriteCostUsd"], Format::Usd),
                    DetailField::new("Total", &["totalCostUsd"], Format::Usd),
                ]),
            },
            Section {
                title: "Prompt",
                body: SectionBody::Text(&["inputText"]),
            },
            Section {
                title: "Answer",
                body: SectionBody::Text(&["outputText"]),
            },
        ],
        related: &[
            RelatedSpec {
                title: "Trace",
                source: Source::get(AGENTS, "/v1/agents/{agentId}/runs/{id}/observability"),
                columns: &[
                    Column::new("Steps", &["cycleCount"], Format::Number),
                    Column::new(
                        "Step time",
                        &["totalCycleDurationMs"],
                        Format::Duration(TimeUnit::Millis),
                    ),
                    Column::new(
                        "First token",
                        &["timeToFirstTokenMs"],
                        Format::Duration(TimeUnit::Millis),
                    ),
                    Column::new("Tools used", &["tools"], Format::Count),
                    Column::new("Child runs", &["children"], Format::Count),
                    Column::new("Team turns", &["team.turns"], Format::Number),
                    Column::new("Handoffs", &["team.handoffs"], Format::Number),
                    Column::new("Trace id", &["traceId"], Format::Text),
                ],
                when: None,
                empty: "No detailed observability",
                limit: 1,
            },
            RelatedSpec {
                title: "Governance",
                source: Source::get(AGENTS, "/v1/agents/{agentId}/runs/{id}/governance"),
                columns: &[
                    Column::new("Governance", &["governanceStatus"], Format::Text),
                    Column::new("Safety", &["safetyStatus"], Format::Text),
                    Column::new("Evaluation", &["evaluationStatus"], Format::Text),
                    Column::new("Safety events", &["safetyEvents"], Format::Count),
                    Column::new("Recorded", &["recordedAt"], Format::RelTime),
                ],
                when: None,
                empty: "No governance record",
                limit: 1,
            },
            RelatedSpec {
                title: "Evaluations",
                source: Source::get(AGENTS, "/v1/agents/{agentId}/runs/{id}/evaluations"),
                columns: &[
                    Column::new("Evaluator", &["evaluator"], Format::Text),
                    Column::new("Status", &["status"], Format::Text),
                    Column::new("Score", &["score"], Format::Number),
                    Column::new("Passed", &["passed"], Format::Bool),
                    Column::new("Reason", &["reason", "label"], Format::Text),
                    Column::new("Cost", &["totalCostUsd"], Format::Usd),
                ],
                when: None,
                empty: "Not evaluated · E runs the quality suite",
                limit: 10,
            },
            RelatedSpec {
                title: "Feedback",
                source: Source::get(AGENTS, "/v1/agents/{agentId}/runs/{id}/feedback"),
                columns: &[
                    Column::new("By", &["recordedBy"], Format::Text),
                    Column::new("Score", &["score"], Format::Number),
                    Column::new("Comment", &["comment"], Format::Text),
                    Column::new("When", &["updatedAt", "createdAt"], Format::RelTime),
                ],
                when: None,
                empty: "No feedback · f rates this run",
                limit: 10,
            },
            RelatedSpec {
                title: "Custom metrics",
                source: Source::get(AGENTS, "/v1/agents/{agentId}/runs/{id}/metrics"),
                columns: &[
                    Column::new("Metric", &["name"], Format::Text),
                    Column::new("Value", &["value"], Format::Text),
                    Column::new("Unit", &["unit"], Format::Text),
                    Column::new("Recorded", &["recordedAt"], Format::RelTime),
                ],
                when: None,
                empty: "No custom metrics",
                limit: 20,
            },
        ],
        ..DetailSpec::BLANK
    }),
    actions: &[ANSWER_RUN, CANCEL_RUN, RATE_RUN, EVALUATE_RUN],
    links: &[TO_AGENT],
    empty:
        "No runs yet. Unless you created this agent or are an admin, only your own runs are listed.",
    poll: Some(Duration::from_secs(10)),
    ..ScreenSpec::BLANK
};

// ---------- approval requests ----------

const WAITING: Cond = Cond {
    field: &["status"],
    any_of: &["waiting_for_approval"],
};

static APPROVE_FORM: FormSpec = FormSpec {
    title: "Approve version",
    submit: "Approve",
    fields: &[FormField::new("comment", "Comment", FieldKind::Multiline)
        .max_len(1000)
        .hint("Record what was reviewed. Approving makes this version the one that runs.")],
};

static REJECT_FORM: FormSpec = FormSpec {
    title: "Request changes",
    submit: "Request changes",
    fields: &[
        FormField::new("comment", "Required changes", FieldKind::Multiline)
            .required()
            .max_len(1000)
            .hint("Explain the required changes."),
    ],
};

const APPROVE: ActionSpec = ActionSpec::new(
    'v',
    "Approve version",
    ActionKind::Form(
        &APPROVE_FORM,
        RequestSpec {
            source: Source::new(
                AGENTS,
                Verb::Post,
                "/v1/approval-requests/{id}/decision",
                Some(r#"{"decision":"approved","comment":"{comment}"}"#),
            ),
            success: "Agent version approved",
            after: After::RefreshAndClose,
        },
    ),
)
.when(WAITING);

const REQUEST_CHANGES: ActionSpec = ActionSpec::new(
    'c',
    "Request changes",
    ActionKind::Form(
        &REJECT_FORM,
        RequestSpec {
            source: Source::new(
                AGENTS,
                Verb::Post,
                "/v1/approval-requests/{id}/decision",
                Some(r#"{"decision":"rejected","comment":"{comment}"}"#),
            ),
            success: "Changes requested from the agent owner",
            after: After::RefreshAndClose,
        },
    ),
)
.when(WAITING);

const SNAPSHOT_RUNTIME_TOOLS: &[&str] =
    &["definitionSnapshot.metadata.groundzeroRuntime.approvalRequiredTools"];

pub static APPROVAL_SCREEN: ScreenSpec = ScreenSpec {
    noun: "approval request",
    columns: &[
        Column::new("Agent", &["definitionSnapshot.name"], Format::Text),
        Column::new("Version", &["agentVersion"], Format::Version).width(8),
        Column::new("Requested by", &["requestedBy"], Format::Text),
        Column::new("Requested", &["requestedAt"], Format::RelTime).width(10),
        Column::new("Status", &["status"], Format::Status(&APPROVAL_STATUS)),
        Column::new("Note", &["requestComment"], Format::Text),
    ],
    sort: Some(Sort {
        field: &["requestedAt"],
        descending: true,
    }),
    identity: Identity {
        id: ID,
        title: &["definitionSnapshot.name"],
    },
    detail: Some(DetailSpec {
        badge: Some((&["status"], &APPROVAL_STATUS)),
        subtitle: &[
            (&["agentVersion"], Format::Version),
            (&["requestedBy"], Format::Text),
            (&["requestedAt"], Format::RelTime),
        ],
        sections: &[
            Section {
                title: "Request",
                body: SectionBody::Fields(&[
                    DetailField::new("Note", &["requestComment"], Format::Text),
                    DetailField::new("Requested", &["requestedAt"], Format::DateTime),
                    DetailField::new(
                        "Project",
                        &[
                            "definitionSnapshot.tags.project-name",
                            "definitionSnapshot.metadata.projectName",
                        ],
                        Format::Text,
                    )
                    .optional(),
                ]),
            },
            Section {
                title: "Model",
                body: SectionBody::Fields(&[
                    DetailField::new("Model", &["definitionSnapshot.modelId"], Format::Model),
                    DetailField::new(
                        "Temperature",
                        &["definitionSnapshot.temperature"],
                        Format::Number,
                    ),
                    DetailField::new("Top P", &["definitionSnapshot.topP"], Format::Number),
                    DetailField::new(
                        "Max tokens",
                        &["definitionSnapshot.maxTokens"],
                        Format::Number,
                    ),
                    DetailField::new("Max turns", &["definitionSnapshot.maxTurns"], Format::Number),
                    DetailField::new(
                        "Memory window",
                        &["definitionSnapshot.memoryWindow"],
                        Format::Number,
                    ),
                ]),
            },
            Section {
                title: "Safety",
                body: SectionBody::Fields(&[
                    DetailField::new(
                        "Content guardrails",
                        &["definitionSnapshot.metadata.groundzeroRuntime.contentGuardrails.enabled"],
                        Format::Bool,
                    ),
                    DetailField::new(
                        "PII redaction",
                        &["definitionSnapshot.metadata.groundzeroRuntime.piiRedaction.enabled"],
                        Format::Bool,
                    ),
                    DetailField::new(
                        "Response format",
                        &["definitionSnapshot.metadata.groundzeroRuntime.outcomeSchema.title"],
                        Format::Text,
                    )
                    .optional(),
                ]),
            },
            Section {
                title: "Tools",
                body: SectionBody::Fields(&[
                    DetailField::new("Tools", &["definitionSnapshot.tools"], Format::Text),
                    DetailField::new("Need approval", SNAPSHOT_RUNTIME_TOOLS, Format::Text)
                        .optional(),
                ]),
            },
            Section {
                title: "Review",
                body: SectionBody::Fields(&[
                    DetailField::new("Reviewed by", &["reviewedBy"], Format::Text).optional(),
                    DetailField::new("Reviewed", &["reviewedAt"], Format::DateTime).optional(),
                    DetailField::new("Comment", &["reviewComment"], Format::Text).optional(),
                ]),
            },
            Section {
                title: "Instructions",
                body: SectionBody::Text(&["definitionSnapshot.instructions"]),
            },
        ],
        ..DetailSpec::BLANK
    }),
    actions: &[APPROVE, REQUEST_CHANGES],
    links: &[TO_AGENT],
    empty: "No approval requests are waiting. Only requests for an agent's current version that you can approve are listed.",
    ..ScreenSpec::BLANK
};

// ---------- tools and models ----------

pub static TOOL_SCREEN: ScreenSpec = ScreenSpec {
    noun: "tool",
    columns: &[
        Column::new("Tool", NAME, Format::Text),
        Column::new("Category", &["category"], Format::Label(TOOL_CATEGORIES)),
        Column::new("Writes", &["isMutating"], Format::Bool).width(6),
        Column::new("Destructive", &["isDestructive"], Format::Bool).width(11),
        Column::new("Approval", &["requiresApproval"], Format::Bool).width(8),
        Column::new("Params", &["inputSchema.properties"], Format::Count).width(6),
        Column::new("Description", &["description"], Format::Text),
    ],
    identity: Identity {
        id: NAME,
        title: NAME,
    },
    detail: Some(DetailSpec {
        subtitle: &[(&["category"], Format::Label(TOOL_CATEGORIES))],
        sections: &[
            Section {
                title: "Effect",
                body: SectionBody::Fields(&[
                    DetailField::new("Writes data", &["isMutating"], Format::Bool),
                    DetailField::new("Destructive", &["isDestructive"], Format::Bool),
                    DetailField::new("Elevated", &["isElevated"], Format::Bool),
                    DetailField::new("Always needs approval", &["requiresApproval"], Format::Bool),
                ]),
            },
            Section {
                title: "Description",
                body: SectionBody::Text(&["description"]),
            },
            Section {
                title: "Parameters",
                body: SectionBody::Object(&["inputSchema.properties"]),
            },
        ],
        ..DetailSpec::BLANK
    }),
    empty: "No tools available.",
    ..ScreenSpec::BLANK
};

pub static MODEL_SCREEN: ScreenSpec = ScreenSpec {
    noun: "model",
    columns: &[
        Column::new("Model", NAME, Format::Text),
        Column::new("Provider", &["providerName"], Format::Text),
        Column::new("Default", &["isDefault"], Format::Bool).width(7),
        Column::new("Max output", &["maxOutputTokens"], Format::Compact).width(10),
        Column::new("Input $/M", &["pricing.inputUsdPerMillion"], Format::Usd),
        Column::new("Output $/M", &["pricing.outputUsdPerMillion"], Format::Usd),
    ],
    sort: Some(Sort {
        field: &["capabilityOrder"],
        descending: false,
    }),
    identity: Identity {
        id: ID,
        title: NAME,
    },
    detail: Some(DetailSpec {
        subtitle: &[(ID, Format::Text), (&["providerName"], Format::Text)],
        sections: &[
            Section {
                title: "Price per million tokens (estimated)",
                body: SectionBody::Fields(&[
                    DetailField::new("Input", &["pricing.inputUsdPerMillion"], Format::Usd),
                    DetailField::new("Output", &["pricing.outputUsdPerMillion"], Format::Usd),
                    DetailField::new(
                        "Cache read",
                        &["pricing.cacheReadUsdPerMillion"],
                        Format::Usd,
                    ),
                    DetailField::new(
                        "Cache write",
                        &["pricing.cacheWriteUsdPerMillion"],
                        Format::Usd,
                    ),
                    DetailField::new(
                        "Cache write (1 h)",
                        &["pricing.cacheWrite1hUsdPerMillion"],
                        Format::Usd,
                    )
                    .optional(),
                    DetailField::new(
                        "Long context from",
                        &["pricing.longContextThresholdTokens"],
                        Format::Number,
                    )
                    .optional(),
                    DetailField::new(
                        "Long input",
                        &["pricing.longInputUsdPerMillion"],
                        Format::Usd,
                    )
                    .optional(),
                    DetailField::new(
                        "Long output",
                        &["pricing.longOutputUsdPerMillion"],
                        Format::Usd,
                    )
                    .optional(),
                    DetailField::new("Effective from", &["pricing.effectiveFrom"], Format::Date)
                        .optional(),
                    DetailField::new("Source", &["pricing.source"], Format::Text).optional(),
                ]),
            },
            Section {
                title: "Capabilities",
                body: SectionBody::Fields(&[
                    DetailField::new("Max output tokens", &["maxOutputTokens"], Format::Number),
                    DetailField::new("Tools", &["supportsTools"], Format::Bool),
                    DetailField::new(
                        "Structured output",
                        &["supportsNativeStructuredOutput"],
                        Format::Bool,
                    ),
                    DetailField::new(
                        "Sampling parameters",
                        &["supportsSamplingParameters"],
                        Format::Bool,
                    ),
                    DetailField::new("API", &["runtimeApi"], Format::Text),
                ]),
            },
        ],
        ..DetailSpec::BLANK
    }),
    links: &[LinkSpec {
        key: 'U',
        label: "AI usage",
        goto: Goto {
            target: Target {
                module: "Catalog",
                resource: "AI usage",
            },
            select: Select::Row {
                field: &["modelId"],
                from: ID,
            },
        },
        when: None,
    }],
    empty: "No models are offered to agents on this site.",
    ..ScreenSpec::BLANK
};

/// Enter on an agent lists its runs (a static, since it points at a static spec).
static RUN_DRILL: [Drill; 1] = [Drill {
    noun: "agent",
    id_keys: ID,
    source: Source::get(AGENTS, "/v1/agents/{id}/runs?limit=100&offset=0"),
    screen: Some(&RUN_SCREEN),
    when: None,
    repeats: false,
}];

pub const MODULE: Module = Module {
    title: "Agents",
    icon: "◉",
    resources: &[
        Resource {
            label: "Agents",
            view: View::Grid(GridSpec {
                source: Source::get(AGENTS, "/v1/agents?limit=100&offset=0"),
                drills: &RUN_DRILL,
                detail: None,
                screen: Some(&AGENT_SCREEN),
            }),
        },
        crate::tui::nav::screen(
            "Approval requests",
            Source::get(AGENTS, "/v1/approval-requests"),
            &APPROVAL_SCREEN,
        ),
        crate::tui::nav::screen("Tools", Source::get(AGENTS, "/v1/tools"), &TOOL_SCREEN),
        crate::tui::nav::screen("Models", Source::get(AGENTS, "/v1/models"), &MODEL_SCREEN),
    ],
};

#[cfg(test)]
mod tests;
