use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{multipart, Method, RequestBuilder, Response};
use serde_json::Value;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::cognito::{self, PoolConfig};
use crate::config::{self, Resolved};
use crate::output::OutputFormat;

/// Refresh this many seconds before the ID token actually expires.
const REFRESH_MARGIN_SECS: i64 = 120;
const ERROR_SNIPPET_CHARS: usize = 500;
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
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

#[derive(Debug, Clone)]
pub struct ApiClient {
    client: reqwest::Client,
    resolved: Resolved,
    session: Arc<Mutex<Session>>,
    format: OutputFormat,
    quiet: bool,
    verbose: u8,
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
        })
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
        let interactive =
            std::io::stderr().is_terminal() && !self.quiet && self.format == OutputFormat::Table;
        if !interactive {
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
        let expiring = session
            .expires_at
            .is_some_and(|exp| config::now_unix() + REFRESH_MARGIN_SECS >= exp);
        if !(expiring && session.from_config && !session.refresh_failed) {
            return session.token.clone();
        }
        let (Some(refresh_token), Some(pool)) =
            (session.refresh_token.clone(), session.pool.clone())
        else {
            return session.token.clone();
        };
        match cognito::refresh(&self.client, &pool, &refresh_token).await {
            Ok(tokens) => {
                let expires_at = config::now_unix() + tokens.expires_in;
                session.token = Some(tokens.id_token.clone());
                session.access_token = tokens.access_token.clone();
                session.expires_at = Some(expires_at);
                if let Err(err) = persist_refresh(&self.resolved.profile_name, &tokens, expires_at)
                {
                    crate::output::warn(&format!(
                        "signed in, but cannot save the new token: {err:#}"
                    ));
                }
            }
            Err(err) => {
                session.refresh_failed = true;
                if !self.quiet {
                    crate::output::warn(&format!("{err:#}"));
                }
            }
        }
        session.token.clone()
    }

    /// The `Authorization` value for `service`: the raw ID token, except AdminQueries,
    /// which wants the Cognito access token.
    async fn authorization(&self, service: &str) -> Option<String> {
        let id_token = self.token().await?;
        match service {
            ADMIN_QUERIES => self.session.lock().await.access_token.clone(),
            _ => Some(id_token),
        }
    }

    /// Headers the web app sends: the raw ID token (see `authorization`), the tenant site as `Origin` + `gz-site`, and the
    /// lakehouse provider's `x-access-type: web` marker.
    async fn authed(&self, service: &str, request: RequestBuilder) -> RequestBuilder {
        let mut request = request;
        if let Some(value) = self.authorization(service).await {
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
        request
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
        let url = self.url_for(service, path)?;
        let _spinner = self.spinner(&format!("{method} {url}"));
        let mut request = self
            .authed(service, self.client.request(method.clone(), &url))
            .await;
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        if !query.is_empty() {
            request = request.query(query);
        }
        let sends_body = matches!(method, Method::POST | Method::PUT | Method::PATCH);
        if let Some(payload) = body {
            request = request.json(&payload);
        } else if sends_body {
            request = request.json(&serde_json::json!({}));
        }
        let response = self.send(request, &method, &url).await?;
        let text = response.text().await.unwrap_or_default();
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        check_envelope(&parsed, method.as_str(), &url)?;
        Ok(parsed)
    }

    /// GETs `path` and reports the HTTP status and round-trip time, whatever
    /// the status. Used by `gz doctor` to check reachability.
    pub async fn probe(&self, service: &str, path: &str) -> Result<(u16, Duration)> {
        let url = self.url_for(service, path)?;
        let request = self
            .authed(service, self.client.get(&url))
            .await
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
            .authed(service, self.client.request(method.clone(), &url))
            .await
            .header("Accept", "text/event-stream")
            .timeout(Duration::from_secs(60 * 60));
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(payload) = body {
            request = request.json(&payload);
        }
        self.send(request, &method, &url).await
    }

    async fn send(&self, request: RequestBuilder, method: &Method, url: &str) -> Result<Response> {
        if self.verbose > 0 {
            eprintln!("> {method} {url}");
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("request failed: {method} {url}"))?;
        let status = response.status();
        if self.verbose > 0 {
            eprintln!("< {status}");
        }
        if status.is_success() {
            return Ok(response);
        }
        let text = response.text().await.unwrap_or_default();
        if self.verbose > 1 {
            eprintln!("{text}");
        }
        anyhow::bail!("{method} {url} → HTTP {status}: {}", error_detail(&text))
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
        self.send(request, &method, url.split('?').next().unwrap_or(url))
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
            .authed(service, self.client.post(&url))
            .await
            .json(&body.unwrap_or_else(|| serde_json::json!({})));
        let response = self.send(request, &Method::POST, &url).await?;
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
            .authed(service, self.client.post(&url))
            .await
            .multipart(form);
        let response = self.send(request, &Method::POST, &url).await?;
        let text = response.text().await.unwrap_or_default();
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
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
        Value::Object(map) => ["message", "detail", "error", "response", "errorMessage"]
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

    #[test]
    fn rejects_error_envelopes() {
        assert!(check_envelope(&json!({"success": false, "message": "nope"}), "GET", "u").is_err());
        assert!(check_envelope(&json!({"status": 400, "response": "bad"}), "GET", "u").is_err());
        assert!(check_envelope(&json!({"success": true, "data": [1]}), "GET", "u").is_ok());
        assert!(check_envelope(&json!({"status": 200, "response": []}), "GET", "u").is_ok());
        assert!(check_envelope(&json!([1, 2]), "GET", "u").is_ok());
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
}
