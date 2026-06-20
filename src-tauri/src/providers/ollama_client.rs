//! Ollama Tier 1 HTTP client.
//!
//! Ollama is Tier 1 — it does NOT implement [`Tier2Provider`].
//! Errors map directly to [`ConductorError`] at every raise site.
//! No [`ProviderError`] intermediary — Tier 1 maps directly.
//!
//! `stream` is always `false` in Release 1 — resolved by `StepExecutor`.
//!
//! Two [`reqwest::Client`] instances are held:
//! - `client` (120s): inference calls (`/api/generate`, `/api/chat`, `/api/create`)
//! - `health_client` (5s): health, model enumeration, modelfile show

use std::time::Duration;

use reqwest::Client;
use serde_json::Value;

use crate::conductor::failure::ConductorError;
use crate::providers::types::{
    ChatMessage, ContextWindowStatus, ContextWindowStatusKind, GenerateOptions,
    GenerateRequest, GenerateResponse, ModelfileVersion, ProviderHealth,
    ProviderStatus, RecommendedAction,
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
fn base_url() -> String {
    let host = std::env::var("OLLAMA_HOST")
        .unwrap_or_else(|_| "host.docker.internal".to_owned());
    let port = std::env::var("OLLAMA_PORT")
        .unwrap_or_else(|_| "11434".to_owned());
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

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Ollama Tier 1 HTTP client.
///
/// Holds three `reqwest::Client` instances with different timeouts:
/// - `client` (120s): inference calls (`/api/generate`, `/api/chat`).
/// - `health_client` (5s): health check, tags, show.
/// - `modelfile_client` (300s): modelfile application (`/api/create`).
///
/// Construct once per Conductor actor. Not `Clone` — single owner per actor.
pub struct OllamaClient {
    client: Client,
    health_client: Client,
    modelfile_client: Client,
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
        let url = format!("{}/api/tags", base_url());
        match self.health_client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let available = resp
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|v| {
                        v.get("models")
                            .and_then(|m| m.as_array())
                            .cloned()
                    })
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

    /// Primary Tier 1 inference call.
    ///
    /// Latency is always tracked — never hardcoded to 0.
    /// `stream` is always `false` in Release 1 (resolved by `StepExecutor`).
    ///
    /// When `request.options` is `None`, all four fields fall back to
    /// the sane Tier 1 defaults. This matches the Python oracle's explicit
    /// fallback construction in `generate()`.
    ///
    /// Errors map directly to [`ConductorError`] — no `ProviderError` boundary.
    ///
    /// Python oracle: `generate()`
    pub async fn generate(
        &self,
        request: &GenerateRequest,
    ) -> Result<GenerateResponse, ConductorError> {
        let options = request.options.clone().unwrap_or(GenerateOptions {
            temperature: 0.5,
            top_p: 0.90,
            num_ctx: 2048,
            num_predict: 2048,
        });

        let payload = serde_json::json!({
            "model": request.model,
            "prompt": request.prompt,
            "stream": false,
            "options": {
                "temperature": options.temperature,
                "top_p": options.top_p,
                "num_ctx": options.num_ctx,
                "num_predict": options.num_predict,
            }
        });

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
        if !status.is_success() {
            return Err(ConductorError::OllamaGeneration {
                plain_language: "The local AI returned an unexpected response. \
                    [Try again] [Get help]"
                    .to_owned(),
            });
        }

        let data: Value = resp.json().await.map_err(|_| ConductorError::OllamaGeneration {
            plain_language: "The local AI returned an unexpected response. \
                [Try again] [Get help]"
                .to_owned(),
        })?;

        Ok(GenerateResponse {
            content: data["response"].as_str().unwrap_or("").to_owned(),
            model: data["model"]
                .as_str()
                .unwrap_or(&request.model)
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
        if !status.is_success() {
            return Err(ConductorError::OllamaGeneration {
                plain_language: "The local AI returned an unexpected response. [Try again]"
                    .to_owned(),
            });
        }

        let data: Value = resp.json().await.map_err(|_| ConductorError::OllamaGeneration {
            plain_language: "The local AI returned an unexpected response. [Try again]".to_owned(),
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
            .filter(|l| !l.trim().is_empty())
            .last()
            .and_then(|line| serde_json::from_str::<Value>(line).ok())
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(|s| s == "success"))
            .unwrap_or(false);

        success
    }
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

    // -- estimate_token_count ------------------------------------------------

    #[test]
    fn estimate_buffer_for_prose_task() {
        // "prose" is in BUFFERED_TASK_TYPES — gets 20% buffer
        let base = 400usize / 4; // 100
        let expected = (100f64 * 1.20) as u32; // 120
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
        assert!(result.plain_language.as_deref().unwrap_or("").contains("couldn't determine"));
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
