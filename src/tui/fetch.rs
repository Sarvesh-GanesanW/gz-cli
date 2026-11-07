use reqwest::Method;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::http::ApiClient;

/// Where an answer lands: a module's grid, or the detail popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchTarget {
    Module { screen: usize },
    Detail { title: String },
}

#[derive(Debug, Clone)]
pub struct FetchReq {
    pub id: u64,
    pub target: FetchTarget,
    pub service: String,
    pub method: Method,
    pub path: String,
    pub body: Option<Value>,
}

#[derive(Debug)]
pub struct FetchDone {
    pub id: u64,
    pub target: FetchTarget,
    pub result: Result<Value, String>,
}

pub fn spawn(client: &ApiClient, req: FetchReq, tx: UnboundedSender<FetchDone>) {
    let client = client.clone();
    tokio::spawn(async move {
        let result = client
            .request_json(&req.service, req.method, &req.path, &[], req.body)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(FetchDone {
            id: req.id,
            target: req.target,
            result,
        });
    });
}
