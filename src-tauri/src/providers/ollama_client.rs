//! Ollama qr_local HTTP client.
//!
//! Ollama is qr_local — it does NOT implement [`QrHostedProvider`].
//! Inference-call errors map directly to [`ConductorError`] at every raise
//! site. No [`ProviderError`] intermediary — qr_local maps directly.
//! EXCEPTION (items.id=436): `pull_model`/`delete_model` map to
//! [`OllamaModelError`] instead — model install/delete has no
//! step_id/FocusRun context, so it isn't a fit for `ConductorError`'s
//! Conductor step-execution failure taxonomy. See that type's own doc
//! comment.
//!
//! `stream` is always `false` in Release 1 — resolved by `StepExecutor`.
//!
//! Four [`reqwest::Client`] instances are held:
//! - `client` (120s): inference calls (`/api/generate`, `/api/chat`, `/api/create`)
//! - `health_client` (5s): health, model enumeration, modelfile show
//! - `modelfile_client` (300s): modelfile application (`/api/create`)
//! - `pull_client` (no timeout): model pull/delete (items.id=436)
//!
//! # Sidecar trust gate (items.id=586)
//! Every method that makes an HTTP call checks `ollama_sidecar::is_trusted()`
//! first and short-circuits to the same shape each already returns for a
//! real network failure, never making the call at all, if it's false.
//! `OllamaClient` itself has no notion of *why* -- the gate is a single
//! process-wide flag `ollama_sidecar.rs` owns, fail-closed by default, set
//! true only once that module has itself confirmed the sidecar it started
//! is ready, and false again the moment it observes that sidecar has died.
//! Without this, `OllamaClient` would otherwise independently re-probe
//! port `QR_OLLAMA_PORT` on every call with no knowledge of whether QR
//! started what's answering there -- confirmed concretely this session
//! (not hypothetically): after a `cargo tauri dev` watcher restart left an
//! old `ollama serve` instance still bound to the port, the *new* process's
//! own `ensure_available()` correctly logged "sidecar failed to start or
//! become ready" (the port was taken), but prior to this gate, `generate`/
//! `chat` calls against that same new process would still have silently
//! talked to the stale instance regardless, since nothing connected the
//! two. `check_modelfile_version` makes no HTTP call of its own (only
//! calls the already-gated `get_applied_modelfile_version`), so it needs
//! no separate check.

use std::time::Duration;

use futures_util::{Stream, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::conductor::failure::ConductorError;
use crate::providers::types::{
    ChatMessage, ContextWindowStatus, ContextWindowStatusKind, GenerateOptions, GenerateRequest,
    GenerateResponse, ModelfileVersion, ProviderHealth, ProviderStatus, RecommendedAction,
};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const OLLAMA_TIMEOUT_SECS: u64 = 120;
const OLLAMA_CONNECT_TIMEOUT_SECS: u64 = 5;
const OLLAMA_MODELFILE_TIMEOUT_SECS: u64 = 300;

const CONTEXT_WARNING_THRESHOLD_DEFAULT: f64 = 0.75;
const CONTEXT_HARD_LIMIT_DEFAULT: f64 = 0.95;

/// Task types that receive a 20% safety buffer in token estimation.
///
/// These types produce denser token output than the 4-char/token heuristic
/// assumes, increasing the risk of silent context window overflow.
/// Over-estimation is safe (triggers compaction earlier, never fails hard).
///
/// Python oracle: `_BUFFERED_TASK_TYPES` frozenset in ollama_client.py
const BUFFERED_TASK_TYPES: &[&str] = &["code", "research", "creative_writing", "prose"];

/// Returns `http://{OLLAMA_HOST}:{OLLAMA_PORT}`.
///
/// `OLLAMA_HOST` must be a bare hostname or IP — not a full URL.
/// This matches Python oracle: `f"http://{host}:{port}"`.
///
/// Default port is `ollama_sidecar::QR_OLLAMA_PORT` (21434) — QR's own
/// dedicated sidecar port, never 11434, never negotiated
/// (decisions.id=840). Safe to change here as the single default: nothing
/// in this codebase sets `OLLAMA_HOST`/`OLLAMA_PORT` on QR's own process
/// environment today (`ollama_sidecar.rs` only sets them on the *spawned
/// child's* environment).
fn base_url() -> String {
    let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned());
    let port = std::env::var("OLLAMA_PORT")
        .unwrap_or_else(|_| crate::ollama_sidecar::QR_OLLAMA_PORT.to_string());
    format!("http://{}:{}", host, port)
}

fn context_warning_threshold() -> f64 {
    std::env::var("QR_CONTEXT_WARNING_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CONTEXT_WARNING_THRESHOLD_DEFAULT)
}

fn context_hard_limit() -> f64 {
    std::env::var("QR_CONTEXT_HARD_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CONTEXT_HARD_LIMIT_DEFAULT)
}

/// The sidecar trust gate (see module doc comment, items.id=586). Logs once
/// per refused call rather than silently returning the unavailable shape --
/// the refusal itself is the interesting event, distinct from an ordinary
/// network failure.
fn sidecar_trusted() -> bool {
    let trusted = crate::ollama_sidecar::is_trusted();
    if !trusted {
        log::warn!(
            "ollama_client: QR's own sidecar is not confirmed running -- refusing to contact \
             127.0.0.1:{} (items.id=586)",
            crate::ollama_sidecar::QR_OLLAMA_PORT
        );
    }
    trusted
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Ollama qr_local HTTP client.
///
/// Holds four `reqwest::Client` instances with different timeouts:
/// - `client` (120s): inference calls (`/api/generate`, `/api/chat`).
/// - `health_client` (5s): health check, tags, show.
/// - `modelfile_client` (300s): modelfile application (`/api/create`).
/// - `pull_client` (no timeout): model pull/delete (`/api/pull`,
///   `/api/delete`, items.id=436) — a pull can legitimately run for many
///   minutes; cancellation is via `CancellationToken`
///   (`providers::ollama_install::run_install`), not a deadline.
///
/// Construct once per Conductor actor. Not `Clone` — single owner per actor.
pub struct OllamaClient {
    client: Client,
    health_client: Client,
    modelfile_client: Client,
    pull_client: Client,
}

impl Default for OllamaClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(OLLAMA_TIMEOUT_SECS))
                .build()
                .expect("reqwest client build should never fail"),
            health_client: Client::builder()
                .timeout(Duration::from_secs(OLLAMA_CONNECT_TIMEOUT_SECS))
                .build()
                .expect("reqwest health client build should never fail"),
            modelfile_client: Client::builder()
                .timeout(Duration::from_secs(OLLAMA_MODELFILE_TIMEOUT_SECS))
                .build()
                .expect("reqwest modelfile client build should never fail"),
            pull_client: Client::builder()
                .build()
                .expect("reqwest pull client build should never fail"),
        }
    }

    // -----------------------------------------------------------------------
    // Health
    // -----------------------------------------------------------------------

    /// Check Ollama connectivity and enumerate available models.
    ///
    /// Never raises — always returns a [`ProviderHealth`].
    /// Called at startup and periodically by the health monitor.
    ///
    /// Python oracle: `check_ollama_health()`
    pub async fn check_health(&self) -> ProviderHealth {
        if !sidecar_trusted() {
            return ProviderHealth {
                provider: "ollama".to_owned(),
                status: ProviderStatus::Unavailable,
                checked_at: crate::providers::utils::now(),
                error: Some("sidecar_untrusted".to_owned()),
                available_models: vec![],
            };
        }

        let url = format!("{}/api/tags", base_url());
        match self.health_client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let available = resp
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("models").and_then(|m| m.as_array()).cloned())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|m| m.get("name")?.as_str().map(str::to_owned))
                    .collect();
                ProviderHealth {
                    provider: "ollama".to_owned(),
                    status: ProviderStatus::Available,
                    checked_at: crate::providers::utils::now(),
                    error: None,
                    available_models: available,
                }
            }
            Ok(resp) => ProviderHealth {
                provider: "ollama".to_owned(),
                status: ProviderStatus::Degraded,
                checked_at: crate::providers::utils::now(),
                error: Some(format!("HTTP {}", resp.status())),
                available_models: vec![],
            },
            Err(e) if e.is_timeout() => ProviderHealth {
                provider: "ollama".to_owned(),
                status: ProviderStatus::Unavailable,
                checked_at: crate::providers::utils::now(),
                error: Some("timeout".to_owned()),
                available_models: vec![],
            },
            Err(_) => ProviderHealth {
                provider: "ollama".to_owned(),
                status: ProviderStatus::Unavailable,
                checked_at: crate::providers::utils::now(),
                error: Some("connection_refused".to_owned()),
                available_models: vec![],
            },
        }
    }

    // -----------------------------------------------------------------------
    // Single-turn generation
    // -----------------------------------------------------------------------

    /// Primary qr_local inference call.
    ///
    /// Latency is always tracked — never hardcoded to 0.
    /// `stream` is always `false` in Release 1 (resolved by `StepExecutor`).
    ///
    /// When `request.options` is `None`, all four fields fall back to
    /// the sane qr_local defaults. This matches the Python oracle's explicit
    /// fallback construction in `generate()`.
    ///
    /// Errors map directly to [`ConductorError`] — no `ProviderError` boundary.
    ///
    /// Python oracle: `generate()`
    pub async fn generate(
        &self,
        request: &GenerateRequest,
    ) -> Result<GenerateResponse, ConductorError> {
        if !sidecar_trusted() {
            return Err(ConductorError::OllamaUnavailable {
                plain_language: "The local AI isn't responding. \
                    [Try again] [Use an external service] [Get help]"
                    .to_owned(),
            });
        }

        let options = request.options.clone().unwrap_or(GenerateOptions {
            temperature: 0.5,
            top_p: 0.90,
            num_ctx: 2048,
            num_predict: 2048,
        });

        let mut payload = serde_json::json!({
            "model": request.model_id,
            "prompt": request.prompt,
            "stream": false,
            "options": {
                "temperature": options.temperature,
                "top_p": options.top_p,
                "num_ctx": options.num_ctx,
                "num_predict": options.num_predict,
            }
        });
        if let Some(images) = &request.images {
            if !images.is_empty() {
                payload["images"] = serde_json::json!(images);
            }
        }

        let url = format!("{}/api/generate", base_url());
        let start = std::time::Instant::now();

        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ConductorError::OllamaTimeout {
                        plain_language: "The local AI took too long to respond. \
                            [Try again] [Use an external service]"
                            .to_owned(),
                    }
                } else {
                    ConductorError::OllamaUnavailable {
                        plain_language: "The local AI isn't responding. \
                            [Try again] [Use an external service] [Get help]"
                            .to_owned(),
                    }
                }
            })?;

        let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
        let status = resp.status();

        if status == 400 {
            return Err(ConductorError::OllamaInvalidRequest {
                plain_language: "The local AI didn't understand the request. \
                    This is likely a configuration issue. [Get help]"
                    .to_owned(),
            });
        }
        // items.id=587: confirmed live against a real Ollama instance --
        // an unpulled model returns HTTP 404 {"error":"model '<name>' not
        // found"} on /api/generate, distinct from any other generation
        // fault. Checked before the generic !status.is_success() branch
        // below, which would otherwise swallow this into the generic
        // "returned an unexpected response" message.
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ConductorError::OllamaModelMissing {
                plain_language: "Quiet Rabbit's local models aren't installed yet. \
                    [Get help]"
                    .to_owned(),
            });
        }
        if !status.is_success() {
            return Err(ConductorError::OllamaGeneration {
                plain_language: "The local AI returned an unexpected response. \
                    [Try again] [Get help]"
                    .to_owned(),
            });
        }

        let data: Value = resp
            .json()
            .await
            .map_err(|_| ConductorError::OllamaGeneration {
                plain_language: "The local AI returned an unexpected response. \
                [Try again] [Get help]"
                    .to_owned(),
            })?;

        Ok(GenerateResponse {
            content: data["response"].as_str().unwrap_or("").to_owned(),
            model: data["model"]
                .as_str()
                .unwrap_or(&request.model_id)
                .to_owned(),
            prompt_token_count: data["prompt_eval_count"].as_u64().unwrap_or(0) as u32,
            output_token_count: data["eval_count"].as_u64().unwrap_or(0) as u32,
            latency_ms,
            completion_status: Default::default(),
        })
    }

    // -----------------------------------------------------------------------
    // Multi-turn chat
    // -----------------------------------------------------------------------

    /// Multi-turn chat for interview flows, Focus Builder, and disclosure dialogs.
    ///
    /// Latency is always tracked — never hardcoded to 0.
    /// `stream` is always `false` in Release 1.
    ///
    /// `_task_type` is required for oracle signature parity — it is used by
    /// routing and evaluation layers but is NOT part of the Ollama chat payload.
    ///
    /// `num_predict` is intentionally excluded from the chat payload per the
    /// Python oracle spec (`chat()` constructs only temperature/top_p/num_ctx).
    ///
    /// Python oracle: `chat()`
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        model: &str,
        _task_type: &str,
        options: Option<GenerateOptions>,
    ) -> Result<GenerateResponse, ConductorError> {
        if !sidecar_trusted() {
            return Err(ConductorError::OllamaUnavailable {
                plain_language: "The local AI isn't responding. [Try again] [Get help]".to_owned(),
            });
        }

        let opts = options.unwrap_or(GenerateOptions {
            temperature: 0.5,
            top_p: 0.90,
            num_ctx: 2048,
            // num_predict is oracle default but NOT serialized in chat payload.
            // Set to oracle default so the struct is valid; excluded from JSON below.
            num_predict: 2048,
        });

        // num_predict intentionally excluded from options block per Python oracle.
        let payload = serde_json::json!({
            "model": model,
            "messages": messages.iter().map(|m| serde_json::json!({
                "role": m.role,
                "content": m.content,
            })).collect::<Vec<_>>(),
            "stream": false,
            "options": {
                "temperature": opts.temperature,
                "top_p": opts.top_p,
                "num_ctx": opts.num_ctx,
            }
        });

        let url = format!("{}/api/chat", base_url());
        let start = std::time::Instant::now();

        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ConductorError::OllamaTimeout {
                        plain_language: "The local AI took too long to respond. [Try again]"
                            .to_owned(),
                    }
                } else {
                    ConductorError::OllamaUnavailable {
                        plain_language: "The local AI isn't responding. [Try again] [Get help]"
                            .to_owned(),
                    }
                }
            })?;

        let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
        let status = resp.status();

        // Mirror generate() error taxonomy for parity (F1 classification).
        if status == 400 {
            return Err(ConductorError::OllamaInvalidRequest {
                plain_language: "The local AI didn't understand the request. \
                    This is likely a configuration issue. [Get help]"
                    .to_owned(),
            });
        }
        // items.id=587: same model-not-found 404 as generate() above.
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ConductorError::OllamaModelMissing {
                plain_language: "Quiet Rabbit's local models aren't installed yet. \
                    [Get help]"
                    .to_owned(),
            });
        }
        if !status.is_success() {
            return Err(ConductorError::OllamaGeneration {
                plain_language: "The local AI returned an unexpected response. [Try again]"
                    .to_owned(),
            });
        }

        let data: Value = resp
            .json()
            .await
            .map_err(|_| ConductorError::OllamaGeneration {
                plain_language: "The local AI returned an unexpected response. [Try again]"
                    .to_owned(),
            })?;

        Ok(GenerateResponse {
            content: data["message"]["content"].as_str().unwrap_or("").to_owned(),
            model: data["model"].as_str().unwrap_or(model).to_owned(),
            prompt_token_count: data["prompt_eval_count"].as_u64().unwrap_or(0) as u32,
            output_token_count: data["eval_count"].as_u64().unwrap_or(0) as u32,
            latency_ms,
            completion_status: Default::default(),
        })
    }

    // -----------------------------------------------------------------------
    // Modelfile management
    // -----------------------------------------------------------------------

    /// Read the `QR-MODELFILE-VERSION` comment from the applied Modelfile.
    ///
    /// Returns `None` if the model is not found or the comment is absent.
    ///
    /// Python oracle: `get_applied_modelfile_version()`
    async fn get_applied_modelfile_version(&self, model_name: &str) -> Option<String> {
        if !sidecar_trusted() {
            return None;
        }

        let url = format!("{}/api/show", base_url());
        let resp = self
            .health_client
            .post(&url)
            .json(&serde_json::json!({ "name": model_name }))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let data: Value = resp.json().await.ok()?;
        let modelfile = data.get("modelfile")?.as_str()?;
        for line in modelfile.lines() {
            if let Some(rest) = line.strip_prefix("# QR-MODELFILE-VERSION:") {
                return Some(rest.trim().to_owned());
            }
        }
        None
    }

    /// Check whether the applied Modelfile matches the expected version.
    ///
    /// Python oracle: `check_modelfile_version()`
    pub async fn check_modelfile_version(
        &self,
        model_id: &str,
        expected_version: &str,
    ) -> ModelfileVersion {
        let applied = self.get_applied_modelfile_version(model_id).await;
        let is_current = applied.as_deref() == Some(expected_version);
        ModelfileVersion {
            model_id: model_id.to_owned(),
            expected_version: expected_version.to_owned(),
            applied_version: applied,
            is_current,
        }
    }

    /// Apply a Modelfile via `/api/create`.
    ///
    /// Accepts modelfile content as `&str` — file I/O is the caller's
    /// responsibility (keeps provider client as a pure network gateway).
    ///
    /// Validates NDJSON response body — HTTP 200 does not guarantee success.
    /// Checks the final non-empty line for `{"status":"success"}` before
    /// confirming. This is a documented NotebookLM bug fix — do not simplify
    /// to `resp.status().is_success()`.
    ///
    /// Returns `true` if applied successfully, `false` otherwise.
    /// Never raises — infallible per oracle contract.
    ///
    /// Python oracle: `apply_modelfile()`
    pub async fn apply_modelfile(&self, model_name: &str, modelfile_content: &str) -> bool {
        if !sidecar_trusted() {
            return false;
        }

        let url = format!("{}/api/create", base_url());
        let resp = self
            .modelfile_client
            .post(&url)
            .json(&serde_json::json!({
                "name": model_name,
                "modelfile": modelfile_content,
            }))
            .send()
            .await;

        let resp = match resp {
            Ok(r) if r.status().is_success() => r,
            _ => return false,
        };

        let body = match resp.text().await {
            Ok(t) => t,
            Err(_) => return false,
        };

        // Validate final non-empty NDJSON line for {"status":"success"}.
        // HTTP 200 alone does not mean the model was created successfully.
        let success = body
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .and_then(|line| serde_json::from_str::<Value>(line).ok())
            .and_then(|v| {
                v.get("status")
                    .and_then(|s| s.as_str())
                    .map(|s| s == "success")
            })
            .unwrap_or(false);

        success
    }

    // -----------------------------------------------------------------------
    // Model management (items.id=436)
    // -----------------------------------------------------------------------

    /// Pull a model via `/api/pull`, returning a stream of decoded NDJSON
    /// progress lines.
    ///
    /// Unlike `apply_modelfile()` above, this genuinely streams: each
    /// `Bytes` chunk from `reqwest::Response::bytes_stream()` is buffered
    /// and split on `\n` as it arrives, so a caller can report live
    /// progress on a pull that runs for minutes, rather than waiting for
    /// the whole body and inspecting only the last line. No
    /// `serde_json::Deserializer::from_reader`/`StreamDeserializer`
    /// precedent existed in this codebase to reuse — this is new
    /// streaming-parse code (`ndjson_lines` below).
    ///
    /// Resolves explicitly against Ollama's own configured default
    /// registry (`registry.ollama.ai`) — `tag` is passed straight through
    /// as `{"model": tag}` with no registry-host override in the request,
    /// matching decisions.id=840's requirement that pulls never resolve
    /// against a user-suppliable registry host. Callers (
    /// `providers::ollama_install`) are additionally responsible for only
    /// ever passing a `tag` that matches a curated `providers.local_model_tag`
    /// row — this method itself does not re-validate the curated list.
    ///
    /// A per-line `{"error": "..."}` (Ollama can report this mid-stream on
    /// an otherwise-200 response, e.g. a failed digest) is decoded into
    /// `PullProgressLine::error`, not raised here — the caller decides how
    /// to react, since this method's job is decoding, not orchestration.
    pub async fn pull_model(
        &self,
        tag: &str,
    ) -> Result<impl Stream<Item = Result<PullProgressLine, OllamaModelError>>, OllamaModelError>
    {
        if !sidecar_trusted() {
            return Err(OllamaModelError::SidecarUntrusted);
        }

        let url = format!("{}/api/pull", base_url());
        let resp = self
            .pull_client
            .post(&url)
            .json(&serde_json::json!({ "model": tag }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(OllamaModelError::UnexpectedStatus { status, body });
        }

        Ok(ndjson_lines(resp.bytes_stream()))
    }

    /// Look up a model's content digest via `/api/tags`, matching on exact
    /// `name`. Returns `None` if the model isn't found or the request
    /// fails — never raises, matching `check_health()`'s own
    /// never-raises contract, since this is used only for the
    /// bug-report/support-tracing `providers.local_model_digest` column,
    /// not anything routing-critical.
    ///
    /// Deliberately re-queries `/api/tags` after a successful pull rather
    /// than reusing a digest seen mid-stream in `/api/pull`'s NDJSON lines
    /// — those digests identify individual manifest layers (e.g. the GGUF
    /// weights blob), not necessarily the same digest Ollama reports for
    /// the model as a whole via `/api/tags`.
    pub async fn get_model_digest(&self, tag: &str) -> Option<String> {
        if !sidecar_trusted() {
            return None;
        }

        let url = format!("{}/api/tags", base_url());
        let resp = self.health_client.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let data: Value = resp.json().await.ok()?;
        data.get("models")?
            .as_array()?
            .iter()
            .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(tag))
            .and_then(|m| m.get("digest"))
            .and_then(|d| d.as_str())
            .map(str::to_owned)
    }

    /// Delete a locally installed model via `/api/delete`.
    ///
    /// Uses HTTP `DELETE`, not `POST` — confirmed live against the
    /// installed Ollama version (0.34.4): `POST /api/delete` returns
    /// `405 method not allowed`, `DELETE /api/delete` reaches the handler
    /// correctly. Resolves decisions.id=840's noted open compatibility
    /// question for that version.
    ///
    /// Treats `404 Not Found` ("model not found") as success — an
    /// already-absent model is not an error for an idempotent delete
    /// (the caller may be retrying after a partial failure, or the file
    /// was already removed by some other path).
    pub async fn delete_model(&self, tag: &str) -> Result<(), OllamaModelError> {
        if !sidecar_trusted() {
            return Err(OllamaModelError::SidecarUntrusted);
        }

        let url = format!("{}/api/delete", base_url());
        let resp = self
            .pull_client
            .delete(&url)
            .json(&serde_json::json!({ "model": tag }))
            .send()
            .await?;

        match resp.status() {
            s if s.is_success() => Ok(()),
            reqwest::StatusCode::NOT_FOUND => Ok(()),
            status => {
                let body = resp.text().await.unwrap_or_default();
                Err(OllamaModelError::UnexpectedStatus { status, body })
            }
        }
    }
}

/// Errors from `pull_model`/`delete_model`. Deliberately NOT
/// `ConductorError`: that type's variants (`OllamaUnavailable`,
/// `OllamaTimeout`, etc.) are coupled to the Conductor step-execution
/// failure taxonomy (`conductor/failure.rs`'s F1/F_SYSTEM handling,
/// `step_id`/`focus_id`, retry-count branching) — a background model
/// install/delete has no step_id or FocusRun context and isn't a
/// candidate for that state machine's retry/tier-fallback semantics. This
/// is its own small error type instead, surfaced to the frontend as a
/// plain string by `commands::model_install`.
#[derive(Debug, thiserror::Error)]
pub enum OllamaModelError {
    #[error("request to Ollama failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Ollama returned HTTP {status}: {body}")]
    UnexpectedStatus {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("could not parse Ollama's response: {0}")]
    Decode(#[from] serde_json::Error),
    /// A mid-stream `{"error": "..."}` NDJSON line on an otherwise-200
    /// `/api/pull` response (e.g. a failed digest). Distinct from
    /// `UnexpectedStatus` — the HTTP status was fine, Ollama's own pull
    /// logic reported the failure inline.
    #[error("Ollama reported a pull failure: {0}")]
    Reported(String),
    /// items.id=586: refused before making any HTTP call, because QR's own
    /// sidecar trust gate (`ollama_sidecar::is_trusted()`) is false --
    /// distinct from `Http`/`UnexpectedStatus`, which both imply a call
    /// was actually attempted.
    #[error("QR's own sidecar is not confirmed running; refusing to contact it")]
    SidecarUntrusted,
}

/// One decoded line of Ollama's `/api/pull` NDJSON stream.
///
/// Field set matches Ollama's documented pull-progress shape. `error` is
/// `Some` only on a mid-stream failure line (e.g. a digest mismatch) —
/// `pull_model()` decodes it but leaves reacting to it to the caller.
#[derive(Debug, Clone, Deserialize)]
pub struct PullProgressLine {
    pub status: String,
    pub digest: Option<String>,
    pub total: Option<u64>,
    pub completed: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Adapts a raw `Stream<Item = reqwest::Result<Bytes>>` (as returned by
/// `Response::bytes_stream()`) into a stream of decoded NDJSON lines,
/// buffering partial lines across chunk boundaries. Blank lines are
/// skipped. On stream end, any remaining non-blank buffered content is
/// decoded as a final line (handles a response whose last line has no
/// trailing `\n`).
fn ndjson_lines<B: AsRef<[u8]>>(
    byte_stream: impl Stream<Item = reqwest::Result<B>> + Unpin,
) -> impl Stream<Item = Result<PullProgressLine, OllamaModelError>> {
    futures_util::stream::unfold(
        (byte_stream, Vec::<u8>::new(), false),
        |(mut byte_stream, mut buf, mut done)| async move {
            loop {
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let line = &line[..line.len() - 1]; // strip the '\n'
                    if line.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    let parsed = serde_json::from_slice::<PullProgressLine>(line)
                        .map_err(OllamaModelError::from);
                    return Some((parsed, (byte_stream, buf, done)));
                }

                if done {
                    if !buf.is_empty() && !buf.iter().all(u8::is_ascii_whitespace) {
                        let parsed = serde_json::from_slice::<PullProgressLine>(&buf)
                            .map_err(OllamaModelError::from);
                        buf.clear();
                        return Some((parsed, (byte_stream, buf, done)));
                    }
                    return None;
                }

                match byte_stream.next().await {
                    Some(Ok(bytes)) => buf.extend_from_slice(bytes.as_ref()),
                    Some(Err(e)) => {
                        return Some((Err(OllamaModelError::from(e)), (byte_stream, buf, true)))
                    }
                    None => done = true,
                }
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Free functions — no HTTP client required
// ---------------------------------------------------------------------------

/// Heuristic token count: ~4 chars per token.
///
/// Applies a 20% safety buffer for task types in `BUFFERED_TASK_TYPES`
/// that produce denser token output than the heuristic assumes.
/// Over-estimation is safe (triggers compaction earlier, never fails hard).
/// Under-estimation risks silent context window overflow.
///
/// Python oracle: `estimate_token_count()`
pub fn estimate_token_count(text: &str, task_type: &str) -> u32 {
    let base = (text.len() / 4) as u32;
    if BUFFERED_TASK_TYPES.contains(&task_type) {
        (base as f64 * 1.20) as u32
    } else {
        base
    }
}

/// Check whether a prompt fits within a model's context window.
///
/// `context_window` comes from the routing table model config.
/// Returns status and recommended action for `StepExecutor`.
/// Thresholds are env-tunable: `QR_CONTEXT_WARNING_THRESHOLD` (default 0.75)
/// and `QR_CONTEXT_HARD_LIMIT` (default 0.95).
///
/// `context_window == 0` triggers fail-safe: returns `Exceeded` with
/// `usage_fraction = 1.0` — matches Python oracle's missing-config guard.
///
/// Python oracle: `check_context_window()`
pub fn check_context_window(
    prompt: &str,
    task_type: &str,
    context_window: u32,
) -> ContextWindowStatus {
    if context_window == 0 {
        return ContextWindowStatus {
            status: ContextWindowStatusKind::Exceeded,
            token_estimate: estimate_token_count(prompt, task_type),
            context_window: 0,
            usage_fraction: 1.0,
            plain_language: Some(
                "Quiet Rabbit couldn't determine the model's capacity. [Get help]".to_owned(),
            ),
            recommended_action: Some(RecommendedAction::CompactThenEscalate),
        };
    }

    let token_estimate = estimate_token_count(prompt, task_type);
    let usage_fraction = token_estimate as f64 / context_window as f64;
    let hard_limit = context_hard_limit();
    let warn_threshold = context_warning_threshold();

    if usage_fraction >= hard_limit {
        return ContextWindowStatus {
            status: ContextWindowStatusKind::Exceeded,
            token_estimate,
            context_window,
            usage_fraction,
            plain_language: Some(
                "This is too long for local processing. \
                [Use an external service] [Shorten the document]"
                    .to_owned(),
            ),
            recommended_action: Some(RecommendedAction::CompactThenEscalate),
        };
    }

    if usage_fraction >= warn_threshold {
        let plain_language = if task_type == "long_context" {
            "This document is long. Local processing may miss details toward the end. \
            [Use an external service] [Continue locally]"
                .to_owned()
        } else {
            "This is getting long — results may be less complete toward the end.".to_owned()
        };
        return ContextWindowStatus {
            status: ContextWindowStatusKind::Warn,
            token_estimate,
            context_window,
            usage_fraction,
            plain_language: Some(plain_language),
            recommended_action: Some(RecommendedAction::CompactThenEscalate),
        };
    }

    ContextWindowStatus {
        status: ContextWindowStatusKind::Ok,
        token_estimate,
        context_window,
        usage_fraction,
        plain_language: None,
        recommended_action: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- sidecar trust gate (items.id=586) -----------------------------------

    /// Proves the gate actually stops the HTTP call from happening at all,
    /// not just that it returns an error -- a loopback listener standing in
    /// for "an unidentified process holds the port" must see zero
    /// connections while untrusted, even though `base_url()` points
    /// straight at it.
    #[tokio::test]
    async fn untrusted_sidecar_makes_zero_network_calls() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        crate::ollama_sidecar::force_trust_for_test(false);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let connection_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = connection_count.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stream.is_ok() {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        });

        let saved_host = std::env::var("OLLAMA_HOST").ok();
        let saved_port = std::env::var("OLLAMA_PORT").ok();
        std::env::set_var("OLLAMA_HOST", "127.0.0.1");
        std::env::set_var("OLLAMA_PORT", port.to_string());

        let client = OllamaClient::new();

        let health = client.check_health().await;
        assert_eq!(health.status, ProviderStatus::Unavailable);
        assert_eq!(health.error.as_deref(), Some("sidecar_untrusted"));

        let request = GenerateRequest {
            provider_id: None,
            model_id: "test-model".to_owned(),
            prompt: "hi".to_owned(),
            images: None,
            task_type: "generic".to_owned(),
            stream: Some(false),
            options: None,
        };
        assert!(client.generate(&request).await.is_err());
        assert!(client.get_model_digest("test-model").await.is_none());

        // Give a real (if untrusted) call every chance to have landed.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            connection_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no HTTP call should reach the port while the sidecar is untrusted"
        );

        match saved_host {
            Some(v) => std::env::set_var("OLLAMA_HOST", v),
            None => std::env::remove_var("OLLAMA_HOST"),
        }
        match saved_port {
            Some(v) => std::env::set_var("OLLAMA_PORT", v),
            None => std::env::remove_var("OLLAMA_PORT"),
        }
    }

    /// items.id=587: a real HTTP/1.1 404 response (confirmed live against a
    /// real Ollama instance to be the model-not-found shape) must map to
    /// OllamaModelMissing, not the generic OllamaGeneration branch. No HTTP-
    /// mocking crate exists in this project yet -- a raw std TcpListener
    /// writing the response by hand, same low-tech style as
    /// untrusted_sidecar_makes_zero_network_calls above, avoids adding one
    /// just for this.
    #[tokio::test]
    async fn generate_maps_a_404_not_found_response_to_model_missing() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        crate::ollama_sidecar::force_trust_for_test(true);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let body = r#"{"error":"model 'definitely-not-a-real-model-xyz' not found"}"#;
                let response = format!(
                    "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let saved_host = std::env::var("OLLAMA_HOST").ok();
        let saved_port = std::env::var("OLLAMA_PORT").ok();
        std::env::set_var("OLLAMA_HOST", "127.0.0.1");
        std::env::set_var("OLLAMA_PORT", port.to_string());

        let client = OllamaClient::new();
        let request = GenerateRequest {
            provider_id: None,
            model_id: "definitely-not-a-real-model-xyz".to_owned(),
            prompt: "hi".to_owned(),
            images: None,
            task_type: "generic".to_owned(),
            stream: Some(false),
            options: None,
        };

        match client.generate(&request).await {
            Err(ConductorError::OllamaModelMissing { plain_language }) => {
                assert!(
                    plain_language.contains("aren't installed"),
                    "unexpected message: {plain_language}"
                );
            }
            other => panic!("expected OllamaModelMissing, got {other:?}"),
        }

        match saved_host {
            Some(v) => std::env::set_var("OLLAMA_HOST", v),
            None => std::env::remove_var("OLLAMA_HOST"),
        }
        match saved_port {
            Some(v) => std::env::set_var("OLLAMA_PORT", v),
            None => std::env::remove_var("OLLAMA_PORT"),
        }
    }

    // -- estimate_token_count ------------------------------------------------

    #[test]
    fn estimate_buffer_for_prose_task() {
        // "prose" is in BUFFERED_TASK_TYPES — gets 20% buffer
        let expected = ((400usize / 4) as f64 * 1.20) as u32; // 120
        assert_eq!(estimate_token_count(&"a".repeat(400), "prose"), expected);
    }

    #[test]
    fn estimate_no_buffer_for_unknown_task() {
        let text = "a".repeat(400);
        assert_eq!(estimate_token_count(&text, "unknown"), 100);
    }

    #[test]
    fn estimate_buffer_for_code_task() {
        let text = "a".repeat(400);
        assert_eq!(estimate_token_count(&text, "code"), 120);
    }

    #[test]
    fn estimate_buffer_for_research_task() {
        let text = "a".repeat(400);
        assert_eq!(estimate_token_count(&text, "research"), 120);
    }

    #[test]
    fn estimate_buffer_for_creative_writing_task() {
        let text = "a".repeat(400);
        assert_eq!(estimate_token_count(&text, "creative_writing"), 120);
    }

    #[test]
    fn estimate_zero_length_text() {
        assert_eq!(estimate_token_count("", "code"), 0);
        assert_eq!(estimate_token_count("", "unknown"), 0);
    }

    // -- check_context_window ------------------------------------------------

    #[test]
    fn context_window_zero_returns_exceeded_with_full_fraction() {
        let result = check_context_window("some prompt", "research", 0);
        assert_eq!(result.status, ContextWindowStatusKind::Exceeded);
        assert_eq!(result.usage_fraction, 1.0);
        assert_eq!(result.context_window, 0);
        assert!(result
            .plain_language
            .as_deref()
            .unwrap_or("")
            .contains("couldn't determine"));
        assert!(result.recommended_action.is_some());
    }

    #[test]
    fn context_window_ok_below_threshold() {
        // 100 tokens into 2048 context ≈ 4.9% — well under 75% warning threshold
        let result = check_context_window(&"a".repeat(400), "generic", 2048);
        assert_eq!(result.status, ContextWindowStatusKind::Ok);
        assert!(result.plain_language.is_none());
        assert!(result.recommended_action.is_none());
    }

    #[test]
    fn context_window_warn_between_thresholds() {
        // 1600 tokens / 2048 ≈ 78% — between 75% warning and 95% hard limit
        let result = check_context_window(&"a".repeat(6400), "generic", 2048);
        assert_eq!(result.status, ContextWindowStatusKind::Warn);
        assert!(result.recommended_action.is_some());
    }

    #[test]
    fn context_window_exceeded_above_hard_limit() {
        // 1950 tokens / 2048 ≈ 95.2% — above 95% hard limit
        let result = check_context_window(&"a".repeat(7800), "generic", 2048);
        assert_eq!(result.status, ContextWindowStatusKind::Exceeded);
    }

    #[test]
    fn context_window_warn_long_context_task_has_distinct_message() {
        let result = check_context_window(&"a".repeat(6400), "long_context", 2048);
        assert_eq!(result.status, ContextWindowStatusKind::Warn);
        let msg = result.plain_language.unwrap();
        assert!(msg.contains("Local processing may miss"));
    }

    #[test]
    fn context_window_warn_non_long_context_has_general_message() {
        let result = check_context_window(&"a".repeat(6400), "generic", 2048);
        assert_eq!(result.status, ContextWindowStatusKind::Warn);
        let msg = result.plain_language.unwrap();
        assert!(msg.contains("getting long"));
    }
}
