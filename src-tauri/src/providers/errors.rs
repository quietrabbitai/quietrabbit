//! Provider-local error type for hosted (qr_hosted) HTTP client errors.
//! TODO(tier-terminology): needs a combined term for cloud_anonymous+cloud_frontier before this converts
//!
//! `ProviderError` carries provider-specific diagnostic fields (provider id,
//! HTTP status codes) until the subsystem boundary, then converts to
//! `ConductorError` via `From`. Ollama (qr_local) maps directly to
//! `ConductorError` at call sites — this enum covers hosted providers only.
//! TODO(tier-terminology): needs a combined term for cloud_anonymous+cloud_frontier before this converts
//!
//! Every `plain_language` is composed at the raising site from the provider
//! row's own display name (decisions.id=872: no provider is named in code).
//! None of these variants may carry a credential, a request header or an
//! upstream response body (items.id=709): the `provider` field is the row id,
//! never secret material.

use thiserror::Error;

use crate::conductor::failure::ConductorError;

/// Hosted provider errors with full diagnostic context.
///
/// Each variant carries `plain_language` — the user-facing message composed
/// at the raising site, following the Python oracle pattern where call sites
/// own the localized message string.
///
/// Convert to `ConductorError` at the conductor boundary via `From`.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// API key absent from `integration_keys.db`, or a user variable the
    /// provider row declares (for example an account id) is not on the key
    /// record yet.
    #[error("provider '{}': API key not found", provider)]
    MissingApiKey {
        provider: String,
        plain_language: String,
    },

    /// API key rejected by provider (HTTP 401 or 403).
    #[error("provider '{}': API key rejected", provider)]
    InvalidApiKey {
        provider: String,
        plain_language: String,
    },

    /// Provider rate limit hit (HTTP 429). Retryable via executor retry loop.
    /// `retry_after_secs` is the `Retry-After` header in delta-seconds form;
    /// the retry loop does not honor it yet, it only reaches the message.
    #[error("provider '{}': rate limit hit (429)", provider)]
    RateLimit {
        provider: String,
        retry_after_secs: Option<u64>,
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

    /// The provider answered with a 5xx status. Retryable like `Unavailable`.
    #[error("provider '{}': server error (HTTP {})", provider, status_code)]
    ServerError {
        provider: String,
        status_code: u16,
        plain_language: String,
    },

    /// HTTP 404 on a generation request: the model id is unknown to the
    /// provider, usually because it was retired. Terminal.
    #[error("provider '{}': model not found (404)", provider)]
    ModelNotFound {
        provider: String,
        plain_language: String,
    },

    /// HTTP 402: the account needs a payment method or has run out of
    /// credit. Terminal.
    #[error("provider '{}': payment or quota required (402)", provider)]
    PaymentRequired {
        provider: String,
        plain_language: String,
    },

    /// The provider answered 2xx but the body was not the expected shape. Terminal.
    #[error("provider '{}': malformed response", provider)]
    MalformedResponse {
        provider: String,
        plain_language: String,
    },

    /// The model produced no answer text and stopped because it hit the
    /// output token limit (hidden reasoning tokens can consume the whole
    /// budget). Terminal: raise the model row's token limit.
    #[error("provider '{}': output budget exhausted", provider)]
    OutputBudgetExhausted {
        provider: String,
        plain_language: String,
    },

    /// A provider or model row (or the request) cannot be turned into a
    /// valid call: unsupported wire format, unsafe URL template or variable,
    /// forbidden header or option, missing token limit. Raised before any
    /// network traffic. Terminal.
    #[error("provider '{}': misconfigured", provider)]
    Misconfigured {
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

    /// Request has no provider set, or its resolved provider has no row.
    #[error("unknown provider '{}'", provider)]
    UnknownProvider {
        provider: String,
        plain_language: String,
    },

    /// No Tier 1.5 (qr_hosted) provider configured — install interview not completed.
    #[error("no Tier 1.5 (qr_hosted) provider configured")]
    MissingQrHostedConfig { plain_language: String },
}

impl From<ProviderError> for ConductorError {
    fn from(e: ProviderError) -> Self {
        match e {
            ProviderError::MissingApiKey { plain_language, .. } => {
                ConductorError::MissingApiKey { plain_language }
            }
            ProviderError::InvalidApiKey { plain_language, .. } => {
                ConductorError::InvalidApiKey { plain_language }
            }
            ProviderError::RateLimit { plain_language, .. } => {
                ConductorError::ProviderRateLimit { plain_language }
            }
            ProviderError::Timeout { plain_language, .. } => {
                ConductorError::ProviderTimeout { plain_language }
            }
            ProviderError::Unavailable { plain_language, .. }
            | ProviderError::ServerError { plain_language, .. } => {
                ConductorError::ProviderUnavailable { plain_language }
            }
            ProviderError::ModelNotFound { plain_language, .. }
            | ProviderError::PaymentRequired { plain_language, .. }
            | ProviderError::MalformedResponse { plain_language, .. }
            | ProviderError::OutputBudgetExhausted { plain_language, .. }
            | ProviderError::Misconfigured { plain_language, .. }
            | ProviderError::ProviderFailure { plain_language, .. } => {
                ConductorError::Provider { plain_language }
            }
            ProviderError::UnknownProvider { plain_language, .. } => {
                ConductorError::UnknownProvider { plain_language }
            }
            ProviderError::MissingQrHostedConfig { plain_language } => {
                ConductorError::MissingQrHostedConfig { plain_language }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conductor(e: ProviderError) -> ConductorError {
        ConductorError::from(e)
    }

    /// Retryable classes stay retryable and terminal classes stay terminal:
    /// the failure handler keys its retry decision off these variants.
    #[test]
    fn conversion_keeps_retry_classes() {
        let pl = || "text".to_owned();
        assert!(matches!(
            conductor(ProviderError::ServerError {
                provider: "p".into(),
                status_code: 503,
                plain_language: pl()
            }),
            ConductorError::ProviderUnavailable { .. }
        ));
        assert!(matches!(
            conductor(ProviderError::RateLimit {
                provider: "p".into(),
                retry_after_secs: Some(3),
                plain_language: pl()
            }),
            ConductorError::ProviderRateLimit { .. }
        ));
        for e in [
            ProviderError::ModelNotFound {
                provider: "p".into(),
                plain_language: pl(),
            },
            ProviderError::PaymentRequired {
                provider: "p".into(),
                plain_language: pl(),
            },
            ProviderError::MalformedResponse {
                provider: "p".into(),
                plain_language: pl(),
            },
            ProviderError::OutputBudgetExhausted {
                provider: "p".into(),
                plain_language: pl(),
            },
            ProviderError::Misconfigured {
                provider: "p".into(),
                plain_language: pl(),
            },
        ] {
            assert!(matches!(conductor(e), ConductorError::Provider { .. }));
        }
    }
}
