//! A tiny scripted HTTP server for integration tests: parses one request
//! per connection and records it.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub target: String,
    pub body: Value,
}

pub fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while raw.len() < head_end + length {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let mut first = head.lines().next()?.split_whitespace();
    Some(Recorded {
        method: first.next()?.to_string(),
        target: first.next()?.to_string(),
        body: serde_json::from_slice(&raw[head_end..head_end + length]).unwrap_or(Value::Null),
    })
}

pub fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(reply.as_bytes()).ok();
}

/// Output with ANSI color codes removed, for text assertions.
#[allow(dead_code)]
pub fn plain(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
