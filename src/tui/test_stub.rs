//! A one-request HTTP server for tests that check a request's wire shape
//! (method, path, body) and feed back a canned answer. Never the real
//! backend.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

use crate::http::ApiClient;

/// One canned HTTP answer.
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.as_bytes().to_vec(),
        }
    }
}

/// Serves `replies` to consecutive requests, one per connection; returns the
/// base URL and, once done, every raw request (head and body) it saw.
pub async fn serve(replies: Vec<Reply>) -> (String, JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut seen = Vec::new();
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            seen.push(read_request(&mut socket).await);
            let head = format!(
                "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.status,
                reply.content_type,
                reply.body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&reply.body).await.unwrap();
        }
        seen
    });
    (base, handle)
}

/// Reads one request: the head, then as many body bytes as it announces.
async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut raw = Vec::new();
    let mut chunk = vec![0u8; 16384];
    loop {
        let n = socket.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&raw).to_string();
        let Some((head, body)) = text.split_once("\r\n\r\n") else {
            continue;
        };
        let length = head
            .lines()
            .find_map(|l| {
                l.to_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        if body.len() >= length {
            break;
        }
    }
    String::from_utf8_lossy(&raw).to_string()
}

/// An API client whose every service points at `base`.
pub fn client(base: &str) -> ApiClient {
    let resolved = crate::config::resolve(
        &crate::config::Config::default(),
        Some("t"),
        Some(base.to_string()),
        Some("tok".into()),
    );
    ApiClient::new(resolved, crate::output::OutputFormat::Json, true, 0, 5).unwrap()
}
