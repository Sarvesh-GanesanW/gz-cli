use reqwest::Method;
use serde_json::Value;

use crate::http::ApiClient;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchTarget {
    Module { screen: u8, kind: u8, depth: u8 },
    ModuleDetail { title: String },
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
        let value = client
            .request_json(
                &req.service,
                req.method.clone(),
                &req.path,
                &[],
                req.body.clone(),
            )
            .await;
        let done = FetchDone {
            id: req.id,
            target: req.target,
            label: req.label,
            result: value.map_err(|e| format!("{e:#}")),
        };
        let _ = tx.send(done);
    });
}
