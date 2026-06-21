// src-tauri/src/commands/onboarding.rs
//
// Group 3 — Onboarding.
// Commands: get_onboarding_focus_suggestions, submit_onboarding_persona_selection,
//           submit_onboarding_focus_selection.
//
// All three commands are STUBS for Release 1.
// Onboarding orchestration (persona seeding, focus suggestion logic, profile
// assignment) is not yet implemented in the Conductor layer.
// Each returns "not_implemented" until the onboarding layer is built.

#[tauri::command]
pub async fn get_onboarding_focus_suggestions(
    _personas: Vec<String>,
) -> Result<serde_json::Value, String> {
    Err("not_implemented".to_string())
}

#[tauri::command]
pub async fn submit_onboarding_persona_selection(
    _personas: Vec<String>,
) -> Result<Vec<String>, String> {
    Err("not_implemented".to_string())
}

#[tauri::command]
pub async fn submit_onboarding_focus_selection(
    _focus_selections: Vec<serde_json::Value>,
) -> Result<Vec<String>, String> {
    Err("not_implemented".to_string())
}
