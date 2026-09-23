use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{multipart, Method};
use serde_json::Value;
use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

use crate::config::{self, Resolved};
use crate::output::OutputFormat;

#[derive(Debug, Clone)]
pub struct ApiClient {
    client: reqwest::Client,
    resolved: Resolved,
    format: OutputFormat,
    quiet: bool,
    verbose: u8,
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
        if resolved.token().is_some()
            && resolved.token_source == "config"
            && resolved.token_expired()
        {
            crate::output::warn("stored token is expired; run `gz auth login` again");
        }
        Ok(Self {
            client,
            resolved,
            format,
            quiet,
            verbose,
        })
    }

    pub fn resolved(&self) -> &Resolved {
        &self.resolved
    }

    pub fn clone_client(&self) -> Self {
        self.clone()
    }

    pub fn raw(&self) -> &reqwest::Client {
        &self.client
    }

    fn spinner(&self, message: &str) -> Option<ProgressBar> {
        let interactive =
            std::io::stderr().is_terminal() && !self.quiet && self.format == OutputFormat::Table;
        if !interactive {
            return None;
        }
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::with_template("{spinner} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner())
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"),
        );
        spinner.set_message(message.to_string());
        spinner.enable_steady_tick(Duration::from_millis(80));
        Some(spinner)
    }

    fn authed(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut request = request;
        if let Some(token) = self.resolved.token() {
            request = request.bearer_auth(token);
        }
        if let Some(site) = self.resolved.site() {
            request = request.header("gz-site", site);
        }
        request
    }

    pub async fn request_json(
        &self,
        service: &str,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let url = self
            .resolved
            .base_for(service)
            .map(|b| config::join_url(&b, path))?;
        self.exec_json(&url, method, query, body).await
    }

    pub async fn exec_json(
        &self,
        url: &str,
        method: Method,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let spinner = self.spinner(&format!("{method} {url}"));
        let mut request = self.authed(self.client.request(method.clone(), url));
        if !query.is_empty() {
            request = request.query(query);
        }
        let needs_body = matches!(
            method,
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        );
        if let Some(payload) = body {
            request = request.json(&payload);
        } else if needs_body {
            request = request.json(&serde_json::json!({}));
        }
        if self.verbose > 0 {
            eprintln!("> {method} {url}");
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("request failed: {method} {url}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if let Some(spinner) = spinner {
            spinner.finish_and_clear();
        }
        if self.verbose > 0 {
            eprintln!("< {status}");
            if self.verbose > 1 {
                eprintln!("{text}");
            }
        }
        if !status.is_success() {
            let snippet = text.chars().take(500).collect::<String>();
            anyhow::bail!("{method} {url} → HTTP {status}: {snippet}");
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        check_envelope(&parsed, method.as_str(), url)?;
        Ok(parsed)
    }

    pub async fn download(
        &self,
        service: &str,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
        dest: &Path,
    ) -> Result<u64> {
        let url = self
            .resolved
            .base_for(service)
            .map(|b| config::join_url(&b, path))?;
        let spinner = self.spinner(&format!("POST {url}"));
        let mut request = self.authed(self.client.request(Method::POST, &url));
        if !query.is_empty() {
            request = request.query(query);
        }
        request = request.json(&body.unwrap_or_else(|| serde_json::json!({})));
        let response = request
            .send()
            .await
            .with_context(|| format!("download failed: {url}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let snippet = text.chars().take(300).collect::<String>();
            anyhow::bail!("POST {url} → HTTP {status}: {snippet}");
        }
        let bytes = response
            .bytes()
            .await
            .context("cannot read download body")?;
        if let Some(spinner) = spinner {
            spinner.finish_and_clear();
        }
        std::fs::write(dest, &bytes).with_context(|| format!("cannot write {}", dest.display()))?;
        Ok(bytes.len() as u64)
    }

    pub async fn upload_file(
        &self,
        service: &str,
        path: &str,
        meta: &Value,
        file_path: &Path,
        file_field: &str,
        meta_field: Option<&str>,
    ) -> Result<Value> {
        let url = self
            .resolved
            .base_for(service)
            .map(|b| config::join_url(&b, path))?;
        let spinner = self.spinner(&format!("upload {}", file_path.display()));
        let file_name = file_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "upload.bin".to_string());
        let bytes = std::fs::read(file_path)
            .with_context(|| format!("cannot read {}", file_path.display()))?;
        let part = multipart::Part::bytes(bytes).file_name(file_name);
        let mut form = multipart::Form::new().part(file_field.to_string(), part);
        match meta_field {
            Some(field) => {
                form = form.text(field.to_string(), serde_json::to_string(meta)?);
            }
            None => {
                if let Value::Object(map) = meta {
                    for (key, val) in map {
                        let text = val
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| val.to_string());
                        form = form.text(key.clone(), text);
                    }
                }
            }
        }
        let request = self.authed(self.client.request(Method::POST, &url).multipart(form));
        let response = request
            .send()
            .await
            .with_context(|| format!("upload failed: {url}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if let Some(spinner) = spinner {
            spinner.finish_and_clear();
        }
        if !status.is_success() {
            let snippet = text.chars().take(500).collect::<String>();
            anyhow::bail!("POST {url} → HTTP {status}: {snippet}");
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).or_else(|_| Ok(Value::String(text)))
    }
}

/// Providers often answer HTTP 200 with `{success: false}` or `{status: 4xx}`;
/// treat those as failures so scripts see a non-zero exit.
pub fn check_envelope(value: &Value, method: &str, url: &str) -> Result<()> {
    let Value::Object(map) = value else {
        return Ok(());
    };
    if map.get("success") == Some(&Value::Bool(false)) {
        let detail = map
            .get("message")
            .or_else(|| map.get("error"))
            .map(|v| v.to_string())
            .unwrap_or_else(|| "request failed".to_string());
        anyhow::bail!("{method} {url} → success=false: {detail}");
    }
    if let Some(status) = map.get("status").and_then(Value::as_i64) {
        if status >= 400 {
            let detail = map
                .get("response")
                .map(|v| v.to_string())
                .unwrap_or_default();
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
}
