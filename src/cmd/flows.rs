//! `gz flows`: Flow Builder flows in the agent builder (`/v1/flows`), the
//! same API the web app's Flow Builder uses (ground-zero
//! src/pages/Utils/flowUtils.js). Flows are defined in YAML or JSON files
//! and run with a live per-step display; approval steps prompt here.

use anyhow::{bail, Context, Result};
use owo_colors::OwoColorize;
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::io::IsTerminal;
use std::path::Path;

use crate::cli::FlowsAction;
use crate::cmd::flow_run::{self, compact, format_duration, Board};
use crate::cmd::{confirm, new_uuid, prompt_line, read_data_arg, seg, Runtime};
use crate::output::{self, OutputFormat};

const AGENTS: &str = "agents";

/// Top-level fields a flow definition may carry (FlowCreate/FlowUpdate in
/// gz-agent-builder schemas.py; the web app's COPY_FIELDS).
const DEFINITION_FIELDS: &[&str] = &[
    "name",
    "description",
    "projectId",
    "tags",
    "parameters",
    "triggers",
    "nodes",
    "edges",
    "output",
    "layout",
    "settings",
];

/// A commented starting point for `gz flows init`.
pub const EXAMPLE: &str = r#"# A Groundzero flow (the Flow Builder's format). Create it with:
#   gz flows create -f flow.yaml
# Then check it, submit it for approval, and run it:
#   gz flows validate <id> · gz flows submit <id> · gz flows run <id> --param region=APAC
name: invoice-review
description: Extract invoices, have a team review them, and ask finance to sign off
projectId: 1000                   # a DE/ML project id (gz projects list)

# Run parameters, as JSON Schema; steps read them as {{params.<name>}}.
parameters:
  properties:
    region: {type: string, description: Which region to process}
  required: [region]

triggers:
  - id: manual
    type: trigger.manual

# Steps. ids match ^[a-z][a-z0-9_]{0,39}$; `gz flows node-types` lists every type.
nodes:
  - id: extract
    type: agent.invoke
    title: Extract invoices
    config:
      agentId: 00000000-0000-0000-0000-000000000001   # gz agents list
      input: "Extract this week's invoices for {{params.region}}"
  - id: review
    type: agent.team
    title: Review team
    config:
      mode: supervisor              # or handoff (members pass the task on)
      members:
        - {key: billing, agentId: 00000000-0000-0000-0000-000000000002, description: Checks amounts}
        - {key: compliance, agentId: 00000000-0000-0000-0000-000000000003, description: Checks policy}
      input: "{{nodes.extract.output}}"
      maxMemberCalls: 6
  - id: approve
    type: people.approval
    title: Finance sign-off
    config:
      instructions: Approve posting these invoices
  - id: post
    type: platform.action
    title: Post invoices
    config:
      tool: post_invoices
      arguments: {region: "{{params.region}}"}

# Edges connect steps; an approval step's ports are ok, rejected, failed.
edges:
  - {id: e1, source: extract, target: review}
  - {id: e2, source: review, target: approve}
  - {id: e3, source: approve, sourcePort: ok, target: post}
"#;

fn flow(id: &str, rest: &str) -> String {
    format!("/v1/flows/{}{rest}", seg(id))
}

/// `/v1/flows` exists only on an unmerged agent builder branch; the deployed
/// service answers FastAPI's bare 404 for every route under it.
fn explain_missing_flows(error: anyhow::Error) -> anyhow::Error {
    let text = error.to_string();
    if text.contains("HTTP 404") && text.ends_with(": Not Found") {
        anyhow::anyhow!(
            "flows aren't available on this agent builder; build a team instead: \
             gz agents create with a `team` definition, or gz agents add-sub-agent"
        )
    } else {
        error
    }
}

pub async fn run(rt: &Runtime, action: &FlowsAction) -> Result<()> {
    dispatch(rt, action).await.map_err(explain_missing_flows)
}

async fn dispatch(rt: &Runtime, action: &FlowsAction) -> Result<()> {
    match action {
        FlowsAction::List {
            project,
            search,
            status,
        } => {
            let mut query = Vec::new();
            if let Some(project) = project {
                query.push(("projectId".to_string(), project.to_string()));
            }
            if let Some(search) = search {
                query.push(("search".to_string(), search.clone()));
            }
            if let Some(status) = status {
                query.push(("status".to_string(), status.clone()));
            }
            let value = rt
                .http
                .request_json(AGENTS, Method::GET, "/v1/flows", &query, None)
                .await?;
            rt.show_columns(
                &value,
                &[
                    "name",
                    "id",
                    "projectId",
                    "version",
                    "approvalStatus",
                    "updatedAt",
                ],
            )
        }
        FlowsAction::Get { id } => rt.get(AGENTS, &flow(id, ""), &[]).await,
        FlowsAction::NodeTypes => {
            let value = rt
                .http
                .request_json(AGENTS, Method::GET, "/v1/flows/node-types", &[], None)
                .await?;
            rt.show_columns(&value, &["type", "category", "title", "description"])
        }
        FlowsAction::Init { path, force } => init(path, *force),
        FlowsAction::Create { file, project } => create(rt, file, *project).await,
        FlowsAction::Update { id, file } => {
            let mut definition = load_definition(file)?;
            definition.remove("id");
            rt.send(
                AGENTS,
                Method::PATCH,
                &flow(id, ""),
                Value::Object(definition),
            )
            .await
        }
        FlowsAction::Delete { id, yes } => {
            confirm(*yes, &format!("Delete flow {id} and its job?"))?;
            rt.send_empty(AGENTS, Method::DELETE, &flow(id, "")).await
        }
        FlowsAction::Validate { target } => validate(rt, target).await,
        FlowsAction::Versions { id } => {
            let value = rt
                .http
                .request_json(AGENTS, Method::GET, &flow(id, "/versions"), &[], None)
                .await?;
            rt.show_columns(
                &value,
                &[
                    "id",
                    "agentVersion",
                    "status",
                    "requestedBy",
                    "requestedAt",
                    "reviewedBy",
                    "reviewComment",
                ],
            )
        }
        FlowsAction::Submit { id, comment } => {
            rt.send(
                AGENTS,
                Method::POST,
                &flow(id, "/submit"),
                json!({"comment": comment}),
            )
            .await
        }
        FlowsAction::Decide {
            id,
            approval,
            reject,
            comment,
        } => {
            if *reject && comment.is_none() {
                bail!("a rejection needs --comment explaining why");
            }
            let decision = if *reject { "rejected" } else { "approved" };
            let path = flow(id, &format!("/approvals/{approval}/decision"));
            rt.send(
                AGENTS,
                Method::POST,
                &path,
                json!({"decision": decision, "comment": comment}),
            )
            .await
        }
        FlowsAction::Run {
            id,
            params,
            params_file,
            latest,
            background,
        } => {
            let parameters = run_parameters(params, params_file.as_deref())?;
            start_run(rt, id, parameters, *latest, *background).await
        }
        FlowsAction::Resume {
            id,
            run,
            approve,
            reject,
            comment,
            background,
        } => {
            let responses = manual_responses(approve, reject, comment.as_deref())?;
            resume(rt, id, run, responses, *background).await
        }
        FlowsAction::Runs { id } => {
            let value = rt
                .http
                .request_json(AGENTS, Method::GET, &flow(id, "/runs"), &[], None)
                .await?;
            rt.show_columns(
                &value,
                &["runId", "id", "status", "mode", "startedAt", "finishedAt"],
            )
        }
        FlowsAction::Show { id, run } => show_run(rt, id, run).await,
        FlowsAction::Cancel { id, run } => {
            let path = flow(id, &format!("/runs/{}/cancel", seg(run)));
            rt.send_empty(AGENTS, Method::POST, &path).await
        }
        FlowsAction::Triggers { id } => {
            let value = rt
                .http
                .request_json(AGENTS, Method::GET, &flow(id, "/triggers"), &[], None)
                .await?;
            rt.show_columns(
                &value,
                &[
                    "id",
                    "type",
                    "title",
                    "active",
                    "nextFireAt",
                    "lastFiredAt",
                    "lastError",
                ],
            )
        }
    }
}

fn init(path: &str, force: bool) -> Result<()> {
    let target = Path::new(path);
    if target.exists() && !force {
        bail!("{path} already exists; pass --force to overwrite it");
    }
    std::fs::write(target, EXAMPLE).with_context(|| format!("cannot write {path}"))?;
    output::success(&format!(
        "wrote {path} · set projectId and the agent ids, then `gz flows create -f {path}`"
    ));
    Ok(())
}

/// Reads a flow definition from YAML or JSON (by extension, else whichever
/// parses). Unknown top-level fields are refused here so a typo doesn't
/// silently vanish on the server.
pub fn load_definition(path: &str) -> Result<Map<String, Value>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
    let lower = path.to_ascii_lowercase();
    let value: Value = if lower.ends_with(".json") {
        serde_json::from_str(&text).with_context(|| format!("{path} is not valid JSON"))?
    } else if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        serde_yaml::from_str(&text).with_context(|| format!("{path} is not valid YAML"))?
    } else {
        serde_json::from_str(&text)
            .or_else(|_| serde_yaml::from_str(&text))
            .with_context(|| format!("{path} is neither JSON nor YAML"))?
    };
    let Value::Object(definition) = value else {
        bail!("{path} must define a flow object (name, projectId, nodes, …)");
    };
    let unknown: Vec<&String> = definition
        .keys()
        .filter(|key| key.as_str() != "id" && !DEFINITION_FIELDS.contains(&key.as_str()))
        .collect();
    if !unknown.is_empty() {
        bail!(
            "{path}: unknown field(s) {}; a flow has {}",
            unknown
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            DEFINITION_FIELDS.join(", ")
        );
    }
    Ok(definition)
}

/// The create body: a new UUID (as the web app generates), the project
/// from `--project` or the file.
fn create_body(mut definition: Map<String, Value>, project: Option<i64>) -> Result<Value> {
    if let Some(project) = project {
        definition.insert("projectId".to_string(), json!(project));
    }
    if !definition.get("projectId").is_some_and(Value::is_i64) {
        bail!("set projectId in the file or pass --project <id> (see `gz projects list`)");
    }
    if !definition.get("name").is_some_and(Value::is_string) {
        bail!("the flow needs a name");
    }
    definition
        .entry("id".to_string())
        .or_insert_with(|| json!(new_uuid()));
    Ok(Value::Object(definition))
}

async fn create(rt: &Runtime, file: &str, project: Option<i64>) -> Result<()> {
    let body = create_body(load_definition(file)?, project)?;
    let created = rt
        .http
        .request_json(AGENTS, Method::POST, "/v1/flows", &[], Some(body))
        .await?;
    if rt.format != OutputFormat::Table {
        return rt.show(created).await;
    }
    let id = created["id"].as_str().unwrap_or("?");
    output::success(&format!(
        "created flow {} ({id})",
        created["name"].as_str().unwrap_or("")
    ));
    output::info(&format!(
        "next: gz flows validate {id} · gz flows submit {id}"
    ));
    Ok(())
}

/// Validates a local file (sent as a draft) or a saved flow by id.
async fn validate(rt: &Runtime, target: &str) -> Result<()> {
    let definition = if Path::new(target).is_file() {
        Value::Object(load_definition(target)?)
    } else {
        rt.http
            .request_json(AGENTS, Method::GET, &flow(target, ""), &[], None)
            .await?
    };
    let reply = rt
        .http
        .request_json(
            AGENTS,
            Method::POST,
            "/v1/flows/validate",
            &[],
            Some(definition.clone()),
        )
        .await?;
    if rt.format != OutputFormat::Table {
        rt.show(reply.clone()).await?;
    }
    let errors = issue_lines(&reply["errors"], &definition);
    let warnings = issue_lines(&reply["warnings"], &definition);
    if rt.format == OutputFormat::Table {
        for warning in &warnings {
            output::warn(warning);
        }
    }
    if reply["valid"].as_bool().unwrap_or(errors.is_empty()) && errors.is_empty() {
        if rt.format == OutputFormat::Table {
            output::success(&format!("{target} is valid"));
        }
        return Ok(());
    }
    bail!(
        "{target} is not valid:\n{}",
        errors
            .iter()
            .map(|line| format!("  ✗ {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// One readable line per validation issue: the server's `path` with
/// `nodes[2]` turned into the step's id, e.g. `step review: config.members[0].agentId: …`.
pub fn issue_lines(issues: &Value, definition: &Value) -> Vec<String> {
    issues
        .as_array()
        .into_iter()
        .flatten()
        .map(|issue| {
            let message = ["message", "msg", "detail"]
                .iter()
                .find_map(|key| issue[key].as_str())
                .unwrap_or("invalid");
            let path = issue["path"].as_str().unwrap_or_default();
            match readable_path(path, definition) {
                Some(place) => format!("{place}: {message}"),
                None => message.to_string(),
            }
        })
        .collect()
}

fn readable_path(path: &str, definition: &Value) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    for (list, noun) in [
        ("nodes", "step"),
        ("triggers", "trigger"),
        ("edges", "edge"),
    ] {
        let Some(rest) = path.strip_prefix(&format!("{list}[")) else {
            continue;
        };
        let (index, tail) = rest.split_once(']')?;
        let item = &definition[list][index.parse::<usize>().ok()?];
        let name = item["id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("#{}", index.parse::<usize>().unwrap_or(0) + 1));
        let tail = tail.trim_start_matches('.');
        return Some(if tail.is_empty() {
            format!("{noun} {name}")
        } else {
            format!("{noun} {name}: {tail}")
        });
    }
    Some(path.to_string())
}

/// `--param key=value` (values parse as JSON when they can: numbers,
/// booleans, lists) merged over `--params @file.json`.
pub fn run_parameters(params: &[String], file: Option<&str>) -> Result<Value> {
    let mut parameters = match read_data_arg(file)? {
        Some(Value::Object(map)) => map,
        Some(_) => bail!("--params must be a JSON object of parameter values"),
        None => Map::new(),
    };
    for item in params {
        let (key, raw) = item
            .split_once('=')
            .with_context(|| format!("--param must be name=value, got '{item}'"))?;
        let value = serde_json::from_str(raw).unwrap_or_else(|_| json!(raw));
        parameters.insert(key.trim().to_string(), value);
    }
    Ok(Value::Object(parameters))
}

async fn start_run(
    rt: &Runtime,
    id: &str,
    parameters: Value,
    latest: bool,
    background: bool,
) -> Result<()> {
    let version = if latest { "latest" } else { "approved" };
    let body = json!({"parameters": parameters, "version": version, "background": true});
    let started = rt
        .http
        .request_json(AGENTS, Method::POST, &flow(id, "/runs"), &[], Some(body))
        .await?;
    let run_id = started["runId"]
        .as_str()
        .context("the flow did not return a run id")?
        .to_string();
    if background {
        return print_background(rt, id, &run_id, started).await;
    }
    let definition = rt
        .http
        .request_json(AGENTS, Method::GET, &flow(id, ""), &[], None)
        .await
        .unwrap_or(Value::Null);
    if rt.format == OutputFormat::Table && !rt.quiet {
        let name = definition["name"].as_str().unwrap_or(id);
        eprintln!("{}", format!("● {name} · run {run_id} · {version}").bold());
    }
    drive(rt, id, &run_id, Board::for_flow(&definition)).await
}

async fn print_background(rt: &Runtime, id: &str, run_id: &str, started: Value) -> Result<()> {
    if rt.format != OutputFormat::Table {
        return rt.show(started).await;
    }
    println!("{run_id}");
    output::info(&format!(
        "check it: gz flows show {id} {run_id} · cancel: gz flows cancel {id} {run_id}"
    ));
    Ok(())
}

/// Follows a run segment by segment. A segment that stops for approvals is
/// answered here when a person is at the terminal (each answer starts a new
/// segment, as in the web app's run view), else explained.
async fn drive(rt: &Runtime, id: &str, run_id: &str, mut board: Board) -> Result<()> {
    let mut segment = run_id.to_string();
    loop {
        let end = flow_run::follow_segment(rt, id, &segment, &mut board).await?;
        match end.status.as_deref() {
            Some("interrupted") => {
                let interrupts = pending_interrupts(rt, id, &segment, end.interrupts).await?;
                let can_ask = rt.format == OutputFormat::Table && std::io::stdin().is_terminal();
                if !can_ask || interrupts.is_empty() {
                    explain_pause(id, &segment, &interrupts);
                    bail!("run {segment} is waiting for approval");
                }
                let responses = ask_approvals(&interrupts)?;
                let resumed = resume_request(rt, id, &segment, responses).await?;
                segment = resumed;
            }
            Some("failed") => bail!(
                "flow run {segment} failed: {}",
                end.error
                    .unwrap_or_else(|| "see `gz flows show`".to_string())
            ),
            Some("cancelled") => bail!("flow run {segment} was cancelled"),
            _ => {
                print_result(rt, &board, end.result.as_ref());
                return Ok(());
            }
        }
    }
}

/// The paused segment's interrupts; the stream normally carries them, the
/// run view is the fallback.
async fn pending_interrupts(
    rt: &Runtime,
    id: &str,
    run_id: &str,
    streamed: Vec<Value>,
) -> Result<Vec<Value>> {
    if !streamed.is_empty() {
        return Ok(streamed);
    }
    let view = rt
        .http
        .request_json(
            AGENTS,
            Method::GET,
            &flow(id, &format!("/runs/{}", seg(run_id))),
            &[],
            None,
        )
        .await?;
    Ok(view["pendingInterrupts"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

fn explain_pause(id: &str, run_id: &str, interrupts: &[Value]) {
    output::warn("the run paused for approval:");
    for interrupt in interrupts {
        eprintln!("  · {}", interrupt_summary(interrupt));
        let iid = interrupt["id"].as_str().unwrap_or("?");
        eprintln!(
            "    gz flows resume {id} {run_id} --approve '{iid}'   (or --reject '{iid}' --comment …)"
        );
    }
}

fn interrupt_summary(interrupt: &Value) -> String {
    let title = interrupt["title"]
        .as_str()
        .or(interrupt["nodeId"].as_str())
        .unwrap_or("approval");
    let reason = interrupt["reason"].as_str().unwrap_or_default();
    match interrupt["toolName"].as_str() {
        Some(tool) => format!("{title}: {tool} — {reason}"),
        None if reason.is_empty() => title.to_string(),
        None => format!("{title}: {reason}"),
    }
}

/// Asks once per pending approval; anything but yes is a refusal.
fn ask_approvals(interrupts: &[Value]) -> Result<Vec<Value>> {
    let mut responses = Vec::new();
    for interrupt in interrupts {
        eprintln!();
        eprintln!(
            "{} {}",
            "?".yellow().bold(),
            interrupt_summary(interrupt).bold()
        );
        if let Some(preview) = preview_text(&interrupt["inputPreview"]) {
            for line in preview.lines().take(12) {
                eprintln!("  {}", line.dimmed());
            }
        }
        let answer = prompt_line("  approve [y] · reject [n] · reject with a note [c] › ")?;
        let comment = match answer.to_ascii_lowercase().as_str() {
            "c" => Some(prompt_line("  note for the run › ")?),
            _ => None,
        };
        let approved = matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes");
        responses.push(response(interrupt, approved, comment));
    }
    Ok(responses)
}

fn preview_text(preview: &Value) -> Option<String> {
    match preview {
        Value::Null => None,
        Value::Object(map) if map.is_empty() => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        other => serde_json::to_string_pretty(other).ok(),
    }
}

/// One `interruptResponses` entry. Approval steps are rejected; tool
/// approvals are denied (both refuse; the engine reports them differently).
pub fn response(interrupt: &Value, approved: bool, comment: Option<String>) -> Value {
    let refusal = if interrupt["name"].as_str() == Some("orchestration_approval") {
        "rejected"
    } else {
        "denied"
    };
    let mut entry = json!({
        "interruptId": interrupt["id"],
        "decision": if approved { "approved" } else { refusal },
    });
    if let Some(comment) = comment.filter(|c| !c.trim().is_empty()) {
        entry["comment"] = json!(comment);
    }
    entry
}

fn manual_responses(
    approve: &[String],
    reject: &[String],
    comment: Option<&str>,
) -> Result<Vec<Value>> {
    if approve.is_empty() && reject.is_empty() {
        bail!("pass --approve <interrupt-id> or --reject <interrupt-id> (see `gz flows show`)");
    }
    let mut responses: Vec<Value> = approve
        .iter()
        .map(|id| json!({"interruptId": id, "decision": "approved"}))
        .collect();
    responses.extend(reject.iter().map(|id| {
        let mut entry = json!({"interruptId": id, "decision": "rejected"});
        if let Some(comment) = comment {
            entry["comment"] = json!(comment);
        }
        entry
    }));
    Ok(responses)
}

/// Posts the decisions to the paused segment; returns the new segment's run id.
async fn resume_request(
    rt: &Runtime,
    id: &str,
    run_id: &str,
    responses: Vec<Value>,
) -> Result<String> {
    let body = json!({"interruptResponses": responses, "background": true});
    let path = flow(id, &format!("/runs/{}/resume", seg(run_id)));
    let resumed = rt
        .http
        .request_json(AGENTS, Method::POST, &path, &[], Some(body))
        .await?;
    Ok(resumed["runId"].as_str().unwrap_or(run_id).to_string())
}

async fn resume(
    rt: &Runtime,
    id: &str,
    run: &str,
    responses: Vec<Value>,
    background: bool,
) -> Result<()> {
    let segment = resume_request(rt, id, run, responses).await?;
    if background {
        return print_background(rt, id, &segment, json!({"runId": segment})).await;
    }
    let definition = rt
        .http
        .request_json(AGENTS, Method::GET, &flow(id, ""), &[], None)
        .await
        .unwrap_or(Value::Null);
    drive(rt, id, &segment, Board::for_flow(&definition)).await
}

fn print_result(rt: &Runtime, board: &Board, result: Option<&Value>) {
    if rt.format != OutputFormat::Table {
        return;
    }
    if let Some(result) = result {
        let text = result["text"]
            .as_str()
            .or(result["output"].as_str())
            .or(result.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string_pretty(result).unwrap_or_default());
        if !text.trim().is_empty() {
            println!("{}", text.trim_end());
        }
    }
    let mut summary = vec!["✓ flow completed".to_string()];
    if let Some((input, output_tokens, cost)) = board.totals {
        if input + output_tokens > 0 {
            summary.push(format!(
                "↑{} ↓{} tokens",
                compact(input),
                compact(output_tokens)
            ));
        }
        if let Some(cost) = cost.filter(|c| *c > 0.0) {
            summary.push(format!("${cost:.4}"));
        }
    }
    eprintln!("{}", summary.join(" · ").green());
}

async fn show_run(rt: &Runtime, id: &str, run: &str) -> Result<()> {
    let view = rt
        .http
        .request_json(
            AGENTS,
            Method::GET,
            &flow(id, &format!("/runs/{}", seg(run))),
            &[],
            None,
        )
        .await?;
    if rt.format != OutputFormat::Table {
        return rt.show(view).await;
    }
    let status = view["status"].as_str().unwrap_or("unknown");
    println!(
        "{} {}",
        format!("run {run}").bold(),
        output::dim(&format!("· {status}"))
    );
    if let Some(error) = view["error"]["message"]
        .as_str()
        .or(view["error"]["code"].as_str())
    {
        output::warn(error);
    }
    output::print_value(&Value::Array(step_rows(&view)), rt.format, rt.limit)?;
    let pending = view["pendingInterrupts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !pending.is_empty() {
        let active = view["activeRunId"].as_str().unwrap_or(run);
        explain_pause(id, active, &pending);
    }
    Ok(())
}

/// One row per step execution: status, duration, tokens, cost, error.
pub fn step_rows(view: &Value) -> Vec<Value> {
    let mut executions = view["nodeExecutions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    executions.sort_by_key(|e| e["sequence"].as_u64().unwrap_or(0));
    executions
        .iter()
        .map(|execution| {
            let tokens = (
                execution["inputTokens"].as_u64().unwrap_or(0),
                execution["outputTokens"].as_u64().unwrap_or(0),
            );
            let mut step = execution["nodeId"].as_str().unwrap_or("?").to_string();
            if let Some(member) = execution["memberKey"].as_str() {
                step.push_str(&format!(" · {member}"));
            }
            json!({
                "step": step,
                "type": execution["nodeType"],
                "status": execution["status"],
                "attempt": execution["attempt"],
                "duration": execution["durationMs"].as_u64().map(format_duration),
                "tokens": (tokens.0 + tokens.1 > 0)
                    .then(|| format!("↑{} ↓{}", compact(tokens.0), compact(tokens.1))),
                "cost": execution["totalCostUsd"],
                "error": execution["error"]["message"].as_str().or(execution["error"]["code"].as_str()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, text: &str) -> String {
        let path = std::env::temp_dir().join(format!("gz-flow-{}-{name}", std::process::id()));
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    #[test]
    fn the_example_loads_and_makes_a_create_body() {
        let path = temp_file("example.yaml", EXAMPLE);
        let definition = load_definition(&path).unwrap();
        assert_eq!(definition["nodes"].as_array().unwrap().len(), 4);
        assert_eq!(definition["nodes"][1]["config"]["mode"], "supervisor");
        let body = create_body(definition, Some(7)).unwrap();
        assert_eq!(body["projectId"], 7);
        assert_eq!(body["id"].as_str().unwrap().len(), 36, "a fresh UUID");
    }

    #[test]
    fn loading_refuses_unknown_fields_and_non_objects() {
        let typo = temp_file("typo.yaml", "name: x\nprojectId: 1\nnode: []\n");
        let err = load_definition(&typo).unwrap_err().to_string();
        assert!(err.contains("unknown field(s) node"), "{err}");
        let list = temp_file("list.json", "[1, 2]");
        assert!(load_definition(&list).is_err());
        let bare = temp_file("bare", r#"{"name": "x", "nodes": []}"#);
        assert_eq!(load_definition(&bare).unwrap()["name"], "x");
    }

    #[test]
    fn create_needs_a_project_and_a_name() {
        let mut map = Map::new();
        map.insert("name".into(), json!("x"));
        assert!(create_body(map.clone(), None)
            .unwrap_err()
            .to_string()
            .contains("--project"));
        let mut no_name = Map::new();
        no_name.insert("projectId".into(), json!(3));
        assert!(create_body(no_name, None).is_err());
        map.insert("id".into(), json!("keep-me"));
        assert_eq!(create_body(map, Some(1)).unwrap()["id"], "keep-me");
    }

    #[test]
    fn issues_name_the_step_trigger_or_edge() {
        let definition = json!({
            "nodes": [{"id": "extract"}, {"id": "review"}],
            "triggers": [{"id": "manual"}],
            "edges": [{"id": "e1"}, {}],
        });
        let issues = json!([
            {"code": "MemberUnknown", "message": "agent not found", "path": "nodes[1].config.members[0].agentId"},
            {"code": "NodeTypeUnknown", "message": "unknown type", "path": "nodes[0]"},
            {"code": "TriggerDuplicateId", "message": "used twice", "path": "triggers[0].id"},
            {"code": "EdgeTargetUnknown", "message": "no such step", "path": "edges[1].target"},
            {"code": "FlowFieldInvalid", "message": "a flow must be an object", "path": ""},
            {"code": "X", "message": "bad output", "path": "output.nodeId"},
        ]);
        assert_eq!(
            issue_lines(&issues, &definition),
            vec![
                "step review: config.members[0].agentId: agent not found",
                "step extract: unknown type",
                "trigger manual: id: used twice",
                "edge #2: target: no such step",
                "a flow must be an object",
                "output.nodeId: bad output",
            ]
        );
    }

    #[test]
    fn parameters_parse_json_values_over_a_file() {
        let file = temp_file("params.json", r#"{"region": "EU", "limit": 5}"#);
        let params = run_parameters(
            &["region=APAC".into(), "dry=true".into(), "tags=[1,2]".into()],
            Some(&format!("@{file}")),
        )
        .unwrap();
        assert_eq!(
            params,
            json!({"region": "APAC", "limit": 5, "dry": true, "tags": [1, 2]})
        );
        assert!(run_parameters(&["oops".into()], None).is_err());
    }

    #[test]
    fn approval_answers_use_the_engine_vocabulary() {
        let step = json!({"id": "orc:1:approval", "name": "orchestration_approval"});
        let tool = json!({"id": "orc:2:action", "name": "tool_approval"});
        assert_eq!(
            response(&step, true, None),
            json!({"interruptId": "orc:1:approval", "decision": "approved"})
        );
        assert_eq!(
            response(&step, false, Some("wrong totals".into())),
            json!({"interruptId": "orc:1:approval", "decision": "rejected", "comment": "wrong totals"})
        );
        assert_eq!(response(&tool, false, None)["decision"], "denied");
        let manual = manual_responses(&["a".into()], &["b".into()], Some("no")).unwrap();
        assert_eq!(
            manual[1],
            json!({"interruptId": "b", "decision": "rejected", "comment": "no"})
        );
        assert!(manual_responses(&[], &[], None).is_err());
    }

    #[test]
    fn step_rows_follow_execution_order() {
        let rows = step_rows(&json!({"nodeExecutions": [
            {"nodeId": "review", "memberKey": "billing", "nodeType": "agent", "status": "completed",
             "sequence": 2, "attempt": 1, "durationMs": 900, "inputTokens": 1200, "outputTokens": 40, "totalCostUsd": "0.0010"},
            {"nodeId": "extract", "nodeType": "agent", "status": "failed", "sequence": 1, "attempt": 2,
             "error": {"code": "Timeout", "message": "took too long"}},
        ]}));
        assert_eq!(rows[0]["step"], "extract");
        assert_eq!(rows[0]["error"], "took too long");
        assert_eq!(rows[1]["step"], "review · billing");
        assert_eq!(rows[1]["tokens"], "↑1.2k ↓40");
        assert_eq!(rows[1]["duration"], "900ms");
    }
}
