use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::Value;

use crate::cli::{AgentsAction, ChatAction};
use crate::cmd::Runtime;

fn guess_content_type(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
}

pub async fn agents(rt: &Runtime, action: &AgentsAction) -> Result<()> {
    match action {
        AgentsAction::List { body } => {
            rt.call("agents", Method::GET, "/v1/agents", body, None)
                .await
        }
        AgentsAction::Get { id } => {
            let value = rt
                .http
                .request_json(
                    "agents",
                    Method::GET,
                    &format!("/v1/agents/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        AgentsAction::Create { body } => {
            rt.call("agents", Method::POST, "/v1/agents", body, None)
                .await
        }
        AgentsAction::Update { id, body } => {
            rt.call(
                "agents",
                Method::PATCH,
                &format!("/v1/agents/{id}"),
                body,
                None,
            )
            .await
        }
        AgentsAction::Delete { id } => {
            let value = rt
                .http
                .request_json(
                    "agents",
                    Method::DELETE,
                    &format!("/v1/agents/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        AgentsAction::Test { body } => {
            rt.call("agents", Method::POST, "/v1/agents/test", body, None)
                .await
        }
        AgentsAction::Runs { id, body } => {
            rt.call(
                "agents",
                Method::GET,
                &format!("/v1/agents/{id}/runs"),
                body,
                None,
            )
            .await
        }
        AgentsAction::Run { id, run } => {
            let value = rt
                .http
                .request_json(
                    "agents",
                    Method::GET,
                    &format!("/v1/agents/{id}/runs/{run}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        AgentsAction::Events { id, run } => {
            let value = rt
                .http
                .request_json(
                    "agents",
                    Method::GET,
                    &format!("/v1/agents/{id}/runs/{run}/events"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        AgentsAction::Observability { id, run } => {
            let value = rt
                .http
                .request_json(
                    "agents",
                    Method::GET,
                    &format!("/v1/agents/{id}/runs/{run}/observability"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        AgentsAction::Start { id, body } => {
            rt.call(
                "agents",
                Method::POST,
                &format!("/v1/agents/{id}/runs"),
                body,
                None,
            )
            .await
        }
        AgentsAction::Resume { id, run, body } => {
            rt.call(
                "agents",
                Method::POST,
                &format!("/v1/agents/{id}/runs/{run}/resume"),
                body,
                None,
            )
            .await
        }
        AgentsAction::Sessions { id } => {
            let value = rt
                .http
                .request_json(
                    "agents",
                    Method::GET,
                    &format!("/v1/agents/{id}/sessions"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        AgentsAction::Messages { id, session } => {
            let value = rt
                .http
                .request_json(
                    "agents",
                    Method::GET,
                    &format!("/v1/agents/{id}/sessions/{session}/messages"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        AgentsAction::Approvals { id } => {
            let value = rt
                .http
                .request_json(
                    "agents",
                    Method::GET,
                    &format!("/v1/agents/{id}/approvals"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        AgentsAction::ApprovalRequests { body } => {
            rt.call("agents", Method::GET, "/v1/approval-requests", body, None)
                .await
        }
        AgentsAction::Decide { approval, body } => {
            rt.call(
                "agents",
                Method::POST,
                &format!("/v1/approval-requests/{approval}/decision"),
                body,
                None,
            )
            .await
        }
        AgentsAction::Publications => {
            let value = rt
                .http
                .request_json("agents", Method::GET, "/v1/publications", &[], None)
                .await?;
            rt.show(value).await
        }
        AgentsAction::Quota { id } => {
            let value = rt
                .http
                .request_json(
                    "agents",
                    Method::GET,
                    &format!("/v1/agents/{id}/quota"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        AgentsAction::SetQuota { id, body } => {
            rt.call(
                "agents",
                Method::PUT,
                &format!("/v1/agents/{id}/quota"),
                body,
                None,
            )
            .await
        }
        AgentsAction::Tags { id } => {
            let value = rt
                .http
                .request_json(
                    "agents",
                    Method::GET,
                    &format!("/v1/agents/{id}/tags"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        AgentsAction::Tag { id, body } => {
            rt.call(
                "agents",
                Method::POST,
                &format!("/v1/agents/{id}/tags"),
                body,
                None,
            )
            .await
        }
        AgentsAction::Tools => {
            let value = rt
                .http
                .request_json("agents", Method::GET, "/v1/tools", &[], None)
                .await?;
            rt.show(value).await
        }
        AgentsAction::Models => {
            let value = rt
                .http
                .request_json("agents", Method::GET, "/v1/models", &[], None)
                .await?;
            rt.show(value).await
        }
        AgentsAction::Metrics { body } => {
            rt.call("agents", Method::GET, "/v1/agents/metrics", body, None)
                .await
        }
    }
}

pub async fn chat(rt: &Runtime, action: &ChatAction) -> Result<()> {
    match action {
        ChatAction::Run { body } => {
            rt.call("chat", Method::POST, "/chat/runs", body, None)
                .await
        }
        ChatAction::Cancel { run, body } => {
            rt.call(
                "chat",
                Method::POST,
                &format!("/chat/runs/{run}/cancel"),
                body,
                None,
            )
            .await
        }
        ChatAction::Stop { body } => {
            rt.call("chat", Method::POST, "/chat/stop", body, None)
                .await
        }
        ChatAction::NewConversation { body } => {
            rt.call("chat", Method::POST, "/chat/conversations", body, None)
                .await
        }
        ChatAction::Feedback { body } => {
            rt.call("chat", Method::POST, "/chat/feedback", body, None)
                .await
        }
        ChatAction::Enhance { body } => {
            rt.call("chat", Method::POST, "/chat/enhance-prompt", body, None)
                .await
        }
        ChatAction::AnalyseLogs { body } => {
            rt.call("chat", Method::POST, "/chat/analyze-logs", body, None)
                .await
        }
        ChatAction::Lineage { body } => {
            rt.call("chat", Method::POST, "/chat/data-lineage", body, None)
                .await
        }
        ChatAction::Conversations { body } => {
            rt.call(
                "chat",
                Method::GET,
                "/chat/conversations?assistantMode=chat",
                body,
                None,
            )
            .await
        }
        ChatAction::Conversation { id } => {
            let value = rt
                .http
                .request_json(
                    "chat",
                    Method::GET,
                    &format!("/chat/conversations/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        ChatAction::DeleteConversation { id } => {
            let body = crate::cmd::object_body(vec![("conversationIds", serde_json::json!([id]))]);
            let value = rt
                .http
                .request_json(
                    "chat",
                    Method::POST,
                    "/chat/conversations/bulk-delete",
                    &[],
                    Some(body),
                )
                .await?;
            rt.show(value).await
        }
        ChatAction::Rename { id, name } => {
            let body = crate::cmd::object_body(vec![
                ("conversationId", serde_json::json!(id)),
                ("conversationName", serde_json::json!(name)),
            ]);
            let value = rt
                .http
                .request_json("chat", Method::POST, "/chat/conversations", &[], Some(body))
                .await?;
            rt.show(value).await
        }
        ChatAction::Budget => {
            let value = rt
                .http
                .request_json("chat", Method::GET, "/chat/budget-status", &[], None)
                .await?;
            rt.show(value).await
        }
        ChatAction::Metrics => {
            let value = rt
                .http
                .request_json("chat", Method::GET, "/chat/metrics", &[], None)
                .await?;
            rt.show(value).await
        }
        ChatAction::Upload { file, body } => {
            let _ = body;
            let path = std::path::Path::new(file);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "upload.bin".to_string());
            let bytes =
                std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
            let ticket = rt
                .http
                .request_json(
                    "chat",
                    Method::POST,
                    &format!("/chat/upload/presigned-url?filename={name}"),
                    &[],
                    None,
                )
                .await?;
            let upload_url = ticket
                .get("uploadUrl")
                .and_then(|v| v.as_str())
                .with_context(|| format!("no uploadUrl in presigned response: {ticket}"))?
                .to_string();
            let (s3_key, bucket) = (
                ticket.get("s3Key").cloned().unwrap_or(Value::Null),
                ticket.get("bucket").cloned().unwrap_or(Value::Null),
            );
            rt.http
                .put_bytes(&upload_url, guess_content_type(&name), bytes)
                .await?;
            let done = crate::cmd::object_body(vec![("s3Key", s3_key), ("bucket", bucket)]);
            let value = rt
                .http
                .request_json("chat", Method::POST, "/chat/upload", &[], Some(done))
                .await?;
            rt.show(value).await
        }
    }
}
