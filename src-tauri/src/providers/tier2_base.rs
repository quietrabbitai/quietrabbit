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
    async fn generate(&self, request: &GenerateRequest)
        -> Result<GenerateResponse, ConductorError>;

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

    /// Return the bare wire model name from a `GenerateRequest`, after
    /// asserting the request was actually routed to this provider.
    ///
    /// `request.provider_id`/`request.model_id` are already-resolved fields
    /// (see `GenerateRequest` doc, decisions.id=813) — this validates
    /// `request.provider_id` against `self.provider_id()` and returns
    /// `request.model_id` directly. No string parsing.
    ///
    /// Returns `Err(ConductorError::UnknownProvider)` if:
    /// - `request.provider_id` is `None` (request never resolved a Tier 2 provider)
    /// - `request.provider_id` does not match `self.provider_id()`
    ///
    /// Python oracle: `Tier2Provider.model_id_from_request()`
    fn model_id_from_request<'a>(
        &self,
        request: &'a GenerateRequest,
    ) -> Result<&'a str, ConductorError> {
        match request.provider_id.as_deref() {
            Some(id) if id == self.provider_id() => Ok(request.model_id.as_str()),
            Some(other) => Err(ConductorError::UnknownProvider {
                plain_language: format!(
                    "Request provider '{}' does not match provider '{}'. \
                     Check routing configuration. [Get help]",
                    other,
                    self.provider_id(),
                ),
            }),
            None => Err(ConductorError::UnknownProvider {
                plain_language: format!(
                    "Request has no provider set; expected '{}'. [Get help]",
                    self.provider_id(),
                ),
            }),
        }
    }
}
