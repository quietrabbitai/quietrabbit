// src-tauri/src/commands/system.rs
//
// Group 12 — System.
// Commands: get_health, get_capability_profile.
//
// get_health: checks Ollama availability and returns provider health status.
//   Tier 2 configured status is a stub (integration_keys store not yet ported).
// get_capability_profile: returns installed models and benchmark status.
//   recommended_routing omitted -- evaluation/scores DB not yet ported.
//   Release 1 benchmark_status values: "pending" (models present, no scores
//   yet) or "unavailable" (no models detected). "complete" requires scores DB.

use serde::Serialize;
use specta::Type;

use crate::providers::ollama_client::OllamaClient;
use crate::providers::types::{ProviderHealth, ProviderStatus};

// ---------------------------------------------------------------------------
// Response structs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Type)]
pub struct HealthResponse {
    pub ollama: ProviderHealth,
    /// Always false until integration_keys store is ported (Group 9 stub).
    pub tier2_configured: bool,
}

#[derive(Debug, Serialize, Type)]
pub struct CapabilityProfileResponse {
    pub installed_models: Vec<String>,
    /// Release 1: "pending" | "unavailable" only.
    /// "complete" requires evaluation/scores DB port (post-Release 1).
    /// STUB: recommended_routing omitted until scores DB is ported.
    pub benchmark_status: String,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_health(
    client: tauri::State<'_, OllamaClient>,
) -> Result<HealthResponse, String> {
    let ollama = client.check_health().await;

    Ok(HealthResponse {
        ollama,
        tier2_configured: false, // STUB: integration_keys store not yet ported
    })
}

#[tauri::command]
pub async fn get_capability_profile(
    client: tauri::State<'_, OllamaClient>,
) -> Result<CapabilityProfileResponse, String> {
    let health = client.check_health().await;

    let installed_models = if health.status == ProviderStatus::Available {
        health.available_models
    } else {
        vec![]
    };

    let benchmark_status = if installed_models.is_empty() {
        "unavailable".to_string()
    } else {
        // Cached scores only in Release 1 -- no live benchmark trigger via IPC.
        // Returns "pending" until evaluation/scores DB is ported.
        "pending".to_string()
    };

    Ok(CapabilityProfileResponse {
        installed_models,
        benchmark_status,
    })
}
