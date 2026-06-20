//! Abstract base trait for all Tier 2 external providers.
//! Concrete implementations: `groq.rs`, and future `mistral.rs`.
//!
//! CONTRACT:
//! - All Tier 2 providers receive abstracted field values only.
//!   Raw personal field values never appear in prompts routed here.
//!   The disclosure buffer enforces this upstream in `StepExecutor` Step 8.
//! - `generate()` is the primary interface. Called by `StepExecutor` Step 10
//!   when `execution_tier >= 2`.
//! - Disclosure log write failure is fatal before `generate()` is called —
//!   `ConductorError::DisclosureLogWrite` halts the run. This trait never
//!   writes the log.
//! - Key retrieval is the concrete implementation's responsibility.
//!   The base trait prescribes no source — this allows env-var providers
//!   (Layer 6) and `InMemoryKeyRegistry` providers (Layer 8) to share
//!   the same interface.
//! - All provider errors must be mapped to `ConductorError` variants (F10).
//!   Callers must never see raw `reqwest` errors.
//! - Stateless single-request completion model only.
//!   No tools, function calling, retrieval, or multi-step pipelines.
//!   Hybrid provider patterns are Release 2+.
//!
//! HONEST FREE-TIER FRAMING (CLAUDE.md):
//! Groq: US-based, free tier, faster iteration.
//! Mistral: EU/GDPR, paid, stronger privacy guarantees.
//! No prescribed default — user chooses at install time.
//! This trait makes no recommendation.

use async_trait::async_trait;

use crate::conductor::failure::ConductorError;
use crate::providers::types::{GenerateRequest, GenerateResponse, ProviderHealth};

/// Abstract base for Tier 2 external providers.
///
/// Stateless single-request completion model — no tools, no memory,
/// no multi-turn state. All state management is in `TaskTrack` (executor).
///
/// Implementors must be `Send + Sync` — provider instances are shared
/// across async tasks within the Conductor actor.
#[async_trait]
pub trait Tier2Provider: Send + Sync {
    /// Short stable identifier used in `disclosure_log.provider`,
    /// model prefix validation, and error messages.
    ///
    /// Examples: `"groq"`, `"mistral"`
    ///
    /// Must match the prefix used in `StepExecutor` model IDs
    /// (e.g. `"groq:llama-3.1-8b-instant"`).
    fn provider_id(&self) -> &str;

    /// Human-readable provider name.
    ///
    /// Examples: `"Groq"`, `"Mistral"`
    fn display_name(&self) -> &str;

    /// Send a generation request to the external provider.
    /// Returns `GenerateResponse` on success.
    ///
    /// Privacy contract: the prompt in `request` contains abstracted field
    /// values only — enforced upstream by `StepExecutor` Step 8 (disclosure
    /// buffer). This method does not perform privacy validation.
    ///
    /// Key retrieval: the concrete implementation is responsible for
    /// obtaining the API key. The trait prescribes no source.
    ///
    /// Required error mapping — callers must never see raw `reqwest` errors:
    /// - `ConductorError::MissingApiKey`       — key absent from store
    /// - `ConductorError::InvalidApiKey`       — 401 from provider
    /// - `ConductorError::ProviderRateLimit`   — 429 (retryable)
    /// - `ConductorError::ProviderTimeout`     — timeout (retryable)
    /// - `ConductorError::ProviderUnavailable` — connection error (retryable)
    /// - `ConductorError::Provider`            — unexpected HTTP status (terminal)
    async fn generate(
        &self,
        request: &GenerateRequest,
    ) -> Result<GenerateResponse, ConductorError>;

    /// Check provider availability.
    ///
    /// Must complete within 3 seconds.
    ///
    /// # Implementor contract
    /// This method **must never return `Err` or panic**. All transport
    /// errors, timeouts, and unexpected failures must be caught internally
    /// and represented as `ProviderHealth` status values:
    /// - Timeout         → `status: Unavailable`
    /// - Partial failure → `status: Degraded`
    /// - Any error       → `ProviderHealth { status: Unavailable, error: Some(msg) }`
    ///
    /// Implementations are responsible for enforcing the 3-second timeout
    /// bound by whatever means suits the transport (client-level timeout,
    /// `tokio::time::timeout`, etc.).
    async fn health_check(&self) -> ProviderHealth;

    /// Extract the bare model name from a `GenerateRequest.model` string.
    ///
    /// Expected format: `"provider_id:model_name"`
    /// (e.g. `"groq:llama-3.1-8b-instant"`).
    ///
    /// Validates that the prefix matches `self.provider_id()` and that
    /// the model name segment is non-empty.
    ///
    /// Returns `Err(ConductorError::UnknownProvider)` if:
    /// - the model string contains no `:`
    /// - the prefix does not match `self.provider_id()`
    /// - the model name segment after `:` is empty
    ///
    /// Python oracle: `Tier2Provider.model_id_from_request()`
    fn model_id_from_request<'a>(
        &self,
        request: &'a GenerateRequest,
    ) -> Result<&'a str, ConductorError> {
        let model = request.model.as_str();
        match model.split_once(':') {
            None => Err(ConductorError::UnknownProvider {
                plain_language: format!(
                    "Model ID '{}' is missing a provider prefix. \
                     Expected format: '{}:model-name'. [Get help]",
                    model,
                    self.provider_id(),
                ),
            }),
            Some((prefix, _)) if prefix != self.provider_id() => {
                Err(ConductorError::UnknownProvider {
                    plain_language: format!(
                        "Model prefix '{}' does not match provider '{}'. \
                         Check path routing configuration. [Get help]",
                        prefix,
                        self.provider_id(),
                    ),
                })
            }
            Some((_, "")) => Err(ConductorError::UnknownProvider {
                plain_language: format!(
                    "Model ID '{}' has an empty model name after the provider prefix. \
                     Expected format: '{}:model-name'. [Get help]",
                    model,
                    self.provider_id(),
                ),
            }),
            Some((_, model_name)) => Ok(model_name),
        }
    }
}
