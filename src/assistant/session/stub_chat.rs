//! A scripted chat server for session tests: each route answers with a
//! fixed reply, optionally after a delay or by holding a stream open, and
//! every request is recorded.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::assistant::session::Session;
use crate::http::ApiClient;

/// How one route answers.
#[derive(Clone)]
pub enum Answer {
    Json(&'static str),
    /// JSON, after a pause (a slow server).
    SlowJson(Duration, &'static str),
    /// A finished event stream.
    Events(String),
    /// An event stream that sends this much, then stays open.
    OpenEvents(String),
    /// The first answer for the first request on this route, the second
    /// for the second, and so on; the last one repeats.
    InTurn(Vec<Answer>),
}

#[derive(Clone, Default)]
pub struct Seen(Arc<Mutex<Vec<(String, Value)>>>);

impl Seen {
    /// Every `METHOD path` received, in order.
    pub fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(l, _)| l.clone())
            .collect()
    }

    pub fn has(&self, line: &str) -> bool {
        self.lines().iter().any(|l| l == line)
    }

    /// The body of the first request to `line`.
    pub fn body(&self, line: &str) -> Option<Value> {
        let seen = self.0.lock().unwrap();
        seen.iter().find(|(l, _)| l == line).map(|(_, b)| b.clone())
    }
}

/// Serves `routes` (by `METHOD path`, query left out); anything else gets
/// `{}`. Returns the base URL and what arrived.
pub fn serve(routes: Vec<(&'static str, Answer)>) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes: Arc<HashMap<&str, Answer>> = Arc::new(routes.into_iter().collect());
    let seen = Seen::default();
    let record = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (routes, record) = (routes.clone(), record.clone());
            std::thread::spawn(move || answer(stream, &routes, &record));
        }
    });
    (base, seen)
}

fn answer(mut stream: TcpStream, routes: &HashMap<&str, Answer>, seen: &Seen) {
    let Some((line, body)) = read_request(&mut stream) else {
        return;
    };
    let hits = {
        let mut seen = seen.0.lock().unwrap();
        seen.push((line.clone(), body));
        seen.iter().filter(|(l, _)| *l == line).count()
    };
    let answer = match routes.get(line.as_str()).cloned() {
        Some(Answer::InTurn(answers)) => answers.get(hits - 1).or(answers.last()).cloned(),
        other => other,
    };
    match answer {
        Some(Answer::Json(text)) => reply(&mut stream, "application/json", text),
        Some(Answer::SlowJson(pause, text)) => {
            std::thread::sleep(pause);
            reply(&mut stream, "application/json", text);
        }
        Some(Answer::Events(text)) => reply(&mut stream, "text/event-stream", &text),
        Some(Answer::OpenEvents(text)) => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
            stream.write_all(head.as_bytes()).ok();
            stream.write_all(text.as_bytes()).ok();
            stream.flush().ok();
            std::thread::sleep(Duration::from_secs(30));
        }
        Some(Answer::InTurn(_)) | None => reply(&mut stream, "application/json", "{}"),
    }
}

/// `METHOD /path` and the JSON body of one request.
fn read_request(stream: &mut TcpStream) -> Option<(String, Value)> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let head_end = loop {
        let n = stream.read(&mut buf).ok().filter(|n| *n > 0)?;
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let length: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    while raw.len() < head_end + length {
        let n = stream.read(&mut buf).ok().filter(|n| *n > 0)?;
        raw.extend_from_slice(&buf[..n]);
    }
    let mut parts = head.lines().next()?.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?.split('?').next()?;
    let body = serde_json::from_slice(&raw[head_end..]).unwrap_or(Value::Null);
    Some((format!("{method} {path}"), body))
}

fn reply(stream: &mut TcpStream, content_type: &str, body: &str) {
    let text = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(text.as_bytes()).ok();
}

/// A session against `base`, working in its own folder, with no audit log
/// or history written.
pub fn session(base: String, name: &str) -> (Session, PathBuf) {
    let dir = std::env::temp_dir().join(format!("gz-session-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let resolved = crate::config::resolve(
        &crate::config::Config::default(),
        Some("session-test"),
        Some(base),
        Some("stub-token".to_string()),
    );
    let http = ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 30).unwrap();
    let mut session = Session::new(http, &dir).unwrap();
    session.audit = None;
    session.history = None;
    (session, dir)
}

/// An SSE stream: each `data` an event with an id, then the run's end.
pub fn events(data: &[Value], status: &str) -> String {
    let mut text: String = data
        .iter()
        .enumerate()
        .map(|(i, d)| format!("id: {}\ndata: {d}\n\n", i + 1))
        .collect();
    text.push_str(&format!(
        "id: {}\ndata: {{\"type\":\"run_status\",\"status\":\"{status}\"}}\n\ndata: [DONE]\n\n",
        data.len() + 1
    ));
    text
}

/// A client tool request event.
pub fn call(id: &str, tool: &str, input: Value) -> Value {
    serde_json::json!({
        "type": "client_tool_request",
        "interruptId": id,
        "toolUseId": id,
        "toolName": tool,
        "input": input,
    })
}
