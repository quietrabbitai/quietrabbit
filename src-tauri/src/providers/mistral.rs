// src-tauri/src/providers/mistral.rs
// Mistral Tier 2 provider — implements Tier2Provider for Mistral's La Plateforme API.
//
// Model: mistral-small-latest (drafting — fast, cheap paid tier)
// Provider: Mistral (EU-based, GDPR)
// API: https://api.mistral.ai/v1 (OpenAI-compatible chat/completions)
//
// KEY RETRIEVAL (Layer 6 dev bridge):
// API key read from MISTRAL_API_KEY environment variable.
// Layer 8: replace get_api_key() body with integration_keys.db retrieval
//   via InMemoryKeyRegistry. Signature and error contract are stable across layers.
//
// HONEST FRAMING (CLAUDE.md):
// Mistral is EU-based. Paid tier only. GDPR-native, privacy-first framing.
// Users who want a free US-based option should use Groq instead.
// This provider makes no recommendation — user chooses at install time.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

use crate::conductor::failure::ConductorError;
use crate::providers::errors::ProviderError;
use crate::providers::tier2_base::Tier2Provider;
use crate::providers::types::{
    CompletionStatus, GenerateRequest, GenerateResponse, ProviderHealth, ProviderStatus,
};
use crate::providers::utils::now;

const MISTRAL_API_BASE: &str = "https://api.mistral.ai/v1";
const MISTRAL_TIMEOUT_SECONDS: u64 = 30;
const MISTRAL_HEALTH_TIMEOUT_SECONDS: u64 = 3;

/// Mistral Tier 2 provider using OpenAI-compatible chat/completions endpoint.
/// Stateless — no session, no memory, no tools.
/// HTTP transport: reqwest (async).
pub struct MistralProvider {
    /// Reusable HTTP client with MISTRAL_TIMEOUT_SECONDS timeout.
    client: Client,
    /// Separate client for health checks with MISTRAL_HEALTH_TIMEOUT_SECONDS.
    health_client: Client,
}

impl MistralProvider {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(MISTRAL_TIMEOUT_SECONDS))
            .build()
            .expect("failed to build Mistral HTTP client");
        let health_client = Client::builder()
            .timeout(Duration::from_secs(MISTRAL_HEALTH_TIMEOUT_SECONDS))
            .build()
            .expect("failed to build Mistral health-check HTTP client");
        Self {
            client,
            health_client,
        }
    }

    /// Retrieve the Mistral API key.
    /// Layer 6: reads MISTRAL_API_KEY environment variable.
    /// Layer 8: replace with integration_keys.db retrieval via InMemoryKeyRegistry.
    fn get_api_key(&self) -> Result<String, ConductorError> {
        let key = std::env::var("MISTRAL_API_KEY").unwrap_or_default();
        let key = key.trim().to_owned();
        if key.is_empty() {
            return Err(ConductorError::from(ProviderError::MissingApiKey {
                provider: "mistral".to_owned(),
                plain_language: "Mistral API key is not configured. \
                    Add MISTRAL_API_KEY to your environment to use \
                    the Writing Assistant. [Get help]"
                    .to_owned(),
            }));
        }
        Ok(key)
    }
}

impl Default for MistralProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tier2Provider for MistralProvider {
    fn provider_id(&self) -> &str {
        "mistral"
    }

    fn display_name(&self) -> &str {
        "Mistral"
    }

    /// Send a chat completion request to Mistral.
    ///
    /// Prompt delivered as a single user message — StepExecutor Step 8 has
    /// already assembled all context (voice profile, abstracted fields,
    /// prior step outputs) into the prompt string. No system message needed.
    ///
    /// Privacy contract: prompt contains abstracted values only (Gate1 +
    /// disclosure buffer guarantee). This method does not validate privacy.
    async fn generate(
        &self,
        request: &GenerateRequest,
    ) -> Result<GenerateResponse, ConductorError> {
        let api_key = self.get_api_key()?;
        let model_name = self.model_id_from_request(request)?;

        let payload = json!({
            "model": model_name,
            "messages": [
                {"role": "user", "content": request.prompt}
            ],
            "temperature": request.options.as_ref().map(|o| o.temperature).unwrap_or(0.5),
            "max_tokens": request.options.as_ref().map(|o| o.num_predict).unwrap_or(1024),
            "stream": false,
        });

        let start = std::time::Instant::now();

        let http_response = self
            .client
            .post(format!("{MISTRAL_API_BASE}/chat/completions"))
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ConductorError::from(ProviderError::Timeout {
                        provider: "mistral".to_owned(),
                        plain_language: "Mistral didn't respond in time. \
                            [Try again] [Use local AI instead]"
                            .to_owned(),
                    })
                } else if e.is_connect() {
                    ConductorError::from(ProviderError::Unavailable {
                        provider: "mistral".to_owned(),
                        plain_language: "Mistral is unreachable. \
                            Check your internet connection. \
                            [Try again] [Use local AI instead]"
                            .to_owned(),
                    })
                } else {
                    ConductorError::from(ProviderError::Unavailable {
                        provider: "mistral".to_owned(),
                        plain_language: "Mistral connection failed. \
                            [Try again] [Use local AI instead]"
                            .to_owned(),
                    })
                }
            })?;

        let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
        let status = http_response.status();

        if status == 401 {
            return Err(ConductorError::from(ProviderError::InvalidApiKey {
                provider: "mistral".to_owned(),
                plain_language: "Mistral API key was rejected. \
                    Check your key is correct and has not expired. [Get help]"
                    .to_owned(),
            }));
        }
        if status == 429 {
            return Err(ConductorError::from(ProviderError::RateLimit {
                provider: "mistral".to_owned(),
                plain_language: "Mistral rate limit reached. \
                    [Try again in a moment] [Use local AI instead]"
                    .to_owned(),
            }));
        }
        if !status.is_success() {
            return Err(ConductorError::from(ProviderError::ProviderFailure {
                provider: "mistral".to_owned(),
                status_code: Some(status.as_u16()),
                plain_language: format!(
                    "Mistral returned an unexpected error ({}). \
                    [Try again] [Get help]",
                    status.as_u16()
                ),
            }));
        }

        let body: Value = http_response.json().await.map_err(|_| {
            ConductorError::from(ProviderError::ProviderFailure {
                provider: "mistral".to_owned(),
                status_code: Some(status.as_u16()),
                plain_language: "Mistral returned an unexpected response format. \
                    [Try again] [Get help]"
                    .to_owned(),
            })
        })?;

        let content = body["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| {
                ConductorError::from(ProviderError::ProviderFailure {
                    provider: "mistral".to_owned(),
                    status_code: Some(status.as_u16()),
                    plain_language: "Mistral returned an unexpected response format. \
                        [Try again] [Get help]"
                        .to_owned(),
                })
            })?
            .to_owned();

        let prompt_tokens = body["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32;
        let completion_tokens = body["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32;

        Ok(GenerateResponse {
            content,
            model: request.model.clone(),
            prompt_token_count: prompt_tokens,
            output_token_count: completion_tokens,
            latency_ms,
            completion_status: CompletionStatus::Complete,
        })
    }

    /// Check Mistral availability via the models endpoint.
    ///
    /// Never propagates errors — all failures are returned as `ProviderHealth`.
    /// Timeout → unavailable. 401 → degraded (key issue, not network issue).
    /// 3-second timeout enforced via health_client transport configuration.
    async fn health_check(&self) -> ProviderHealth {
        let api_key = match self.get_api_key() {
            Ok(k) => k,
            Err(_) => {
                return ProviderHealth {
                    provider: "mistral".to_owned(),
                    status: ProviderStatus::Unavailable,
                    checked_at: now(),
                    error: Some("MISTRAL_API_KEY not configured".to_owned()),
                    available_models: vec![],
                };
            }
        };

        let response = self
            .health_client
            .get(format!("{MISTRAL_API_BASE}/models"))
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await;

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                let error_msg = if e.is_timeout() {
                    "health check timed out".to_owned()
                } else {
                    e.to_string()
                };
                return ProviderHealth {
                    provider: "mistral".to_owned(),
                    status: ProviderStatus::Unavailable,
                    checked_at: now(),
                    error: Some(error_msg),
                    available_models: vec![],
                };
            }
        };

        let status = response.status();

        if status == 401 {
            return ProviderHealth {
                provider: "mistral".to_owned(),
                status: ProviderStatus::Degraded,
                checked_at: now(),
                error: Some("API key rejected (401)".to_owned()),
                available_models: vec![],
            };
        }

        if !status.is_success() {
            return ProviderHealth {
                provider: "mistral".to_owned(),
                status: ProviderStatus::Degraded,
                checked_at: now(),
                error: Some(format!("unexpected status {}", status.as_u16())),
                available_models: vec![],
            };
        }

        let models: Vec<String> = response
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["data"].as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| m["id"].as_str().map(|s| s.to_owned()))
            .collect();

        ProviderHealth {
            provider: "mistral".to_owned(),
            status: ProviderStatus::Available,
            checked_at: now(),
            error: None,
            available_models: models,
        }
    }
}
