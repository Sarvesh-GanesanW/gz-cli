//! Minimal server-sent-events reader for chat and agent run streams.

use anyhow::Result;
use reqwest::Response;
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Event {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

impl Event {
    /// `data: [DONE]` or `event: done` marks the end of a stream.
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]" || self.event.as_deref() == Some("done")
    }

    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.data).ok()
    }
}

/// What the consumer wants after each event.
pub enum Flow {
    Continue,
    Stop,
}

/// Splits a byte stream into events; tolerates frames split across chunks.
#[derive(Default)]
pub struct Parser {
    buffer: String,
}

impl Parser {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Event> {
        self.buffer
            .push_str(&String::from_utf8_lossy(chunk).replace("\r\n", "\n"));
        let mut events = Vec::new();
        while let Some(end) = self.buffer.find("\n\n") {
            let frame: String = self.buffer.drain(..end + 2).collect();
            if let Some(event) = parse_frame(&frame) {
                events.push(event);
            }
        }
        events
    }
}

fn parse_frame(frame: &str) -> Option<Event> {
    let mut event = Event::default();
    let mut data_lines = Vec::new();
    for line in frame.lines() {
        if line.starts_with(':') {
            continue; // comment / heartbeat
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "id" => event.id = Some(value.to_string()),
            "event" => event.event = Some(value.to_string()),
            "data" => data_lines.push(value),
            _ => {}
        }
    }
    if data_lines.is_empty() && event.event.is_none() {
        return None;
    }
    event.data = data_lines.join("\n");
    Some(event)
}

/// Feeds every event of `response` to `on_event` until the stream ends or
/// the callback says stop. Returns the id of the last event seen.
pub async fn read(
    response: Response,
    last_id: Option<String>,
    on_event: impl FnMut(&Event) -> Result<Flow>,
) -> Result<Option<String>> {
    let mut last_id = last_id;
    read_watched(response, &mut last_id, None, on_event).await?;
    Ok(last_id)
}

/// How a read ended without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The callback said stop.
    Stopped,
    /// The server closed the stream.
    Closed,
}

/// The connection broke or went silent: reconnecting may recover it.
#[derive(Debug)]
pub struct Broken(pub String);

impl std::fmt::Display for Broken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Broken {}

/// [`read`], keeping `last_id` current as events arrive (so a broken read
/// still says where to resume) and, with `stall`, giving up on a stream
/// that sends nothing (not even a heartbeat) for that long. Transport
/// failures and stalls come back as [`Broken`]; errors from `on_event`
/// come back as they are.
pub async fn read_watched(
    mut response: Response,
    last_id: &mut Option<String>,
    stall: Option<Duration>,
    mut on_event: impl FnMut(&Event) -> Result<Flow>,
) -> Result<Ended> {
    let mut parser = Parser::default();
    loop {
        let chunk = match stall {
            Some(limit) => match tokio::time::timeout(limit, response.chunk()).await {
                Ok(chunk) => chunk,
                Err(_) => {
                    let why = format!("no data for {}s", limit.as_secs());
                    return Err(Broken(why).into());
                }
            },
            None => response.chunk().await,
        };
        let chunk = match chunk {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return Ok(Ended::Closed),
            Err(err) => return Err(Broken(format!("stream interrupted: {err}")).into()),
        };
        for event in parser.feed(&chunk) {
            if event.id.is_some() {
                last_id.clone_from(&event.id);
            }
            if let Flow::Stop = on_event(&event)? {
                return Ok(Ended::Stopped);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frames_split_across_chunks() {
        let mut parser = Parser::default();
        assert!(parser.feed(b"retry: 15000\n\nid: 1\nda").is_empty());
        let events =
            parser.feed(b"ta: {\"type\":\"llm_chunk\"}\n\n: ping\n\nid: 2\ndata: [DONE]\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id.as_deref(), Some("1"));
        assert_eq!(events[0].json().unwrap()["type"], "llm_chunk");
        assert!(events[1].is_done());
    }

    #[test]
    fn joins_multiline_data_and_reads_event_names() {
        let mut parser = Parser::default();
        let events = parser.feed(b"event: done\r\ndata: a\r\ndata: b\r\n\r\n");
        assert_eq!(events[0].data, "a\nb");
        assert!(events[0].is_done());
    }

    #[tokio::test]
    async fn a_silent_stream_is_broken_and_keeps_the_last_id() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _request_head = stream.read(&mut request);
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
            stream.write_all(head.as_bytes()).ok();
            stream.write_all(b"id: 7\ndata: {}\n\n").ok();
            std::thread::sleep(Duration::from_secs(5));
        });
        let response = reqwest::get(format!("http://{address}/")).await.unwrap();
        let mut last_id = None;
        let read = read_watched(
            response,
            &mut last_id,
            Some(Duration::from_millis(300)),
            |_| Ok(Flow::Continue),
        )
        .await;
        let err = read.unwrap_err();
        assert!(err.downcast_ref::<Broken>().is_some(), "{err}");
        assert_eq!(last_id.as_deref(), Some("7"));
    }
}
