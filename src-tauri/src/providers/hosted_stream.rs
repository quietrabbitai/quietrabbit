// src-tauri/src/providers/hosted_stream.rs
//
// items.id=715 (decisions.id=874, 875, 883; PROVIDER_REGISTRY_AND_TIER_MODEL_
// SPEC.md Part 8i): streamed generation for the hosted client. One method,
// `HostedClient::generate_stream_metered`, chosen PER CALL by the caller
// (never by a row option); everything else about a hosted call (HTTPS only,
// no redirects, the key only in a sensitive header, the fail-closed token
// limit, error mapping that never copies upstream text) is the same code the
// whole-body call uses.
//
// Two wire formats, one decoder each, named by protocol:
//   openai_chat        data: {chunk}  ...  data: [DONE]
//                      text in choices[0].delta.content; a final usage frame
//                      (the request asks for one with stream_options)
//   anthropic_messages event frames: message_start, content_block_delta,
//                      message_delta (usage, cumulative), message_stop
//
// RESULT SHAPE: the method returns a `HostedAttempt` in exactly the shape the
// whole-body call produces, so the spend gate settles it with the SAME
// `settlement_for` (spend/gate.rs). Every ending maps as follows:
//   complete + final usage        Ok, usage Some           -> Reported
//   complete, no usage            Ok, usage None           -> Estimated
//   error frame / cut / timeout / size cap / no completion signal
//     after the request was sent  Err, usage None, 2xx     -> Interrupt
//   user cancel after dispatch    Err(StreamCancelled), reached_provider
//     (including before the first byte)                    -> Interrupt
//   cancel before dispatch        Err, reached_provider false -> Release
// Only a FINAL usage figure counts as usage (an Anthropic message_start
// figure is not final; the OpenAI-style usage frame is). Cancel never maps to
// Release once the request may have been sent.
//
// NO AUTO-RETRY AFTER TEXT WAS SHOWN: a failure after the first delta is a
// terminal `StreamInterrupted`; a failure before any text keeps its ordinary
// (possibly retryable) class.
//
// PRIVACY (decisions.id=883): delta text is display-only. It goes to the
// `StreamSink` and into the returned `received_text`, and nowhere else: no
// log line, no Debug output, no error message, nothing persisted here. A
// source-scan test pins that no log macro in this file takes text.
//
// HOSTILE INPUT: bounded in time and size. Headers wait, gap between chunks,
// total duration, wire bytes, accumulated text, one event and one line all
// have caps (StreamLimits, sse.rs); nothing here panics on any byte stream.

use std::time::{Duration, Instant};

use reqwest::header::{HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Method;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::providers::errors::ProviderError;
use crate::providers::hosted_call::{CallContext, Dialect};
use crate::providers::hosted_client::{
    answer_or_error, build_payload, extract_usage, malformed, misconfigured_ctx, parse_chat,
    parse_messages, status_error, token_count, transport_error, HostedAttempt, HostedClient,
    HostedOutcome, HostedRequest, HostedUsage, Purpose, RateLimitInfo,
};
use crate::providers::sse::{SseEvent, SseParser, MAX_EVENT_BYTES};
use crate::providers::types::{CompletionStatus, GenerateResponse};

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Time and size limits of one streamed call. The wire-byte and accumulated-
/// text cap is the client's `max_response_bytes`.
#[derive(Debug, Clone, Copy)]
pub struct StreamLimits {
    /// Wait for the response headers.
    pub headers_timeout: Duration,
    /// Longest gap between received chunks (a keepalive comment counts).
    pub idle_timeout: Duration,
    /// Longest the whole call may take, headers to the last byte.
    pub total_timeout: Duration,
    /// Cap on one SSE line, and on one event's data plus name.
    pub max_event_bytes: usize,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            headers_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(60),
            total_timeout: Duration::from_secs(600),
            max_event_bytes: MAX_EVENT_BYTES,
        }
    }
}

/// A delta is passed on when this long has passed since the last one, or when
/// this many bytes are waiting; the first delta goes immediately.
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);
const FLUSH_BYTES: usize = 2048;

// ---------------------------------------------------------------------------
// Sink
// ---------------------------------------------------------------------------

/// Where display-only text goes while a reply streams. Implementations must
/// not log, store or Debug-print `text`.
pub trait StreamSink: Send + Sync {
    fn delta(&self, seq: u32, text: &str);

    /// The step that streamed has finished (after Gate 2). Default: ignore.
    fn end(&self, _end: &StreamEnd) {}
}

/// How a streamed step ended, for the front end's notice (decisions.id=883).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamEnd {
    pub outcome: StreamOutcome,
    /// Gate 2 found a withheld personal value in the reply.
    pub personal_flagged: bool,
    /// ... and its disclosure-log entry could not be written.
    pub audit_unavailable: bool,
}

/// Drops every delta (a run with no window attached).
pub struct NullSink;

impl StreamSink for NullSink {
    fn delta(&self, _seq: u32, _text: &str) {}
}

/// Batches tiny deltas so the front end is not flooded, and guarantees the
/// last words are passed on even if the provider then goes quiet.
struct Coalescer<'a> {
    sink: &'a dyn StreamSink,
    buf: String,
    seq: u32,
    last_flush: Option<Instant>,
}

impl<'a> Coalescer<'a> {
    fn new(sink: &'a dyn StreamSink) -> Self {
        Self {
            sink,
            buf: String::new(),
            seq: 0,
            last_flush: None,
        }
    }

    fn push(&mut self, text: &str) {
        self.buf.push_str(text);
        let due = match self.last_flush {
            None => true,
            Some(t) => t.elapsed() >= FLUSH_INTERVAL,
        };
        if due || self.buf.len() >= FLUSH_BYTES {
            self.flush();
        }
    }

    /// When the waiting text must go out, if any is waiting.
    fn deadline(&self) -> Option<Instant> {
        if self.buf.is_empty() {
            return None;
        }
        self.last_flush.map(|t| t + FLUSH_INTERVAL)
    }

    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        self.sink.delta(self.seq, &self.buf);
        self.seq = self.seq.saturating_add(1);
        self.buf.clear();
        self.last_flush = Some(Instant::now());
    }
}

// ---------------------------------------------------------------------------
// Decoders
// ---------------------------------------------------------------------------

/// What an upstream error frame says went wrong, class only (no upstream
/// text is ever kept).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Upstream {
    Auth,
    Payment,
    RateLimit,
    Timeout,
    NotFound,
    Server(u16),
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    /// A frame that was not the expected JSON shape.
    Malformed,
    /// The provider sent an error frame.
    Upstream(Upstream),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Text(String),
    Skip,
    Done,
}

/// One dialect's reading of the event stream.
pub(crate) struct StreamDecoder {
    dialect: Dialect,
    finish_reason: Option<String>,
    done: bool,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    usage_final: Option<HostedUsage>,
}

fn classify_number(code: u64) -> Upstream {
    match code {
        401 | 403 => Upstream::Auth,
        402 => Upstream::Payment,
        404 => Upstream::NotFound,
        408 => Upstream::Timeout,
        429 => Upstream::RateLimit,
        500..=599 => Upstream::Server(code as u16),
        _ => Upstream::Other,
    }
}

fn classify_word(word: &str) -> Upstream {
    let w = word.to_ascii_lowercase();
    if w.contains("rate_limit") || w.contains("rate limit") || w.contains("too_many") {
        Upstream::RateLimit
    } else if w.contains("authentication")
        || w.contains("permission")
        || w.contains("invalid_api_key")
        || w.contains("unauthorized")
    {
        Upstream::Auth
    } else if w.contains("billing") || w.contains("insufficient") || w.contains("quota") {
        Upstream::Payment
    } else if w.contains("not_found") {
        Upstream::NotFound
    } else if w.contains("timeout") {
        Upstream::Timeout
    } else if w.contains("overloaded") {
        Upstream::Server(529)
    } else if w.contains("api_error") || w.contains("server_error") || w.contains("unavailable") {
        Upstream::Server(500)
    } else {
        Upstream::Other
    }
}

/// Read the class of an error object from its `code` and `type` fields only.
fn classify_error(err: &Value) -> Upstream {
    if let Some(n) = err.get("code").and_then(Value::as_u64) {
        let c = classify_number(n);
        if c != Upstream::Other {
            return c;
        }
    }
    for key in ["type", "code"] {
        if let Some(w) = err.get(key).and_then(Value::as_str) {
            let c = classify_word(w);
            if c != Upstream::Other {
                return c;
            }
        }
    }
    Upstream::Other
}

impl StreamDecoder {
    pub(crate) fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            finish_reason: None,
            done: false,
            input_tokens: None,
            output_tokens: None,
            usage_final: None,
        }
    }

    /// The provider's own end-of-reply signal has been seen. For the chat
    /// dialect a reported finish reason counts too (some servers close the
    /// stream right after it without `[DONE]`).
    pub(crate) fn complete(&self) -> bool {
        match self.dialect {
            Dialect::OpenaiChat => self.done || self.finish_reason.is_some(),
            Dialect::AnthropicMessages => self.done,
        }
    }

    pub(crate) fn finish_reason(&self) -> Option<String> {
        self.finish_reason.clone()
    }

    /// Usage the provider marked final, if any.
    pub(crate) fn usage(&self) -> Option<HostedUsage> {
        self.usage_final
    }

    pub(crate) fn on_event(&mut self, ev: &SseEvent) -> Result<Step, Fault> {
        match self.dialect {
            Dialect::OpenaiChat => self.on_chat(ev),
            Dialect::AnthropicMessages => self.on_messages(ev),
        }
    }

    fn on_chat(&mut self, ev: &SseEvent) -> Result<Step, Fault> {
        let data = ev.data.trim();
        if data == "[DONE]" {
            self.done = true;
            return Ok(Step::Done);
        }
        let v: Value = serde_json::from_str(data).map_err(|_| Fault::Malformed)?;
        // An in-stream error is a top-level `error` object; it can be the
        // first and only event under HTTP 200.
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            return Err(Fault::Upstream(classify_error(err)));
        }
        // A usage object on any frame (usually the last, with an empty or
        // single-choice body) is the accounting frame.
        if let Some(u) = extract_usage(Dialect::OpenaiChat, &v) {
            self.usage_final = Some(u);
        }
        let choice = &v["choices"][0];
        if let Some(reason) = choice["finish_reason"].as_str() {
            if reason == "error" {
                return Err(Fault::Upstream(Upstream::Other));
            }
            self.finish_reason = Some(reason.to_owned());
        }
        match choice["delta"]["content"].as_str() {
            Some(t) if !t.is_empty() => Ok(Step::Text(t.to_owned())),
            _ => Ok(Step::Skip),
        }
    }

    fn on_messages(&mut self, ev: &SseEvent) -> Result<Step, Fault> {
        let v: Value = serde_json::from_str(ev.data.trim()).map_err(|_| Fault::Malformed)?;
        let kind = v["type"].as_str().or(ev.event.as_deref()).unwrap_or("");
        match kind {
            "message_start" => {
                if let Some(n) = v["message"]["usage"]["input_tokens"].as_u64() {
                    self.input_tokens = Some(token_count(&Value::from(n)));
                }
                Ok(Step::Skip)
            }
            "content_block_start" => match v["content_block"]["text"].as_str() {
                Some(t) if v["content_block"]["type"] == "text" && !t.is_empty() => {
                    Ok(Step::Text(t.to_owned()))
                }
                _ => Ok(Step::Skip),
            },
            "content_block_delta" => match v["delta"]["text"].as_str() {
                Some(t) if v["delta"]["type"] == "text_delta" && !t.is_empty() => {
                    Ok(Step::Text(t.to_owned()))
                }
                _ => Ok(Step::Skip),
            },
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    self.finish_reason = Some(r.to_owned());
                }
                if let Some(n) = v["usage"]["output_tokens"].as_u64() {
                    self.output_tokens = Some(token_count(&Value::from(n)));
                }
                if let Some(n) = v["usage"]["input_tokens"].as_u64() {
                    self.input_tokens = Some(token_count(&Value::from(n)));
                }
                // Final only when both figures are known; a partial figure
                // would under-count, so it is not used.
                if let (Some(i), Some(o)) = (self.input_tokens, self.output_tokens) {
                    self.usage_final = Some(HostedUsage {
                        prompt_tokens: i,
                        completion_tokens: o,
                        reasoning_tokens: None,
                    });
                }
                Ok(Step::Skip)
            }
            "message_stop" => {
                self.done = true;
                Ok(Step::Done)
            }
            "error" => Err(Fault::Upstream(classify_error(&v["error"]))),
            _ => Ok(Step::Skip),
        }
    }
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// A streamed call's attempt plus the text the user was shown, which Gate 2
/// scans even when the call did not complete (decisions.id=883).
pub struct StreamAttempt {
    pub attempt: HostedAttempt,
    pub received_text: String,
    /// The call ended because the user cancelled.
    pub cancelled: bool,
}

// Never prints reply text (decisions.id=883).
impl std::fmt::Debug for StreamAttempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamAttempt")
            .field("ok", &self.attempt.result.is_ok())
            .field("usage", &self.attempt.usage)
            .field("http_status", &self.attempt.http_status)
            .field("reached_provider", &self.attempt.reached_provider)
            .field("received_len", &self.received_text.len())
            .field("cancelled", &self.cancelled)
            .finish()
    }
}

#[derive(Debug)]
enum End {
    Done,
    Eof,
    Cancelled,
    Idle,
    Transport(bool),
    Sse,
    TooLarge,
    Fault(Fault),
}

fn upstream_error(ctx: &CallContext, kind: Upstream) -> ProviderError {
    let name = &ctx.display_name;
    let provider = ctx.provider_id.clone();
    match kind {
        Upstream::Auth => ProviderError::InvalidApiKey {
            provider,
            plain_language: format!(
                "{name} rejected the API key. Check that it is correct and has not \
                 expired. [Open Settings] [Get help]"
            ),
        },
        Upstream::Payment => ProviderError::PaymentRequired {
            provider,
            plain_language: format!(
                "{name} needs a payment method or more credit on your account \
                 before it can answer. [Open {name}] [Use local AI instead]"
            ),
        },
        Upstream::RateLimit => ProviderError::RateLimit {
            provider,
            retry_after_secs: None,
            plain_language: format!(
                "{name} rate limit reached. [Try again in a moment] [Use local AI instead]"
            ),
        },
        Upstream::Timeout => ProviderError::Timeout {
            provider,
            plain_language: format!(
                "{name} didn't respond in time. [Try again] [Use local AI instead]"
            ),
        },
        Upstream::NotFound => ProviderError::ModelNotFound {
            provider,
            plain_language: format!(
                "{name} doesn't recognize this model any more; it may have been retired. \
                 Choose another model in Settings. [Open Settings] [Get help]"
            ),
        },
        Upstream::Server(code) => ProviderError::ServerError {
            provider,
            status_code: code,
            plain_language: format!(
                "{name} had a problem on its side ({code}). [Try again] [Use local AI instead]"
            ),
        },
        Upstream::Other => ProviderError::ProviderFailure {
            provider,
            status_code: None,
            plain_language: format!("{name} returned an unexpected error. [Try again] [Get help]"),
        },
    }
}

fn interrupted(ctx: &CallContext) -> ProviderError {
    ProviderError::StreamInterrupted {
        provider: ctx.provider_id.clone(),
        plain_language: format!(
            "{} stopped sending its reply part-way through. [Try again] [Use local AI instead]",
            ctx.display_name
        ),
    }
}

fn cancelled_error(ctx: &CallContext) -> ProviderError {
    ProviderError::StreamCancelled {
        provider: ctx.provider_id.clone(),
        plain_language: "The reply was stopped. [Try again]".to_owned(),
    }
}

/// The error for a stream that ended badly. Once any text has been shown the
/// error is the terminal `StreamInterrupted` (no retry over shown text);
/// before that it keeps the ordinary class.
fn end_error(ctx: &CallContext, end: End, shown: bool) -> ProviderError {
    match end {
        End::Cancelled => cancelled_error(ctx),
        _ if shown => interrupted(ctx),
        End::Idle => ProviderError::Timeout {
            provider: ctx.provider_id.clone(),
            plain_language: format!(
                "{} didn't respond in time. [Try again] [Use local AI instead]",
                ctx.display_name
            ),
        },
        End::Transport(timed_out) => {
            if timed_out {
                ProviderError::Timeout {
                    provider: ctx.provider_id.clone(),
                    plain_language: format!(
                        "{} didn't respond in time. [Try again] [Use local AI instead]",
                        ctx.display_name
                    ),
                }
            } else {
                ProviderError::Unavailable {
                    provider: ctx.provider_id.clone(),
                    plain_language: format!(
                        "The connection to {} failed. [Try again] [Use local AI instead]",
                        ctx.display_name
                    ),
                }
            }
        }
        End::Eof => ProviderError::Unavailable {
            provider: ctx.provider_id.clone(),
            plain_language: format!(
                "The connection to {} failed. [Try again] [Use local AI instead]",
                ctx.display_name
            ),
        },
        End::Sse | End::TooLarge | End::Fault(Fault::Malformed) => {
            malformed(ctx, "its reply stream was not in a form QR could read")
        }
        End::Fault(Fault::Upstream(kind)) => upstream_error(ctx, kind),
        // Done never reaches here (it is a success).
        End::Done => interrupted(ctx),
    }
}

// ---------------------------------------------------------------------------
// The call
// ---------------------------------------------------------------------------

struct Loop<'a> {
    parser: SseParser,
    decoder: StreamDecoder,
    coalescer: Coalescer<'a>,
    text: String,
    max_text: usize,
}

impl Loop<'_> {
    /// Apply events; `Some(end)` stops the loop.
    fn apply(&mut self, events: Vec<SseEvent>) -> Option<End> {
        for ev in events {
            match self.decoder.on_event(&ev) {
                Ok(Step::Text(t)) => {
                    if self.text.len() + t.len() > self.max_text {
                        return Some(End::TooLarge);
                    }
                    self.text.push_str(&t);
                    self.coalescer.push(&t);
                }
                Ok(Step::Skip) => {}
                Ok(Step::Done) => return Some(End::Done),
                Err(f) => return Some(End::Fault(f)),
            }
        }
        None
    }
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
        None => std::future::pending::<()>().await,
    }
}

impl HostedClient {
    /// Send one generation request as a stream (items.id=715). The sink gets
    /// display-only text as it arrives; the returned attempt has the same
    /// shape as `generate_metered`'s, plus the text received.
    ///
    /// Privacy contract: as `generate`, the prompt is abstracted upstream and
    /// this method writes no disclosure log. It never logs reply text.
    pub async fn generate_stream_metered(
        &self,
        ctx: &CallContext,
        req: &HostedRequest,
        sink: &dyn StreamSink,
        cancel: &CancellationToken,
    ) -> StreamAttempt {
        let limits = self.stream_limits;
        let before_send = |e: ProviderError, cancelled: bool| StreamAttempt {
            attempt: HostedAttempt {
                result: Err(e),
                usage: None,
                rate_limit: RateLimitInfo::default(),
                http_status: None,
                reached_provider: false,
            },
            received_text: String::new(),
            cancelled,
        };

        let payload = match build_payload(ctx, req, true) {
            Ok(p) => p,
            Err(e) => return before_send(e, false),
        };
        let mut headers = match self.headers(ctx, true) {
            Ok(h) => h,
            Err(e) => return before_send(e, false),
        };
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        let body = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(_) => {
                return before_send(
                    misconfigured_ctx(ctx, "the request could not be built"),
                    false,
                )
            }
        };
        // Cancelled before anything was dispatched: provably never sent.
        if cancel.is_cancelled() {
            return before_send(cancelled_error(ctx), true);
        }

        let start = Instant::now();
        let url = ctx.generate_url();
        let sent = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            r = tokio::time::timeout(
                limits.headers_timeout,
                self.send_raw(
                    Method::POST,
                    &url,
                    headers,
                    Some(body),
                    limits.total_timeout,
                ),
            ) => Some(r),
        };
        let response = match sent {
            // Cancel while waiting for the headers: the request may have
            // reached the provider, so this is NOT a release.
            None => {
                return StreamAttempt {
                    attempt: HostedAttempt {
                        result: Err(cancelled_error(ctx)),
                        usage: None,
                        rate_limit: RateLimitInfo::default(),
                        http_status: None,
                        reached_provider: true,
                    },
                    received_text: String::new(),
                    cancelled: true,
                };
            }
            Some(Err(_elapsed)) => {
                return StreamAttempt {
                    attempt: HostedAttempt {
                        result: Err(ProviderError::Timeout {
                            provider: ctx.provider_id.clone(),
                            plain_language: format!(
                                "{} didn't respond in time. [Try again] [Use local AI instead]",
                                ctx.display_name
                            ),
                        }),
                        usage: None,
                        rate_limit: RateLimitInfo::default(),
                        http_status: None,
                        reached_provider: true,
                    },
                    received_text: String::new(),
                    cancelled: false,
                };
            }
            Some(Ok(Err(e))) => {
                let reached = !(e.is_connect() || e.is_builder());
                return StreamAttempt {
                    attempt: HostedAttempt {
                        result: Err(transport_error(ctx, &e)),
                        usage: None,
                        rate_limit: RateLimitInfo::default(),
                        http_status: None,
                        reached_provider: reached,
                    },
                    received_text: String::new(),
                    cancelled: false,
                };
            }
            Some(Ok(Ok(r))) => r,
        };

        let status = response.status();
        let rate_limit = RateLimitInfo::from_headers(response.headers());
        if !status.is_success() {
            return StreamAttempt {
                attempt: HostedAttempt {
                    result: Err(status_error(
                        ctx,
                        status,
                        response.headers(),
                        Purpose::Generate,
                    )),
                    usage: None,
                    rate_limit,
                    http_status: Some(status.as_u16()),
                    reached_provider: true,
                },
                received_text: String::new(),
                cancelled: false,
            };
        }
        let http_status = Some(status.as_u16());

        // A server that ignored `stream: true` and sent one JSON body: use the
        // whole-body parse and pass the text on as a single delta.
        let is_json = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("json"));
        if is_json {
            return self
                .finish_whole_body(ctx, response, sink, cancel, rate_limit, start)
                .await;
        }

        let mut state = Loop {
            parser: SseParser::with_limit(limits.max_event_bytes),
            decoder: StreamDecoder::new(ctx.dialect),
            coalescer: Coalescer::new(sink),
            text: String::new(),
            max_text: self.max_response_bytes,
        };
        let mut response = response;
        let mut wire_bytes: usize = 0;
        let mut last_chunk = Instant::now();

        let end = loop {
            let idle_left = limits.idle_timeout.saturating_sub(last_chunk.elapsed());
            let flush_at = state.coalescer.deadline();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break End::Cancelled,
                _ = sleep_until_opt(flush_at), if flush_at.is_some() => {
                    state.coalescer.flush();
                }
                r = tokio::time::timeout(idle_left, response.chunk()) => match r {
                    Err(_) => break End::Idle,
                    Ok(Err(e)) => break End::Transport(e.is_timeout()),
                    Ok(Ok(None)) => {
                        // Clean end of the body: a last unterminated event
                        // may still complete.
                        let tail = match state.parser.finish() {
                            Ok(t) => t,
                            Err(_) => break End::Sse,
                        };
                        if let Some(end) = state.apply(tail) {
                            break end;
                        }
                        break if state.decoder.complete() { End::Done } else { End::Eof };
                    }
                    Ok(Ok(Some(chunk))) => {
                        last_chunk = Instant::now();
                        wire_bytes = wire_bytes.saturating_add(chunk.len());
                        if wire_bytes > self.max_response_bytes {
                            break End::TooLarge;
                        }
                        let events = match state.parser.push(&chunk) {
                            Ok(e) => e,
                            Err(_) => break End::Sse,
                        };
                        if let Some(end) = state.apply(events) {
                            break end;
                        }
                    }
                },
            }
        };

        // Whatever happened, the user is shown everything received.
        state.coalescer.flush();
        let shown = !state.text.is_empty();
        let usage = state.decoder.usage();
        let cancelled = matches!(end, End::Cancelled);
        let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

        let result = match end {
            End::Done => {
                let reason = state.decoder.finish_reason();
                let length_reason = match ctx.dialect {
                    Dialect::OpenaiChat => "length",
                    Dialect::AnthropicMessages => "max_tokens",
                };
                let text = std::mem::take(&mut state.text);
                let received = text.clone();
                let answered = answer_or_error(
                    ctx,
                    if text.is_empty() { None } else { Some(text) },
                    reason.as_deref(),
                    length_reason,
                );
                return StreamAttempt {
                    attempt: HostedAttempt {
                        result: answered.map(|content| {
                            HostedOutcome {
                                response: GenerateResponse {
                                    content,
                                    model: ctx.model_id.clone(),
                                    prompt_token_count: usage.map_or(0, |u| u.prompt_tokens),
                                    output_token_count: usage.map_or(0, |u| u.completion_tokens),
                                    latency_ms,
                                    completion_status: CompletionStatus::Complete,
                                },
                                finish_reason: reason.clone(),
                                reasoning_tokens: usage.and_then(|u| u.reasoning_tokens),
                                rate_limit: rate_limit.clone(),
                                truncated: reason.as_deref() == Some(length_reason),
                            }
                            .finished()
                        }),
                        usage,
                        rate_limit,
                        http_status,
                        reached_provider: true,
                    },
                    received_text: received,
                    cancelled: false,
                };
            }
            other => Err(end_error(ctx, other, shown)),
        };

        StreamAttempt {
            attempt: HostedAttempt {
                result,
                usage,
                rate_limit,
                http_status,
                reached_provider: true,
            },
            received_text: std::mem::take(&mut state.text),
            cancelled,
        }
    }

    /// The server answered a streamed request with one JSON body.
    async fn finish_whole_body(
        &self,
        ctx: &CallContext,
        mut response: reqwest::Response,
        sink: &dyn StreamSink,
        cancel: &CancellationToken,
        rate_limit: RateLimitInfo,
        start: Instant,
    ) -> StreamAttempt {
        let status = Some(response.status().as_u16());
        let failed = |e: ProviderError, cancelled: bool, rl: RateLimitInfo| StreamAttempt {
            attempt: HostedAttempt {
                result: Err(e),
                usage: None,
                rate_limit: rl,
                http_status: status,
                reached_provider: true,
            },
            received_text: String::new(),
            cancelled,
        };
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => return failed(cancelled_error(ctx), true, rate_limit),
                r = tokio::time::timeout(self.stream_limits.idle_timeout, response.chunk()) => r,
            };
            match next {
                Err(_) => {
                    return failed(
                        ProviderError::Timeout {
                            provider: ctx.provider_id.clone(),
                            plain_language: format!(
                                "{} didn't respond in time. [Try again] [Use local AI instead]",
                                ctx.display_name
                            ),
                        },
                        false,
                        rate_limit,
                    )
                }
                Ok(Err(e)) => return failed(transport_error(ctx, &e), false, rate_limit),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(c))) => {
                    if buf.len() + c.len() > self.max_response_bytes {
                        return failed(
                            malformed(ctx, "it was larger than QR accepts"),
                            false,
                            rate_limit,
                        );
                    }
                    buf.extend_from_slice(&c);
                }
            }
        }
        let body: Value = match serde_json::from_slice(&buf) {
            Ok(v) => v,
            Err(_) => {
                return failed(malformed(ctx, "it was not valid JSON"), false, rate_limit);
            }
        };
        let usage = extract_usage(ctx.dialect, &body);
        let parsed = match ctx.dialect {
            Dialect::OpenaiChat => parse_chat(ctx, &body, usage),
            Dialect::AnthropicMessages => parse_messages(ctx, &body, usage),
        };
        let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
        let received = parsed
            .as_ref()
            .map(|p| p.content.clone())
            .unwrap_or_default();
        if !received.is_empty() {
            sink.delta(0, &received);
        }
        StreamAttempt {
            attempt: HostedAttempt {
                result: parsed.map(|p| {
                    HostedOutcome {
                        response: GenerateResponse {
                            content: p.content,
                            model: ctx.model_id.clone(),
                            prompt_token_count: p.prompt_tokens,
                            output_token_count: p.completion_tokens,
                            latency_ms,
                            completion_status: CompletionStatus::Complete,
                        },
                        finish_reason: p.finish_reason,
                        reasoning_tokens: p.reasoning_tokens,
                        rate_limit: rate_limit.clone(),
                        truncated: p.truncated,
                    }
                    .finished()
                }),
                usage,
                rate_limit,
                http_status: status,
                reached_provider: true,
            },
            received_text: received,
            cancelled: false,
        }
    }
}

#[cfg(test)]
#[path = "hosted_stream_tests.rs"]
mod tests;
