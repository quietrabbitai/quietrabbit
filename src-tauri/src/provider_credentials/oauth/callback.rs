// src-tauri/src/provider_credentials/oauth/callback.rs
//
// The loopback callback listener for an OAuth connect (items.id=734). This is
// the ONLY place in the crate that binds a listener for a provider flow (a
// source-scan test pins that).
//
// WHAT IT ACCEPTS. One request: `GET /callback?state=...&code=...` whose Host
// is `localhost`, `127.0.0.1` or `[::1]` on our port and whose `state` equals
// the one this flow issued. The state is checked first; the code is not even
// decoded until it matches. Anything else (another method or path, a wrong
// state, a missing or odd code, a stalled or oversize request) gets a fixed
// error page and the listener keeps waiting, because a stray request (a
// favicon fetch, a port scan) must not end the user's flow. After
// `max_invalid` such requests the flow gives up.
//
// WHEN IT CLOSES. On the first valid callback, at the flow deadline, on cancel,
// or after too many invalid requests. The listeners are owned by `serve`, so
// returning closes them.
//
// WHERE IT LISTENS. 127.0.0.1 on an ephemeral port and, best effort, [::1] on
// the same port, because a browser may resolve `localhost` to ::1 first and a
// v4-only listener would not hear it. Loopback only; never a wildcard address.
//
// RESPONSES. Two constant pages. Nothing from the request is copied into a
// response. The code, state and request bytes live in zeroizing buffers and
// are never logged.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::pkce::same_secret;

/// The host the provider is told to send the browser to. The listener answers
/// on both loopback addresses, so `localhost` works whichever one a browser
/// resolves first. (items.id=726 checks whether the literal `127.0.0.1` is
/// also accepted; if so this is the one line to change.)
pub const CALLBACK_HOST: &str = "localhost";
pub const CALLBACK_PATH: &str = "/callback";

/// Largest request head read, in bytes.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024;
/// Longest code accepted, in bytes.
pub const MAX_CODE_BYTES: usize = 512;

const SUCCESS_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Quiet Rabbit</title></head><body><p>Quiet Rabbit received the approval. You can close this tab and return to Quiet Rabbit.</p></body></html>";
const FAILURE_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Quiet Rabbit</title></head><body><p>This request was not accepted. You can close this tab.</p></body></html>";

// ---------------------------------------------------------------------------
// Listening
// ---------------------------------------------------------------------------

pub struct Listeners {
    v4: TcpListener,
    v6: Option<TcpListener>,
    port: u16,
}

impl Listeners {
    pub fn port(&self) -> u16 {
        self.port
    }

    #[cfg(test)]
    pub fn addrs(&self) -> Vec<std::net::SocketAddr> {
        let mut out = vec![self.v4.local_addr().unwrap()];
        if let Some(v6) = &self.v6 {
            out.push(v6.local_addr().unwrap());
        }
        out
    }
}

/// Bind the loopback listeners on one ephemeral port. A v6 bind that cannot
/// happen (no IPv6) leaves a v4-only listener. A v6 port already taken by
/// another process means that port is not ours alone, so another is tried.
pub async fn bind_loopback() -> io::Result<Listeners> {
    let mut last = io::Error::new(io::ErrorKind::AddrInUse, "no free loopback port");
    for _ in 0..5 {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = v4.local_addr()?.port();
        match TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await {
            Ok(v6) => {
                return Ok(Listeners {
                    v4,
                    v6: Some(v6),
                    port,
                })
            }
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => last = e,
            Err(_) => return Ok(Listeners { v4, v6: None, port }),
        }
    }
    Err(last)
}

async fn accept_any(l: &Listeners) -> io::Result<TcpStream> {
    match &l.v6 {
        Some(v6) => tokio::select! {
            r = l.v4.accept() => r.map(|x| x.0),
            r = v6.accept() => r.map(|x| x.0),
        },
        None => l.v4.accept().await.map(|x| x.0),
    }
}

// ---------------------------------------------------------------------------
// Parsing (pure)
// ---------------------------------------------------------------------------

/// Why a request was not the callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    BadRequest,
    NotFound,
    MethodNotAllowed,
    TooLarge,
    TimedOut,
}

impl Reject {
    fn status(self) -> (u16, &'static str) {
        match self {
            Self::BadRequest => (400, "Bad Request"),
            Self::NotFound => (404, "Not Found"),
            Self::MethodNotAllowed => (405, "Method Not Allowed"),
            Self::TooLarge => (431, "Request Header Fields Too Large"),
            Self::TimedOut => (408, "Request Timeout"),
        }
    }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-decode a query value into a zeroizing string. None when an escape
/// is malformed or the result is not UTF-8.
fn decode_component(raw: &str) -> Option<Zeroizing<String>> {
    let src = raw.as_bytes();
    let mut out = Zeroizing::new(Vec::with_capacity(src.len()));
    let mut i = 0;
    while i < src.len() {
        match src[i] {
            b'%' => {
                let hi = hex(*src.get(i + 1)?)?;
                let lo = hex(*src.get(i + 2)?)?;
                out.push(hi << 4 | lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    let text = std::str::from_utf8(&out).ok()?;
    Some(Zeroizing::new(text.to_owned()))
}

fn host_allowed(value: &str, port: u16) -> bool {
    let v = value.trim();
    [
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
    ]
    .iter()
    .any(|h| h.eq_ignore_ascii_case(v))
}

/// Decide whether `head` (a request's bytes up to the blank line) is the
/// callback with the right state, and return its code if so. The order is
/// method, path, Host, state, and only then the code.
pub fn parse_request(head: &[u8], port: u16, state: &str) -> Result<Zeroizing<String>, Reject> {
    let text = std::str::from_utf8(head).map_err(|_| Reject::BadRequest)?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or(Reject::BadRequest)?;
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Reject::BadRequest);
    };
    if !version.starts_with("HTTP/1.") {
        return Err(Reject::BadRequest);
    }
    if method != "GET" {
        return Err(Reject::MethodNotAllowed);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK_PATH {
        return Err(Reject::NotFound);
    }

    let mut host = None;
    for line in lines.take_while(|l| !l.is_empty()) {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("host") && host.replace(value).is_some() {
                return Err(Reject::BadRequest);
            }
        }
    }
    if !host.is_some_and(|h| host_allowed(h, port)) {
        return Err(Reject::BadRequest);
    }

    let (mut state_raw, mut code_raw) = (None, None);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let slot = match key {
            "state" => &mut state_raw,
            "code" => &mut code_raw,
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return Err(Reject::BadRequest);
        }
    }

    // State first: a request that cannot prove it belongs to this flow never
    // gets its code looked at.
    let given = state_raw
        .and_then(decode_component)
        .ok_or(Reject::BadRequest)?;
    if !same_secret(&given, state) {
        return Err(Reject::BadRequest);
    }
    let code = code_raw
        .and_then(decode_component)
        .ok_or(Reject::BadRequest)?;
    if code.is_empty() || code.len() > MAX_CODE_BYTES || !code.chars().all(|c| c.is_ascii_graphic())
    {
        return Err(Reject::BadRequest);
    }
    Ok(code)
}

// ---------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// How long the listener stays open: the provider's code lifetime.
    pub flow_ttl: Duration,
    /// How long one connection may take to send its request head.
    pub read_timeout: Duration,
    /// Invalid requests tolerated before the flow gives up.
    pub max_invalid: usize,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            flow_ttl: Duration::from_secs(10 * 60),
            read_timeout: Duration::from_secs(5),
            max_invalid: 20,
        }
    }
}

pub enum CallbackOutcome {
    Code(Zeroizing<String>),
    Timeout,
    Cancelled,
    TooManyInvalid,
}

impl std::fmt::Debug for CallbackOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Code(_) => "CallbackOutcome::Code([redacted])",
            Self::Timeout => "CallbackOutcome::Timeout",
            Self::Cancelled => "CallbackOutcome::Cancelled",
            Self::TooManyInvalid => "CallbackOutcome::TooManyInvalid",
        })
    }
}

fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

async fn read_head(stream: &mut TcpStream) -> Result<Zeroizing<Vec<u8>>, Reject> {
    let mut buf = Zeroizing::new(Vec::with_capacity(1024));
    let mut chunk = Zeroizing::new([0u8; 1024]);
    loop {
        let n = stream
            .read(chunk.as_mut_slice())
            .await
            .map_err(|_| Reject::BadRequest)?;
        if n == 0 {
            return Err(Reject::BadRequest);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = head_end(&buf) {
            buf.truncate(end);
            return Ok(buf);
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return Err(Reject::TooLarge);
        }
    }
}

async fn respond(stream: &mut TcpStream, status: (u16, &str), body: &'static str) {
    let allow = if status.0 == 405 {
        "Allow: GET\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n{allow}Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\n\r\n",
        status.0,
        status.1,
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

async fn handle(
    mut stream: TcpStream,
    port: u16,
    state: &str,
    read_timeout: Duration,
) -> Result<Zeroizing<String>, Reject> {
    let head = match tokio::time::timeout(read_timeout, read_head(&mut stream)).await {
        Ok(Ok(head)) => head,
        Ok(Err(reject)) => {
            respond(&mut stream, reject.status(), FAILURE_PAGE).await;
            return Err(reject);
        }
        Err(_) => {
            respond(&mut stream, Reject::TimedOut.status(), FAILURE_PAGE).await;
            return Err(Reject::TimedOut);
        }
    };
    match parse_request(&head, port, state) {
        Ok(code) => {
            respond(&mut stream, (200, "OK"), SUCCESS_PAGE).await;
            Ok(code)
        }
        Err(reject) => {
            respond(&mut stream, reject.status(), FAILURE_PAGE).await;
            Err(reject)
        }
    }
}

/// Wait for the one valid callback. Consumes the listeners, so they are closed
/// whichever way this returns.
pub async fn serve(
    listeners: Listeners,
    state: &str,
    cfg: &ServeConfig,
    cancel: &CancellationToken,
) -> CallbackOutcome {
    let port = listeners.port;
    let deadline = tokio::time::sleep(cfg.flow_ttl);
    tokio::pin!(deadline);
    let mut invalid = 0usize;
    loop {
        let handled = tokio::select! {
            _ = cancel.cancelled() => return CallbackOutcome::Cancelled,
            _ = &mut deadline => return CallbackOutcome::Timeout,
            accepted = accept_any(&listeners) => match accepted {
                Ok(stream) => tokio::select! {
                    _ = cancel.cancelled() => return CallbackOutcome::Cancelled,
                    _ = &mut deadline => return CallbackOutcome::Timeout,
                    r = handle(stream, port, state, cfg.read_timeout) => r.ok(),
                },
                Err(_) => {
                    // Do not spin on a persistent accept error.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    None
                }
            },
        };
        if let Some(code) = handled {
            return CallbackOutcome::Code(code);
        }
        invalid += 1;
        if invalid >= cfg.max_invalid {
            return CallbackOutcome::TooManyInvalid;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    const STATE: &str = "st_0123456789abcdefghijklmnopqrstuvwxyzABCDE";
    const PORT: u16 = 45678;

    fn get(path_and_query: &str, host: &str) -> Vec<u8> {
        format!("GET {path_and_query} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: t\r\n\r\n")
            .into_bytes()
    }

    fn ok_req(query: &str) -> Vec<u8> {
        get(&format!("/callback?{query}"), &format!("localhost:{PORT}"))
    }

    #[test]
    fn a_valid_callback_yields_its_code() {
        let code = parse_request(
            &ok_req(&format!("code=abc.DEF_123-x~&state={STATE}")),
            PORT,
            STATE,
        )
        .unwrap();
        assert_eq!(&*code, "abc.DEF_123-x~");
        // Parameter order does not matter, percent escapes decode, extra
        // parameters are ignored.
        let code = parse_request(
            &ok_req(&format!("state={STATE}&extra=1&code=a%2Fb%3D")),
            PORT,
            STATE,
        )
        .unwrap();
        assert_eq!(&*code, "a/b=");
        for host in [
            format!("127.0.0.1:{PORT}"),
            format!("[::1]:{PORT}"),
            format!("LOCALHOST:{PORT}"),
        ] {
            let req = get(&format!("/callback?code=c1&state={STATE}"), &host);
            assert!(parse_request(&req, PORT, STATE).is_ok(), "{host}");
        }
    }

    #[test]
    fn everything_else_is_rejected_with_the_right_class() {
        let good_q = format!("code=c1&state={STATE}");
        let host = format!("localhost:{PORT}");
        let cases: Vec<(Vec<u8>, Reject)> = vec![
            // method
            (
                format!("POST /callback?{good_q} HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes(),
                Reject::MethodNotAllowed,
            ),
            (
                format!("HEAD /callback?{good_q} HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes(),
                Reject::MethodNotAllowed,
            ),
            (
                format!("get /callback?{good_q} HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes(),
                Reject::MethodNotAllowed,
            ),
            // path
            (get(&format!("/?{good_q}"), &host), Reject::NotFound),
            (
                get(&format!("/callback/?{good_q}"), &host),
                Reject::NotFound,
            ),
            (
                get(&format!("/callback/x?{good_q}"), &host),
                Reject::NotFound,
            ),
            (get(&format!("/Callback?{good_q}"), &host), Reject::NotFound),
            (get("/favicon.ico", &host), Reject::NotFound),
            // Host (DNS rebinding and wrong port)
            (
                get(&format!("/callback?{good_q}"), "evil.example"),
                Reject::BadRequest,
            ),
            (
                get(&format!("/callback?{good_q}"), "localhost"),
                Reject::BadRequest,
            ),
            (
                get(&format!("/callback?{good_q}"), "localhost:1"),
                Reject::BadRequest,
            ),
            (
                format!("GET /callback?{good_q} HTTP/1.1\r\n\r\n").into_bytes(),
                Reject::BadRequest,
            ),
            (
                format!("GET /callback?{good_q} HTTP/1.1\r\nHost: {host}\r\nHost: {host}\r\n\r\n")
                    .into_bytes(),
                Reject::BadRequest,
            ),
            // state
            (ok_req("code=c1"), Reject::BadRequest),
            (ok_req("code=c1&state="), Reject::BadRequest),
            (ok_req("code=c1&state=wrong"), Reject::BadRequest),
            (
                ok_req(&format!("code=c1&state={STATE}x")),
                Reject::BadRequest,
            ),
            (
                ok_req(&format!("code=c1&state={STATE}&state={STATE}")),
                Reject::BadRequest,
            ),
            // code
            (ok_req(&format!("state={STATE}")), Reject::BadRequest),
            (ok_req(&format!("state={STATE}&code=")), Reject::BadRequest),
            (
                ok_req(&format!("state={STATE}&code=a&code=b")),
                Reject::BadRequest,
            ),
            (
                ok_req(&format!("state={STATE}&code=a%20b")),
                Reject::BadRequest,
            ),
            (
                ok_req(&format!("state={STATE}&code=a%00b")),
                Reject::BadRequest,
            ),
            (
                ok_req(&format!("state={STATE}&code=%ZZ")),
                Reject::BadRequest,
            ),
            (
                ok_req(&format!("state={STATE}&code=%C3%A9")),
                Reject::BadRequest,
            ),
            (
                ok_req(&format!(
                    "state={STATE}&code={}",
                    "a".repeat(MAX_CODE_BYTES + 1)
                )),
                Reject::BadRequest,
            ),
            // malformed
            (b"".to_vec(), Reject::BadRequest),
            (b"\xff\xfe\r\n\r\n".to_vec(), Reject::BadRequest),
            (b"GET\r\n\r\n".to_vec(), Reject::BadRequest),
            (
                b"GET /callback HTTP/2.0\r\n\r\n".to_vec(),
                Reject::BadRequest,
            ),
            (
                b"GET /callback x HTTP/1.1\r\n\r\n".to_vec(),
                Reject::BadRequest,
            ),
        ];
        for (req, want) in cases {
            let got = parse_request(&req, PORT, STATE);
            assert_eq!(
                got.as_ref().err().copied(),
                Some(want),
                "{}",
                String::from_utf8_lossy(&req)
            );
        }
    }

    #[test]
    fn a_code_at_the_limit_is_accepted() {
        let long = "a".repeat(MAX_CODE_BYTES);
        let code =
            parse_request(&ok_req(&format!("state={STATE}&code={long}")), PORT, STATE).unwrap();
        assert_eq!(code.len(), MAX_CODE_BYTES);
    }

    // ---- listener ---------------------------------------------------------

    fn cfg() -> ServeConfig {
        ServeConfig {
            flow_ttl: Duration::from_secs(30),
            read_timeout: Duration::from_millis(300),
            max_invalid: 20,
        }
    }

    async fn browser_get(port: u16, path_and_query: &str) -> String {
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let req = get(path_and_query, &format!("localhost:{port}"));
        s.write_all(&req).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test]
    async fn the_listener_is_loopback_only_on_an_ephemeral_port() {
        let l = bind_loopback().await.unwrap();
        assert_ne!(l.port(), 0);
        let addrs = l.addrs();
        assert!(!addrs.is_empty());
        for a in &addrs {
            assert!(IpAddr::is_loopback(&a.ip()), "{a}");
            assert_eq!(a.port(), l.port());
        }
        assert!(addrs.iter().any(|a| a.is_ipv4()));
    }

    #[tokio::test]
    async fn one_valid_callback_returns_the_code_and_closes_the_listener() {
        let l = bind_loopback().await.unwrap();
        let port = l.port();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async move { serve(l, STATE, &cfg(), &cancel).await });

        let reply = browser_get(port, &format!("/callback?code=the_code_1&state={STATE}")).await;
        assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
        assert!(reply.contains("Referrer-Policy: no-referrer"));
        assert!(reply.contains("Cache-Control: no-store"));
        assert!(reply.contains("default-src 'none'"));
        assert!(
            !reply.contains("the_code_1"),
            "the response must not echo the code"
        );
        match task.await.unwrap() {
            CallbackOutcome::Code(c) => assert_eq!(&*c, "the_code_1"),
            other => panic!("{other:?}"),
        }
        assert!(
            TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .is_err(),
            "the port must be closed after the first valid callback"
        );
    }

    #[tokio::test]
    async fn invalid_requests_do_not_end_the_flow_and_are_never_reflected() {
        let l = bind_loopback().await.unwrap();
        let port = l.port();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async move { serve(l, STATE, &cfg(), &cancel).await });

        let marker = "REFLECT_ME_1234";
        let wrong_state = browser_get(port, &format!("/callback?code=c&state={marker}")).await;
        assert!(wrong_state.starts_with("HTTP/1.1 400 "), "{wrong_state}");
        assert!(!wrong_state.contains(marker));
        let wrong_path = browser_get(port, &format!("/{marker}")).await;
        assert!(wrong_path.starts_with("HTTP/1.1 404 "));
        assert!(!wrong_path.contains(marker));

        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        s.write_all(
            format!("POST /callback HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let post = String::from_utf8_lossy(&out);
        assert!(post.starts_with("HTTP/1.1 405 ") && post.contains("Allow: GET"));

        // Still open: the real callback now works.
        let ok = browser_get(port, &format!("/callback?code=real&state={STATE}")).await;
        assert!(ok.starts_with("HTTP/1.1 200 "));
        assert!(matches!(task.await.unwrap(), CallbackOutcome::Code(_)));
    }

    #[tokio::test]
    async fn a_stalled_connection_does_not_block_a_later_valid_one() {
        let l = bind_loopback().await.unwrap();
        let port = l.port();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async move { serve(l, STATE, &cfg(), &cancel).await });
        let _stalled = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let ok = browser_get(port, &format!("/callback?code=real&state={STATE}")).await;
        assert!(ok.starts_with("HTTP/1.1 200 "), "{ok}");
        assert!(matches!(task.await.unwrap(), CallbackOutcome::Code(_)));
    }

    #[tokio::test]
    async fn an_oversize_head_is_refused() {
        let l = bind_loopback().await.unwrap();
        let port = l.port();
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        let task = tokio::spawn(async move { serve(l, STATE, &cfg(), &c2).await });
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let junk = format!(
            "GET /callback HTTP/1.1\r\nX: {}\r\n",
            "a".repeat(MAX_REQUEST_BYTES + 100)
        );
        let _ = s.write_all(junk.as_bytes()).await;
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 431 "));
        cancel.cancel();
        assert!(matches!(task.await.unwrap(), CallbackOutcome::Cancelled));
    }

    #[tokio::test]
    async fn cancel_closes_the_listener() {
        let l = bind_loopback().await.unwrap();
        let port = l.port();
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        let task = tokio::spawn(async move { serve(l, STATE, &cfg(), &c2).await });
        cancel.cancel();
        assert!(matches!(task.await.unwrap(), CallbackOutcome::Cancelled));
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn the_deadline_closes_the_listener() {
        let l = bind_loopback().await.unwrap();
        let port = l.port();
        let short = ServeConfig {
            flow_ttl: Duration::from_millis(150),
            ..cfg()
        };
        let cancel = CancellationToken::new();
        let out = serve(l, STATE, &short, &cancel).await;
        assert!(matches!(out, CallbackOutcome::Timeout), "{out:?}");
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn too_many_invalid_requests_end_the_flow() {
        let l = bind_loopback().await.unwrap();
        let port = l.port();
        let few = ServeConfig {
            max_invalid: 3,
            ..cfg()
        };
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async move { serve(l, STATE, &few, &cancel).await });
        for _ in 0..3 {
            let _ = browser_get(port, "/nope").await;
        }
        assert!(matches!(
            task.await.unwrap(),
            CallbackOutcome::TooManyInvalid
        ));
    }
}
