//! The lakehouse calls behind the SQL shell, verified against
//! groundzero-iceberg-provider-api (app_routes/sessions.py,
//! session_state.py, lakehouse_queries.py) and the web app's SQL editor
//! (ground-zero/src/pages/Utils/etlUtlis.js, lakehouseQueryApi.js,
//! pages/lakehouse/TabsContainer/lakehouseStatementRun.js).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cmd::seg;
use crate::http::ApiClient;
use crate::sql::results::{self, Page, Reply};

const SVC: &str = "lakehouse";
/// Starting a session waits for its Spark pod, often a minute or more.
const START_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Rows fetched per page in the shell.
pub const PAGE_SIZE: u32 = 100;
const POLL_FAST: Duration = Duration::from_millis(500);
const POLL_SLOW: Duration = Duration::from_secs(2);
const FAST_POLLS: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warehouse {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compute {
    pub id: i64,
    pub name: String,
    pub description: String,
}

/// The query running right now, so a cancel from the keyboard can name it.
pub type RunningQuery = Arc<Mutex<Option<String>>>;

/// What a statement produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub query_id: Option<String>,
    pub reply: Reply,
    pub elapsed: Duration,
}

#[derive(Clone)]
pub struct Lakehouse {
    http: ApiClient,
}

fn list_of(value: &Value) -> Vec<Value> {
    crate::output::tabular(value)
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn id_of(row: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| match &row[k] {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    })
}

impl Lakehouse {
    pub fn new(http: ApiClient) -> Self {
        Self { http }
    }

    pub async fn warehouses(&self) -> Result<Vec<Warehouse>> {
        let value = self
            .http
            .request_json(SVC, Method::GET, "/lakehouse/warehouse", &[], None)
            .await?;
        Ok(list_of(&value)
            .iter()
            .filter_map(|row| {
                Some(Warehouse {
                    id: id_of(row, &["id", "Id"])?,
                    name: row["name"].as_str()?.to_string(),
                })
            })
            .collect())
    }

    pub async fn computes(&self) -> Result<Vec<Compute>> {
        let value = self
            .http
            .request_json("rtes", Method::GET, "/rte/compute", &[], None)
            .await?;
        Ok(list_of(&value)
            .iter()
            .filter_map(|row| {
                Some(Compute {
                    id: id_of(row, &["Id", "id"])?,
                    name: row["Name"].as_str().or(row["name"].as_str())?.to_string(),
                    description: row["Description"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect())
    }

    /// `Starting`, `Running`, `Stopping`, or `Stopped`.
    pub async fn session_state(&self, session: &str) -> Result<String> {
        let path = format!("/lakehouse/sessionstate/{}", seg(session));
        let value = self
            .http
            .request_json(SVC, Method::GET, &path, &[], None)
            .await?;
        Ok(value["response"]["sessionStatus"]
            .as_str()
            .unwrap_or("Stopped")
            .to_string())
    }

    /// Starts a session; the server answers once the session is ready.
    pub async fn start(&self, compute: i64, warehouse: i64) -> Result<String> {
        let body = json!({"computeId": compute, "warehouseId": warehouse});
        let value = self
            .http
            .request_json_timeout(
                SVC,
                Method::POST,
                "/lakehouse/startsession",
                Some(body),
                START_TIMEOUT,
            )
            .await
            .context("cannot start a lakehouse session")?;
        value["response"]["sessionId"]
            .as_str()
            .map(str::to_string)
            .context("the session started without an id")
    }

    pub async fn stop(&self, session: &str) -> Result<()> {
        let body = json!({"sessionId": session});
        self.http
            .request_json(SVC, Method::POST, "/lakehouse/stopsession", &[], Some(body))
            .await
            .map(|_| ())
    }

    /// Runs one statement: submit, wait while it runs, read its first page.
    pub async fn run(
        &self,
        session: &str,
        warehouse: &str,
        statement: &str,
        running: &RunningQuery,
    ) -> Result<Outcome> {
        let started = Instant::now();
        let body = json!({
            "query": statement,
            "catalog": warehouse,
            "sessionId": session,
            "pageSize": PAGE_SIZE,
            "pageNumber": 1,
            "asyncQuery": true,
            // The session keeps the result so later pages and \o can read it
            // (the web app's editor asks the same way); rows still come as
            // JSON from /queryresults.
            "resultFormat": "arrow",
        });
        let value = self
            .http
            .request_json(SVC, Method::POST, "/lakehouse/submitquery", &[], Some(body))
            .await?;
        let query_id = value["queryExecutionId"].as_str().map(str::to_string);
        let mut reply = results::parse_reply(&value["response"]);
        if matches!(reply, Reply::Running | Reply::Stored) {
            let id = query_id
                .clone()
                .context("the session kept the query but gave no id")?;
            if reply == Reply::Running {
                set_running(running, Some(id.clone()));
                let waited = self.wait(session, &id).await;
                set_running(running, None);
                waited?;
            }
            reply = Reply::Rows(self.page(session, &id, warehouse, 1).await?);
        }
        Ok(Outcome {
            query_id,
            reply,
            elapsed: started.elapsed(),
        })
    }

    /// Polls a running query until it finishes; errors if it fails or is
    /// cancelled.
    async fn wait(&self, session: &str, query: &str) -> Result<()> {
        let path = format!("/lakehouse/querystatus/{}/{}", seg(session), seg(query));
        for poll in 0.. {
            tokio::time::sleep(if poll < FAST_POLLS {
                POLL_FAST
            } else {
                POLL_SLOW
            })
            .await;
            let value = self
                .http
                .request_json(SVC, Method::GET, &path, &[], None)
                .await?;
            let status = &value["response"];
            match status["status"].as_str().unwrap_or_default() {
                "SUCCEEDED" => return Ok(()),
                "FAILED" => bail!("{}", status["error"].as_str().unwrap_or("the query failed")),
                "CANCELLED" => bail!("Query cancelled"),
                _ => {}
            }
        }
        unreachable!("the poll loop only ends by returning")
    }

    /// One page of a finished query's result.
    pub async fn page(
        &self,
        session: &str,
        query: &str,
        warehouse: &str,
        number: u32,
    ) -> Result<Page> {
        let path = format!("/lakehouse/queryresults/{}/{}", seg(session), seg(query));
        let params = [
            ("pageNumber".to_string(), number.to_string()),
            ("pageSize".to_string(), PAGE_SIZE.to_string()),
            ("catalog".to_string(), warehouse.to_string()),
        ];
        let value = self
            .http
            .request_json(SVC, Method::GET, &path, &params, None)
            .await
            .map_err(|err| {
                if err.to_string().contains("CHUNKS_REQUIRED") {
                    anyhow::anyhow!(
                        "this result is larger than the session keeps in memory; use `gz sql export` for all rows"
                    )
                } else {
                    err
                }
            })?;
        Ok(results::parse_page(&value["response"]))
    }

    pub async fn cancel(&self, session: &str, query: &str) -> Result<()> {
        let path = format!("/lakehouse/cancelquery/{}/{}", seg(session), seg(query));
        self.http
            .request_json(SVC, Method::POST, &path, &[], None)
            .await
            .map(|_| ())
    }
}

fn set_running(running: &RunningQuery, query: Option<String>) {
    if let Ok(mut slot) = running.lock() {
        *slot = query;
    }
}

/// Finds a warehouse by name (case-insensitive) or id.
pub fn pick_warehouse<'a>(list: &'a [Warehouse], wanted: &str) -> Option<&'a Warehouse> {
    list.iter()
        .find(|w| w.name.eq_ignore_ascii_case(wanted) || w.id.to_string() == wanted)
}

/// Finds a compute size by name (case-insensitive) or id.
pub fn pick_compute<'a>(list: &'a [Compute], wanted: &str) -> Option<&'a Compute> {
    list.iter()
        .find(|c| c.name.eq_ignore_ascii_case(wanted) || c.id.to_string() == wanted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-request HTTP stub; returns its base URL and the request line.
    async fn one_shot(reply: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            head.lines().next().unwrap_or_default().to_string()
        });
        (base, handle)
    }

    fn lake_at(base: &str) -> Lakehouse {
        let config = crate::config::Config::default();
        let resolved = crate::config::resolve(
            &config,
            Some("t"),
            Some(base.to_string()),
            Some("tok".into()),
        );
        let http = ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5).unwrap();
        Lakehouse::new(http)
    }

    #[tokio::test]
    async fn cancel_posts_to_the_query_path() {
        let (base, request) = one_shot(r#"{"status":200,"response":{"status":"CANCELLED"}}"#).await;
        lake_at(&base).cancel("s1", "q 1").await.unwrap();
        assert_eq!(
            request.await.unwrap(),
            "POST /lakehouse/cancelquery/s1/q%201 HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn session_state_reads_the_status_field() {
        let (base, request) =
            one_shot(r#"{"status":200,"response":{"sessionId":"s1","sessionStatus":"Starting"}}"#)
                .await;
        assert_eq!(
            lake_at(&base).session_state("s1").await.unwrap(),
            "Starting"
        );
        assert!(request
            .await
            .unwrap()
            .starts_with("GET /lakehouse/sessionstate/s1 "));
    }

    #[test]
    fn picks_by_name_or_id() {
        let warehouses = vec![
            Warehouse {
                id: 1108,
                name: "GZWarehousePROD".into(),
            },
            Warehouse {
                id: 1114,
                name: "Lineage".into(),
            },
        ];
        assert_eq!(
            pick_warehouse(&warehouses, "gzwarehouseprod").unwrap().id,
            1108
        );
        assert_eq!(pick_warehouse(&warehouses, "1114").unwrap().name, "Lineage");
        assert!(pick_warehouse(&warehouses, "nope").is_none());
        let computes = vec![Compute {
            id: 1000,
            name: "mini".into(),
            description: "Mini".into(),
        }];
        assert_eq!(pick_compute(&computes, "MINI").unwrap().id, 1000);
    }
}
