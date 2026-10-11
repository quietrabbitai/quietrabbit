// src-tauri/src/providers/loopback_test_server.rs
//
// Test-only HTTP/1.1 server on 127.0.0.1 for the hosted client's loopback
// tests (items.id=709, items.id=673). No mocking crate exists in this project
// (ollama_client.rs's tests use a raw std TcpListener); this is the same
// low-tech approach with request capture and a connection counter so tests
// can assert both WHAT was sent and that NOTHING was sent.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One request as the server received it. Header names are lowercased.
#[derive(Debug, Clone)]
pub(crate) struct Captured {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Captured {
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == lower)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("captured body must be JSON")
    }
}

/// A canned response.
#[derive(Debug, Clone)]
pub(crate) struct Canned {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay: Duration,
}

impl Canned {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: body.as_bytes().to_vec(),
            delay: Duration::ZERO,
        }
    }

    pub fn bytes(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
            delay: Duration::ZERO,
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

pub(crate) struct LoopbackServer {
    port: u16,
    captured: Arc<Mutex<Vec<Captured>>>,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl LoopbackServer {
    /// Serve `responses` in order, repeating the last one for any further
    /// connection. One request per connection (`Connection: close`).
    pub fn start(responses: Vec<Canned>) -> Self {
        assert!(!responses.is_empty(), "at least one canned response");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let (cap, conns, stop_flag) = (captured.clone(), connections.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut index = 0usize;
            for stream in listener.incoming() {
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                conns.fetch_add(1, Ordering::SeqCst);
                let canned = responses[index.min(responses.len() - 1)].clone();
                index += 1;
                if let Some(request) = read_request(&mut stream) {
                    cap.lock().unwrap().push(request);
                }
                std::thread::sleep(canned.delay);
                let mut head = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    canned.status,
                    canned.body.len()
                );
                for (n, v) in &canned.headers {
                    head.push_str(&format!("{n}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&canned.body);
                let _ = stream.flush();
            }
        });

        Self {
            port,
            captured,
            connections,
            stop,
        }
    }

    /// `http://127.0.0.1:<port>`
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn captured(&self) -> Vec<Captured> {
        self.captured.lock().unwrap().clone()
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so the thread exits.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Captured> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let content_length = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Captured {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}
