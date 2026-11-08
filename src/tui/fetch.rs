//! Executes the app's requests off the UI thread and reports back.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::cmd::stream;
use crate::cmd::{new_uuid, seg};
use crate::http::ApiClient;
use crate::sse::{self, Event, Flow};
use crate::tui::chat::{self, ChatUpdate, RunRequest, Signal};
use crate::tui::clipboard;
use crate::tui::nav::ResourceId;
use crate::tui::screen::After;
use crate::tui::views::ViewId;
use crate::tui::watch::Feed;

/// Where a fetched answer lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchTarget {
    /// The grid of a resource view.
    Grid(ResourceId),
    /// The detail popup.
    Detail {
        title: String,
    },
    ChatList,
    ChatTranscript {
        conversation_id: String,
    },
    /// A full-screen view on the stack.
    View(ViewId),
    /// A row action's request.
    Action(ActionDone),
    /// Choices for a form's select field.
    FormChoices,
    /// Fire-and-forget work outside any view; only failures are shown.
    Background,
    /// The background check for news (`watch.rs`); failures stay quiet.
    Watch(Feed),
    /// A task a row action started outside any view: its message (or
    /// failure) is shown, then its screen and view are refreshed.
    TaskDone {
        resource: Option<ResourceId>,
        view: Option<ViewId>,
    },
}

/// How to report a finished row action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionDone {
    pub label: &'static str,
    /// Toast when the server sends no message of its own.
    pub success: &'static str,
    pub after: After,
    /// Singular noun for error messages ("connection").
    pub noun: &'static str,
    pub service: &'static str,
    pub resource: Option<ResourceId>,
    pub view: Option<ViewId>,
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

/// What a task's code returns: a JSON answer, or a failure in words.
pub type TaskFuture = Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;

type TaskFn = Arc<dyn Fn(ApiClient) -> TaskFuture + Send + Sync>;

/// Async work beyond one JSON request (a download to disk, a multipart
/// upload, a lakehouse session), run with the app's API client. Its answer
/// comes back like a fetch's, to `target`.
#[derive(Clone)]
pub struct Task {
    pub id: u64,
    pub target: FetchTarget,
    /// What it does, for tests and debugging ("download report.csv").
    pub label: String,
    run: TaskFn,
}

impl Task {
    pub fn new<F, Fut>(id: u64, target: FetchTarget, label: impl Into<String>, run: F) -> Self
    where
        F: Fn(ApiClient) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, String>> + Send + 'static,
    {
        Self {
            id,
            target,
            label: label.into(),
            run: Arc::new(move |client| Box::pin(run(client))),
        }
    }

    /// Runs the task's code (the executor does this off the UI thread).
    pub fn start(&self, client: ApiClient) -> TaskFuture {
        (self.run)(client)
    }
}

impl std::fmt::Debug for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Task")
            .field("id", &self.id)
            .field("target", &self.target)
            .field("label", &self.label)
            .finish()
    }
}

/// Work the app asks the event loop to do.
#[derive(Debug, Clone)]
pub enum Request {
    Fetch(FetchReq),
    StartChat(RunRequest),
    CancelChat {
        tag: u64,
        run_id: Option<String>,
    },
    /// Put text on the terminal's clipboard (OSC 52).
    Copy(String),
    Task(Task),
    /// Follow a run's server-sent events for a view.
    Stream(StreamReq),
    /// Stop following stream `id` (the view stopped the run).
    StopStream {
        id: u64,
    },
}

/// A run's event stream a view follows (`GET …/events`), reconnecting
/// after the last event id when the server asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamReq {
    pub id: u64,
    pub view: ViewId,
    pub service: String,
    pub path: String,
}

/// One step of a followed stream, as the view receives it.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
    Event(Event),
    /// The stream ended (`[DONE]`, or the server closed it for good).
    Ended,
    Failed(String),
}

/// Answers coming back to the app.
#[derive(Debug)]
pub enum Inbound {
    Fetched {
        id: u64,
        target: FetchTarget,
        result: Result<Value, String>,
    },
    Chat {
        tag: u64,
        update: ChatUpdate,
    },
    Streamed {
        id: u64,
        view: ViewId,
        item: StreamItem,
    },
}

/// Runs requests and remembers chat tasks so they can be cancelled.
pub struct Executor {
    client: ApiClient,
    tx: UnboundedSender<Inbound>,
    chats: HashMap<u64, JoinHandle<()>>,
    streams: HashMap<u64, JoinHandle<()>>,
}

impl Executor {
    pub fn new(client: ApiClient, tx: UnboundedSender<Inbound>) -> Self {
        Self {
            client,
            tx,
            chats: HashMap::new(),
            streams: HashMap::new(),
        }
    }

    pub fn run(&mut self, request: Request) {
        match request {
            Request::Fetch(req) => self.fetch(req),
            Request::StartChat(req) => {
                let (client, tx, tag) = (self.client.clone(), self.tx.clone(), req.tag);
                let handle = tokio::spawn(async move {
                    if let Err(err) = stream_chat(&client, &req, &tx).await {
                        let _ = tx.send(Inbound::Chat {
                            tag,
                            update: ChatUpdate::Failed(format!("{err:#}")),
                        });
                    }
                });
                self.chats.retain(|_, h| !h.is_finished());
                self.chats.insert(tag, handle);
            }
            Request::CancelChat { tag, run_id } => self.cancel(tag, run_id),
            Request::Copy(text) => clipboard::copy(&text),
            Request::Task(task) => self.task(task),
            Request::Stream(req) => self.follow(req),
            Request::StopStream { id } => {
                if let Some(handle) = self.streams.remove(&id) {
                    handle.abort();
                }
            }
        }
    }

    fn follow(&mut self, req: StreamReq) {
        let (client, tx) = (self.client.clone(), self.tx.clone());
        let (id, view) = (req.id, req.view);
        let handle = tokio::spawn(async move {
            let send = |item| {
                let _ = tx.send(Inbound::Streamed { id, view, item });
            };
            let mut last_id = None;
            let result = stream::read_run(&client, &req.service, &req.path, &mut last_id, {
                |event, _| {
                    send(StreamItem::Event(event.clone()));
                    Ok(Flow::Continue)
                }
            })
            .await;
            send(match result {
                Ok(()) => StreamItem::Ended,
                Err(err) => StreamItem::Failed(format!("{err:#}")),
            });
        });
        self.streams.retain(|_, h| !h.is_finished());
        self.streams.insert(id, handle);
    }

    fn task(&self, task: Task) {
        let (client, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let result = task.start(client).await;
            let _ = tx.send(Inbound::Fetched {
                id: task.id,
                target: task.target,
                result,
            });
        });
    }

    fn fetch(&self, req: FetchReq) {
        let (client, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let result = client
                .request_json(&req.service, req.method, &req.path, &[], req.body)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Inbound::Fetched {
                id: req.id,
                target: req.target,
                result,
            });
        });
    }

    /// Stops reading the stream now and asks the server to stop the run.
    fn cancel(&mut self, tag: u64, run_id: Option<String>) {
        if let Some(handle) = self.chats.remove(&tag) {
            handle.abort();
        }
        let (client, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            if let Some(run_id) = run_id {
                let path = format!("/chat/runs/{}/cancel", seg(&run_id));
                let _ = client
                    .request_json("chat", Method::POST, &path, &[], None)
                    .await;
            }
            let _ = tx.send(Inbound::Chat {
                tag,
                update: ChatUpdate::Finished {
                    status: Some("cancelled".to_string()),
                },
            });
        });
    }
}

/// Starts a run like `gz chat ask` (fresh `X-Request-ID`, the refresh
/// token for long tool calls), then streams its events, reconnecting after
/// the last event id when the server asks.
async fn stream_chat(
    client: &ApiClient,
    req: &RunRequest,
    tx: &UnboundedSender<Inbound>,
) -> Result<()> {
    let send = |update| {
        let _ = tx.send(Inbound::Chat {
            tag: req.tag,
            update,
        });
    };
    let mut headers = vec![("X-Request-ID", new_uuid())];
    if let Some(refresh) = client.refresh_token().await {
        headers.push(("X-Refresh-Token", refresh));
    }
    let run = client
        .request_json_with(
            "chat",
            Method::POST,
            "/chat/runs",
            &[],
            Some(req.body()),
            &headers,
        )
        .await?;
    let run_id = run["runId"]
        .as_str()
        .context("chat did not return a run id")?
        .to_string();
    send(ChatUpdate::Started {
        run_id: run_id.clone(),
    });
    let events = format!("/chat/runs/{}/events", seg(&run_id));
    let mut last_id: Option<String> = None;
    loop {
        let query: Vec<(String, String)> = last_id
            .iter()
            .map(|id| ("after".into(), id.clone()))
            .collect();
        let response = client
            .stream("chat", Method::GET, &events, &query, None)
            .await?;
        let mut reconnect = false;
        let mut ended = false;
        last_id = sse::read(response, last_id, |event| {
            Ok(match chat::interpret(event) {
                Signal::Update(update) => {
                    let stop = matches!(update, ChatUpdate::Failed(_) | ChatUpdate::Paused(_));
                    ended |= stop;
                    send(update);
                    if stop {
                        Flow::Stop
                    } else {
                        Flow::Continue
                    }
                }
                Signal::Reconnect => {
                    reconnect = true;
                    Flow::Stop
                }
                Signal::Done => Flow::Stop,
                Signal::Ignore => Flow::Continue,
            })
        })
        .await?;
        if ended {
            return Ok(());
        }
        if !reconnect {
            break;
        }
    }
    send(ChatUpdate::Finished { status: None });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_stub;

    #[tokio::test]
    async fn tasks_run_with_the_client_and_answer_like_fetches() {
        let (base, server) = test_stub::serve(vec![test_stub::Reply::json(r#"{"ok":1}"#)]).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut executor = Executor::new(test_stub::client(&base), tx);
        let task = Task::new(
            7,
            FetchTarget::Background,
            "probe",
            |client: ApiClient| async move {
                let answer = client
                    .request_json("data", Method::GET, "/probe", &[], None)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::json!({"wrapped": answer}))
            },
        );
        executor.run(Request::Task(task));
        let Some(Inbound::Fetched { id, target, result }) = rx.recv().await else {
            panic!("the executor should report back");
        };
        assert_eq!((id, target), (7, FetchTarget::Background));
        assert_eq!(result.unwrap()["wrapped"]["ok"], 1);
        assert!(server.await.unwrap()[0].starts_with("GET /probe HTTP/1.1"));
    }
}
