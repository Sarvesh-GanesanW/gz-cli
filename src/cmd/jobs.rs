use anyhow::Result;
use reqwest::Method;
use serde_json::json;

use crate::cli::{ExternalAction, JobsAction, OntologyAction};
use crate::cmd::{object_body, Runtime};

const SVC: &str = "etl";

pub async fn run(rt: &Runtime, action: &JobsAction) -> Result<()> {
    match action {
        JobsAction::Submit { body } => rt.call(SVC, Method::POST, "/etl/job", body, None).await,
        JobsAction::Abort {
            execution,
            session,
            job_id,
        } => {
            let mut pairs = vec![
                ("action", json!("Abort")),
                ("jobIdentifier", json!(execution)),
            ];
            if let Some(session) = session {
                pairs.push(("sessionId", json!(session)));
            }
            if let Some(job_id) = job_id {
                pairs.push(("jobId", json!(job_id)));
            }
            let value = rt
                .http
                .request_json(SVC, Method::POST, "/etl/job", &[], Some(object_body(pairs)))
                .await?;
            rt.show(value).await
        }
        JobsAction::Status { job } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::POST,
                    "/etl/jobStatus",
                    &[],
                    Some(object_body(vec![("jobIdentifier", json!(job))])),
                )
                .await?;
            rt.show(value).await
        }
        JobsAction::Logs {
            job,
            session,
            next_token,
            body,
        } => {
            let mut pairs = vec![("jobIdentifier", json!(job))];
            if let Some(session) = session {
                pairs.push(("sessionId", json!(session)));
            }
            if let Some(token) = next_token {
                pairs.push(("nextToken", json!(token)));
            }
            rt.call(
                SVC,
                Method::POST,
                "/etl/logs",
                body,
                Some(object_body(pairs)),
            )
            .await
        }
        JobsAction::EksLogs { body } => {
            rt.call(SVC, Method::POST, "/etl/ekslogs", body, None).await
        }
        JobsAction::Schedule { body } => {
            rt.call(SVC, Method::POST, "/etl/submitScheduledJob", body, None)
                .await
        }
        JobsAction::Read { body } => rt.call(SVC, Method::POST, "/etl/getdata", body, None).await,
        JobsAction::Metrics { body } => {
            rt.call(SVC, Method::GET, "/etl/consumption-metrics", body, None)
                .await
        }
        JobsAction::ExecutionMetrics { execution } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/etl/consumption-metrics/{execution}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        JobsAction::ProjectMetrics => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    "/etl/consumption-metrics/aggregates/projects",
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        JobsAction::Insights { job_id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/etl/consumption-metrics/jobs/{job_id}/insights"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        JobsAction::Runtime { body } => {
            rt.call(SVC, Method::POST, "/etl/observability/runtime", body, None)
                .await
        }
        JobsAction::Collect { execution, body } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("/etl/observability/executions/{execution}/collect"),
                body,
                None,
            )
            .await
        }
        JobsAction::LineageHealth => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/etl/lineage/health", &[], None)
                .await?;
            rt.show(value).await
        }
        JobsAction::LineageJob { job_id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/etl/lineage/jobs/{job_id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        JobsAction::LineageDataset { dataset_id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/etl/lineage/datasets/{dataset_id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        JobsAction::LineageAssets { body } => {
            rt.call(SVC, Method::GET, "/etl/lineage/assets", body, None)
                .await
        }
        JobsAction::IngestLineage { body } => {
            rt.call(SVC, Method::POST, "/etl/lineage/catalog-events", body, None)
                .await
        }
        JobsAction::Ontology { action } => ontology(rt, action).await,
        JobsAction::External { action } => external(rt, action).await,
    }
}

async fn ontology(rt: &Runtime, action: &OntologyAction) -> Result<()> {
    match action {
        OntologyAction::List => {
            let value = rt
                .http
                .request_json(SVC, Method::GET, "/etl/ontology/models", &[], None)
                .await?;
            rt.show(value).await
        }
        OntologyAction::Get { id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/etl/ontology/models/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        OntologyAction::Delete { id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::DELETE,
                    &format!("/etl/ontology/models/{id}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        OntologyAction::Usage { id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/etl/ontology/models/{id}/usage"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        OntologyAction::Calculate { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/etl/ontology/models/calculate",
                body,
                None,
            )
            .await
        }
        OntologyAction::Releases { id } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/etl/ontology/models/{id}/releases"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
        OntologyAction::Release { id, body } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("/etl/ontology/models/{id}/releases"),
                body,
                None,
            )
            .await
        }
        OntologyAction::Citations { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/etl/ontology/citation-events",
                body,
                None,
            )
            .await
        }
        OntologyAction::Resolve { body } => {
            rt.call(
                SVC,
                Method::POST,
                "/etl/ontology/context:resolve",
                body,
                None,
            )
            .await
        }
        OntologyAction::Define { id, body } => {
            rt.call(
                SVC,
                Method::POST,
                &format!("/etl/ontology/models/{id}/definitions"),
                body,
                None,
            )
            .await
        }
        OntologyAction::PatchDefinition {
            id,
            definition,
            body,
        } => {
            rt.call(
                SVC,
                Method::PATCH,
                &format!("/etl/ontology/models/{id}/definitions/{definition}"),
                body,
                None,
            )
            .await
        }
        OntologyAction::PatchDefinitions { id, body } => {
            rt.call(
                SVC,
                Method::PATCH,
                &format!("/etl/ontology/models/{id}/definitions"),
                body,
                None,
            )
            .await
        }
        OntologyAction::ReadRelease { id, release } => {
            let value = rt
                .http
                .request_json(
                    SVC,
                    Method::GET,
                    &format!("/etl/ontology/models/{id}/releases/{release}"),
                    &[],
                    None,
                )
                .await?;
            rt.show(value).await
        }
    }
}

async fn external(rt: &Runtime, action: &ExternalAction) -> Result<()> {
    match action {
        ExternalAction::Submit { body } => {
            rt.call(SVC, Method::POST, "/external/etl/job", body, None)
                .await
        }
        ExternalAction::Status { body } => {
            rt.call(SVC, Method::GET, "/external/etl/job/status", body, None)
                .await
        }
        ExternalAction::Logs { body } => {
            rt.call(SVC, Method::GET, "/external/etl/job/logs", body, None)
                .await
        }
    }
}
