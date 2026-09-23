use anyhow::Result;
use reqwest::Method;

use crate::cli::{AgentsAction, ChatAction};
use crate::cmd::Runtime;

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
    }
}
