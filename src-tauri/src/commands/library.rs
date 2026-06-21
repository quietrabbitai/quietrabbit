// src-tauri/src/commands/library.rs
//
// Group 7 — Library.
// Commands: list_outputs, get_output, delete_output.
//
// list_outputs: STUB -- no list function in output_store. Flagged to Chat-PM.
// get_output: wired to output_store::get_output().
// delete_output: STUB -- full zero-then-delete sequence deferred to Layer 5+
//   (see output_store::delete_output comment for correct deletion sequence).
//
// Library visibility gap (post-Release 1):
//   output_store::get_output() enforces status='active' and per-scope DB
//   isolation (user_id/persona_id/key_hex opens the correct encrypted DB),
//   but does not enforce Focus profile visibility rules (Open/Organized/
//   Protected). That filtering layer is not yet implemented in the store.
//
// key_hex/user_id/persona_id via IPC: intentional for Release 1 (no auth
//   layer yet). Layer 8 will move session key management into tauri::State.

use serde::Serialize;
use specta::Type;

use crate::persistence::output_store;

// ---------------------------------------------------------------------------
// Response structs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Type)]
pub struct OutputInfo {
    pub id: String,
    pub focus_run_id: String,
    pub output_type: String,
    pub content: String,
    pub sensitivity: String,
    pub status: String,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// STUB -- list_outputs requires a list function in output_store (not yet added).
#[tauri::command]
pub async fn list_outputs(
    _focus_id: Option<String>,
    _topic_id: Option<String>,
    _output_type: Option<String>,
) -> Result<Vec<OutputInfo>, String> {
    Err("not_implemented".to_string())
}

#[tauri::command]
pub async fn get_output(
    output_id: String,
    user_id: String,
    persona_id: String,
    key_hex: String,
) -> Result<OutputInfo, String> {
    let record = output_store::get_output(&user_id, &persona_id, &key_hex, &output_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not_found".to_string())?;

    Ok(OutputInfo {
        id: record.id,
        focus_run_id: record.focus_run_id,
        output_type: record.output_type,
        content: record.content,
        sensitivity: record.sensitivity,
        status: record.status,
        created_at: record.created_at,
    })
}

/// STUB -- full zero-then-delete sequence deferred to Layer 5+.
#[tauri::command]
pub async fn delete_output(
    _output_id: String,
    _deep_purge: Option<bool>,
) -> Result<(), String> {
    Err("not_implemented".to_string())
}
