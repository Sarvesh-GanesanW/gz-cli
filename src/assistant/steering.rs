//! Messages the user sends while gz works, delivered within the running
//! turn: gz-chat-eks live steering
//! (`POST /chat/runs/{id}/steer`, api/routes/chat_steering.py) hands a note
//! to the model before its next step without stopping the run.
//!
//! The server takes notes only while the run is active. While it waits on
//! gz (a local tool or an approval) notes stay here, and the session sends
//! them as soon as the run resumes, so the model reads them right after the
//! tool calls finish. Notes still here when the turn ends go out as the next
//! messages.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use reqwest::Method;
use serde_json::json;

use crate::cmd::seg;
use crate::cmd::stream::run_headers;
use crate::http::ApiClient;

const CHAT: &str = "chat";
/// The longest note the server takes; longer ones go after the turn.
pub const MAX_NOTE_CHARS: usize = 2_000;

/// A note the run took; the model reads it before its next step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentNote {
    pub steer_id: String,
    pub text: String,
}

/// The notes waiting for the run, shared by the UI and the session.
#[derive(Clone)]
pub struct Steering {
    http: ApiClient,
    active_run: Arc<Mutex<Option<String>>>,
    pending: Arc<Mutex<VecDeque<String>>>,
    /// One delivery at a time, so notes reach the run in the order typed.
    sending: Arc<tokio::sync::Mutex<()>>,
}

impl Steering {
    pub fn new(http: ApiClient, active_run: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            http,
            active_run,
            pending: Arc::default(),
            sending: Arc::default(),
        }
    }

    /// Adds a note to send at the next chance.
    pub fn queue(&self, text: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.push_back(text.trim().to_string());
        }
    }

    /// The notes not yet delivered, oldest first.
    pub fn pending(&self) -> Vec<String> {
        self.pending
            .lock()
            .map(|p| p.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Removes and returns every note not yet delivered (to edit them, or
    /// to send them as messages once the turn ends).
    pub fn take_all(&self) -> Vec<String> {
        self.pending
            .lock()
            .map(|mut p| p.drain(..).collect())
            .unwrap_or_default()
    }

    /// Sends the waiting notes, oldest first, while the run takes them. A
    /// note the run refuses (it waits on gz, or it ended) and every note
    /// after it stay for the next try, so the order holds.
    pub async fn deliver(&self) -> Vec<SentNote> {
        let _one_at_a_time = self.sending.lock().await;
        let mut sent = Vec::new();
        loop {
            let Some(run) = self.active_run.lock().ok().and_then(|r| r.clone()) else {
                break;
            };
            let Some(text) = self.pending.lock().ok().and_then(|p| p.front().cloned()) else {
                break;
            };
            if text.chars().count() > MAX_NOTE_CHARS {
                break;
            }
            let headers = run_headers(&self.http).await;
            let path = format!("/chat/runs/{}/steer", seg(&run));
            let reply = self
                .http
                .request_json_with(
                    CHAT,
                    Method::POST,
                    &path,
                    &[],
                    Some(json!({"message": text})),
                    &headers,
                )
                .await;
            let Ok(reply) = reply else {
                break;
            };
            if let Ok(mut pending) = self.pending.lock() {
                pending.pop_front();
            }
            sent.push(SentNote {
                steer_id: reply["steerId"].as_str().unwrap_or_default().to_string(),
                text,
            });
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    type Seen = Arc<Mutex<Vec<(String, Value)>>>;

    /// A chat server answering one request per scripted (status, body).
    fn stub_server(replies: Vec<(u16, &'static str)>) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = Arc::default();
        let record = seen.clone();
        std::thread::spawn(move || {
            for (status, reply) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                let (line, body) = loop {
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
                    if raw.len() >= split + 4 + length {
                        let line = text.lines().next().unwrap_or_default().to_string();
                        break (
                            line,
                            serde_json::from_str(&text[split + 4..]).unwrap_or(Value::Null),
                        );
                    }
                };
                record.lock().unwrap().push((line, body));
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (base, seen)
    }

    fn steering(base: String, run: Option<&str>) -> Steering {
        let resolved = crate::config::resolve(
            &crate::config::Config::default(),
            Some("steering-test"),
            Some(base),
            Some("stub-token".to_string()),
        );
        let http = ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5).unwrap();
        Steering::new(http, Arc::new(Mutex::new(run.map(str::to_string))))
    }

    #[tokio::test]
    async fn notes_go_to_the_run_in_the_order_typed() {
        let (base, seen) = stub_server(vec![
            (202, r#"{"accepted": true, "steerId": "steer_1"}"#),
            (202, r#"{"accepted": true, "steerId": "steer_2"}"#),
        ]);
        let steering = steering(base, Some("chatrun_1"));
        steering.queue(" use python ");
        steering.queue("and add tests");
        let sent = steering.deliver().await;
        assert_eq!(
            sent,
            vec![
                SentNote {
                    steer_id: "steer_1".into(),
                    text: "use python".into()
                },
                SentNote {
                    steer_id: "steer_2".into(),
                    text: "and add tests".into()
                },
            ]
        );
        assert!(steering.pending().is_empty());
        let seen = seen.lock().unwrap();
        assert!(
            seen[0].0.starts_with("POST /chat/runs/chatrun_1/steer"),
            "{}",
            seen[0].0
        );
        assert_eq!(seen[0].1, serde_json::json!({"message": "use python"}));
    }

    #[tokio::test]
    async fn a_refused_note_and_the_ones_after_it_wait_for_the_next_try() {
        let (base, _seen) = stub_server(vec![
            (202, r#"{"accepted": true, "steerId": "steer_1"}"#),
            (409, r#"{"detail": {"error": "run_awaiting_approval"}}"#),
        ]);
        let steering = steering(base, Some("chatrun_1"));
        for note in ["one", "two", "three"] {
            steering.queue(note);
        }
        assert_eq!(steering.deliver().await.len(), 1);
        assert_eq!(steering.pending(), vec!["two", "three"]);
    }

    #[tokio::test]
    async fn nothing_is_sent_without_a_run_or_past_the_note_limit() {
        let idle = steering("http://127.0.0.1:9".into(), None);
        idle.queue("hello");
        assert!(idle.deliver().await.is_empty());
        assert_eq!(idle.take_all(), vec!["hello"]);

        let running = steering("http://127.0.0.1:9".into(), Some("chatrun_1"));
        running.queue(&"x".repeat(MAX_NOTE_CHARS + 1));
        assert!(running.deliver().await.is_empty(), "too long for a note");
        assert_eq!(running.pending().len(), 1);
    }
}
