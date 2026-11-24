//! MCP over HTTP: the Streamable HTTP transport (one endpoint; each message
//! is a POST answered with JSON or an event stream, plus an optional GET
//! stream for the server's own messages) and the older HTTP+SSE transport
//! (a GET event stream that names a POST endpoint and carries every
//! answer). Remote servers get the user's configured headers, and an OAuth
//! bearer token when the user signed in with `gz mcp login`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::client::{server_request_answer, RpcError, MAX_MESSAGE_BYTES};
use super::config::Remote;
use super::oauth::{self, AuthStore};
use crate::sse;

const SESSION_HEADER: &str = "Mcp-Session-Id";
const PROTOCOL_HEADER: &str = "MCP-Protocol-Version";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Said when a server refuses Streamable HTTP's first request outright.
const LEGACY_HINT: &str = "if the server uses the older HTTP+SSE transport, set type = \"sse\"";
/// How often the server-message stream is reopened after it drops, in a
/// row, before gz stops listening (tool calls still work without it).
const MAX_LISTEN_RETRIES: u32 = 5;
const LISTEN_RETRY_DELAY: Duration = Duration::from_secs(2);
/// How long gz waits for a server to take a message that has no answer
/// (a notification, or gz's answer to a server's ping): a server that
/// never responds must not hold up the call that sent it.
const NOTIFICATION_WAIT: Duration = Duration::from_secs(5);
/// Redirects followed within the server's own origin.
const MAX_REDIRECTS: usize = 5;

/// The HTTP client for a remote MCP server and its sign-in. Redirects are
/// followed only within the same origin, since every request carries the
/// server's configured headers; a redirect elsewhere comes back as the 3xx
/// response, for [`redirect_refused`] to explain. `total` bounds whole
/// requests, for callers that read no streams.
pub fn client(total: Option<Duration>) -> reqwest::Result<reqwest::Client> {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        let same_origin = attempt
            .previous()
            .first()
            .is_some_and(|first| first.origin() == attempt.url().origin());
        if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if same_origin {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(policy)
        .user_agent(format!("gz-cli/{}", env!("CARGO_PKG_VERSION")));
    if let Some(total) = total {
        builder = builder.timeout(total);
    }
    builder.build()
}

/// Why gz stopped at a redirect to another origin, and what to do.
pub fn redirect_refused(from: &str, response: &reqwest::Response) -> String {
    let to = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("an unnamed address");
    format!(
        "{from} redirected to {to} (HTTP {}), another origin; gz doesn't follow it, so the \
         server's headers and sign-in never leave its origin. If the server moved, set its url \
         to the new address",
        response.status().as_u16()
    )
}

/// Who the connection signs in as, for the bearer token.
#[derive(Clone)]
pub struct Auth {
    pub store: AuthStore,
    pub name: String,
}

/// Sends messages to the server's POST endpoint; cheap to clone, so the
/// background readers can answer the server's own requests.
#[derive(Clone)]
struct Poster {
    http: reqwest::Client,
    endpoint: String,
    /// The configured server URL: the key for the sign-in.
    url: String,
    headers: Vec<(String, String)>,
    auth: Option<Auth>,
    session: Arc<Mutex<Option<String>>>,
    protocol: Arc<Mutex<Option<String>>>,
}

impl Poster {
    fn has_own_authorization(&self) -> bool {
        self.headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("authorization"))
    }

    async fn bearer(&self) -> Option<String> {
        let auth = self
            .auth
            .as_ref()
            .filter(|_| !self.has_own_authorization())?;
        oauth::bearer(&self.http, &auth.store, &auth.name, &self.url).await
    }

    fn session(&self) -> Option<String> {
        self.session.lock().ok().and_then(|s| s.clone())
    }

    fn decorate(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        if let Some(session) = self.session() {
            request = request.header(SESSION_HEADER, session);
        }
        if let Some(version) = self.protocol.lock().ok().and_then(|p| p.clone()) {
            request = request.header(PROTOCOL_HEADER, version);
        }
        request
    }

    /// POSTs one message. A 401 is retried once after refreshing the
    /// sign-in, then reported as needing a login.
    async fn post(&self, message: &Value) -> Result<reqwest::Response, RpcError> {
        let mut refreshed = false;
        loop {
            let mut request = self
                .decorate(self.http.post(&self.endpoint))
                .header("Accept", "application/json, text/event-stream")
                .json(message);
            if let Some(token) = self.bearer().await {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.map_err(|err| {
                RpcError::Exited(format!(
                    "cannot reach {}: {}",
                    self.endpoint,
                    root_cause(&err)
                ))
            })?;
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                if !refreshed && self.try_refresh().await {
                    refreshed = true;
                    continue;
                }
                return Err(self.needs_login());
            }
            if status == reqwest::StatusCode::NOT_FOUND && self.session().is_some() {
                return Err(RpcError::SessionExpired);
            }
            if status.is_redirection() {
                return Err(RpcError::Protocol(redirect_refused(
                    &self.endpoint,
                    &response,
                )));
            }
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(RpcError::Server {
                    code: i64::from(status.as_u16()),
                    message: format!("HTTP {status}: {}", oauth::excerpt(&body)),
                });
            }
            self.remember_session(&response);
            return Ok(response);
        }
    }

    async fn try_refresh(&self) -> bool {
        let Some(auth) = self.auth.as_ref().filter(|_| !self.has_own_authorization()) else {
            return false;
        };
        oauth::refresh(&self.http, &auth.store, &auth.name, &self.url)
            .await
            .is_ok()
    }

    fn needs_login(&self) -> RpcError {
        let name = self.auth.as_ref().map_or("<name>", |a| a.name.as_str());
        if self.has_own_authorization() {
            return RpcError::NeedsLogin(format!(
                "the server refused the Authorization header set for {name} (HTTP 401); fix it \
                 in the server's headers"
            ));
        }
        RpcError::NeedsLogin(format!(
            "{name} needs you to sign in: run `gz mcp login {name}` in a terminal, then \
             /mcp reconnect {name}"
        ))
    }

    fn remember_session(&self, response: &reqwest::Response) {
        let Some(id) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
        else {
            return;
        };
        if let Ok(mut session) = self.session.lock() {
            if session.is_none() {
                *session = Some(id.to_string());
            }
        }
    }

    /// POSTs a message that has no answer, waiting at most
    /// [`NOTIFICATION_WAIT`] for the server to take it.
    async fn post_one_way(&self, message: &Value) -> Result<(), RpcError> {
        match tokio::time::timeout(NOTIFICATION_WAIT, self.post(message)).await {
            Ok(reply) => reply.map(drop),
            Err(_) => Err(RpcError::TimedOut(NOTIFICATION_WAIT)),
        }
    }

    /// Answers a request the server sent (only `ping` is supported).
    async fn answer(&self, method: &str, id: Value) {
        let answer = server_request_answer(method, id);
        // Best effort: a server that can't take the answer will say so on
        // the next real request.
        let _ = self.post_one_way(&answer).await;
    }

    /// The saved access token as it stands, without refreshing: for the
    /// session's closing DELETE, which can't wait.
    fn saved_bearer(&self) -> Option<String> {
        let auth = self
            .auth
            .as_ref()
            .filter(|_| !self.has_own_authorization())?;
        let saved = auth.store.get(&auth.name, &self.url).ok().flatten()?;
        Some(saved.access_token)
    }
}

/// A message from the server, by kind.
enum Incoming {
    Response { id: Value, message: Value },
    Request { method: String, id: Value },
    Notification { method: String },
    Other,
}

fn classify(message: Value) -> Incoming {
    let method = message["method"].as_str().map(str::to_string);
    let id = message.get("id").cloned().filter(|id| !id.is_null());
    match (method, id) {
        (Some(method), Some(id)) => Incoming::Request { method, id },
        (Some(method), None) => Incoming::Notification { method },
        (None, Some(id)) => Incoming::Response { id, message },
        (None, None) => Incoming::Other,
    }
}

/// What gz does with a message nobody is waiting for: answer requests,
/// note a changed tool list.
async fn handle_unprompted(poster: &Poster, tools_changed: &AtomicBool, incoming: Incoming) {
    match incoming {
        Incoming::Request { method, id } => poster.answer(&method, id).await,
        Incoming::Notification { method } => note_notification(tools_changed, &method),
        Incoming::Response { .. } | Incoming::Other => {}
    }
}

fn note_notification(tools_changed: &AtomicBool, method: &str) {
    if method == "notifications/tools/list_changed" {
        tools_changed.store(true, Ordering::SeqCst);
    }
}

/// A JSON-RPC result or error as gz's answer to a request.
fn reply_result(message: &Value) -> Result<Value, RpcError> {
    if let Some(error) = message.get("error") {
        return Err(RpcError::Server {
            code: error["code"].as_i64().unwrap_or_default(),
            message: error["message"]
                .as_str()
                .unwrap_or("the server reported an error")
                .to_string(),
        });
    }
    Ok(message.get("result").cloned().unwrap_or(Value::Null))
}

/// Reads SSE events from a response body, chunk by chunk; holds back an
/// incomplete line so a character split across chunks stays whole.
struct EventReader {
    response: reqwest::Response,
    parser: sse::Parser,
    pending: Vec<u8>,
    /// Bytes of complete lines fed since the last blank line: the event
    /// the parser is still collecting.
    unfinished_event: usize,
    ready: std::collections::VecDeque<sse::Event>,
}

impl EventReader {
    fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            parser: sse::Parser::default(),
            pending: Vec::new(),
            unfinished_event: 0,
            ready: Default::default(),
        }
    }

    /// The next event, or `None` when the stream ends.
    async fn next(&mut self) -> Result<Option<sse::Event>, RpcError> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Ok(Some(event));
            }
            let chunk = match self.response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => return Ok(None),
                Err(err) => {
                    return Err(RpcError::Exited(format!(
                        "the server's event stream broke: {}",
                        root_cause(&err)
                    )))
                }
            };
            self.pending.extend_from_slice(&chunk);
            if self.pending.len() > MAX_MESSAGE_BYTES {
                return Err(too_big_event());
            }
            if let Some(end) = self.pending.iter().rposition(|b| *b == b'\n') {
                let complete: Vec<u8> = self.pending.drain(..=end).collect();
                self.count_unfinished_event(&complete)?;
                self.ready.extend(self.parser.feed(&complete));
            }
        }
    }
}

impl EventReader {
    /// Keeps one event within [`MAX_MESSAGE_BYTES`] across lines too: the
    /// parser holds lines until a blank one ends the event.
    fn count_unfinished_event(&mut self, complete_lines: &[u8]) -> Result<(), RpcError> {
        for line in complete_lines.split_inclusive(|b| *b == b'\n') {
            if matches!(line, b"\n" | b"\r\n") {
                self.unfinished_event = 0;
                continue;
            }
            self.unfinished_event += line.len();
            if self.unfinished_event > MAX_MESSAGE_BYTES {
                return Err(too_big_event());
            }
        }
        Ok(())
    }
}

fn too_big_event() -> RpcError {
    RpcError::Protocol(format!(
        "the server sent an event over {} MiB",
        MAX_MESSAGE_BYTES / (1024 * 1024)
    ))
}

fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
}

/// One connection to a remote MCP server.
pub struct HttpConnection {
    poster: Poster,
    mode: Mode,
    next_id: u64,
    tools_changed: Arc<AtomicBool>,
    /// Why the server's stream ended, once it has (the SSE transport).
    closed: Arc<Mutex<Option<String>>>,
    background: Vec<JoinHandle<()>>,
}

enum Mode {
    Streamable {
        listening: bool,
    },
    Legacy {
        answers: mpsc::UnboundedReceiver<Value>,
    },
}

impl HttpConnection {
    /// Connects: for Streamable HTTP nothing is sent until the first
    /// request; for HTTP+SSE the event stream is opened and its POST
    /// endpoint awaited (up to `limit`).
    pub async fn connect(
        remote: &Remote,
        auth: Option<Auth>,
        limit: Duration,
    ) -> Result<Self, RpcError> {
        let http = client(None)
            .map_err(|err| RpcError::Protocol(format!("cannot build an HTTP client: {err}")))?;
        let poster = Poster {
            http,
            endpoint: remote.url.clone(),
            url: remote.url.clone(),
            headers: remote.headers.clone(),
            auth,
            session: Arc::default(),
            protocol: Arc::default(),
        };
        let mut connection = Self {
            poster,
            mode: Mode::Streamable { listening: false },
            next_id: 1,
            tools_changed: Arc::default(),
            closed: Arc::default(),
            background: Vec::new(),
        };
        if remote.sse {
            connection.open_legacy_stream(limit).await?;
        }
        Ok(connection)
    }

    pub fn is_alive(&self) -> bool {
        self.closed.lock().map(|c| c.is_none()).unwrap_or(false)
    }

    pub fn exit_reason(&self) -> String {
        self.closed
            .lock()
            .ok()
            .and_then(|c| c.clone())
            .unwrap_or_else(|| "the connection closed".to_string())
    }

    /// Whether the server said its tool list changed since last asked.
    pub fn take_tools_changed(&self) -> bool {
        self.tools_changed.swap(false, Ordering::SeqCst)
    }

    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, RpcError> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let reply = match tokio::time::timeout(limit, self.exchange(&message, id)).await {
            Ok(reply) => reply,
            Err(_) => {
                let cancel = json!({"requestId": id, "reason": "gz stopped waiting"});
                // Best effort: the caller already reports the timeout.
                let _ = self.notify("notifications/cancelled", cancel).await;
                return Err(RpcError::TimedOut(limit));
            }
        };
        if method != "initialize" {
            return reply;
        }
        self.after_initialize(reply.as_ref().ok());
        let streamable = matches!(self.mode, Mode::Streamable { .. });
        reply.map_err(|err| match err {
            RpcError::Server { code, message } if streamable && matches!(code, 404 | 405) => {
                RpcError::Server {
                    code,
                    message: format!("{message}; {LEGACY_HINT}"),
                }
            }
            other => other,
        })
    }

    /// Sends a notification, waiting at most [`NOTIFICATION_WAIT`] for the
    /// server to take it.
    pub async fn notify(&mut self, method: &str, params: Value) -> Result<(), RpcError> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.poster.post_one_way(&message).await
    }

    async fn exchange(&mut self, message: &Value, id: u64) -> Result<Value, RpcError> {
        let response = self.poster.post(message).await?;
        match &mut self.mode {
            Mode::Streamable { .. } => {
                read_streamable_reply(&self.poster, &self.tools_changed, response, id).await
            }
            Mode::Legacy { answers } => {
                drop(response);
                loop {
                    let Some(message) = answers.recv().await else {
                        return Err(RpcError::Exited(
                            self.closed
                                .lock()
                                .ok()
                                .and_then(|c| c.clone())
                                .unwrap_or_else(|| {
                                    "the server closed its event stream".to_string()
                                }),
                        ));
                    };
                    if message["id"] == json!(id) {
                        return reply_result(&message);
                    }
                }
            }
        }
    }

    /// Keeps the negotiated protocol version for later requests' headers,
    /// and opens the stream for the server's own messages.
    fn after_initialize(&mut self, result: Option<&Value>) {
        let Some(version) = result.and_then(|r| r["protocolVersion"].as_str()) else {
            return;
        };
        if let Ok(mut protocol) = self.poster.protocol.lock() {
            *protocol = Some(version.to_string());
        }
        if let Mode::Streamable { listening } = &mut self.mode {
            if !*listening {
                *listening = true;
                let task = tokio::spawn(listen(self.poster.clone(), self.tools_changed.clone()));
                self.background.push(task);
            }
        }
    }

    /// HTTP+SSE: opens the event stream and waits for its `endpoint` event.
    async fn open_legacy_stream(&mut self, limit: Duration) -> Result<(), RpcError> {
        let started = tokio::time::Instant::now();
        let response = tokio::time::timeout(limit, self.open_event_stream(None))
            .await
            .map_err(|_| RpcError::TimedOut(limit))??;
        let Some(response) = response else {
            return Err(RpcError::Protocol(format!(
                "{} doesn't offer an event stream; if it uses Streamable HTTP, set type = \"http\"",
                self.poster.url
            )));
        };
        let (endpoint_tx, endpoint_rx) = oneshot::channel();
        let (answers_tx, answers_rx) = mpsc::unbounded_channel();
        let reader = LegacyReader {
            poster: self.poster.clone(),
            tools_changed: self.tools_changed.clone(),
            closed: self.closed.clone(),
        };
        self.background
            .push(tokio::spawn(reader.run(response, endpoint_tx, answers_tx)));
        let endpoint = match tokio::time::timeout_at(started + limit, endpoint_rx).await {
            Ok(Ok(endpoint)) => endpoint?,
            Ok(Err(_)) => return Err(RpcError::Exited(self.exit_reason())),
            Err(_) => return Err(RpcError::TimedOut(limit)),
        };
        self.poster.endpoint = endpoint;
        self.mode = Mode::Legacy {
            answers: answers_rx,
        };
        Ok(())
    }

    /// GETs the event stream; `None` when the server doesn't offer one.
    async fn open_event_stream(
        &self,
        last_event: Option<&str>,
    ) -> Result<Option<reqwest::Response>, RpcError> {
        open_event_stream(&self.poster, last_event).await
    }
}

async fn open_event_stream(
    poster: &Poster,
    last_event: Option<&str>,
) -> Result<Option<reqwest::Response>, RpcError> {
    let mut refreshed = false;
    loop {
        let mut request = poster
            .decorate(poster.http.get(&poster.url))
            .header("Accept", "text/event-stream");
        if let Some(id) = last_event {
            request = request.header("Last-Event-ID", id);
        }
        if let Some(token) = poster.bearer().await {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|err| {
            RpcError::Exited(format!("cannot reach {}: {}", poster.url, root_cause(&err)))
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            if !refreshed && poster.try_refresh().await {
                refreshed = true;
                continue;
            }
            return Err(poster.needs_login());
        }
        if status.is_redirection() {
            return Err(RpcError::Protocol(redirect_refused(&poster.url, &response)));
        }
        if !status.is_success() || !is_event_stream(&response) {
            return Ok(None);
        }
        return Ok(Some(response));
    }
}

/// A Streamable HTTP reply: a JSON message (or batch), or an event stream
/// that carries the answer, maybe after requests and notifications.
async fn read_streamable_reply(
    poster: &Poster,
    tools_changed: &AtomicBool,
    response: reqwest::Response,
    id: u64,
) -> Result<Value, RpcError> {
    if is_event_stream(&response) {
        let mut events = EventReader::new(response);
        while let Some(event) = events.next().await? {
            let Some(message) = event.json() else {
                continue;
            };
            match classify(message) {
                Incoming::Response { id: got, message } if got == json!(id) => {
                    return reply_result(&message)
                }
                other => handle_unprompted(poster, tools_changed, other).await,
            }
        }
        return Err(RpcError::Protocol(
            "the server ended its reply stream without answering".to_string(),
        ));
    }
    let body = response
        .bytes()
        .await
        .map_err(|err| RpcError::Exited(format!("the reply broke off: {}", root_cause(&err))))?;
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(RpcError::Protocol(format!(
            "the server sent a reply over {} MiB",
            MAX_MESSAGE_BYTES / (1024 * 1024)
        )));
    }
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(RpcError::Protocol(
            "the server accepted the request but sent no answer".to_string(),
        ));
    }
    let parsed: Value = serde_json::from_slice(&body)
        .map_err(|err| RpcError::Protocol(format!("the reply is not JSON: {err}")))?;
    let messages = match parsed {
        Value::Array(batch) => batch,
        single => vec![single],
    };
    let mut answer = None;
    for message in messages {
        match classify(message) {
            Incoming::Response { id: got, message } if got == json!(id) => {
                answer = Some(reply_result(&message));
            }
            other => handle_unprompted(poster, tools_changed, other).await,
        }
    }
    answer.unwrap_or_else(|| {
        Err(RpcError::Protocol(
            "the server's reply doesn't answer the request".to_string(),
        ))
    })
}

/// Streamable HTTP: listens on the GET stream for the server's own
/// messages, reopening it when it drops; stops for good when the server
/// offers none.
async fn listen(poster: Poster, tools_changed: Arc<AtomicBool>) {
    let mut last_event: Option<String> = None;
    let mut failures = 0;
    while failures < MAX_LISTEN_RETRIES {
        let response = match open_event_stream(&poster, last_event.as_deref()).await {
            Ok(Some(response)) => response,
            Ok(None) | Err(RpcError::NeedsLogin(_)) => return,
            Err(_) => {
                failures += 1;
                tokio::time::sleep(LISTEN_RETRY_DELAY).await;
                continue;
            }
        };
        failures = 0;
        let mut events = EventReader::new(response);
        while let Ok(Some(event)) = events.next().await {
            if event.id.is_some() {
                last_event = event.id.clone();
            }
            if let Some(message) = event.json() {
                handle_unprompted(&poster, &tools_changed, classify(message)).await;
            }
        }
        failures += 1;
        tokio::time::sleep(LISTEN_RETRY_DELAY).await;
    }
}

/// HTTP+SSE: reads the one event stream, which names the POST endpoint
/// first and then carries every answer.
struct LegacyReader {
    poster: Poster,
    tools_changed: Arc<AtomicBool>,
    closed: Arc<Mutex<Option<String>>>,
}

impl LegacyReader {
    async fn run(
        mut self,
        response: reqwest::Response,
        endpoint_tx: oneshot::Sender<Result<String, RpcError>>,
        answers: mpsc::UnboundedSender<Value>,
    ) {
        let mut endpoint_tx = Some(endpoint_tx);
        let mut events = EventReader::new(response);
        let why = loop {
            let event = match events.next().await {
                Ok(Some(event)) => event,
                Ok(None) => break "the server closed its event stream".to_string(),
                Err(err) => break err.to_string(),
            };
            if event.event.as_deref() == Some("endpoint") {
                let resolved = self.resolve_endpoint(event.data.trim());
                if let Ok(endpoint) = &resolved {
                    self.poster.endpoint = endpoint.clone();
                }
                if let Some(tx) = endpoint_tx.take() {
                    let _ = tx.send(resolved);
                }
                continue;
            }
            let Some(message) = event.json() else {
                continue;
            };
            match classify(message) {
                Incoming::Response { message, .. } => {
                    let _ = answers.send(message);
                }
                other => handle_unprompted(&self.poster, &self.tools_changed, other).await,
            }
        };
        if let Ok(mut closed) = self.closed.lock() {
            *closed = Some(why);
        }
    }

    /// The POST endpoint, which must be on the server's own origin so
    /// headers and tokens never go elsewhere.
    fn resolve_endpoint(&self, data: &str) -> Result<String, RpcError> {
        let base = url::Url::parse(&self.poster.url)
            .map_err(|err| RpcError::Protocol(format!("bad server url: {err}")))?;
        let endpoint = base.join(data).map_err(|err| {
            RpcError::Protocol(format!("the server named a bad endpoint '{data}': {err}"))
        })?;
        if endpoint.origin() != base.origin() {
            return Err(RpcError::Protocol(format!(
                "the server named an endpoint on another origin ({endpoint}); gz only posts to {}",
                base.origin().ascii_serialization()
            )));
        }
        Ok(endpoint.to_string())
    }
}

impl Drop for HttpConnection {
    fn drop(&mut self) {
        for task in &self.background {
            task.abort();
        }
        // Ends the server's session, as the spec asks; best effort.
        let (Some(session), Mode::Streamable { .. }) = (self.poster.session(), &self.mode) else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let poster = self.poster.clone();
            runtime.spawn(async move {
                let mut request = poster
                    .decorate(poster.http.delete(&poster.url))
                    .header(SESSION_HEADER, session)
                    .timeout(NOTIFICATION_WAIT);
                if let Some(token) = poster.saved_bearer() {
                    request = request.bearer_auth(token);
                }
                let _ = request.send().await;
            });
        }
    }
}

/// The innermost error: reqwest's own message is often "error sending
/// request" with the real reason (refused, DNS) underneath.
fn root_cause(err: &reqwest::Error) -> String {
    let mut source: &dyn std::error::Error = err;
    while let Some(inner) = source.source() {
        source = inner;
    }
    source.to_string()
}
