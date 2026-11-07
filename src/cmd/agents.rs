use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::{AgentsAction, ChatAction};
use crate::cmd::stream::{self, Outcome};
use crate::cmd::{new_uuid, read_text_arg, seg, Runtime};
use crate::output::{self, OutputFormat};

const AGENTS: &str = "agents";
const CHAT: &str = "chat";
const TITLE_CHARS: usize = 60;

pub async fn agents(rt: &Runtime, action: &AgentsAction) -> Result<()> {
    let agent = |id: &str, rest: &str| format!("/v1/agents/{}{rest}", seg(id));
    match action {
        AgentsAction::List { body } => rt.call(AGENTS, Method::GET, "/v1/agents", body, None).await,
        AgentsAction::Get { id } => rt.get(AGENTS, &agent(id, ""), &[]).await,
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
            rt.get(AGENTS, &agent(id, &format!("/runs/{}", seg(run))), &[])
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
    }
}

/// Starts the run in the background, then streams its events — the same
/// two steps the agent builder's preview pane takes.
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
        ChatAction::Delete { id } => rt.send_empty(CHAT, Method::DELETE, &conversation(id)).await,
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
            let body = json!({"interruptId": interrupt, "approved": !deny, "remember": remember});
            let path = format!("/chat/runs/{}/resume", seg(run));
            let resumed = rt
                .http
                .request_json_with(
                    CHAT,
                    Method::POST,
                    &path,
                    &[],
                    Some(body),
                    &run_headers(rt).await,
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

/// Headers the web app sends with every run start/resume: an idempotency
/// key and the refresh token for tools that outlive the ID token.
async fn run_headers(rt: &Runtime) -> Vec<(&'static str, String)> {
    let mut headers = vec![("X-Request-ID", new_uuid())];
    if let Some(refresh) = rt.http.refresh_token().await {
        headers.push(("X-Refresh-Token", refresh));
    }
    headers
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
            &run_headers(rt).await,
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
        let id = interrupt["interruptId"]
            .as_str()
            .or(interrupt["id"].as_str())
            .unwrap_or("<interrupt-id>");
        let what = interrupt["toolName"].as_str().unwrap_or("an action");
        output::warn(&format!("paused: approval needed for {what}"));
        output::info(&format!(
            "approve: gz chat approve {run_id} --interrupt {id}   (add --deny to refuse)"
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
        Some("failed" | "timed_out") => anyhow::bail!(
            "the run ended with status {}",
            outcome.status.as_deref().unwrap_or_default()
        ),
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
    let path = std::path::Path::new(file);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .context("the upload needs a file name")?;
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let query = [("filename".to_string(), name.clone())];
    let ticket = rt
        .http
        .request_json(
            CHAT,
            Method::POST,
            "/chat/upload/presigned-url",
            &query,
            None,
        )
        .await?;
    let upload_url = ticket["uploadUrl"]
        .as_str()
        .context("the server did not return an upload URL")?;
    let content_type = ticket["contentType"]
        .as_str()
        .context("the server did not say which content type it signed")?;
    rt.http.put_bytes(upload_url, content_type, bytes).await?;
    let done = json!({"s3Key": ticket["s3Key"], "bucket": ticket["bucket"]});
    rt.send(CHAT, Method::POST, "/chat/upload", done).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
