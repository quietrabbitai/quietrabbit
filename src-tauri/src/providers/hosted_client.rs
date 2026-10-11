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
//   - `stream` is always false (items.id=715 owns streaming).
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
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Stateless: holds a connection pool and limits, never a key or a provider.
pub struct HostedClient {
    http: Client,
    generate_timeout: Duration,
    health_timeout: Duration,
    max_response_bytes: usize,
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
        }
    }

    /// Send one generation request.
    ///
    /// Privacy contract: the prompt contains abstracted values only
    /// (enforced upstream by StepExecutor Step 8). This method performs no
    /// privacy validation and writes no disclosure log.
    pub async fn generate(
        &self,
        ctx: &CallContext,
        req: &HostedRequest,
    ) -> Result<HostedOutcome, ProviderError> {
        // Fail closed before any traffic: payload (token limit) and headers.
        let payload = build_payload(ctx, req)?;
        let headers = self.headers(ctx, true)?;
        let body = serde_json::to_vec(&payload)
            .map_err(|_| misconfigured_ctx(ctx, "the request could not be built"))?;

        let start = Instant::now();
        let response = self
            .send(
                ctx,
                Method::POST,
                &ctx.generate_url(),
                headers,
                Some(body),
                self.generate_timeout,
            )
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(
                ctx,
                status,
                response.headers(),
                Purpose::Generate,
            ));
        }
        let bytes = self.read_body(ctx, response).await?;
        let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

        let body: Value =
            serde_json::from_slice(&bytes).map_err(|_| malformed(ctx, "it was not valid JSON"))?;
        let parsed = match ctx.dialect {
            Dialect::OpenaiChat => parse_chat(ctx, &body)?,
            Dialect::AnthropicMessages => parse_messages(ctx, &body)?,
        };

        Ok(HostedOutcome {
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
        })
    }

    /// GET the provider's model list with the key. `Ok(ids)` means the key
    /// works and the endpoint is reachable; 401 or 403 is `InvalidApiKey`.
    pub async fn check_key(&self, ctx: &CallContext) -> Result<Vec<String>, ProviderError> {
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
        parse_model_ids(ctx, &body)
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
    fn headers(&self, ctx: &CallContext, with_body: bool) -> Result<HeaderMap, ProviderError> {
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

    async fn send(
        &self,
        ctx: &CallContext,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Response, ProviderError> {
        let mut request = self
            .http
            .request(method, url)
            .headers(headers)
            .timeout(timeout);
        if let Some(b) = body {
            request = request.body(b);
        }
        request.send().await.map_err(|e| transport_error(ctx, &e))
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

/// Build the JSON body. Fails closed when no output token limit is set.
fn build_payload(ctx: &CallContext, req: &HostedRequest) -> Result<Value, ProviderError> {
    let limit = req
        .max_output_tokens
        .filter(|n| *n > 0)
        .map(u64::from)
        .or_else(|| ctx.row_token_limit().map(|(_, v)| v))
        .ok_or_else(|| {
            misconfigured_ctx(
                ctx,
                "this model has no output length limit set, and QR won't send a request \
                 without one",
            )
        })?;

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
    payload.insert("stream".to_owned(), Value::Bool(false));
    Ok(Value::Object(payload))
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

struct Parsed {
    content: String,
    finish_reason: Option<String>,
    prompt_tokens: u32,
    completion_tokens: u32,
    reasoning_tokens: Option<u32>,
}

fn token_count(v: &Value) -> u32 {
    v.as_u64()
        .map(|n| n.min(u64::from(u32::MAX)) as u32)
        .unwrap_or(0)
}

fn parse_chat(ctx: &CallContext, body: &Value) -> Result<Parsed, ProviderError> {
    let choice = &body["choices"][0];
    if choice.is_null() {
        return Err(malformed(ctx, "it had no answer in it"));
    }
    let finish_reason = choice["finish_reason"].as_str().map(str::to_owned);
    let content = choice["message"]["content"].as_str().map(str::to_owned);
    let content = answer_or_error(ctx, content, finish_reason.as_deref(), "length")?;

    let usage = &body["usage"];
    Ok(Parsed {
        content,
        finish_reason,
        prompt_tokens: token_count(&usage["prompt_tokens"]),
        completion_tokens: token_count(&usage["completion_tokens"]),
        reasoning_tokens: usage["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .map(|n| n.min(u64::from(u32::MAX)) as u32),
    })
}

fn parse_messages(ctx: &CallContext, body: &Value) -> Result<Parsed, ProviderError> {
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

    let usage = &body["usage"];
    Ok(Parsed {
        content,
        finish_reason,
        prompt_tokens: token_count(&usage["input_tokens"]),
        completion_tokens: token_count(&usage["output_tokens"]),
        reasoning_tokens: None,
    })
}

/// Present, non-empty answer text → Ok. Empty or missing text with the
/// "ran out of tokens" finish reason → OutputBudgetExhausted (hidden
/// reasoning can consume the whole budget). A present-but-empty answer with
/// any other reason is returned as is (unchanged behavior); a missing one is
/// malformed. Non-empty text cut off by the limit stays a normal answer: its
/// truncation is tracked under items.id=696 and 716.
fn answer_or_error(
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

fn parse_model_ids(ctx: &CallContext, body: &Value) -> Result<Vec<String>, ProviderError> {
    let list = body["data"]
        .as_array()
        .or_else(|| body["models"].as_array())
        .ok_or_else(|| malformed(ctx, "the model list had an unexpected shape"))?;
    Ok(list
        .iter()
        .filter_map(|m| {
            m["id"]
                .as_str()
                .or_else(|| m["name"].as_str())
                .map(str::to_owned)
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Error mapping (copy comes from the row's display name)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Generate,
    ModelList,
}

fn misconfigured_ctx(ctx: &CallContext, detail: &str) -> ProviderError {
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

fn malformed(ctx: &CallContext, what: &str) -> ProviderError {
    ProviderError::MalformedResponse {
        provider: ctx.provider_id.clone(),
        plain_language: format!(
            "{} sent a reply QR couldn't read ({what}). [Try again] [Get help]",
            ctx.display_name
        ),
    }
}

fn transport_error(ctx: &CallContext, e: &reqwest::Error) -> ProviderError {
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

fn status_error(
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
