use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::{AgentsAction, ChatAction, GrantScope};
use crate::cmd::stream::{self, Outcome};
use crate::cmd::sub_agents;
use crate::cmd::{new_uuid, read_text_arg, seg, Runtime};
use crate::output::{self, OutputFormat};

const AGENTS: &str = "agents";
const CHAT: &str = "chat";
const TITLE_CHARS: usize = 60;

pub async fn agents(rt: &Runtime, action: &AgentsAction) -> Result<()> {
    let agent = |id: &str, rest: &str| format!("/v1/agents/{}{rest}", seg(id));
    match action {
        AgentsAction::List { body } => {
            let list = rt
                .fetch(AGENTS, Method::GET, "/v1/agents", body, None)
                .await?;
            if rt.format != OutputFormat::Table {
                return rt.show(list).await;
            }
            rt.show_columns(
                &sub_agents::with_kind(&list),
                &[
                    "name",
                    "id",
                    "kind",
                    "version",
                    "isPublished",
                    "approvalStatus",
                ],
            )
        }
        AgentsAction::Get { id } => show_agent(rt, &agent(id, "")).await,
        AgentsAction::Create { body } => {
            rt.call(AGENTS, Method::POST, "/v1/agents", body, None)
                .await
        }
        AgentsAction::Update { id, body } => {
            rt.call(AGENTS, Method::PATCH, &agent(id, ""), body, None)
                .await
        }
        AgentsAction::Delete { id } => rt.send_empty(AGENTS, Method::DELETE, &agent(id, "")).await,
        AgentsAction::Test { body } => {
            rt.call(AGENTS, Method::POST, "/v1/agents/test", body, None)
                .await
        }
        AgentsAction::Invoke {
            id,
            message,
            draft,
            session,
        } => invoke(rt, id, &read_text_arg(message)?, *draft, session.as_deref()).await,
        AgentsAction::Runs { id, body } => {
            rt.call(AGENTS, Method::GET, &agent(id, "/runs"), body, None)
                .await
        }
        AgentsAction::Run { id, run } => {
            show_run(rt, &agent(id, &format!("/runs/{}", seg(run)))).await
        }
        AgentsAction::SubAgents { id } => {
            let parent = rt
                .http
                .request_json(AGENTS, Method::GET, &agent(id, ""), &[], None)
                .await?;
            let list = json!({"items": parent["subAgents"].clone()});
            if parent["subAgents"].as_array().is_none_or(Vec::is_empty)
                && rt.format == OutputFormat::Table
            {
                output::info(&format!("agent {id} has no sub-agents · add one: gz agents add-sub-agent {id} <child> --alias <name>"));
                return Ok(());
            }
            rt.show_columns(&list, &["alias", "agentId", "description"])
        }
        AgentsAction::AddSubAgent {
            id,
            child,
            alias,
            description,
        } => {
            let entry = sub_agents::new_entry(child, alias, description.as_deref());
            edit_sub_agents(rt, id, |current| {
                sub_agents::add_sub_agent(current, id, entry)
            })
            .await
        }
        AgentsAction::RemoveSubAgent { id, alias } => {
            edit_sub_agents(rt, id, |current| {
                sub_agents::remove_sub_agent(current, alias)
            })
            .await
        }
        AgentsAction::Events { id, run } => {
            let outcome = stream::follow(
                rt,
                AGENTS,
                &agent(id, &format!("/runs/{}/events", seg(run))),
            )
            .await?;
            report(rt, &outcome, None)
        }
        AgentsAction::Cancel { id, run } => {
            rt.send_empty(
                AGENTS,
                Method::POST,
                &agent(id, &format!("/runs/{}/cancel", seg(run))),
            )
            .await
        }
        AgentsAction::Observability { id, run } => {
            let path = agent(id, &format!("/runs/{}/observability", seg(run)));
            rt.get(AGENTS, &path, &[]).await
        }
        AgentsAction::Resume { id, run, body } => {
            let path = agent(id, &format!("/runs/{}/resume", seg(run)));
            rt.call(AGENTS, Method::POST, &path, body, None).await
        }
        AgentsAction::Sessions { id } => rt.get(AGENTS, &agent(id, "/sessions"), &[]).await,
        AgentsAction::Messages { id, session } => {
            let path = agent(id, &format!("/sessions/{}/messages", seg(session)));
            rt.get(AGENTS, &path, &[]).await
        }
        AgentsAction::Approvals { id } => rt.get(AGENTS, &agent(id, "/approvals"), &[]).await,
        AgentsAction::Publish { id } => {
            rt.send_empty(AGENTS, Method::POST, &agent(id, "/publish"))
                .await
        }
        AgentsAction::ApprovalRequests { body } => {
            rt.call(AGENTS, Method::GET, "/v1/approval-requests", body, None)
                .await
        }
        AgentsAction::Decide { approval, body } => {
            let path = format!("/v1/approval-requests/{}/decision", seg(approval));
            rt.call(AGENTS, Method::POST, &path, body, None).await
        }
        AgentsAction::Publications => rt.get(AGENTS, "/v1/publications", &[]).await,
        AgentsAction::Quota { id } => rt.get(AGENTS, &agent(id, "/quota"), &[]).await,
        AgentsAction::SetQuota { id, body } => {
            rt.call(AGENTS, Method::PUT, &agent(id, "/quota"), body, None)
                .await
        }
        AgentsAction::Tags { id } => rt.get(AGENTS, &agent(id, "/tags"), &[]).await,
        AgentsAction::Tag { id, body } => {
            rt.call(AGENTS, Method::POST, &agent(id, "/tags"), body, None)
                .await
        }
        AgentsAction::Tools => rt.get(AGENTS, "/v1/tools", &[]).await,
        AgentsAction::Models => rt.get(AGENTS, "/v1/models", &[]).await,
        AgentsAction::Metrics { body } => {
            rt.call(AGENTS, Method::GET, "/v1/agents/metrics", body, None)
                .await
        }
        AgentsAction::AgentMetrics { id } => rt.get(AGENTS, &agent(id, "/metrics"), &[]).await,
        AgentsAction::Timeseries { from, to, bucket } => {
            let query = vec![
                ("from".to_string(), from.clone()),
                ("to".to_string(), to.clone()),
                ("bucket".to_string(), bucket.clone()),
            ];
            let value = rt
                .http
                .request_json(
                    AGENTS,
                    Method::GET,
                    "/v1/agents/metrics/timeseries",
                    &query,
                    None,
                )
                .await?;
            rt.show_columns(
                &value,
                &[
                    "bucketStart",
                    "completedCount",
                    "failedCount",
                    "interruptedCount",
                    "estimatedModelCostUsd",
                    "p95ModelLatencyMs",
                ],
            )
        }
        AgentsAction::SubmitApproval { id, comment } => {
            rt.send(
                AGENTS,
                Method::POST,
                &agent(id, "/approval-requests"),
                json!({"comment": comment}),
            )
            .await
        }
        AgentsAction::Catalog { offset } => {
            let mut query = Vec::new();
            if let Some(limit) = rt.limit {
                query.push(("limit".to_string(), limit.to_string()));
            }
            if let Some(offset) = offset {
                query.push(("offset".to_string(), offset.to_string()));
            }
            let value = rt
                .http
                .request_json(AGENTS, Method::GET, "/v1/admin/publications", &query, None)
                .await?;
            rt.show_columns(
                &value,
                &[
                    "name",
                    "id",
                    "status",
                    "modelId",
                    "publishedBy",
                    "approvedBy",
                ],
            )
        }
        AgentsAction::ImportPublication {
            publication,
            project,
            on_conflict,
            name,
        } => {
            let body = import_body(*project, on_conflict, name.as_deref());
            let path = format!("/v1/admin/publications/{}/import", seg(publication));
            rt.send(AGENTS, Method::POST, &path, body).await
        }
        AgentsAction::RunGovernance { id, run } => {
            rt.get(
                AGENTS,
                &agent(id, &format!("/runs/{}/governance", seg(run))),
                &[],
            )
            .await
        }
        AgentsAction::RunMetrics { id, run } => {
            rt.get(
                AGENTS,
                &agent(id, &format!("/runs/{}/metrics", seg(run))),
                &[],
            )
            .await
        }
        AgentsAction::Evaluations { id, run } => {
            rt.get(
                AGENTS,
                &agent(id, &format!("/runs/{}/evaluations", seg(run))),
                &[],
            )
            .await
        }
        AgentsAction::Feedback { id, run } => {
            rt.get(
                AGENTS,
                &agent(id, &format!("/runs/{}/feedback", seg(run))),
                &[],
            )
            .await
        }
        AgentsAction::Audit { id, run } => {
            let query: Vec<(String, String)> = run
                .iter()
                .map(|r| ("runId".to_string(), r.clone()))
                .collect();
            rt.get(AGENTS, &agent(id, "/audit"), &query).await
        }
        AgentsAction::ClearQuota { id } => {
            rt.send_empty(AGENTS, Method::DELETE, &agent(id, "/quota"))
                .await
        }
        AgentsAction::Untag { id, key } => {
            rt.send_empty(
                AGENTS,
                Method::DELETE,
                &agent(id, &format!("/tags/{}", seg(key))),
            )
            .await
        }
    }
}

fn import_body(project: u64, on_conflict: &str, name: Option<&str>) -> Value {
    let mut body = json!({"projectId": project, "conflictStrategy": on_conflict});
    if let Some(name) = name {
        body["targetName"] = json!(name);
    }
    body
}

/// Starts the run in the background, then streams its events — the same
/// two steps the agent builder's preview pane takes.
/// Reads the agent's `subAgents`, applies `change`, and PATCHes the result.
async fn edit_sub_agents(
    rt: &Runtime,
    id: &str,
    change: impl FnOnce(&[Value]) -> Result<Vec<Value>>,
) -> Result<()> {
    let path = format!("/v1/agents/{}", seg(id));
    let parent = rt
        .http
        .request_json(AGENTS, Method::GET, &path, &[], None)
        .await?;
    let current = parent["subAgents"].as_array().cloned().unwrap_or_default();
    let next = change(&current)?;
    let count = next.len();
    let updated = rt
        .http
        .request_json(
            AGENTS,
            Method::PATCH,
            &path,
            &[],
            Some(json!({"subAgents": next})),
        )
        .await?;
    if rt.format != OutputFormat::Table {
        return rt.show(updated).await;
    }
    output::success(&format!("agent {id} now has {count} sub-agent(s)"));
    Ok(())
}

/// An agent; for table output also its team members or sub-agents.
async fn show_agent(rt: &Runtime, path: &str) -> Result<()> {
    let agent = rt
        .http
        .request_json(AGENTS, Method::GET, path, &[], None)
        .await?;
    if rt.format != OutputFormat::Table {
        return rt.show(agent).await;
    }
    let (heading, members) = match agent["team"]["members"].as_array() {
        Some(members) => (sub_agents::kind_label(&agent), members.clone()),
        None => (
            "sub-agents".to_string(),
            agent["subAgents"].as_array().cloned().unwrap_or_default(),
        ),
    };
    rt.show(agent).await?;
    if members.is_empty() {
        return Ok(());
    }
    println!("{}", output::dim(&heading));
    rt.show_columns(&Value::Array(members), &["alias", "agentId", "description"])
}

/// A run, then the child runs its sub-agents started.
async fn show_run(rt: &Runtime, path: &str) -> Result<()> {
    let run = rt
        .http
        .request_json(AGENTS, Method::GET, path, &[], None)
        .await?;
    let children = sub_agents::children_rows(&run);
    if rt.format != OutputFormat::Table || children.is_empty() {
        return rt.show(run).await;
    }
    let mut parent = run.clone();
    if let Value::Object(map) = &mut parent {
        map.remove("children");
    }
    rt.show(parent).await?;
    println!("{}", output::dim("child runs"));
    output::print_value(&Value::Array(children), rt.format, rt.limit)
}

async fn invoke(
    rt: &Runtime,
    id: &str,
    message: &str,
    draft: bool,
    session: Option<&str>,
) -> Result<()> {
    let mode = if draft { "preview" } else { "invoke" };
    let mut body = json!({"message": message, "background": true});
    if let Some(session) = session {
        body["sessionId"] = json!(session);
    }
    let path = format!("/v1/agents/{}/{mode}", seg(id));
    let run = rt
        .http
        .request_json(AGENTS, Method::POST, &path, &[], Some(body))
        .await?;
    let run_id = run["runId"]
        .as_str()
        .context("agent did not return a run id")?;
    let session_id = run["sessionId"].as_str().map(str::to_string);
    let events = format!("/v1/agents/{}/runs/{}/events", seg(id), seg(run_id));
    let outcome = stream::follow(rt, AGENTS, &events).await?;
    let next = session_id.map(|s| format!("continue: gz agents invoke {id} \"…\" --session {s}"));
    report(rt, &outcome, next.as_deref())
}

pub async fn chat(rt: &Runtime, action: &ChatAction) -> Result<()> {
    let conversation = |id: &str| format!("/chat/conversations/{}", seg(id));
    match action {
        ChatAction::Ask {
            prompt,
            conversation,
            title,
        } => {
            ask(
                rt,
                &read_text_arg(prompt)?,
                conversation.as_deref(),
                title.as_deref(),
            )
            .await
        }
        ChatAction::List { search } => {
            let mut query = vec![("assistantMode".to_string(), "chat".to_string())];
            if let Some(term) = search {
                query.push(("search".to_string(), term.clone()));
            }
            let value = rt
                .http
                .request_json(CHAT, Method::GET, "/chat/conversations", &query, None)
                .await?;
            rt.show_columns(
                &value,
                &[
                    "conversationName",
                    "conversationId",
                    "groundzeroMode",
                    "modelProvider",
                ],
            )
        }
        ChatAction::Show { id } => {
            let value = rt
                .http
                .request_json(CHAT, Method::GET, &conversation(id), &[], None)
                .await?;
            if rt.format == OutputFormat::Table && print_transcript(&value) {
                return Ok(());
            }
            rt.show(value).await
        }
        ChatAction::Delete { ids } => delete_conversations(rt, ids).await,
        ChatAction::Fork {
            id,
            through,
            exclude_selected,
            name,
        } => {
            let body = fork_body(through.as_deref(), *exclude_selected, name.as_deref());
            rt.send(
                CHAT,
                Method::POST,
                &format!("{}/fork", conversation(id)),
                body,
            )
            .await
        }
        ChatAction::Steer { run, message } => {
            let body = json!({"message": read_text_arg(message)?});
            rt.send(
                CHAT,
                Method::POST,
                &format!("/chat/runs/{}/steer", seg(run)),
                body,
            )
            .await
        }
        ChatAction::Status { run } => rt.get(CHAT, &format!("/chat/runs/{}", seg(run)), &[]).await,
        ChatAction::Grants { conversation } => {
            let query: Vec<(String, String)> = conversation
                .iter()
                .map(|id| ("conversationId".to_string(), id.clone()))
                .collect();
            rt.get(CHAT, "/chat/approval-grants", &query).await
        }
        ChatAction::Revoke {
            tool,
            scope,
            conversation,
        } => {
            let mut body = json!({"scope": scope.as_str(), "toolName": tool});
            if let (GrantScope::Conversation, Some(id)) = (scope, conversation) {
                body["conversationId"] = json!(id);
            }
            rt.send(CHAT, Method::DELETE, "/chat/approval-grants", body)
                .await
        }
        ChatAction::Capabilities => rt.get(CHAT, "/chat/capabilities", &[]).await,
        ChatAction::Rename { id, name } => {
            let body = json!({"conversationId": id, "conversationName": name});
            rt.send(CHAT, Method::POST, "/chat/conversations", body)
                .await
        }
        ChatAction::Approve {
            run,
            interrupt,
            deny,
            remember,
        } => {
            let decision = if *deny {
                crate::assistant::Decision::Deny
            } else {
                remember.decision()
            };
            let body = crate::assistant::session::server_answer(interrupt, decision);
            let path = format!("/chat/runs/{}/resume", seg(run));
            let resumed = rt
                .http
                .request_json_with(
                    CHAT,
                    Method::POST,
                    &path,
                    &[],
                    Some(body),
                    &stream::run_headers(&rt.http).await,
                )
                .await?;
            let run_id = resumed["runId"].as_str().unwrap_or(run);
            let outcome =
                stream::follow(rt, CHAT, &format!("/chat/runs/{}/events", seg(run_id))).await?;
            report_chat(rt, run_id, &outcome, None)
        }
        ChatAction::Run { body } => rt.call(CHAT, Method::POST, "/chat/runs", body, None).await,
        ChatAction::Budget => rt.get(CHAT, "/chat/budget-status", &[]).await,
        ChatAction::Metrics => rt.get(CHAT, "/chat/metrics", &[]).await,
        ChatAction::Upload { file } => upload(rt, file).await,
        ChatAction::Cancel { run } => {
            rt.send_empty(
                CHAT,
                Method::POST,
                &format!("/chat/runs/{}/cancel", seg(run)),
            )
            .await
        }
        ChatAction::Stop { conversation } => {
            rt.send(
                CHAT,
                Method::POST,
                "/chat/stop",
                json!({"conversationId": conversation}),
            )
            .await
        }
        ChatAction::Feedback { body } => {
            rt.call(CHAT, Method::POST, "/chat/feedback", body, None)
                .await
        }
        ChatAction::Enhance { prompt } => {
            let body = json!({"prompt": read_text_arg(prompt)?});
            rt.send(CHAT, Method::POST, "/chat/enhance-prompt", body)
                .await
        }
        ChatAction::AnalyseLogs { body } => {
            rt.call(CHAT, Method::POST, "/chat/analyze-logs", body, None)
                .await
        }
        ChatAction::Lineage { body } => {
            rt.call(CHAT, Method::POST, "/chat/data-lineage", body, None)
                .await
        }
    }
}

/// One id uses the single delete (404 when missing); several use the bulk
/// endpoint, which answers 200 even for ids it couldn't find — so report them.
async fn delete_conversations(rt: &Runtime, ids: &[String]) -> Result<()> {
    if let [id] = ids {
        rt.http
            .request_json(
                CHAT,
                Method::DELETE,
                &format!("/chat/conversations/{}", seg(id)),
                &[],
                None,
            )
            .await?;
        output::success(&format!("deleted conversation {id}"));
        return Ok(());
    }
    let body = json!({"conversationIds": ids});
    let value = rt
        .http
        .request_json(
            CHAT,
            Method::POST,
            "/chat/conversations/bulk-delete",
            &[],
            Some(body),
        )
        .await?;
    if rt.format != OutputFormat::Table {
        return rt.show(value).await;
    }
    output::success(&format!(
        "deleted {} conversation(s)",
        value["deletedCount"]
    ));
    for (key, what) in [
        ("notFoundConversationIds", "not found"),
        ("failedConversationIds", "could not be deleted"),
    ] {
        let missing: Vec<&str> = value[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if !missing.is_empty() {
            output::warn(&format!("{what}: {}", missing.join(", ")));
        }
    }
    Ok(())
}

fn fork_body(through: Option<&str>, exclude_selected: bool, name: Option<&str>) -> Value {
    let mut body = json!({});
    if let Some(timestamp) = through {
        body["throughTimestamp"] = json!(timestamp);
        body["excludeSelected"] = json!(exclude_selected);
    }
    if let Some(name) = name {
        body["conversationName"] = json!(name);
    }
    body
}

/// Sends one message and streams the reply. A new chat gets a fresh
/// conversation UUID; `--conversation` continues an existing one.
async fn ask(
    rt: &Runtime,
    prompt: &str,
    conversation: Option<&str>,
    title: Option<&str>,
) -> Result<()> {
    if prompt.trim().is_empty() {
        anyhow::bail!("the prompt is empty");
    }
    let is_new = conversation.is_none();
    let conversation_id = conversation.map(str::to_string).unwrap_or_else(new_uuid);
    let mut body = json!({
        "userInput": prompt,
        "conversationId": conversation_id,
        "assistantMode": "chat",
        "useRag": false,
        "supportsActionConfirmations": true,
    });
    if is_new {
        let name = title
            .map(str::to_string)
            .unwrap_or_else(|| default_title(prompt));
        body["conversationName"] = json!(name);
    }
    let run = rt
        .http
        .request_json_with(
            CHAT,
            Method::POST,
            "/chat/runs",
            &[],
            Some(body),
            &stream::run_headers(&rt.http).await,
        )
        .await?;
    let run_id = run["runId"]
        .as_str()
        .context("chat did not return a run id")?;
    let outcome = stream::follow(rt, CHAT, &format!("/chat/runs/{}/events", seg(run_id))).await?;
    let next = format!("continue: gz chat ask -c {conversation_id} \"…\"");
    report_chat(rt, run_id, &outcome, Some(&next))
}

fn default_title(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or(prompt).trim();
    if line.chars().count() <= TITLE_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(TITLE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

fn report_chat(rt: &Runtime, run_id: &str, outcome: &Outcome, next: Option<&str>) -> Result<()> {
    if let Some(interrupt) = &outcome.interrupt {
        let approval = stream::Approval::from_event(interrupt);
        let by = outcome
            .asked_by
            .as_deref()
            .map(|name| format!(" (asked by subagent {name})"))
            .unwrap_or_default();
        output::warn(&format!(
            "paused: approval needed for {}{by}",
            approval.summary.as_deref().unwrap_or(&approval.tool)
        ));
        output::info(&format!(
            "approve: gz chat approve {run_id} --interrupt '{}'   (add --deny to refuse)",
            approval.interrupt_id
        ));
        return Ok(());
    }
    report(rt, outcome, next)
}

fn report(rt: &Runtime, outcome: &Outcome, next: Option<&str>) -> Result<()> {
    if let Some(error) = &outcome.error {
        anyhow::bail!("{error}");
    }
    match outcome.status.as_deref() {
        Some(status) if stream::failure_reason(status, None).is_some() => {
            anyhow::bail!(
                "{}",
                stream::failure_reason(status, None).unwrap_or_default()
            )
        }
        Some("cancelled") => output::warn("the run was cancelled"),
        _ => {
            if let (Some(next), OutputFormat::Table, false) = (next, rt.format, rt.quiet) {
                eprintln!("{}", next.dimmed());
            }
        }
    }
    Ok(())
}

/// Prints `messages` as a readable transcript; false if the shape is unknown.
fn print_transcript(value: &Value) -> bool {
    let Some(messages) = value["messages"].as_array() else {
        return false;
    };
    if let Some(title) = value["conversationName"].as_str() {
        println!("{}\n", title.bold());
    }
    for message in messages {
        let role = message["speaker"]
            .as_str()
            .or(message["role"].as_str())
            .unwrap_or("assistant")
            .to_ascii_lowercase();
        let text = if message["message"].is_string() {
            message_text(&message["message"])
        } else {
            message_text(&message["content"])
        };
        if text.trim().is_empty() {
            continue;
        }
        let label = if role == "human" || role == "user" {
            "you".cyan().bold().to_string()
        } else {
            "gz".magenta().bold().to_string()
        };
        println!("{label}  {}\n", text.trim());
    }
    true
}

fn message_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str().or(b.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Presigned S3 upload: ask for a URL, PUT the bytes with the exact content
/// type the URL was signed for, then register the object with chat.
async fn upload(rt: &Runtime, file: &str) -> Result<()> {
    let reply = crate::assistant::attachments::upload(&rt.http, std::path::Path::new(file)).await?;
    rt.show(reply).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_body_uses_camel_case_fields() {
        assert_eq!(
            import_body(7, "keep_both", Some("copy")),
            json!({"projectId": 7, "conflictStrategy": "keep_both", "targetName": "copy"})
        );
        assert!(import_body(7, "fail", None).get("targetName").is_none());
    }

    #[test]
    fn fork_body_only_sends_what_was_asked() {
        assert_eq!(fork_body(None, false, None), json!({}));
        assert_eq!(
            fork_body(Some("2026-11-07T06:58:01Z"), true, Some("copy")),
            json!({"throughTimestamp": "2026-11-07T06:58:01Z", "excludeSelected": true, "conversationName": "copy"})
        );
    }

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(default_title("hello\nworld"), "hello");
        let long = "x".repeat(100);
        assert_eq!(default_title(&long).chars().count(), TITLE_CHARS);
    }

    #[test]
    fn reads_text_from_message_blocks() {
        assert_eq!(message_text(&json!("hi")), "hi");
        assert_eq!(
            message_text(&json!([{"text": "a"}, {"type": "tool"}, {"text": "b"}])),
            "a\nb"
        );
    }
}
