//! `GET /status` on port 8080 for the dashboard, one request per connection, each on its own thread so one slow or idle peer never holds the next poll.

use crate::status::Server;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// Request line and headers above this are answered `400`.
const MAX_REQUEST: usize = 8 * 1024;

pub fn serve(listener: TcpListener, server: Arc<Server>) {
    for conn in listener.incoming().flatten() {
        let s = server.clone();
        std::thread::spawn(move || handle(conn, &s));
    }
}

/// Up to the end of the headers; `None` when the peer sends more than `MAX_REQUEST`, stalls past the 5 s timeout or closes first.
fn read_head(c: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = c.read(&mut chunk).ok()?;
        if n == 0 || buf.len() + n > MAX_REQUEST {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8(buf).ok()
}

fn handle(mut c: TcpStream, server: &Server) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = c.set_write_timeout(Some(Duration::from_secs(5)));
    let (code, body) = match read_head(&mut c) {
        None => (400, r#"{"error":"bad request"}"#.to_string()),
        Some(head) => {
            let mut parts = head.lines().next().unwrap_or("").split(' ');
            match (parts.next(), parts.next()) {
                (Some("GET"), Some("/status")) => (200, serde_json::to_string(&server.status()).unwrap_or_default()),
                _ => (404, r#"{"error":"not found"}"#.to_string()),
            }
        }
    };
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let head = format!("HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    let _ = c.write_all(head.as_bytes()).and_then(|_| c.write_all(body.as_bytes()));
    // drain what the peer still sends, so closing does not reset the connection before it reads the answer
    let _ = c.shutdown(Shutdown::Write);
    let _ = c.set_read_timeout(Some(Duration::from_millis(200)));
    let mut sink = [0u8; 1024];
    while matches!(c.read(&mut sink), Ok(n) if n > 0) {}
}
