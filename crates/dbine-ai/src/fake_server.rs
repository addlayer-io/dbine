//! A tiny HTTP server for tests: it answers with the given responses in
//! order, one per connection, and records each request. Never a real service.

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    /// Lower-cased names.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Serves `responses` (status, content type, body); returns its base URL
/// (`http://127.0.0.1:port`) and the requests it got.
pub async fn serve(responses: Vec<(u16, &'static str, String)>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for (status, ctype, body) in responses {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            let head_end = loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break None;
                }
                raw.extend_from_slice(&buf[..n]);
                if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(i);
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
            let mut lines = head.lines();
            let first: Vec<&str> = lines.next().unwrap_or("").split(' ').collect();
            let headers: Vec<(String, String)> = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                .collect();
            let len: usize = headers
                .iter()
                .find(|(k, _)| k == "content-length")
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0);
            let mut body_in = raw[head_end + 4..].to_vec();
            while body_in.len() < len {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                body_in.extend_from_slice(&buf[..n]);
            }
            log.lock().unwrap().push(Seen {
                method: first.first().unwrap_or(&"").to_string(),
                path: first.get(1).unwrap_or(&"").to_string(),
                headers,
                body: String::from_utf8_lossy(&body_in).to_string(),
            });
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(reply.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (base, seen)
}
