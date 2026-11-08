//! When the chat summarizes older messages (auto-compact), and compacting a
//! conversation now (`/compact`), like Claude Code. The threshold is a token
//! count in 100k steps, sent as `autoCompactTokens` with every run; the
//! server clamps it to the model's window (gz-chat-eks
//! agent/context_reduction.py). `/compact` calls
//! `POST /chat/conversations/{id}/compact` (gz-chat-eks
//! api/routes/chat/conversation_compaction.py).

use anyhow::{bail, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cmd::seg;
use crate::http::ApiClient;

/// Thresholds come in these steps (the server refuses anything else).
pub const STEP: u64 = 100_000;
/// The picker's top step when the model's window isn't known yet.
const FALLBACK_WINDOW: u64 = 1_000_000;
/// The word that clears the setting back to the server's default.
pub const DEFAULT_WORD: &str = "default";

/// Reads `300k`, `300K`, `300000` or `300,000` as tokens, and `default` as
/// `None`. Anything that isn't a positive multiple of 100k is refused.
pub fn parse(text: &str) -> Result<Option<u64>> {
    let text = text.trim().to_ascii_lowercase().replace([',', '_'], "");
    if text == DEFAULT_WORD {
        return Ok(None);
    }
    let (digits, scale) = match text.strip_suffix('k') {
        Some(digits) => (digits, 1_000),
        None => (text.as_str(), 1),
    };
    let tokens = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .filter(|n| *n > 0 && n % STEP == 0);
    match tokens {
        Some(tokens) => Ok(Some(tokens)),
        None => bail!(
            "auto-compact takes tokens in 100k steps (100k, 200k, 300k, …) or `{DEFAULT_WORD}`, not `{text}`"
        ),
    }
}

/// `300k`, `1M`, or `default` when unset.
pub fn label(tokens: Option<u64>) -> String {
    match tokens {
        Some(tokens) if tokens % 1_000_000 == 0 => format!("{}M", tokens / 1_000_000),
        Some(tokens) => format!("{}k", tokens / 1_000),
        None => DEFAULT_WORD.to_string(),
    }
}

/// The picker's choices: 100k up to the window rounded down to 100k (at
/// least one step), or up to 1M while the window is unknown.
pub fn steps(window: Option<u64>) -> Vec<u64> {
    let top = (window.unwrap_or(FALLBACK_WINDOW) / STEP).max(1);
    (1..=top).map(|n| n * STEP).collect()
}

/// What `/compact` did, as the server reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// False when only the latest turns remained, so nothing changed.
    pub compacted: bool,
    pub summarized_messages: u64,
    pub before_tokens: u64,
    pub after_tokens: u64,
}

impl Report {
    fn from_reply(reply: &Value) -> Result<Self> {
        let count = |key: &str| reply[key].as_u64();
        let (Some(compacted), Some(summarized), Some(before), Some(after)) = (
            reply["compacted"].as_bool(),
            count("summarizedMessages"),
            count("beforeTokens"),
            count("afterTokens"),
        ) else {
            bail!("the chat server sent an unexpected compaction reply; is it up to date?");
        };
        Ok(Self {
            compacted,
            summarized_messages: summarized,
            before_tokens: before,
            after_tokens: after,
        })
    }
}

/// Summarizes the conversation's older turns on the server; `instructions`
/// focuses the summary.
pub async fn compact(
    http: &ApiClient,
    conversation_id: &str,
    instructions: Option<&str>,
) -> Result<Report> {
    let path = format!("/chat/conversations/{}/compact", seg(conversation_id));
    let body = match instructions {
        Some(text) => json!({"instructions": text}),
        None => json!({}),
    };
    let reply = http
        .request_json("chat", Method::POST, &path, &[], Some(body))
        .await?;
    Report::from_reply(&reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// The request line and JSON body the stub received.
    type Seen = Arc<Mutex<Option<(String, Value)>>>;

    /// A chat server that answers one `/compact` and keeps what it got.
    fn stub_server(reply: &'static str) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(None));
        let record = seen.clone();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                let Some(split) = text.find("\r\n\r\n") else {
                    continue;
                };
                let length = text[..split]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if raw.len() < split + 4 + length {
                    continue;
                }
                let request_line = text.lines().next().unwrap_or_default().to_string();
                let body = serde_json::from_str(&text[split + 4..]).unwrap_or(Value::Null);
                *record.lock().unwrap() = Some((request_line, body));
                break;
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (base, seen)
    }

    fn client(base: String) -> ApiClient {
        let resolved = crate::config::resolve(
            &crate::config::Config::default(),
            Some("compaction-test"),
            Some(base),
            Some("stub-token".to_string()),
        );
        ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5).unwrap()
    }

    #[tokio::test]
    async fn compact_posts_the_focus_to_the_conversation() {
        let (base, seen) = stub_server(
            r#"{"compacted":true,"summarizedMessages":4,"beforeTokens":900,"afterTokens":120}"#,
        );
        let report = compact(&client(base), "conv 1", Some("keep the schema"))
            .await
            .unwrap();
        assert_eq!(report.summarized_messages, 4);
        let (request_line, body) = seen.lock().unwrap().clone().unwrap();
        assert!(
            request_line.starts_with("POST /chat/conversations/conv%201/compact "),
            "{request_line}"
        );
        assert_eq!(body, json!({"instructions": "keep the schema"}));
    }

    #[test]
    fn reads_thresholds_in_100k_steps() {
        assert_eq!(parse("300k").unwrap(), Some(300_000));
        assert_eq!(parse(" 200K ").unwrap(), Some(200_000));
        assert_eq!(parse("300000").unwrap(), Some(300_000));
        assert_eq!(parse("1,000,000").unwrap(), Some(1_000_000));
        assert_eq!(parse("Default").unwrap(), None);
        for bad in [
            "150k",
            "0",
            "0k",
            "-100k",
            "60",
            "lots",
            "",
            "99999999999999999999k",
        ] {
            let err = parse(bad).unwrap_err().to_string();
            assert!(err.contains("100k steps"), "{bad}: {err}");
        }
    }

    #[test]
    fn offers_steps_up_to_the_window() {
        assert_eq!(steps(Some(872_000)).last(), Some(&800_000));
        assert_eq!(steps(Some(872_000)).len(), 8);
        assert_eq!(steps(Some(64_000)), vec![100_000], "always one choice");
        assert_eq!(steps(None).last(), Some(&1_000_000));
        assert_eq!(label(Some(300_000)), "300k");
        assert_eq!(label(Some(1_000_000)), "1M");
        assert_eq!(label(None), "default");
    }

    #[test]
    fn reads_the_compaction_reply() {
        let report = Report::from_reply(&json!({
            "conversationId": "c1", "compacted": true, "summarizedMessages": 40,
            "totalMessages": 44, "beforeTokens": 61234, "afterTokens": 3120,
            "beforeReplayedMessages": 44, "afterReplayedMessages": 4
        }))
        .unwrap();
        assert_eq!(
            report,
            Report {
                compacted: true,
                summarized_messages: 40,
                before_tokens: 61_234,
                after_tokens: 3_120
            }
        );
        assert!(Report::from_reply(&json!({"ok": true})).is_err());
    }
}
