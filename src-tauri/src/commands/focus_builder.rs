// src-tauri/src/commands/focus_builder.rs
//
// Group 8 — Focus Builder [STUB].
// Commands: get_focus_builder_session, submit_focus_builder_step.
//
// Focus Builder is not yet ported. Both commands return not_implemented.
// Unblocked post-migration when the Focus Builder Conductor layer is built.

#[tauri::command]
pub async fn get_focus_builder_session(
    _focus_id: Option<String>,
) -> Result<serde_json::Value, String> {
    Err("not_implemented".to_string())
}

#[tauri::command]
pub async fn submit_focus_builder_step(
    _session_id: String,
    _input: serde_json::Value,
) -> Result<serde_json::Value, String> {
    Err("not_implemented".to_string())
}
