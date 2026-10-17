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

/// One step of a streamed (chunked) response (items.id=715).
#[derive(Debug, Clone)]
pub(crate) enum StreamStep {
    /// Send these bytes as one HTTP chunk, then flush.
    Chunk(Vec<u8>),
    /// Wait (the server thread stays responsive to shutdown).
    Pause(Duration),
    /// Close the connection here without the terminating chunk: a cut.
    Cut,
    /// Announce a chunk longer than what is sent, send part of it, close.
    CutMidChunk(Vec<u8>),
    /// Keep the connection open sending SSE keepalive comments until the
    /// client closes it (or this long passes). A client that closes is
    /// counted by `LoopbackServer::client_closed`.
    Hold(Duration),
}

/// A canned response.
#[derive(Debug, Clone)]
pub(crate) struct Canned {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay: Duration,
    /// When set, the response is chunked and these steps are played.
    stream: Option<Vec<StreamStep>>,
    /// When set, sent as Content-Length instead of the body's real length.
    declared_len: Option<usize>,
}

impl Canned {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: body.as_bytes().to_vec(),
            delay: Duration::ZERO,
            stream: None,
            declared_len: None,
        }
    }

    /// A 200 `text/event-stream` response played step by step as chunks.
    pub fn sse(steps: Vec<StreamStep>) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".to_owned(), "text/event-stream".to_owned())],
            body: Vec::new(),
            delay: Duration::ZERO,
            stream: Some(steps),
            declared_len: None,
        }
    }

    /// Claim `len` bytes of Content-Length but send only the real body: the
    /// connection then closes short.
    pub fn with_declared_length(mut self, len: usize) -> Self {
        self.declared_len = Some(len);
        self
    }

    pub fn bytes(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
            delay: Duration::ZERO,
            stream: None,
            declared_len: None,
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
    closed_by_client: Arc<AtomicUsize>,
    request_seen: Arc<tokio::sync::Notify>,
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
        let closed_by_client = Arc::new(AtomicUsize::new(0));

        let (cap, conns, stop_flag) = (captured.clone(), connections.clone(), stop.clone());
        let closed = closed_by_client.clone();
        let request_seen = Arc::new(tokio::sync::Notify::new());
        let seen = request_seen.clone();
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
                    seen.notify_one();
                }
                std::thread::sleep(canned.delay);
                if let Some(steps) = &canned.stream {
                    play_stream(&mut stream, &canned, steps, &stop_flag, &closed);
                    continue;
                }
                let declared = canned.declared_len.unwrap_or(canned.body.len());
                let mut head = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    canned.status, declared
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
            closed_by_client,
            request_seen,
        }
    }

    /// Wait until a request has been read in full, at most `within`; returns
    /// whether one arrived. A request that landed before the call is not
    /// missed (the notification keeps a permit). For tests that must act
    /// after the client has really dispatched, never after a fixed sleep.
    pub async fn wait_for_request(&self, within: Duration) -> bool {
        tokio::time::timeout(within, self.request_seen.notified())
            .await
            .is_ok()
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

    /// How many `Hold` streams ended because the client closed the connection.
    pub fn client_closed(&self) -> usize {
        self.closed_by_client.load(Ordering::SeqCst)
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so the thread exits.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// Sleep in short slices so shutdown is not held up by a long pause.
fn nap(total: Duration, stop: &AtomicBool) {
    let mut left = total;
    let slice = Duration::from_millis(10);
    while !left.is_zero() && !stop.load(Ordering::SeqCst) {
        let d = left.min(slice);
        std::thread::sleep(d);
        left -= d;
    }
}

fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> bool {
    if data.is_empty() {
        // A zero-length chunk would end the body.
        return true;
    }
    let head = format!("{:x}\r\n", data.len());
    stream.write_all(head.as_bytes()).is_ok()
        && stream.write_all(data).is_ok()
        && stream.write_all(b"\r\n").is_ok()
        && stream.flush().is_ok()
}

fn play_stream(
    stream: &mut TcpStream,
    canned: &Canned,
    steps: &[StreamStep],
    stop: &AtomicBool,
    closed_by_client: &AtomicUsize,
) {
    let mut head = format!(
        "HTTP/1.1 {} X\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n",
        canned.status
    );
    for (n, v) in &canned.headers {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();
    for step in steps {
        match step {
            StreamStep::Chunk(data) => {
                if !write_chunk(stream, data) {
                    return;
                }
            }
            StreamStep::Pause(d) => nap(*d, stop),
            StreamStep::Cut => {
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return;
            }
            StreamStep::CutMidChunk(part) => {
                let head = format!("{:x}\r\n", part.len() + 64);
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(part);
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return;
            }
            StreamStep::Hold(max) => {
                let started = std::time::Instant::now();
                while started.elapsed() < *max && !stop.load(Ordering::SeqCst) {
                    if !write_chunk(stream, b": ka\n\n") {
                        closed_by_client.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                return;
            }
        }
    }
    // Normal end: the terminating chunk.
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
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
