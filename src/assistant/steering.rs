//! Messages the user sends while gz works, delivered within the running
//! turn: gz-chat-eks live steering
//! (`POST /chat/runs/{id}/steer`, api/routes/chat_steering.py) hands a note
//! to the model before its next step without stopping the run.
//!
//! The server takes notes only while the run is active. While it waits on
//! gz (a local tool or an approval) notes stay here without asking, and the
//! session sends them as soon as the resume hands back the continuing run:
//! the earliest moment the server takes them, so the model reads them at its
//! next step, after the tool results. The resumed run starts on the server
//! as the resume returns, so a note can still land just after that step
//! began; the server then hands it back unapplied when the turn ends. Notes
//! the model never read go out as the next messages.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::cmd::seg;
use crate::cmd::stream::run_headers;
use crate::http::ApiClient;

const CHAT: &str = "chat";
/// The longest note the server takes; longer ones go after the turn.
pub const MAX_NOTE_CHARS: usize = 2_000;
/// The run is paused on gz (local tools or an approval): ask again after
/// the resume (chat_steering.py `AWAITING_APPROVAL`).
const RUN_AWAITING_GZ: &str = "run_awaiting_approval";
/// The turn ended and its note queue closed (chat_steering.py `RUN_FINISHED`).
const RUN_NOT_ACTIVE: &str = "run_not_active";

/// Whether the run takes notes now, as far as gz knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Gate {
    /// The run is working: a note reaches the model at its next step.
    #[default]
    Open,
    /// The run waits on gz; notes wait for the resume.
    WaitingOnGz,
    /// The turn is over; notes wait to go as the next message.
    Closed,
}

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
    gate: Arc<Mutex<Gate>>,
    /// One delivery at a time, so notes reach the run in the order typed.
    sending: Arc<tokio::sync::Mutex<()>>,
}

impl Steering {
    pub fn new(http: ApiClient, active_run: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            http,
            active_run,
            pending: Arc::default(),
            gate: Arc::default(),
            sending: Arc::default(),
        }
    }

    /// Adds a note to send at the next chance.
    pub fn queue(&self, text: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.push_back(text.trim().to_string());
        }
    }

    /// The run paused on gz (its local tools or an approval): notes wait
    /// for the resume instead of being refused.
    pub fn run_waits_on_gz(&self) {
        self.set_gate(Gate::WaitingOnGz);
    }

    /// The run (or the one resuming it) is working and takes notes.
    pub fn run_takes_notes(&self) {
        self.set_gate(Gate::Open);
    }

    fn set_gate(&self, gate: Gate) {
        if let Ok(mut current) = self.gate.lock() {
            *current = gate;
        }
    }

    fn gate(&self) -> Gate {
        self.gate.lock().map(|g| *g).unwrap_or_default()
    }

    /// How many of the waiting notes, oldest first, go to the run as notes
    /// at its next step; the rest go as the next message after the turn
    /// (a note too long for the server holds back the ones after it, so
    /// the order holds).
    pub fn deliverable(&self) -> usize {
        if self.gate() == Gate::Closed {
            return 0;
        }
        self.pending
            .lock()
            .map(|p| p.iter().take_while(|text| fits_a_note(text)).count())
            .unwrap_or_default()
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
    /// note the run refuses (it waits on gz, or the turn ended) and every
    /// note after it stay for later, so the order holds; those refusals are
    /// the server's normal answers and are not logged.
    pub async fn deliver(&self) -> Vec<SentNote> {
        let _one_at_a_time = self.sending.lock().await;
        let mut sent = Vec::new();
        while self.gate() == Gate::Open {
            let Some(run) = self.active_run.lock().ok().and_then(|r| r.clone()) else {
                break;
            };
            let Some(text) = self.pending.lock().ok().and_then(|p| p.front().cloned()) else {
                break;
            };
            if !fits_a_note(&text) {
                break;
            }
            let headers = run_headers(&self.http).await;
            let path = format!("/chat/runs/{}/steer", seg(&run));
            let reply = self
                .http
                .post_json_or_refusal(
                    CHAT,
                    &path,
                    json!({"message": text}),
                    &headers,
                    &[RUN_AWAITING_GZ, RUN_NOT_ACTIVE],
                )
                .await;
            let reply = match reply {
                Ok(Ok(reply)) => reply,
                // A refusal for a run the resume already replaced says
                // nothing about the new one: try that one instead.
                Ok(Err(_)) if !self.is_active(&run) => continue,
                Ok(Err(refusal)) => {
                    self.set_gate(match refusal.code.as_str() {
                        RUN_AWAITING_GZ => Gate::WaitingOnGz,
                        _ => Gate::Closed,
                    });
                    break;
                }
                // Logged by the client; the note waits for the next try.
                Err(_) => break,
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

impl Steering {
    fn is_active(&self, run: &str) -> bool {
        self.active_run
            .lock()
            .is_ok_and(|active| active.as_deref() == Some(run))
    }
}

fn fits_a_note(text: &str) -> bool {
    text.chars().count() <= MAX_NOTE_CHARS
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
        assert!(
            crate::error_log::test_log::urls().is_empty(),
            "a paused run's refusal is an answer, not an error"
        );
        assert_eq!(steering.deliverable(), 2, "they still go at the next step");
    }

    #[tokio::test]
    async fn no_note_is_sent_while_the_run_waits_on_gz() {
        let (base, seen) = stub_server(vec![(202, r#"{"accepted": true, "steerId": "steer_1"}"#)]);
        let steering = steering(base, Some("chatrun_1"));
        steering.run_waits_on_gz();
        steering.queue("one");
        assert!(steering.deliver().await.is_empty());
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing asked while paused"
        );
        steering.run_takes_notes();
        assert_eq!(steering.deliver().await.len(), 1);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn after_the_turn_closes_notes_wait_for_the_next_message() {
        let (base, _seen) = stub_server(vec![(
            409,
            r#"{"detail": {"error": "run_not_active", "message": "finished"}}"#,
        )]);
        let steering = steering(base, Some("chatrun_1"));
        steering.queue("one");
        steering.queue("two");
        assert!(steering.deliver().await.is_empty());
        assert!(crate::error_log::test_log::urls().is_empty());
        assert_eq!(steering.deliverable(), 0, "both go after the turn");
        assert_eq!(steering.take_all(), vec!["one", "two"], "kept, in order");
    }

    #[tokio::test]
    async fn an_unexpected_conflict_is_still_reported() {
        let (base, _seen) = stub_server(vec![(
            409,
            r#"{"detail": {"error": "idempotency_conflict", "message": "reused"}}"#,
        )]);
        let steering = steering(base, Some("chatrun_1"));
        steering.queue("one");
        assert!(steering.deliver().await.is_empty());
        let logged = crate::error_log::test_log::urls();
        assert!(
            logged
                .iter()
                .any(|url| url.ends_with("/chat/runs/chatrun_1/steer")),
            "{logged:?}"
        );
        assert_eq!(steering.pending(), vec!["one"]);
    }

    #[test]
    fn a_note_too_long_holds_back_the_ones_after_it() {
        let steering = steering("http://127.0.0.1:9".into(), Some("chatrun_1"));
        steering.queue("short");
        steering.queue(&"x".repeat(MAX_NOTE_CHARS + 1));
        steering.queue("after");
        assert_eq!(steering.deliverable(), 1);
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
