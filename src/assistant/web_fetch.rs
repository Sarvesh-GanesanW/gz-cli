//! `web_fetch`, like Claude Code's WebFetch: gz fetches a public page on
//! the user's machine (after asking, per host) and returns it as text — HTML
//! turned into Markdown-like text — a page at a time. It refuses localhost
//! and private addresses, upgrades http to https, and follows redirects only
//! within the same host; a redirect elsewhere is reported for a second call.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use serde_json::Value;
use url::Url;

const DEFAULT_PAGE_CHARS: usize = 20_000;
const MIN_PAGE_CHARS: usize = 1_000;
const MAX_PAGE_CHARS: usize = 100_000;
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const TIMEOUT: Duration = Duration::from_secs(30);
/// Column width the HTML is laid out at.
const TEXT_WIDTH: usize = 120;

/// A validated `web_fetch` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetch {
    pub url: Url,
    offset: usize,
    page_chars: usize,
}

impl Fetch {
    pub fn parse(input: &Value) -> Result<Self> {
        let raw = input["url"]
            .as_str()
            .filter(|u| !u.is_empty())
            .context("missing string field 'url'")?;
        let mut url = Url::parse(raw).with_context(|| format!("'{raw}' is not a URL"))?;
        match url.scheme() {
            "https" => {}
            // Like Claude Code: plain http is upgraded.
            "http" => {
                url.set_scheme("https").ok();
            }
            other => bail!("only http and https pages can be fetched, not {other}"),
        }
        check_host(&url)?;
        Ok(Self {
            url,
            offset: input["offset"].as_u64().unwrap_or(0) as usize,
            page_chars: input["max_chars"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_PAGE_CHARS)
                .clamp(MIN_PAGE_CHARS, MAX_PAGE_CHARS),
        })
    }

    /// The host a permission is remembered for.
    pub fn host(&self) -> String {
        self.url.host_str().unwrap_or_default().to_ascii_lowercase()
    }

    /// The tool result: one page of the text and how to read on.
    pub async fn run(&self) -> Result<String> {
        let (url, body, content_type) = fetch(&self.url).await?;
        let text = to_text(&body, &content_type)?;
        Ok(self.page(&url, &text))
    }

    fn page(&self, url: &Url, text: &str) -> String {
        let total = text.chars().count();
        if self.offset >= total {
            return format!(
                "{url}: no text at offset {} (the page has {total} characters)",
                self.offset
            );
        }
        let shown: String = text
            .chars()
            .skip(self.offset)
            .take(self.page_chars)
            .collect();
        let end = self.offset + shown.chars().count();
        let mut out = format!("{url}\n\n{shown}");
        if end < total {
            out.push_str(&format!(
                "\n\n… {} more characters; call web_fetch again with offset {end}",
                total - end
            ));
        }
        out
    }
}

/// Refuses hosts that aren't public sites by name: localhost, names
/// without a dot, and literal private addresses. Names are checked again
/// after DNS, in [`check_addresses`].
fn check_host(url: &Url) -> Result<()> {
    let host = url.host_str().context("the URL has no host")?;
    let lower = host.trim_end_matches('.').to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") {
        bail!("gz doesn't fetch from {host}: only public sites");
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => check_ip(IpAddr::V4(ip), host),
        Some(url::Host::Ipv6(ip)) => check_ip(IpAddr::V6(ip), host),
        _ if !lower.contains('.') => bail!("gz doesn't fetch from {host}: only public sites"),
        _ => Ok(()),
    }
}

fn check_ip(ip: IpAddr, host: &str) -> Result<()> {
    let private = match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                // 100.64.0.0/10 (carrier-grade NAT) and cloud metadata ranges.
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| check_ip(IpAddr::V4(v4), host).is_err())
        }
    };
    if private {
        bail!("gz doesn't fetch from {host}: it is a private or local address");
    }
    Ok(())
}

/// Every address `url`'s host resolves to must be public, so a public
/// name can't point gz at the user's own network. Returns the one to
/// connect to, which the request is pinned to so a second lookup can't
/// swap in another.
async fn checked_address(url: &Url) -> Result<SocketAddr> {
    let host = url.host_str().context("the URL has no host")?;
    let port = url.port_or_known_default().unwrap_or(443);
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("cannot find {host}"))?
        .collect();
    for address in &addresses {
        check_ip(address.ip(), host)?;
    }
    addresses
        .first()
        .copied()
        .with_context(|| format!("cannot find {host}"))
}

/// The body, the URL it finally came from, and its content type.
async fn fetch(start: &Url) -> Result<(Url, Vec<u8>, String)> {
    let mut url = start.clone();
    for _ in 0..=MAX_REDIRECTS {
        let address = checked_address(&url).await?;
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("gz/", env!("CARGO_PKG_VERSION")))
            .resolve(url.host_str().unwrap_or_default(), address)
            .build()
            .context("cannot set up the web client")?;
        let mut response = client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("cannot fetch {url}"))?;
        if response.status().is_redirection() {
            let next = response
                .headers()
                .get(LOCATION)
                .and_then(|l| l.to_str().ok())
                .and_then(|l| url.join(l).ok())
                .context("the page redirects without saying where")?;
            if next.host_str() != url.host_str() {
                bail!("{url} redirects to another site, {next}; fetch that URL to follow it");
            }
            url = next;
            continue;
        }
        if !response.status().is_success() {
            bail!("{url} answered HTTP {}", response.status());
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|c| c.to_str().ok())
            .unwrap_or("text/html")
            .to_ascii_lowercase();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.context("the download broke off")? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_BODY_BYTES {
                bail!("{url} is larger than {} MB", MAX_BODY_BYTES / 1024 / 1024);
            }
        }
        return Ok((url, body, content_type));
    }
    bail!("{start} redirects more than {MAX_REDIRECTS} times")
}

/// HTML as readable text; other text types as they are.
fn to_text(body: &[u8], content_type: &str) -> Result<String> {
    let textual = content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("javascript");
    if content_type.contains("html") {
        return html2text::from_read(body, TEXT_WIDTH).context("cannot read the page's HTML");
    }
    if !textual {
        bail!("the page is {content_type}, not text");
    }
    Ok(String::from_utf8_lossy(body).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(url: &str) -> Result<Fetch> {
        Fetch::parse(&json!({"url": url}))
    }

    #[test]
    fn public_pages_pass_and_http_is_upgraded() {
        let fetch = parse("http://docs.rs/serde").unwrap();
        assert_eq!(fetch.url.as_str(), "https://docs.rs/serde");
        assert_eq!(fetch.host(), "docs.rs");
    }

    #[test]
    fn local_and_private_hosts_are_refused() {
        for url in [
            "http://localhost:8080/",
            "https://intranet/",
            "https://printer.local/",
            "https://127.0.0.1/",
            "https://10.0.0.5/",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/",
            "file:///etc/passwd",
        ] {
            assert!(parse(url).is_err(), "{url} should be refused");
        }
    }

    #[test]
    fn pages_are_cut_into_parts_with_a_note() {
        let fetch =
            Fetch::parse(&json!({"url": "https://example.com", "max_chars": 1000})).unwrap();
        let text = "x".repeat(2500);
        let first = fetch.page(&fetch.url, &text);
        assert!(first.ends_with("… 1500 more characters; call web_fetch again with offset 1000"));
        let later = Fetch::parse(&json!({"url": "https://example.com", "offset": 2000})).unwrap();
        assert!(!later.page(&later.url, &text).contains("more characters"));
    }

    #[test]
    fn html_becomes_text_and_binaries_are_refused() {
        let html = b"<html><body><h1>Title</h1><p>Hello <a href=\"/x\">link</a></p></body></html>";
        let text = to_text(html, "text/html; charset=utf-8").unwrap();
        assert!(text.contains("Title") && text.contains("Hello"), "{text}");
        assert!(to_text(b"\x89PNG", "image/png").is_err());
        assert_eq!(
            to_text(b"{\"a\":1}", "application/json").unwrap(),
            "{\"a\":1}"
        );
    }
}
