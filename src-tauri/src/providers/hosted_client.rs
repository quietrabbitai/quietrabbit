// src-tauri/src/providers/hosted_client.rs
//
// items.id=709 (decisions.id=875, PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md
// Part 8b): ONE stateless client for every hosted (qr_hosted) provider. It
// replaces groq.rs, mistral.rs, qr_hosted_base.rs's per-provider trait and
// the executor's static registry. All provider-specific facts arrive in a
// `CallContext` (hosted_call.rs) built from the provider row, the model row
// and the user's stored key; nothing here names a provider or a model.
//
// Two wire formats ("dialects"), named by protocol:
//   openai_chat        POST {base}/chat/completions
//                      usage: prompt_tokens, completion_tokens
//   anthropic_messages POST {base}/v1/messages, system prompt as a top-level
//                      field, max_tokens required
//                      usage: input_tokens, output_tokens
//
// TOKEN BUDGET FAILS CLOSED (items.id=709, for items.id=710's cost cap): a
// call needs an output token limit from the call itself or the model row's
// `request_options`. With neither, `generate` returns Misconfigured BEFORE any
// network traffic. There is no default in code.
//
// SECURITY (all enforced here or in hosted_call.rs, and tested):
//   - HTTPS only (client-level `https_only`, plus the URL check in
//     CallContext::build); redirects are OFF (a 3xx is an error, so a
//     redirect can never carry the key to another host);
//   - connect and total timeouts; the response body is read with a size cap;
//   - certificate verification is never relaxed (a source-scan test fails if
//     any non-test source disables it);
//   - the key is only ever placed in a sensitive header value; it never
//     appears in a URL, a body, a log line, a Debug or Display, or an error;
//   - upstream response bodies are never copied into error text;
//   - `stream` is false on every whole-body call. A streamed call is a
//     separate method (`generate_stream_metered`, hosted_stream.rs) chosen
//     per call, never by a row option (items.id=715).
//
// PROXIES: production honors the standard proxy environment variables
// (HTTP_PROXY, HTTPS_PROXY, NO_PROXY) like any reqwest client; TLS still
// terminates at the provider. The loopback test client disables proxies.

use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{redirect, Client, Method, Response, StatusCode};
use serde_json::{json, Map, Value};
use zeroize::Zeroizing;

use crate::providers::errors::ProviderError;
use crate::providers::hosted_call::{
    AuthScheme, CallContext, Dialect, MESSAGES_DIALECT_VERSION_DEFAULT,
    MESSAGES_DIALECT_VERSION_HEADER,
};
use crate::providers::hosted_stream::StreamLimits;
use crate::providers::types::{CompletionStatus, GenerateResponse, ProviderHealth, ProviderStatus};
use crate::providers::utils::now;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const GENERATE_TIMEOUT: Duration = Duration::from_secs(60);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Keys that carry the output token limit. Handled separately from the other
/// row options so a call-supplied budget can replace the row's value.
const LIMIT_KEYS: [&str; 2] = ["max_completion_tokens", "max_tokens"];

// ---------------------------------------------------------------------------
// Request / outcome
// ---------------------------------------------------------------------------

/// One generation request. Separate from `GenerateRequest` (which carries
/// Ollama-shaped options and no system prompt) so the client takes exactly
/// what the wire formats need.
#[derive(Debug, Clone, Default)]
pub struct HostedRequest {
    /// Optional system prompt (top-level field on the Messages dialect, a
    /// leading system message on the chat dialect).
    pub system: Option<String>,
    /// The assembled prompt, sent as a single user message.
    pub prompt: String,
    pub temperature: Option<f64>,
    /// Output token budget set explicitly for this call. When `None`, the
    /// model row's `request_options` must carry one.
    pub max_output_tokens: Option<u32>,
}

/// A successful generation plus the signals later items need.
#[derive(Debug, Clone)]
pub struct HostedOutcome {
    pub response: GenerateResponse,
    /// `finish_reason` (chat dialect) or `stop_reason` (Messages dialect).
    pub finish_reason: Option<String>,
    /// Hidden reasoning tokens when the provider reports them (inside
    /// `completion_tokens`); feeds output_overhead calibration (items.id=711).
    pub reasoning_tokens: Option<u32>,
    /// The rate-limit headers the response carried (items.id=710): remaining
    /// free-tier quota for the usage summary.
    pub rate_limit: RateLimitInfo,
    /// The answer was cut off by the output token limit (non-empty text with
    /// the "ran out of tokens" finish reason). The call is billed for the
    /// whole limit; surfacing the truncation to the user is items.id=696.
    pub truncated: bool,
}

/// Token usage a provider reported for one call. Present on any 2xx body that
/// carried a usage object, INCLUDING a body that then failed to yield an
/// answer (a budget-exhausted call is billed for the tokens it burned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostedUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub reasoning_tokens: Option<u32>,
}

/// The rate-limit related response headers of one call, captured generically:
/// any header whose lower-cased name contains `ratelimit`, plus `retry-after`.
/// No header name is spelled out here, so no provider is named in code; the
/// accessors find a quantity by the words in its header name.
///
/// Bounded and sanitised: at most `MAX_RATE_HEADERS` entries, values of
/// visible ASCII at most `MAX_RATE_VALUE_LEN` long. The values are never put
/// into an error message or a log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateLimitInfo {
    entries: Vec<(String, String)>,
}

const MAX_RATE_HEADERS: usize = 32;
const MAX_RATE_VALUE_LEN: usize = 128;

impl RateLimitInfo {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        let mut entries = Vec::new();
        for (name, value) in headers {
            let lname = name.as_str().to_ascii_lowercase();
            if !(lname.contains("ratelimit") || lname == "retry-after") {
                continue;
            }
            // to_str() accepts visible ASCII only.
            let Ok(v) = value.to_str() else { continue };
            let v = v.trim();
            if v.is_empty() || v.len() > MAX_RATE_VALUE_LEN {
                continue;
            }
            entries.push((lname, v.to_owned()));
            if entries.len() >= MAX_RATE_HEADERS {
                break;
            }
        }
        entries.sort();
        Self { entries }
    }

    /// Test and fixture constructor.
    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        let mut entries: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), (*v).to_owned()))
            .collect();
        entries.sort();
        Self { entries }
    }

    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The smallest plain-integer value among headers whose name (with the
    /// `ratelimit` word removed) contains every word in `must` and none in
    /// `must_not`. The smallest is the conservative reading when a provider
    /// reports several windows (for example input and output tokens).
    fn min_count(&self, must: &[&str], must_not: &[&str]) -> Option<u64> {
        self.entries
            .iter()
            .filter(|(name, _)| {
                let n = name.replace("ratelimit", "");
                must.iter().all(|w| n.contains(w)) && !must_not.iter().any(|w| n.contains(w))
            })
            .filter_map(|(_, v)| v.parse::<u64>().ok())
            .min()
    }

    fn first_text(&self, must: &[&str], must_not: &[&str]) -> Option<String> {
        self.entries
            .iter()
            .find(|(name, _)| {
                let n = name.replace("ratelimit", "");
                must.iter().all(|w| n.contains(w)) && !must_not.iter().any(|w| n.contains(w))
            })
            .map(|(_, v)| v.clone())
    }

    pub fn remaining_requests(&self) -> Option<u64> {
        self.min_count(&["remaining", "request"], &["token"])
    }
    pub fn limit_requests(&self) -> Option<u64> {
        self.min_count(&["limit", "request"], &["token", "remaining", "reset"])
    }
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.min_count(&["remaining", "token"], &["request"])
    }
    pub fn limit_tokens(&self) -> Option<u64> {
        self.min_count(&["limit", "token"], &["request", "remaining", "reset"])
    }
    /// The provider's own text for when the request window resets (a duration,
    /// a timestamp or a count; interpretation is the consumer's).
    pub fn reset_requests(&self) -> Option<String> {
        self.first_text(&["reset", "request"], &["token"])
    }
    pub fn reset_tokens(&self) -> Option<String> {
        self.first_text(&["reset", "token"], &["request"])
    }
}

/// Everything one metered call produced, success or failure (items.id=710).
#[derive(Debug)]
pub struct HostedAttempt {
    pub result: Result<HostedOutcome, ProviderError>,
    /// Usage the provider reported, when the response body carried any,
    /// whether or not the call then produced an answer.
    pub usage: Option<HostedUsage>,
    pub rate_limit: RateLimitInfo,
    /// The HTTP status of the response, when one arrived. `None` when the
    /// request failed with no response (a read timeout, a reset).
    pub http_status: Option<u16>,
    /// True when the request may have reached the provider: a response of any
    /// status arrived, or the connection failed after the request was sent
    /// (read timeout, reset). False for failures before sending (a
    /// misconfigured call, a connection that never opened), which count
    /// neither as a request nor as spend.
    pub reached_provider: bool,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Stateless: holds a connection pool and limits, never a key or a provider.
pub struct HostedClient {
    pub(super) http: Client,
    pub(super) generate_timeout: Duration,
    health_timeout: Duration,
    pub(super) max_response_bytes: usize,
    /// Streamed calls (items.id=715, hosted_stream.rs).
    pub(super) stream_limits: StreamLimits,
}

impl HostedClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .https_only(true)
            .redirect(redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("failed to build hosted-provider HTTP client");
        Self {
            http,
            generate_timeout: GENERATE_TIMEOUT,
            health_timeout: HEALTH_TIMEOUT,
            max_response_bytes: MAX_RESPONSE_BYTES,
            stream_limits: StreamLimits::default(),
        }
    }

    /// Test-only: plain-HTTP loopback client with proxies off, redirects off
    /// and caller-chosen limits. Does not exist in non-test builds.
    #[cfg(test)]
    pub(crate) fn for_loopback_tests(
        generate_timeout: Duration,
        health_timeout: Duration,
        max_response_bytes: usize,
    ) -> Self {
        let http = Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build loopback test client");
        Self {
            http,
            generate_timeout,
            health_timeout,
            max_response_bytes,
            stream_limits: StreamLimits::default(),
        }
    }

    /// Test-only: replace the streamed-call limits.
    #[cfg(test)]
    pub(crate) fn with_stream_limits(mut self, limits: StreamLimits) -> Self {
        self.stream_limits = limits;
        self
    }

    /// Send one generation request.
    ///
    /// Privacy contract: the prompt contains abstracted values only
    /// (enforced upstream by StepExecutor Step 8). This method performs no
    /// privacy validation and writes no disclosure log.
    ///
    /// Callers that account for spend use `generate_metered`, which also
    /// returns the usage and headers of failed calls.
    pub async fn generate(
        &self,
        ctx: &CallContext,
        req: &HostedRequest,
    ) -> Result<HostedOutcome, ProviderError> {
        self.generate_metered(ctx, req).await.result
    }

    /// `generate`, plus what the spend ledger needs from every outcome
    /// (items.id=710): the usage the provider reported even when the call
    /// then failed to produce an answer, the rate-limit headers, and whether
    /// the request may have reached the provider.
    pub async fn generate_metered(&self, ctx: &CallContext, req: &HostedRequest) -> HostedAttempt {
        let before_send = |e: ProviderError| HostedAttempt {
            result: Err(e),
            usage: None,
            rate_limit: RateLimitInfo::default(),
            http_status: None,
            reached_provider: false,
        };

        // Fail closed before any traffic: payload (token limit) and headers.
        let payload = match build_payload(ctx, req, false) {
            Ok(p) => p,
            Err(e) => return before_send(e),
        };
        let headers = match self.headers(ctx, true) {
            Ok(h) => h,
            Err(e) => return before_send(e),
        };
        let body = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(_) => return before_send(misconfigured_ctx(ctx, "the request could not be built")),
        };

        let start = Instant::now();
        let response = match self
            .send_raw(
                Method::POST,
                &ctx.generate_url(),
                headers,
                Some(body),
                self.generate_timeout,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // A connection that never opened, or a request that could not
                // be built, never reached the provider. Anything else
                // (read timeout, reset) may have.
                let reached = !(e.is_connect() || e.is_builder());
                return HostedAttempt {
                    result: Err(transport_error(ctx, &e)),
                    usage: None,
                    rate_limit: RateLimitInfo::default(),
                    http_status: None,
                    reached_provider: reached,
                };
            }
        };
        let status = response.status();
        let rate_limit = RateLimitInfo::from_headers(response.headers());
        if !status.is_success() {
            return HostedAttempt {
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
            };
        }
        let bytes = match self.read_body(ctx, response).await {
            Ok(b) => b,
            Err(e) => {
                return HostedAttempt {
                    result: Err(e),
                    usage: None,
                    rate_limit,
                    http_status: Some(status.as_u16()),
                    reached_provider: true,
                }
            }
        };
        let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

        let body: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                return HostedAttempt {
                    result: Err(malformed(ctx, "it was not valid JSON")),
                    usage: None,
                    rate_limit,
                    http_status: Some(status.as_u16()),
                    reached_provider: true,
                }
            }
        };

        // Usage first, so a call that burned tokens and then failed to yield
        // an answer (budget exhausted, malformed answer) is still charged.
        let usage = extract_usage(ctx.dialect, &body);
        let parsed = match ctx.dialect {
            Dialect::OpenaiChat => parse_chat(ctx, &body, usage),
            Dialect::AnthropicMessages => parse_messages(ctx, &body, usage),
        };
        let result = parsed.map(|parsed| HostedOutcome {
            response: GenerateResponse {
                content: parsed.content,
                model: ctx.model_id.clone(),
                prompt_token_count: parsed.prompt_tokens,
                output_token_count: parsed.completion_tokens,
                latency_ms,
                completion_status: CompletionStatus::Complete,
            },
            finish_reason: parsed.finish_reason,
            reasoning_tokens: parsed.reasoning_tokens,
            rate_limit: rate_limit.clone(),
            truncated: parsed.truncated,
        });
        HostedAttempt {
            result,
            usage,
            rate_limit,
            http_status: Some(status.as_u16()),
            reached_provider: true,
        }
    }

    /// GET the provider's model list with the key. `Ok(ids)` means the key
    /// works and the endpoint is reachable; 401 or 403 is `InvalidApiKey`.
    pub async fn check_key(&self, ctx: &CallContext) -> Result<Vec<String>, ProviderError> {
        Ok(self
            .list_models(ctx)
            .await?
            .into_iter()
            .map(|m| m.id)
            .collect())
    }

    /// GET the provider's model list with the key and read what it reports
    /// about each model (items.id=711): the id, the context window and the
    /// retirement date where the provider gives them. Everything but the id
    /// is optional and a malformed optional value is dropped, not an error.
    /// The only production caller is the user-clicked refresh command: QR
    /// never refreshes a list by itself (decisions.id=875 point 5).
    pub async fn list_models(&self, ctx: &CallContext) -> Result<Vec<LiveModel>, ProviderError> {
        let url = ctx
            .models_url()
            .ok_or_else(|| misconfigured_ctx(ctx, "it has no model list to check"))?;
        let headers = self.headers(ctx, false)?;
        let response = self
            .send(ctx, Method::GET, &url, headers, None, self.health_timeout)
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(
                ctx,
                status,
                response.headers(),
                Purpose::ModelList,
            ));
        }
        let bytes = self.read_body(ctx, response).await?;
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|_| malformed(ctx, "the model list was not valid JSON"))?;
        parse_live_models(ctx, &body)
    }

    /// Availability check. Never returns `Err` or panics: every failure is a
    /// `ProviderHealth` value. Timeout or unreachable → Unavailable; a
    /// rejected key or any other failure → Degraded.
    pub async fn health_check(&self, ctx: &CallContext) -> ProviderHealth {
        let (status, error, models) = match self.check_key(ctx).await {
            Ok(ids) => (ProviderStatus::Available, None, ids),
            Err(ProviderError::Timeout { .. }) => (
                ProviderStatus::Unavailable,
                Some("health check timed out".to_owned()),
                vec![],
            ),
            Err(ProviderError::Unavailable { .. }) => (
                ProviderStatus::Unavailable,
                Some("provider unreachable".to_owned()),
                vec![],
            ),
            Err(ProviderError::InvalidApiKey { .. }) => (
                ProviderStatus::Degraded,
                Some("API key rejected".to_owned()),
                vec![],
            ),
            Err(other) => (ProviderStatus::Degraded, Some(other.to_string()), vec![]),
        };
        ProviderHealth {
            provider: ctx.provider_id.clone(),
            status,
            checked_at: now(),
            error,
            available_models: models,
        }
    }

    // -- internals ----------------------------------------------------------

    /// Compose request headers: auth (sensitive), dialect headers, row extras.
    pub(super) fn headers(
        &self,
        ctx: &CallContext,
        with_body: bool,
    ) -> Result<HeaderMap, ProviderError> {
        let mut map = HeaderMap::new();
        map.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if with_body {
            map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }

        let unsendable = || {
            misconfigured_ctx(
                ctx,
                "the saved API key contains characters that can't be sent",
            )
        };
        match ctx.auth {
            AuthScheme::Bearer => {
                let raw = Zeroizing::new(format!("Bearer {}", ctx.key.expose()));
                let mut v = HeaderValue::from_str(raw.as_str()).map_err(|_| unsendable())?;
                v.set_sensitive(true);
                map.insert(AUTHORIZATION, v);
            }
            AuthScheme::XApiKey => {
                let mut v = HeaderValue::from_str(ctx.key.expose()).map_err(|_| unsendable())?;
                v.set_sensitive(true);
                map.insert(HeaderName::from_static("x-api-key"), v);
            }
        }

        if ctx.dialect == Dialect::AnthropicMessages {
            map.insert(
                HeaderName::from_static(MESSAGES_DIALECT_VERSION_HEADER),
                HeaderValue::from_static(MESSAGES_DIALECT_VERSION_DEFAULT),
            );
        }
        // Row headers last: a row may override the dialect's version header.
        for (name, value) in &ctx.extra_headers {
            map.insert(name.clone(), value.clone());
        }
        Ok(map)
    }

    /// The raw send, keeping the transport error so a caller can tell a
    /// connection that never opened from one that failed after sending.
    pub(super) async fn send_raw(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Response, reqwest::Error> {
        let mut request = self
            .http
            .request(method, url)
            .headers(headers)
            .timeout(timeout);
        if let Some(b) = body {
            request = request.body(b);
        }
        request.send().await
    }

    async fn send(
        &self,
        ctx: &CallContext,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Response, ProviderError> {
        self.send_raw(method, url, headers, body, timeout)
            .await
            .map_err(|e| transport_error(ctx, &e))
    }

    /// Read the body, refusing anything larger than the cap (declared length
    /// first, then while streaming, so a lying server can't exceed it).
    async fn read_body(
        &self,
        ctx: &CallContext,
        mut response: Response,
    ) -> Result<Vec<u8>, ProviderError> {
        let too_large = || malformed(ctx, "it was larger than QR accepts");
        if response
            .content_length()
            .is_some_and(|len| len > self.max_response_bytes as u64)
        {
            return Err(too_large());
        }
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if buf.len() + chunk.len() > self.max_response_bytes {
                        return Err(too_large());
                    }
                    buf.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(buf),
                Err(e) => return Err(transport_error(ctx, &e)),
            }
        }
    }
}

impl Default for HostedClient {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

/// The output token limit a call will be sent with: the request's own budget
/// when it set one, else the model row's. `None` means the call has no limit
/// and `build_payload` fails closed. One function, used by the payload and by
/// the spend pre-flight, so the cap is computed from exactly what is sent.
pub(crate) fn effective_output_limit(ctx: &CallContext, req: &HostedRequest) -> Option<u64> {
    req.max_output_tokens
        .filter(|n| *n > 0)
        .map(u64::from)
        .or_else(|| ctx.row_token_limit().map(|(_, v)| v))
}

/// The error for a call with no output token limit.
pub(crate) fn missing_limit_error(ctx: &CallContext) -> ProviderError {
    misconfigured_ctx(
        ctx,
        "this model has no output length limit set, and QR won't send a request \
         without one",
    )
}

/// Build the JSON body. Fails closed when no output token limit is set.
///
/// `stream` is the per-call delivery choice (items.id=715): false for a
/// whole-body call (`"stream": false`, and any `stream_options` removed), true
/// for a streamed call (`"stream": true`; the OpenAI-compatible dialect also
/// asks for a final usage frame unless the row sets `stream_options` itself,
/// where JSON null removes it for a provider that rejects the option).
pub(super) fn build_payload(
    ctx: &CallContext,
    req: &HostedRequest,
    stream: bool,
) -> Result<Value, ProviderError> {
    let limit = effective_output_limit(ctx, req).ok_or_else(|| missing_limit_error(ctx))?;

    let mut payload = Map::new();
    payload.insert("model".to_owned(), json!(ctx.model_id));
    match ctx.dialect {
        Dialect::OpenaiChat => {
            let mut messages = Vec::new();
            if let Some(system) = &req.system {
                messages.push(json!({"role": "system", "content": system}));
            }
            messages.push(json!({"role": "user", "content": req.prompt}));
            payload.insert("messages".to_owned(), Value::Array(messages));
        }
        Dialect::AnthropicMessages => {
            payload.insert(
                "messages".to_owned(),
                json!([{"role": "user", "content": req.prompt}]),
            );
            if let Some(system) = &req.system {
                payload.insert("system".to_owned(), json!(system));
            }
        }
    }
    if let Some(t) = req.temperature.filter(|t| t.is_finite()) {
        payload.insert("temperature".to_owned(), json!(t));
    }

    // Row options: shallow merge, nested objects pass through whole, a JSON
    // null removes a key (some models reject `temperature`). Token-limit keys
    // are skipped here and written once below from the call-or-row value.
    for (key, value) in &ctx.options {
        if LIMIT_KEYS.contains(&key.as_str()) {
            continue;
        }
        if value.is_null() {
            payload.remove(key);
        } else {
            payload.insert(key.clone(), value.clone());
        }
    }
    payload.insert(ctx.limit_key_for_call().to_owned(), json!(limit));

    // The client owns these regardless of what a row said (build rejects them
    // already; this keeps the guarantee local to the code that sends).
    payload.insert("model".to_owned(), json!(ctx.model_id));
    payload.insert("stream".to_owned(), Value::Bool(stream));
    if stream {
        if ctx.dialect == Dialect::OpenaiChat && !ctx.options.contains_key("stream_options") {
            payload.insert("stream_options".to_owned(), json!({"include_usage": true}));
        }
    } else {
        payload.remove("stream_options");
    }
    Ok(Value::Object(payload))
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

pub(super) struct Parsed {
    pub(super) content: String,
    pub(super) finish_reason: Option<String>,
    pub(super) truncated: bool,
    pub(super) prompt_tokens: u32,
    pub(super) completion_tokens: u32,
    pub(super) reasoning_tokens: Option<u32>,
}

pub(super) fn token_count(v: &Value) -> u32 {
    v.as_u64()
        .map(|n| n.min(u64::from(u32::MAX)) as u32)
        .unwrap_or(0)
}

/// The usage object of a 2xx body, when it carries one with at least one
/// token count. Read before the answer is examined (items.id=710).
pub(super) fn extract_usage(dialect: Dialect, body: &Value) -> Option<HostedUsage> {
    let usage = body.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    let (prompt_key, completion_key) = match dialect {
        Dialect::OpenaiChat => ("prompt_tokens", "completion_tokens"),
        Dialect::AnthropicMessages => ("input_tokens", "output_tokens"),
    };
    if usage.get(prompt_key).and_then(Value::as_u64).is_none()
        && usage.get(completion_key).and_then(Value::as_u64).is_none()
    {
        return None;
    }
    let reasoning_tokens = match dialect {
        Dialect::OpenaiChat => usage["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .map(|n| n.min(u64::from(u32::MAX)) as u32),
        Dialect::AnthropicMessages => None,
    };
    Some(HostedUsage {
        prompt_tokens: token_count(&usage[prompt_key]),
        completion_tokens: token_count(&usage[completion_key]),
        reasoning_tokens,
    })
}

pub(super) fn parse_chat(
    ctx: &CallContext,
    body: &Value,
    usage: Option<HostedUsage>,
) -> Result<Parsed, ProviderError> {
    let choice = &body["choices"][0];
    if choice.is_null() {
        return Err(malformed(ctx, "it had no answer in it"));
    }
    let finish_reason = choice["finish_reason"].as_str().map(str::to_owned);
    let content = choice["message"]["content"].as_str().map(str::to_owned);
    let content = answer_or_error(ctx, content, finish_reason.as_deref(), "length")?;

    let truncated = finish_reason.as_deref() == Some("length");
    Ok(Parsed {
        content,
        finish_reason,
        truncated,
        prompt_tokens: usage.map_or(0, |u| u.prompt_tokens),
        completion_tokens: usage.map_or(0, |u| u.completion_tokens),
        reasoning_tokens: usage.and_then(|u| u.reasoning_tokens),
    })
}

pub(super) fn parse_messages(
    ctx: &CallContext,
    body: &Value,
    usage: Option<HostedUsage>,
) -> Result<Parsed, ProviderError> {
    let blocks = body["content"]
        .as_array()
        .ok_or_else(|| malformed(ctx, "it had no content in it"))?;
    let text: String = blocks
        .iter()
        .filter(|b| b["type"].as_str() == Some("text"))
        .filter_map(|b| b["text"].as_str())
        .collect();
    let finish_reason = body["stop_reason"].as_str().map(str::to_owned);
    let content = answer_or_error(
        ctx,
        if text.is_empty() { None } else { Some(text) },
        finish_reason.as_deref(),
        "max_tokens",
    )?;

    let truncated = finish_reason.as_deref() == Some("max_tokens");
    Ok(Parsed {
        content,
        finish_reason,
        truncated,
        prompt_tokens: usage.map_or(0, |u| u.prompt_tokens),
        completion_tokens: usage.map_or(0, |u| u.completion_tokens),
        reasoning_tokens: None,
    })
}

/// Present, non-empty answer text → Ok. Empty or missing text with the
/// "ran out of tokens" finish reason → OutputBudgetExhausted (hidden
/// reasoning can consume the whole budget). A present-but-empty answer with
/// any other reason is returned as is (unchanged behavior); a missing one is
/// malformed. Non-empty text cut off by the limit stays a normal answer: its
/// truncation is tracked under items.id=696 and 716.
pub(super) fn answer_or_error(
    ctx: &CallContext,
    content: Option<String>,
    finish_reason: Option<&str>,
    length_reason: &str,
) -> Result<String, ProviderError> {
    match content {
        Some(text) if !text.is_empty() => Ok(text),
        other => {
            if finish_reason == Some(length_reason) {
                Err(ProviderError::OutputBudgetExhausted {
                    provider: ctx.provider_id.clone(),
                    plain_language: format!(
                        "{} used up this model's output limit before writing an answer. \
                         [Try again] [Get help]",
                        ctx.display_name
                    ),
                })
            } else if let Some(text) = other {
                Ok(text)
            } else {
                Err(malformed(ctx, "it had no answer text in it"))
            }
        }
    }
}

/// One model from a provider's live list (items.id=711).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveModel {
    pub id: String,
    /// Context window in tokens, when the list reports one.
    pub context_tokens: Option<u32>,
    /// Retirement date text as the provider gave it, when it reports one.
    pub retires_at: Option<String>,
}

/// The most models one list may contribute. A longer list is cut here, in
/// provider order, so a hostile or broken response cannot grow the store.
pub const MAX_LIVE_MODELS: usize = 2000;
const MAX_MODEL_ID_LEN: usize = 200;

/// Wire field names under which lists report a context window, in the order
/// tried. Names describe a wire format; none names a provider.
const CONTEXT_FIELDS: [&str; 5] = [
    "context_window",
    "max_context_length",
    "max_input_tokens",
    "context_length",
    "inputTokenLimit",
];
const RETIRES_FIELDS: [&str; 3] = ["retires_at", "expiration_date", "expires_at"];

/// A model id is later sent back to the provider as a request field, so a
/// list entry is kept only if it is plain printable ASCII with no spaces.
fn valid_model_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_MODEL_ID_LEN && id.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn live_context(entry: &Value) -> Option<u32> {
    CONTEXT_FIELDS
        .iter()
        .find_map(|k| entry.get(*k).and_then(Value::as_u64))
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
}

fn live_retires_at(entry: &Value) -> Option<String> {
    RETIRES_FIELDS
        .iter()
        .find_map(|k| entry.get(*k).and_then(Value::as_str))
        .filter(|t| {
            !t.is_empty()
                && t.len() <= 40
                && t.bytes().all(|b| {
                    b.is_ascii_digit() || matches!(b, b'-' | b':' | b'T' | b'Z' | b'.' | b'+')
                })
        })
        .map(str::to_owned)
}

fn parse_live_models(ctx: &CallContext, body: &Value) -> Result<Vec<LiveModel>, ProviderError> {
    let list = body["data"]
        .as_array()
        .or_else(|| body["models"].as_array())
        .ok_or_else(|| malformed(ctx, "the model list had an unexpected shape"))?;
    let mut out: Vec<LiveModel> = Vec::new();
    for entry in list {
        let Some(id) = entry["id"].as_str().or_else(|| entry["name"].as_str()) else {
            continue;
        };
        if !valid_model_id(id) || out.iter().any(|m| m.id == id) {
            continue;
        }
        out.push(LiveModel {
            id: id.to_owned(),
            context_tokens: live_context(entry),
            retires_at: live_retires_at(entry),
        });
        if out.len() >= MAX_LIVE_MODELS {
            break;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Error mapping (copy comes from the row's display name)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Purpose {
    Generate,
    ModelList,
}

pub(super) fn misconfigured_ctx(ctx: &CallContext, detail: &str) -> ProviderError {
    // Same wording as hosted_call::misconfigured, built from a context
    // (which no longer has the route).
    ProviderError::Misconfigured {
        provider: ctx.provider_id.clone(),
        plain_language: format!(
            "{} can't be used because {detail}. [Get help] [Use local AI instead]",
            ctx.display_name
        ),
    }
}

pub(super) fn malformed(ctx: &CallContext, what: &str) -> ProviderError {
    ProviderError::MalformedResponse {
        provider: ctx.provider_id.clone(),
        plain_language: format!(
            "{} sent a reply QR couldn't read ({what}). [Try again] [Get help]",
            ctx.display_name
        ),
    }
}

pub(super) fn transport_error(ctx: &CallContext, e: &reqwest::Error) -> ProviderError {
    let name = &ctx.display_name;
    let provider = ctx.provider_id.clone();
    if e.is_timeout() {
        ProviderError::Timeout {
            provider,
            plain_language: format!(
                "{name} didn't respond in time. [Try again] [Use local AI instead]"
            ),
        }
    } else if e.is_builder() {
        misconfigured_ctx(ctx, "its address isn't allowed")
    } else if e.is_connect() {
        ProviderError::Unavailable {
            provider,
            plain_language: format!(
                "{name} is unreachable. Check your internet connection. \
                 [Try again] [Use local AI instead]"
            ),
        }
    } else {
        ProviderError::Unavailable {
            provider,
            plain_language: format!(
                "The connection to {name} failed. [Try again] [Use local AI instead]"
            ),
        }
    }
}

fn retry_after_secs(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

pub(super) fn status_error(
    ctx: &CallContext,
    status: StatusCode,
    headers: &HeaderMap,
    purpose: Purpose,
) -> ProviderError {
    let name = &ctx.display_name;
    let provider = ctx.provider_id.clone();
    let code = status.as_u16();
    match code {
        401 | 403 => ProviderError::InvalidApiKey {
            provider,
            plain_language: format!(
                "{name} rejected the API key. Check that it is correct and has not \
                 expired. [Open Settings] [Get help]"
            ),
        },
        402 => ProviderError::PaymentRequired {
            provider,
            plain_language: format!(
                "{name} needs a payment method or more credit on your account \
                 before it can answer. [Open {name}] [Use local AI instead]"
            ),
        },
        404 if purpose == Purpose::Generate => ProviderError::ModelNotFound {
            provider,
            plain_language: format!(
                "{name} doesn't recognize this model any more; it may have been retired. \
                 Choose another model in Settings. [Open Settings] [Get help]"
            ),
        },
        404 => misconfigured_ctx(ctx, "its model list address wasn't found"),
        408 => ProviderError::Timeout {
            provider,
            plain_language: format!(
                "{name} didn't respond in time. [Try again] [Use local AI instead]"
            ),
        },
        429 => {
            let retry = retry_after_secs(headers);
            let wait = retry
                .map(|s| format!(" Try again in about {s} seconds."))
                .unwrap_or_default();
            ProviderError::RateLimit {
                provider,
                retry_after_secs: retry,
                plain_language: format!(
                    "{name} rate limit reached.{wait} [Try again in a moment] \
                     [Use local AI instead]"
                ),
            }
        }
        500..=599 => ProviderError::ServerError {
            provider,
            status_code: code,
            plain_language: format!(
                "{name} had a problem on its side ({code}). [Try again] [Use local AI instead]"
            ),
        },
        300..=399 => ProviderError::ProviderFailure {
            provider,
            status_code: Some(code),
            plain_language: format!(
                "{name} tried to send QR to a different address, which QR refuses \
                 to follow. [Get help]"
            ),
        },
        _ => ProviderError::ProviderFailure {
            provider,
            status_code: Some(code),
            plain_language: format!(
                "{name} returned an unexpected error ({code}). [Try again] [Get help]"
            ),
        },
    }
}

#[cfg(test)]
#[path = "hosted_client_tests.rs"]
mod tests;
