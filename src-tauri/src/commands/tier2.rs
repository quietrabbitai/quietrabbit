// src-tauri/src/commands/tier2.rs
//
// Group 9 — Tier 2 configuration [STUB].
// Commands: get_tier2_config, set_tier2_provider.
//
// Both commands require integration_keys_store (not yet ported).
// integration_keys.db path exists in providers::utils::db_path_integration_keys()
// but no store module wraps it. Flagged to Chat-PM.
// api_key must NEVER be returned to the frontend (write-only per IPC surface spec).

#[tauri::command]
#[specta::specta]
pub async fn get_tier2_config() -> Result<crate::commands::NotImplementedPlaceholder, String> {
    Err("not_implemented".to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn set_tier2_provider(
    _provider: String,
    _api_key: String,
) -> Result<(), String> {
    Err("not_implemented".to_string())
}
