//! Minimal server-sent-events reader for chat and agent run streams.

use anyhow::{Context, Result};
use reqwest::Response;

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
    mut response: Response,
    last_id: Option<String>,
    mut on_event: impl FnMut(&Event) -> Result<Flow>,
) -> Result<Option<String>> {
    let mut parser = Parser::default();
    let mut last_id = last_id;
    while let Some(chunk) = response.chunk().await.context("stream interrupted")? {
        for event in parser.feed(&chunk) {
            if event.id.is_some() {
                last_id = event.id.clone();
            }
            if let Flow::Stop = on_event(&event)? {
                return Ok(last_id);
            }
        }
    }
    Ok(last_id)
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
}
