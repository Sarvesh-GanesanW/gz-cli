use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{multipart, Method, RequestBuilder, Response};
use serde_json::Value;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::apikey::{self, Credential};
use crate::cognito::{self, PoolConfig};
use crate::config::{self, Resolved};
use crate::error_log::{self, Failure, FailureKind};
use crate::http_retry;
use crate::output::OutputFormat;

/// Refresh this many seconds before the ID token actually expires.
const REFRESH_MARGIN_SECS: i64 = 120;
const ERROR_SNIPPET_CHARS: usize = 500;
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Warm-ups renew an ID token this close to expiring, well before a
/// request would have to wait for it.
const REFRESH_AHEAD_SECS: i64 = 10 * 60;
/// A warm-up gives up after this; it only ever saves time.
const WARM_TIMEOUT: Duration = Duration::from_secs(5);
/// The chat server's public liveness route (gz-chat-eks api/main.py
/// `/health`): no sign-in, no logging, nothing to compute.
const HEALTH_PATH: &str = "/health";
/// The Cognito user-admin gateway (users, groups) discovered from the web app.
pub const ADMIN_QUERIES: &str = "adminqueries";

/// The signed-in session. Shared so the TUI's parallel requests refresh once.
#[derive(Debug)]
struct Session {
    token: Option<String>,
    expires_at: Option<i64>,
    refresh_token: Option<String>,
    access_token: Option<String>,
    pool: Option<PoolConfig>,
    /// Only tokens loaded from the config file are refreshed and saved back.
    from_config: bool,
    refresh_failed: bool,
}

impl Session {
    /// Whether the ID token should be renewed now: a token gz may refresh
    /// (one from the config file, with no failed refresh yet) that expires
    /// within `margin` seconds of `now`.
    fn should_refresh(&self, now: i64, margin: i64) -> bool {
        let expiring = self.expires_at.is_some_and(|exp| now + margin >= exp);
        expiring && self.from_config && !self.refresh_failed
    }
}

#[derive(Debug, Clone)]
pub struct ApiClient {
    client: reqwest::Client,
    resolved: Resolved,
    session: Arc<Mutex<Session>>,
    format: OutputFormat,
    quiet: bool,
    verbose: u8,
    /// How many callers are drawing their own progress line; shared by
    /// clones, so a `Lakehouse` holding a copy sees the caller's guard.
    caller_progress: Arc<AtomicUsize>,
}

/// Keeps request spinners hidden until dropped. Two spinners on one terminal
/// line erase each other, so a caller drawing its own progress line holds
/// this while its requests run.
pub struct CallerProgress(Arc<AtomicUsize>);

impl Drop for CallerProgress {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Clears the spinner however the request ends.
struct Spinner(Option<ProgressBar>);

impl Drop for Spinner {
    fn drop(&mut self) {
        if let Some(bar) = self.0.take() {
            bar.finish_and_clear();
        }
    }
}

impl ApiClient {
    pub fn new(
        resolved: Resolved,
        format: OutputFormat,
        quiet: bool,
        verbose: u8,
        timeout_secs: u64,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .user_agent(format!("gz-cli/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("cannot build HTTP client")?;
        let session = Session {
            token: resolved.token(),
            expires_at: resolved.profile.expires_at,
            refresh_token: resolved.profile.refresh_token.clone(),
            access_token: resolved.profile.access_token.clone(),
            pool: resolved.profile.pool(),
            from_config: resolved.token_source == "config",
            refresh_failed: false,
        };
        Ok(Self {
            client,
            resolved,
            session: Arc::new(Mutex::new(session)),
            format,
            quiet,
            verbose,
            caller_progress: Arc::default(),
        })
    }

    /// Hides request spinners while the returned guard lives.
    pub fn caller_progress(&self) -> CallerProgress {
        self.caller_progress.fetch_add(1, Ordering::SeqCst);
        CallerProgress(Arc::clone(&self.caller_progress))
    }

    /// Request spinners go to an interactive table view, and only while no
    /// caller draws its own progress line.
    fn shows_request_spinner(&self, stderr_is_terminal: bool) -> bool {
        stderr_is_terminal
            && !self.quiet
            && self.format == OutputFormat::Table
            && self.caller_progress.load(Ordering::SeqCst) == 0
    }

    /// The user's `-v` count.
    pub fn verbosity(&self) -> u8 {
        self.verbose
    }

    pub fn resolved(&self) -> &Resolved {
        &self.resolved
    }

    pub fn raw(&self) -> &reqwest::Client {
        &self.client
    }

    /// Whether this session can call the AdminQueries gateway.
    pub async fn has_access_token(&self) -> bool {
        self.session.lock().await.access_token.is_some()
    }

    /// The Cognito refresh token; chat runs forward it so long tool calls
    /// can renew their own credentials, as the web app does.
    pub async fn refresh_token(&self) -> Option<String> {
        self.session.lock().await.refresh_token.clone()
    }

    fn spinner(&self, message: &str) -> Spinner {
        if !self.shows_request_spinner(std::io::stderr().is_terminal()) {
            return Spinner(None);
        }
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg:.dim}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner())
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
        );
        bar.set_message(message.to_string());
        bar.enable_steady_tick(Duration::from_millis(80));
        Spinner(Some(bar))
    }

    /// Current ID token, renewed through Cognito when it is about to expire.
    pub async fn token(&self) -> Option<String> {
        let mut session = self.session.lock().await;
        if !session.should_refresh(config::now_unix(), REFRESH_MARGIN_SECS) {
            return session.token.clone();
        }
        let (Some(refresh_token), Some(pool)) =
            (session.refresh_token.clone(), session.pool.clone())
        else {
            return session.token.clone();
        };
        match cognito::refresh(&self.client, &pool, &refresh_token).await {
            Ok(tokens) => self.keep_refreshed(&mut session, &tokens),
            Err(err) => {
                session.refresh_failed = true;
                if !self.quiet {
                    crate::output::warn(&format!("{err:#}"));
                }
            }
        }
        session.token.clone()
    }

    fn keep_refreshed(&self, session: &mut Session, tokens: &cognito::Tokens) {
        let expires_at = config::now_unix() + tokens.expires_in;
        session.token = Some(tokens.id_token.clone());
        session.access_token = tokens.access_token.clone();
        session.expires_at = Some(expires_at);
        if let Err(err) = persist_refresh(&self.resolved.profile_name, tokens, expires_at) {
            crate::output::warn(&format!(
                "signed in, but cannot save the new token: {err:#}"
            ));
        }
    }

    /// Gets `service` ready for a request soon, off the request's path:
    /// renews an ID token that expires within [`REFRESH_AHEAD_SECS`] and
    /// opens a pooled connection (DNS, TCP and TLS) with the server's public
    /// `/health` route, so the next request does neither. Failures are left
    /// for that request to report.
    pub async fn warm_up(&self, service: &str) {
        self.refresh_ahead().await;
        let Ok(url) = self.url_for(service, HEALTH_PATH) else {
            return;
        };
        let _ = self.client.get(url).timeout(WARM_TIMEOUT).send().await;
    }

    async fn refresh_ahead(&self) {
        let mut session = self.session.lock().await;
        if !session.should_refresh(config::now_unix(), REFRESH_AHEAD_SECS) {
            return;
        }
        let (Some(refresh_token), Some(pool)) =
            (session.refresh_token.clone(), session.pool.clone())
        else {
            return;
        };
        // A failure here is not final: the request path tries again and
        // reports it.
        if let Ok(tokens) = cognito::refresh(&self.client, &pool, &refresh_token).await {
            self.keep_refreshed(&mut session, &tokens);
        }
    }

    /// The `Authorization` value for one request: an API key where the route
    /// accepts it and the key should win (see `apikey::choose`), else the raw
    /// ID token — or the Cognito access token for AdminQueries.
    async fn authorization(
        &self,
        service: &str,
        method: &Method,
        url: &str,
        path: &str,
    ) -> Result<Option<String>> {
        let key = self.resolved.api_key();
        let has_user = self.session.lock().await.token.is_some();
        let route_accepts = apikey::accepts(service, method, path);
        match apikey::choose(
            has_user,
            key.as_ref().map(|(_, source)| *source),
            route_accepts,
        ) {
            Credential::Refuse => anyhow::bail!("{}", apikey::refusal(service)),
            Credential::ApiKey => {
                if !config::is_secure_base(url) {
                    anyhow::bail!("refusing to send an API key over plain http: {url}");
                }
                Ok(key.map(|(key, _)| format!("Bearer {key}")))
            }
            Credential::User => {
                let Some(id_token) = self.token().await else {
                    return Ok(None);
                };
                Ok(match service {
                    ADMIN_QUERIES => self.session.lock().await.access_token.clone(),
                    _ => Some(id_token),
                })
            }
        }
    }

    /// Headers the web app sends: the credential (see `authorization`), the
    /// tenant site as `Origin` + `gz-site`, and the lakehouse provider's
    /// `x-access-type: web` marker.
    async fn authed(
        &self,
        service: &str,
        method: &Method,
        path: &str,
        url: &str,
        request: RequestBuilder,
    ) -> Result<RequestBuilder> {
        let mut request = request;
        if let Some(value) = self.authorization(service, method, url, path).await? {
            request = request.header("Authorization", value);
        }
        if let Some(origin) = self.resolved.web_url() {
            request = request.header("Origin", origin);
        }
        if let Some(site) = self.resolved.site() {
            request = request.header("gz-site", site);
        }
        if service == "lakehouse" {
            request = request.header("x-access-type", "web");
        }
        Ok(request)
    }

    fn url_for(&self, service: &str, path: &str) -> Result<String> {
        self.resolved
            .base_for(service)
            .map(|base| config::join_url(&base, path))
    }

    pub async fn request_json(
        &self,
        service: &str,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        self.request_json_with(service, method, path, query, body, &[])
            .await
    }

    /// [`Self::request_json`] plus extra request headers.
    pub async fn request_json_with(
        &self,
        service: &str,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
        headers: &[(&str, String)],
    ) -> Result<Value> {
        self.json_request(service, method, path, query, body, headers, None)
            .await
    }

    /// [`Self::request_json`] with its own timeout, for calls the server
    /// holds open by design (starting a lakehouse session waits for its pod).
    pub async fn request_json_timeout(
        &self,
        service: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        self.json_request(service, method, path, &[], body, &[], Some(timeout))
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn json_request(
        &self,
        service: &str,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
        headers: &[(&str, String)],
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let url = self.url_for(service, path)?;
        let _spinner = self.spinner(&format!("{method} {url}"));
        let mut request = self
            .json_request_builder(service, &method, path, &url, query, body, headers)
            .await?;
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let response = self
            .send(request, &method, &url, dedupes(service, headers))
            .await?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let text = response.text().await.unwrap_or_default();
        json_reply(&text, status, &headers, &method, &url)
    }

    /// POSTs `body` like [`Self::request_json_with`], except that a 409 or
    /// 429 whose `error` code is one of `expected` comes back as a
    /// [`Refusal`]: an answer the caller handles, so it is neither retried,
    /// logged nor an error. Any other failure is logged and returned as
    /// usual (a 429 here is not retried: the caller decides when to ask
    /// again).
    pub async fn post_json_or_refusal(
        &self,
        service: &str,
        path: &str,
        body: Value,
        headers: &[(&str, String)],
        expected: &[&str],
    ) -> Result<std::result::Result<Value, Refusal>> {
        let method = Method::POST;
        let url = self.url_for(service, path)?;
        let request = self
            .json_request_builder(service, &method, path, &url, &[], Some(body), headers)
            .await?;
        let response = self
            .send_expecting(request, &method, &url, dedupes(service, headers), &REFUSALS)
            .await?;
        let status = response.status().as_u16();
        let reply_headers = response.headers().clone();
        let text = response.text().await.unwrap_or_default();
        if !REFUSALS.contains(&status) {
            return json_reply(&text, status, &reply_headers, &method, &url).map(Ok);
        }
        let code = conflict_code(&text);
        if let Some(code) = code.filter(|code| expected.contains(&code.as_str())) {
            return Ok(Err(Refusal { status, code }));
        }
        Err(failure(
            status,
            &reply_headers,
            &text,
            &method,
            &url,
            None,
            self.verbose,
        ))
    }

    /// The authed request for a JSON call, with its query, body and headers.
    #[allow(clippy::too_many_arguments)]
    async fn json_request_builder(
        &self,
        service: &str,
        method: &Method,
        path: &str,
        url: &str,
        query: &[(String, String)],
        body: Option<Value>,
        headers: &[(&str, String)],
    ) -> Result<RequestBuilder> {
        let mut request = self
            .authed(
                service,
                method,
                path,
                url,
                self.client.request(method.clone(), url),
            )
            .await?;
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        if !query.is_empty() {
            request = request.query(query);
        }
        let sends_body = matches!(*method, Method::POST | Method::PUT | Method::PATCH);
        if let Some(payload) = body {
            request = request.json(&payload);
        } else if sends_body {
            request = request.json(&serde_json::json!({}));
        }
        Ok(request)
    }

    /// GETs `path` and reports the HTTP status and round-trip time, whatever
    /// the status. Used by `gz doctor` to check reachability.
    pub async fn probe(&self, service: &str, path: &str) -> Result<(u16, Duration)> {
        let url = self.url_for(service, path)?;
        let request = self
            .authed(service, &Method::GET, path, &url, self.client.get(&url))
            .await?
            .timeout(PROBE_TIMEOUT);
        let started = std::time::Instant::now();
        let response = request
            .send()
            .await
            .with_context(|| format!("cannot reach {url}"))?;
        Ok((response.status().as_u16(), started.elapsed()))
    }

    /// Opens a long-lived streaming response (server-sent events). No
    /// overall timeout: the stream ends when the server says so.
    pub async fn stream(
        &self,
        service: &str,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> Result<Response> {
        let url = self.url_for(service, path)?;
        let mut request = self
            .authed(
                service,
                &method,
                path,
                &url,
                self.client.request(method.clone(), &url),
            )
            .await?
            .header("Accept", "text/event-stream")
            .timeout(Duration::from_secs(60 * 60));
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(payload) = body {
            request = request.json(&payload);
        }
        self.send(request, &method, &url, false).await
    }

    /// Sends `request`, trying again where that is safe (see
    /// [`http_retry`]); `repeatable` marks a POST the server dedupes.
    async fn send(
        &self,
        request: RequestBuilder,
        method: &Method,
        url: &str,
        repeatable: bool,
    ) -> Result<Response> {
        self.send_expecting(request, method, url, repeatable, &[])
            .await
    }

    /// [`Self::send`], except that a status in `expected` is returned as
    /// the response, untried again and unlogged, for the caller to read.
    async fn send_expecting(
        &self,
        request: RequestBuilder,
        method: &Method,
        url: &str,
        repeatable: bool,
        expected: &[u16],
    ) -> Result<Response> {
        let request = request
            .build()
            .with_context(|| format!("cannot build the request: {method} {url}"))?;
        let repeatable = repeatable || matches!(*method, Method::GET | Method::HEAD);
        let mut failures = 0;
        let mut next = Some(request);
        loop {
            let Some(request) = next.take() else {
                anyhow::bail!("request failed: {method} {url}");
            };
            // A body that can't be copied (a stream) is sent once.
            next = request.try_clone();
            if self.verbose > 0 {
                eprintln!("> {method} {url}");
            }
            crate::timing::mark_with(|| format!("request sent: {method} {}", path_of(url)));
            let response = match self.client.execute(request).await {
                Ok(response) => response,
                Err(err) => {
                    failures += 1;
                    let failure = if err.is_connect() {
                        http_retry::Failure::Connect
                    } else if err.is_timeout() {
                        http_retry::Failure::Timeout
                    } else {
                        http_retry::Failure::Transport
                    };
                    if let Some(wait) = next
                        .as_ref()
                        .and(http_retry::delay(failures, failure, repeatable))
                    {
                        self.note_retry(method, url, &err.to_string(), wait);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    error_log::record(&Failure {
                        method: method.as_str(),
                        url,
                        status: None,
                        headers: None,
                        kind: FailureKind::of_transport(&err),
                    });
                    return Err(err).with_context(|| format!("request failed: {method} {url}"));
                }
            };
            let status = response.status();
            crate::timing::mark_with(|| format!("response headers: {method} {}", path_of(url)));
            if self.verbose > 0 {
                eprintln!("< {status}");
            }
            if status.is_success() || expected.contains(&status.as_u16()) {
                return Ok(response);
            }
            let retry_after = http_retry::retry_after_secs(
                response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok()),
            );
            failures += 1;
            let failure = http_retry::Failure::Status(status.as_u16(), retry_after);
            if let Some(wait) = next
                .as_ref()
                .and(http_retry::delay(failures, failure, repeatable))
            {
                self.note_retry(method, url, status.as_str(), wait);
                tokio::time::sleep(wait).await;
                continue;
            }
            return Err(self.status_error(response, method, url, retry_after).await);
        }
    }

    /// Says (in verbose mode) that a failed try is being repeated.
    fn note_retry(&self, method: &Method, url: &str, why: &str, wait: Duration) {
        if self.verbose > 0 {
            eprintln!("! {method} {url}: {why}; trying again in {wait:?}");
        }
    }

    /// The error for a final non-success answer, logged for `gz bug-report`.
    async fn status_error(
        &self,
        response: Response,
        method: &Method,
        url: &str,
        retry_after: Option<u64>,
    ) -> anyhow::Error {
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let text = response.text().await.unwrap_or_default();
        failure(
            status,
            &headers,
            &text,
            method,
            url,
            retry_after,
            self.verbose,
        )
    }

    /// Raw unauthenticated PUT (presigned S3 URLs must not carry auth headers).
    pub async fn put_bytes(&self, url: &str, content_type: &str, bytes: Vec<u8>) -> Result<()> {
        let _spinner = self.spinner("uploading");
        let request = self
            .client
            .put(url)
            .header("Content-Type", content_type)
            .body(bytes);
        let method = Method::PUT;
        self.send(
            request,
            &method,
            url.split('?').next().unwrap_or(url),
            false,
        )
        .await
        .map(|_| ())
    }

    pub async fn download(
        &self,
        service: &str,
        path: &str,
        body: Option<Value>,
        dest: &Path,
    ) -> Result<u64> {
        let url = self.url_for(service, path)?;
        let _spinner = self.spinner(&format!("downloading {}", dest.display()));
        let request = self
            .authed(service, &Method::POST, path, &url, self.client.post(&url))
            .await?
            .json(&body.unwrap_or_else(|| serde_json::json!({})));
        let response = self.send(request, &Method::POST, &url, false).await?;
        let bytes = response
            .bytes()
            .await
            .context("cannot read download body")?;
        std::fs::write(dest, &bytes).with_context(|| format!("cannot write {}", dest.display()))?;
        Ok(bytes.len() as u64)
    }

    /// Multipart upload. With `meta_field`, `meta` goes in as one JSON text
    /// part; otherwise each top-level key becomes its own form field.
    pub async fn upload_file(
        &self,
        service: &str,
        path: &str,
        meta: &Value,
        file_path: &Path,
        file_field: &str,
        meta_field: Option<&str>,
    ) -> Result<Value> {
        let url = self.url_for(service, path)?;
        let _spinner = self.spinner(&format!("uploading {}", file_path.display()));
        let file_name = file_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "upload.bin".to_string());
        let bytes = std::fs::read(file_path)
            .with_context(|| format!("cannot read {}", file_path.display()))?;
        let part = multipart::Part::bytes(bytes).file_name(file_name);
        let mut form = multipart::Form::new().part(file_field.to_string(), part);
        match meta_field {
            Some(field) => form = form.text(field.to_string(), serde_json::to_string(meta)?),
            None => {
                for (key, val) in meta.as_object().into_iter().flatten() {
                    let text = val
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| val.to_string());
                    form = form.text(key.clone(), text);
                }
            }
        }
        let request = self
            .authed(service, &Method::POST, path, &url, self.client.post(&url))
            .await?
            .multipart(form);
        let response = self.send(request, &Method::POST, &url, false).await?;
        let text = response.text().await.unwrap_or_default();
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
}

/// `url` without its scheme, host and query, for timing traces.
fn path_of(url: &str) -> &str {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = after_scheme.find('/').map_or("/", |at| &after_scheme[at..]);
    path.split('?').next().unwrap_or(path)
}

fn persist_refresh(profile: &str, tokens: &cognito::Tokens, expires_at: i64) -> Result<()> {
    let mut config = config::load_config()?;
    let entry = config.profile_mut(profile);
    entry.token = Some(tokens.id_token.clone());
    entry.access_token = tokens.access_token.clone();
    entry.expires_at = Some(expires_at);
    if let Some(refresh) = &tokens.refresh_token {
        entry.refresh_token = Some(refresh.clone());
    }
    config::save_config(&config).map(|_| ())
}

const CONFLICT: u16 = 409;
const TOO_MANY_REQUESTS: u16 = 429;
/// Statuses whose coded answer may be an expected refusal rather than a
/// failure to retry or log (see [`ApiClient::post_json_or_refusal`]).
const REFUSALS: [u16; 2] = [CONFLICT, TOO_MANY_REQUESTS];

/// A 409 the caller expected (by the server's `error` code): the server
/// said "not now" or "no longer", which is an answer, not a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub code: String,
}

/// Whether a POST may be sent again: the chat service dedupes by request id.
fn dedupes(service: &str, headers: &[(&str, String)]) -> bool {
    service == http_retry::DEDUPING_SERVICE
        && headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(http_retry::REQUEST_ID))
}

/// A successful answer's JSON (`null` when empty); an `{success: false}`
/// style envelope is logged and returned as an error.
fn json_reply(
    text: &str,
    status: u16,
    headers: &reqwest::header::HeaderMap,
    method: &Method,
    url: &str,
) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    let parsed: Value =
        serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()));
    if let Err(err) = check_envelope(&parsed, method.as_str(), url) {
        error_log::record(&Failure {
            method: method.as_str(),
            url,
            status: Some(status),
            headers: Some(headers),
            kind: FailureKind::Envelope,
        });
        return Err(err);
    }
    Ok(parsed)
}

/// A conflict's `error` code: FastAPI nests it under `detail`.
fn conflict_code(text: &str) -> Option<String> {
    let body: Value = serde_json::from_str(text).ok()?;
    body["detail"]["error"]
        .as_str()
        .or(body["error"].as_str())
        .map(str::to_string)
}

/// Logs a final non-success answer for `gz bug-report` and turns it into
/// an error carrying the server's own message.
fn failure(
    status: u16,
    headers: &reqwest::header::HeaderMap,
    text: &str,
    method: &Method,
    url: &str,
    retry_after: Option<u64>,
    verbose: u8,
) -> anyhow::Error {
    error_log::record(&Failure {
        method: method.as_str(),
        url,
        status: Some(status),
        headers: Some(headers),
        kind: FailureKind::Http,
    });
    // Throttled callers (the TUI's log follower) wait as long as asked.
    let retry = retry_after
        .map(|secs| format!(" (retry after {secs}s)"))
        .unwrap_or_default();
    if verbose > 1 {
        eprintln!("{text}");
    }
    let status = reqwest::StatusCode::from_u16(status)
        .map(|code| code.to_string())
        .unwrap_or_else(|_| status.to_string());
    anyhow::anyhow!(
        "{method} {url} → HTTP {status}{retry}: {}",
        error_detail(text)
    )
}

/// A request failure as the user should read it: drops the
/// `POST https://… → ` prefix gz adds, keeping the status and the server's
/// own message.
pub fn readable_error(error: &str) -> String {
    const METHODS: [&str; 5] = ["GET ", "POST ", "PUT ", "PATCH ", "DELETE "];
    match error.split_once(" → ") {
        Some((request, rest)) if METHODS.iter().any(|m| request.starts_with(m)) => rest.to_string(),
        _ => error.to_string(),
    }
}

/// Pulls the human message out of an error body (`{"message"}`, `{"detail"}`,
/// `{"response": {"message"}}`, …); falls back to a trimmed snippet.
pub fn error_detail(text: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if let Some(message) = message_of(&value) {
            return message;
        }
    }
    text.trim().chars().take(ERROR_SNIPPET_CHARS).collect()
}

fn message_of(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        // `msg` is FastAPI's validation-error text.
        Value::Object(map) => [
            "message",
            "detail",
            "msg",
            "error",
            "response",
            "errorMessage",
            // Joi validation in the gateway Lambdas: `[{success, data, errors: [...]}]`.
            "errors",
        ]
        .iter()
        .find_map(|key| map.get(*key).and_then(message_of)),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(message_of).collect();
            (!parts.is_empty()).then(|| parts.join("; "))
        }
        _ => None,
    }
}

/// Providers often answer HTTP 200 with `{success: false}` or `{status: 4xx}`;
/// treat those as failures so scripts see a non-zero exit.
pub fn check_envelope(value: &Value, method: &str, url: &str) -> Result<()> {
    let Value::Object(map) = value else {
        return Ok(());
    };
    if map.get("success") == Some(&Value::Bool(false)) {
        let detail = message_of(value).unwrap_or_else(|| "request failed".to_string());
        anyhow::bail!("{method} {url} → {detail}");
    }
    if let Some(status) = map.get("status").and_then(Value::as_i64) {
        if status >= 400 {
            let detail = map.get("response").and_then(message_of).unwrap_or_default();
            anyhow::bail!("{method} {url} → status {status}: {detail}");
        }
    }
    Ok(())
}

pub fn parse_method(raw: &str) -> Result<Method> {
    match raw.to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::GET),
        "POST" => Ok(Method::POST),
        "PUT" => Ok(Method::PUT),
        "PATCH" => Ok(Method::PATCH),
        "DELETE" => Ok(Method::DELETE),
        "HEAD" => Ok(Method::HEAD),
        other => {
            anyhow::bail!("unsupported method '{other}' (use GET, POST, PUT, PATCH, DELETE, HEAD)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn table_client() -> ApiClient {
        let resolved = config::resolve(
            &config::Config::default(),
            Some("spinner-test"),
            Some("http://127.0.0.1:9".to_string()),
            Some("stub-token".to_string()),
        );
        ApiClient::new(resolved, OutputFormat::Table, false, 0, 5).unwrap()
    }

    #[test]
    fn caller_progress_hides_request_spinners_on_every_clone_until_dropped() {
        let http = table_client();
        let lakehouse_copy = http.clone();
        assert!(lakehouse_copy.shows_request_spinner(true));

        let first = http.caller_progress();
        let second = http.caller_progress();
        assert!(!lakehouse_copy.shows_request_spinner(true));
        drop(first);
        assert!(!lakehouse_copy.shows_request_spinner(true));
        drop(second);
        assert!(lakehouse_copy.shows_request_spinner(true));
        assert!(!lakehouse_copy.shows_request_spinner(false));
    }

    fn session(expires_at: Option<i64>, from_config: bool, refresh_failed: bool) -> Session {
        Session {
            token: Some("t".into()),
            expires_at,
            refresh_token: Some("r".into()),
            access_token: None,
            pool: None,
            from_config,
            refresh_failed,
        }
    }

    #[test]
    fn tokens_are_renewed_inside_the_margin_only() {
        let now = 1_000_000;
        let soon = session(Some(now + 300), true, false);
        assert!(!soon.should_refresh(now, REFRESH_MARGIN_SECS), "5 min left");
        assert!(
            soon.should_refresh(now, REFRESH_AHEAD_SECS),
            "a warm-up renews it"
        );
        assert!(session(Some(now + 60), true, false).should_refresh(now, REFRESH_MARGIN_SECS));
        assert!(!session(Some(now + 3_600), true, false).should_refresh(now, REFRESH_AHEAD_SECS));
        assert!(
            !session(Some(now), false, false).should_refresh(now, 0),
            "GZ_TOKEN is not ours"
        );
        assert!(
            !session(Some(now), true, true).should_refresh(now, 0),
            "after a failure"
        );
        assert!(!session(None, true, false).should_refresh(now, REFRESH_AHEAD_SECS));
    }

    #[tokio::test]
    async fn a_warm_up_opens_a_connection_with_the_health_route() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        let resolved = config::resolve(
            &config::Config::default(),
            Some("warm-test"),
            Some(base),
            Some("stub-token".to_string()),
        );
        let client = ApiClient::new(resolved, OutputFormat::Json, true, 0, 5).unwrap();
        client.warm_up("chat").await;
        let request = seen.join().unwrap();
        assert!(request.starts_with("GET /health HTTP/1.1"), "{request}");
        assert!(
            !request.contains("stub-token"),
            "no credentials on a warm-up"
        );
    }

    #[test]
    fn rejects_error_envelopes() {
        assert!(check_envelope(&json!({"success": false, "message": "nope"}), "GET", "u").is_err());
        assert!(check_envelope(&json!({"status": 400, "response": "bad"}), "GET", "u").is_err());
        assert!(check_envelope(&json!({"success": true, "data": [1]}), "GET", "u").is_ok());
        assert!(check_envelope(&json!({"status": 200, "response": []}), "GET", "u").is_ok());
        assert!(check_envelope(&json!([1, 2]), "GET", "u").is_ok());
    }

    #[test]
    fn readable_errors_drop_the_request_line() {
        assert_eq!(
            readable_error("POST https://x/chat/runs/r/client-tools/resume → HTTP 422 Unprocessable Entity: too many results"),
            "HTTP 422 Unprocessable Entity: too many results"
        );
        assert_eq!(readable_error("stream interrupted"), "stream interrupted");
        assert_eq!(
            error_detail(
                r#"{"detail":[{"loc":["body","toolResults"],"msg":"List should have at most 20 items"}]}"#
            ),
            "List should have at most 20 items"
        );
    }

    #[test]
    fn extracts_readable_error_messages() {
        assert_eq!(
            error_detail(r#"{"message":"Unauthorized"}"#),
            "Unauthorized"
        );
        assert_eq!(error_detail(r#"{"detail":"Not found"}"#), "Not found");
        assert_eq!(
            error_detail(r#"{"status":400,"response":{"message":"bad name"}}"#),
            "bad name"
        );
        assert_eq!(error_detail(r#"[{"message":"a"},{"message":"b"}]"#), "a; b");
        assert_eq!(error_detail("plain text"), "plain text");
    }

    #[test]
    fn reads_gateway_validation_errors_without_echoing_the_body() {
        let body = r#"[{"success":false,"data":{"name":"a b"},"errors":["cron, must have at least 17 characters!","name is invalid"]}]"#;
        assert_eq!(
            error_detail(body),
            "cron, must have at least 17 characters!; name is invalid"
        );
    }

    /// A server that answers each request with the next status in
    /// `statuses` (the last one repeats) and records each request's head.
    fn scripted_server(statuses: Vec<u16>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let heads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = heads.clone();
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { continue };
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(read) = stream.read(&mut buf) else {
                        break;
                    };
                    if read == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..read]);
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&raw).to_lowercase());
                let status = statuses[n.min(statuses.len() - 1)];
                let body = r#"{"ok":true}"#;
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).ok();
            }
        });
        (base, heads)
    }

    fn client_for(base: String) -> ApiClient {
        let resolved = config::resolve(
            &config::Config::default(),
            Some("retry-test"),
            Some(base),
            Some("stub-token".to_string()),
        );
        ApiClient::new(resolved, OutputFormat::Json, true, 0, 5).unwrap()
    }

    #[tokio::test]
    async fn a_get_is_tried_again_after_a_server_error() {
        let (base, heads) = scripted_server(vec![503, 502, 200]);
        let http = client_for(base);
        let reply = http
            .request_json("chat", Method::GET, "/chat/runs/r1", &[], None)
            .await
            .unwrap();
        assert_eq!(reply["ok"], true);
        assert_eq!(heads.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_plain_post_is_never_sent_twice() {
        let (base, heads) = scripted_server(vec![503, 200]);
        let http = client_for(base);
        let failed = http
            .request_json("etl", Method::POST, "/etl/jobs", &[], Some(json!({})))
            .await;
        assert!(failed.is_err());
        assert_eq!(heads.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_chat_post_with_a_request_id_is_retried_with_the_same_id() {
        let (base, heads) = scripted_server(vec![503, 200]);
        let http = client_for(base);
        let headers = [("X-Request-ID", "req-1".to_string())];
        let reply = http
            .request_json_with(
                "chat",
                Method::POST,
                "/chat/runs",
                &[],
                Some(json!({})),
                &headers,
            )
            .await
            .unwrap();
        assert_eq!(reply["ok"], true);
        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 2);
        assert!(heads.iter().all(|h| h.contains("x-request-id: req-1")));
    }

    #[tokio::test]
    async fn a_get_that_times_out_is_sent_once() {
        // Accepts connections and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = accepted.clone();
        std::thread::spawn(move || {
            let mut open = Vec::new();
            for stream in listener.incoming().flatten() {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                open.push(stream);
            }
        });
        let resolved = config::resolve(
            &config::Config::default(),
            Some("retry-test"),
            Some(base),
            Some("stub-token".to_string()),
        );
        let http = ApiClient::new(resolved, OutputFormat::Json, true, 0, 1).unwrap();
        let failed = http
            .request_json("chat", Method::GET, "/chat/runs/r1", &[], None)
            .await;
        assert!(failed.is_err());
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_connection_that_never_opens_is_tried_again() {
        // Nothing listens on port 9; every try fails to connect.
        let http = client_for("http://127.0.0.1:9".to_string());
        let started = std::time::Instant::now();
        let failed = http
            .request_json("etl", Method::POST, "/etl/jobs", &[], Some(json!({})))
            .await;
        assert!(failed.is_err());
        assert!(
            started.elapsed() >= Duration::from_millis(3400),
            "three waits before giving up"
        );
    }
}
