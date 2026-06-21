//! Provider-local error type for Tier 2+ HTTP client errors.
//!
//! `ProviderError` carries provider-specific diagnostic fields (provider name,
//! HTTP status codes) until the subsystem boundary, then converts to
//! `ConductorError` via `From`. Ollama (Tier 1) maps directly to
//! `ConductorError` at call sites — this enum covers Tier 2+ only.

use thiserror::Error;

use crate::conductor::failure::ConductorError;

/// Tier 2+ provider errors with full diagnostic context.
///
/// Each variant carries `plain_language` — the user-facing message composed
/// at the raising site, following the Python oracle pattern where call sites
/// own the localized message string.
///
/// Convert to `ConductorError` at the conductor boundary via `From`.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// API key absent from `integration_keys.db`.
    #[error("provider '{}': API key not found", provider)]
    MissingApiKey {
        provider: String,
        plain_language: String,
    },

    /// API key rejected by provider (HTTP 401).
    #[error("provider '{}': API key rejected (401)", provider)]
    InvalidApiKey {
        provider: String,
        plain_language: String,
    },

    /// Provider rate limit hit (HTTP 429). Retryable via executor retry loop.
    #[error("provider '{}': rate limit hit (429)", provider)]
    RateLimit {
        provider: String,
        plain_language: String,
    },

    /// Provider did not respond within timeout. Retryable.
    #[error("provider '{}': request timed out", provider)]
    Timeout {
        provider: String,
        plain_language: String,
    },

    /// Provider unreachable (connection error). Retryable.
    #[error("provider '{}': connection failed", provider)]
    Unavailable {
        provider: String,
        plain_language: String,
    },

    /// Unexpected HTTP status or provider-specific protocol error. Terminal.
    /// `status_code` is `None` for transport-level failures with no HTTP response.
    #[error("provider '{}': failure (HTTP {:?})", provider, status_code)]
    ProviderFailure {
        provider: String,
        status_code: Option<u16>,
        plain_language: String,
    },

    /// Model ID missing provider prefix or prefix mismatch.
    /// Expected format: `"provider_id:model-name"` (e.g. `"groq:llama-3.1-8b-instant"`).
    #[error("unknown provider '{}'", provider)]
    UnknownProvider {
        provider: String,
        plain_language: String,
    },

    /// No Tier 2 provider configured — install interview not completed.
    #[error("no Tier 2 provider configured")]
    MissingTier2Config {
        plain_language: String,
    },
}

impl From<ProviderError> for ConductorError {
    fn from(e: ProviderError) -> Self {
        match e {
            ProviderError::MissingApiKey { plain_language, .. } =>
                ConductorError::MissingApiKey { plain_language },
            ProviderError::InvalidApiKey { plain_language, .. } =>
                ConductorError::InvalidApiKey { plain_language },
            ProviderError::RateLimit { plain_language, .. } =>
                ConductorError::ProviderRateLimit { plain_language },
            ProviderError::Timeout { plain_language, .. } =>
                ConductorError::ProviderTimeout { plain_language },
            ProviderError::Unavailable { plain_language, .. } =>
                ConductorError::ProviderUnavailable { plain_language },
            ProviderError::ProviderFailure { plain_language, .. } =>
                ConductorError::Provider { plain_language },
            ProviderError::UnknownProvider { plain_language, .. } =>
                ConductorError::UnknownProvider { plain_language },
            ProviderError::MissingTier2Config { plain_language } =>
                ConductorError::MissingTier2Config { plain_language },
        }
    }
}
