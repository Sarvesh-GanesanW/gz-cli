//! Messages the user sends while gz works, delivered within the running
//! turn: gz-chat-eks live steering
//! (`POST /chat/runs/{id}/steer`, api/routes/chat_steering.py) hands a note
//! to the model before its next step without stopping the run.
//!
//! The server takes notes only while the run is active. While it waits on
//! gz (a local tool or an approval) notes stay here without asking, and the
//! UI sends them, apart from the turn, once the resume hands back the
//! continuing run. That run starts its first step on the server as the
//! resume returns, before gz can know its id, so such a note usually
//! misses that step; the server
//! then hands it back unapplied when the turn ends, and notes the model
//! never read go out as the next messages, once each.

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
/// Too many notes wait for the run's next step (chat_steering.py
/// `QUEUE_FULL`, a 429): ask again at the next delivery.
const TOO_MANY_NOTES: &str = "too_many_pending_steers";

/// Whether the run takes notes now, as far as gz knows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Gate {
    /// The run is working: a note reaches the model at its next step.
    #[default]
    Open,
    /// The run waits on gz; notes wait for the resume.
    WaitingOnGz,
    /// This run's turn is over; its notes wait to go as the next message.
    /// It says nothing about the next turn's run.
    Closed(String),
}

/// A note the run took; the model reads it before its next step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentNote {
    pub steer_id: String,
    pub text: String,
}

/// A waiting note; the id tells it apart from an identical retyped one.
#[derive(Debug, Clone)]
struct Note {
    id: u64,
    text: String,
}

/// The notes not yet delivered. The one whose steer POST is on the way
/// sits apart: the server may already have it, so ↑ cannot take it back,
/// and only the turn's end (or a stop) can claim it, after which its late
/// acceptance is dropped instead of reported.
#[derive(Debug, Default)]
struct Notes {
    waiting: VecDeque<Note>,
    in_flight: Option<Note>,
    next_id: u64,
}

impl Notes {
    /// Moves the oldest note into the in-flight slot, if it fits a note.
    fn start_sending(&mut self) -> Option<Note> {
        if !fits_a_note(&self.waiting.front()?.text) {
            return None;
        }
        let note = self.waiting.pop_front()?;
        self.in_flight = Some(note.clone());
        Some(note)
    }

    /// The note's delivery is over: true when it was still in flight,
    /// not claimed by the turn's end or a stop meanwhile.
    fn settle(&mut self, note: &Note) -> bool {
        let ours = self.in_flight.as_ref().is_some_and(|n| n.id == note.id);
        if ours {
            self.in_flight = None;
        }
        ours
    }

    /// The run did not take the note: it goes back first in line, unless
    /// the turn's end or a stop claimed it meanwhile.
    fn not_taken(&mut self, note: Note) {
        if self.settle(&note) {
            self.waiting.push_front(note);
        }
    }
}

/// The notes waiting for the run, shared by the UI and the session.
#[derive(Clone)]
pub struct Steering {
    http: ApiClient,
    active_run: Arc<Mutex<Option<String>>>,
    notes: Arc<Mutex<Notes>>,
    gate: Arc<Mutex<Gate>>,
    /// One delivery at a time, so notes reach the run in the order typed.
    sending: Arc<tokio::sync::Mutex<()>>,
}

impl Steering {
    pub fn new(http: ApiClient, active_run: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            http,
            active_run,
            notes: Arc::default(),
            gate: Arc::default(),
            sending: Arc::default(),
        }
    }

    /// Adds a note to send at the next chance.
    pub fn queue(&self, text: &str) {
        if let Ok(mut notes) = self.notes.lock() {
            let id = notes.next_id;
            notes.next_id += 1;
            notes.waiting.push_back(Note {
                id,
                text: text.trim().to_string(),
            });
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

    /// Whether the run waits on gz, so a note waits for the resume and may
    /// reach the model only after the resumed run's first step.
    pub fn waits_on_gz(&self) -> bool {
        self.gate() == Gate::WaitingOnGz
    }

    fn set_gate(&self, gate: Gate) {
        if let Ok(mut current) = self.gate.lock() {
            *current = gate;
        }
    }

    fn gate(&self) -> Gate {
        self.gate.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Whether the run takes notes now; a closed turn closes only its own
    /// run, so the next turn's notes are not held back by it.
    fn takes_notes(&self) -> bool {
        match self.gate() {
            Gate::Open => true,
            Gate::WaitingOnGz => false,
            Gate::Closed(run) => !self.is_active(&run),
        }
    }

    /// How many of the waiting notes, oldest first, go to the run as notes
    /// at its next step; the rest go as the next message after the turn
    /// (a note too long for the server holds back the ones after it, so
    /// the order holds).
    pub fn deliverable(&self) -> usize {
        if !self.takes_notes() && !self.waits_on_gz() {
            return 0;
        }
        self.notes
            .lock()
            .map(|n| {
                n.waiting
                    .iter()
                    .take_while(|n| fits_a_note(&n.text))
                    .count()
            })
            .unwrap_or_default()
    }

    /// The notes not yet delivered, oldest first, without the one on its
    /// way to the run.
    pub fn pending(&self) -> Vec<String> {
        self.notes
            .lock()
            .map(|n| n.waiting.iter().map(|n| n.text.clone()).collect())
            .unwrap_or_default()
    }

    /// The note on its way to the run, which can no longer be taken back.
    pub fn sending(&self) -> Option<String> {
        self.notes
            .lock()
            .ok()
            .and_then(|n| n.in_flight.as_ref().map(|n| n.text.clone()))
    }

    /// Removes and returns every waiting note (to edit them), leaving the
    /// one on its way to the run.
    pub fn take_all(&self) -> Vec<String> {
        self.notes
            .lock()
            .map(|mut n| n.waiting.drain(..).map(|n| n.text).collect())
            .unwrap_or_default()
    }

    /// The model read a note gz is still sending (its `steer_applied`
    /// came before the reply): when it is the one on its way, it is
    /// delivered, and its late acceptance is dropped. Notes go one at a
    /// time, so the text tells which.
    pub fn read_while_sending(&self, text: &str) -> bool {
        self.notes.lock().is_ok_and(|mut n| {
            let ours = n.in_flight.as_ref().is_some_and(|note| note.text == text);
            if ours {
                n.in_flight = None;
            }
            ours
        })
    }

    /// The turn ended or was stopped: removes and returns every note not
    /// yet delivered, the one on its way first. That one now belongs to
    /// the caller, so a late acceptance of it is not reported as sent: the
    /// run it went to is over and hands back what it never read.
    pub fn claim_all(&self) -> Vec<String> {
        self.notes
            .lock()
            .map(|mut n| {
                let in_flight = n.in_flight.take().map(|n| n.text);
                in_flight
                    .into_iter()
                    .chain(n.waiting.drain(..).map(|n| n.text))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Sends the waiting notes, oldest first, while the run takes them. A
    /// note the run refuses (it waits on gz, the turn ended, or too many
    /// notes wait) and every note after it stay for later, so the order
    /// holds; those refusals are the server's normal answers and are not
    /// logged.
    pub async fn deliver(&self) -> Vec<SentNote> {
        let _one_at_a_time = self.sending.lock().await;
        let mut sent = Vec::new();
        while self.takes_notes() {
            let Some(run) = self.active_run.lock().ok().and_then(|r| r.clone()) else {
                break;
            };
            let Some(note) = self.notes.lock().ok().and_then(|mut n| n.start_sending()) else {
                break;
            };
            let headers = run_headers(&self.http).await;
            let path = format!("/chat/runs/{}/steer", seg(&run));
            let reply = self
                .http
                .post_json_or_refusal(
                    CHAT,
                    &path,
                    json!({"message": note.text}),
                    &headers,
                    &[RUN_AWAITING_GZ, RUN_NOT_ACTIVE, TOO_MANY_NOTES],
                )
                .await;
            let reply = match reply {
                Ok(Ok(reply)) => reply,
                Ok(Err(refusal)) => {
                    self.not_taken(note);
                    // A refusal for a run the resume already replaced says
                    // nothing about the new one: try that one instead.
                    if !self.is_active(&run) {
                        continue;
                    }
                    match refusal.code.as_str() {
                        RUN_AWAITING_GZ => self.set_gate(Gate::WaitingOnGz),
                        RUN_NOT_ACTIVE => self.set_gate(Gate::Closed(run)),
                        // The run reads its notes at its next step; the
                        // next delivery tries again.
                        _ => {}
                    }
                    break;
                }
                // Logged by the client; the note waits for the next try.
                Err(_) => {
                    self.not_taken(note);
                    break;
                }
            };
            let steer_id = reply["steerId"].as_str().unwrap_or_default();
            sent.extend(self.accepted(note, steer_id));
        }
        sent
    }

    /// The run took the note: it is sent, unless the turn's end or a stop
    /// claimed it while the POST was on its way.
    fn accepted(&self, note: Note, steer_id: &str) -> Option<SentNote> {
        let ours = self.notes.lock().is_ok_and(|mut n| n.settle(&note));
        ours.then(|| SentNote {
            steer_id: steer_id.to_string(),
            text: note.text,
        })
    }

    fn not_taken(&self, note: Note) {
        if let Ok(mut notes) = self.notes.lock() {
            notes.not_taken(note);
        }
    }

    fn is_active(&self, run: &str) -> bool {
        self.active_run
            .lock()
            .is_ok_and(|active| active.as_deref() == Some(run))
    }
}

/// A note [`Steering::deliver`] has POSTed, for tests that play out what
/// happens before its reply arrives.
#[cfg(test)]
pub(crate) struct InFlight(Note);

#[cfg(test)]
impl Steering {
    /// Starts sending the oldest note, as [`Self::deliver`] does.
    pub(crate) fn start_sending(&self) -> InFlight {
        let note = self.notes.lock().unwrap().start_sending();
        InFlight(note.expect("a waiting note that fits"))
    }

    /// The run's 202 for that note arrives.
    pub(crate) fn steer_accepted(&self, sending: InFlight, steer_id: &str) -> Option<SentNote> {
        self.accepted(sending.0, steer_id)
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
        held_stub_server(replies, None)
    }

    /// Like [`stub_server`], but the first reply waits until `hold` fires,
    /// so a test can act while that POST is in flight.
    fn held_stub_server(
        replies: Vec<(u16, &'static str)>,
        hold: Option<std::sync::mpsc::Receiver<()>>,
    ) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = Arc::default();
        let record = seen.clone();
        let mut hold = hold;
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
                if let Some(hold) = hold.take() {
                    hold.recv().ok();
                }
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

    /// Starts delivering on a stub whose first 202 waits, and returns once
    /// the steer POST has reached it.
    async fn deliver_held(
        steering: &Steering,
        seen: &Seen,
    ) -> tokio::task::JoinHandle<Vec<SentNote>> {
        let delivering = steering.clone();
        let task = tokio::spawn(async move { delivering.deliver().await });
        while seen.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        task
    }

    #[tokio::test]
    async fn a_note_taken_back_while_its_post_is_in_flight_does_not_pop_another() {
        let (release, hold) = std::sync::mpsc::channel();
        let (base, seen) = held_stub_server(
            vec![(202, r#"{"accepted": true, "steerId": "steer_1"}"#)],
            Some(hold),
        );
        let steering = steering(base, Some("chatrun_1"));
        steering.queue("use tabs");
        let task = deliver_held(&steering, &seen).await;
        assert!(steering.take_all().is_empty(), "the server may have it");
        assert_eq!(steering.sending().as_deref(), Some("use tabs"));
        steering.queue("use tabs not spaces v2");
        release.send(()).unwrap();
        let sent = task.await.unwrap();
        assert_eq!(
            sent.iter().map(|n| n.text.as_str()).collect::<Vec<_>>(),
            vec!["use tabs"]
        );
        assert_eq!(steering.sending(), None);
        assert_eq!(steering.pending(), vec!["use tabs not spaces v2"]);
    }

    #[tokio::test]
    async fn a_note_claimed_by_the_turns_end_is_not_reported_when_its_reply_comes_late() {
        let (release, hold) = std::sync::mpsc::channel();
        let (base, seen) = held_stub_server(
            vec![(202, r#"{"accepted": true, "steerId": "steer_1"}"#)],
            Some(hold),
        );
        let steering = steering(base, Some("chatrun_1"));
        steering.queue("NOTE-A");
        let task = deliver_held(&steering, &seen).await;
        assert_eq!(
            steering.claim_all(),
            vec!["NOTE-A"],
            "it goes after the turn"
        );
        release.send(()).unwrap();
        assert!(task.await.unwrap().is_empty(), "already handed back once");
        assert!(steering.pending().is_empty());
        assert_eq!(steering.sending(), None);
    }

    #[tokio::test]
    async fn a_closed_gate_does_not_outlive_its_run() {
        let (base, _seen) = stub_server(vec![(
            409,
            r#"{"detail": {"error": "run_not_active", "message": "finished"}}"#,
        )]);
        let steering = steering(base, Some("chatrun_1"));
        steering.queue("NOTE-D");
        assert!(steering.deliver().await.is_empty());
        assert_eq!(steering.deliverable(), 0, "r1 is over");
        *steering.active_run.lock().unwrap() = None;
        steering.queue("NOTE-E");
        assert_eq!(steering.deliverable(), 2, "the next turn's run takes them");
    }

    #[tokio::test]
    async fn too_many_waiting_notes_is_a_quiet_wait_not_a_retried_error() {
        let (base, seen) = stub_server(vec![(
            429,
            r#"{"detail": {"error": "too_many_pending_steers", "message": "wait"}}"#,
        )]);
        let steering = steering(base, Some("chatrun_1"));
        steering.queue("one");
        assert!(steering.deliver().await.is_empty());
        assert_eq!(seen.lock().unwrap().len(), 1, "not retried");
        assert!(crate::error_log::test_log::urls().is_empty(), "not logged");
        assert_eq!(steering.pending(), vec!["one"]);
        assert_eq!(steering.deliverable(), 1, "it goes at the next step");
    }
}
