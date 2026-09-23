use reqwest::Method;
use serde_json::Value;

use crate::http::ApiClient;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchTarget {
    Warehouses,
    Namespaces,
    NamespaceSchema,
    TableDescribe,
    TableSample,
    SqlRun,
    JobStatus,
    JobLogs,
    JobSubmit,
    Files,
    Folders,
    FileDownload { dest: String },
    Agents,
    AgentRuns,
    RunDetail,
    Schedules,
    ScheduleDetail,
}

#[derive(Debug, Clone)]
pub struct FetchReq {
    pub id: u64,
    pub target: FetchTarget,
    pub label: String,
    pub service: String,
    pub method: Method,
    pub path: String,
    pub body: Option<Value>,
}

#[derive(Debug)]
pub struct FetchDone {
    pub id: u64,
    pub target: FetchTarget,
    pub label: String,
    pub result: Result<Value, String>,
}

pub fn spawn(client: &ApiClient, req: FetchReq, tx: tokio::sync::mpsc::UnboundedSender<FetchDone>) {
    let client = client.clone();
    tokio::spawn(async move {
        let value = if matches!(req.target, FetchTarget::FileDownload { .. }) {
            download_to_value(&client, &req).await
        } else {
            client
                .request_json(
                    &req.service,
                    req.method.clone(),
                    &req.path,
                    &[],
                    req.body.clone(),
                )
                .await
        };
        let done = FetchDone {
            id: req.id,
            target: req.target,
            label: req.label,
            result: value.map_err(|e| format!("{e:#}")),
        };
        let _ = tx.send(done);
    });
}

async fn download_to_value(client: &ApiClient, req: &FetchReq) -> anyhow::Result<Value> {
    let dest = match &req.target {
        FetchTarget::FileDownload { dest } => dest.clone(),
        _ => return Ok(Value::Null),
    };
    let bytes = client
        .download(
            &req.service,
            &req.path,
            &[],
            req.body.clone(),
            std::path::Path::new(&dest),
        )
        .await?;
    Ok(serde_json::json!({"saved": dest, "bytes": bytes}))
}
