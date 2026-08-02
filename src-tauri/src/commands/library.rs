// src-tauri/src/commands/library.rs
//
// Group 7 — Library.
// Commands: list_outputs, get_output, delete_output.
//
// list_outputs: wired to output_store::list_outputs() (items.id=91 part 1,
//   fixed 2026-07-26). Supports optional focus_id/topic_id/output_type
//   filters, joined through focus_runs.
// get_output: wired to output_store::get_output().
// delete_output: wired to output_store::delete_output() (items.id=91 part 2,
//   complete 2026-07-26). Soft-delete only -- see output_store::delete_output
//   for the deletion sequence and deep_purge's deliberately-unimplemented
//   status.
//
// Library visibility gap (post-Release 1, split to items.id=175):
//   output_store::get_output() enforces status='active' and per-scope DB
//   isolation (user_id/persona_id/key_hex opens the correct encrypted DB),
//   but does not enforce Focus profile visibility rules (Open/Organized/
//   Protected). That filtering layer is not yet implemented in the store.
//   items.id=175's foundation work (personal_003.sql, conductor::visibility::
//   evaluate_object_visibility per decisions.id=513) landed 2026-08-01 --
//   entity-level flags now exist, but wiring this store's get_output() path
//   to call evaluate_object_visibility is separate follow-on work, not yet
//   done. See items.id=91 for the split record.
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

fn to_output_info(record: output_store::OutputRecord) -> OutputInfo {
    OutputInfo {
        id: record.id,
        focus_run_id: record.focus_run_id,
        output_type: record.output_type,
        content: record.content,
        sensitivity: record.sensitivity,
        status: record.status,
        created_at: record.created_at,
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Lists active outputs, optionally filtered by focus_id, topic_id, and/or
/// output_type. Wired to output_store::list_outputs() (items.id=91, part 1).
///
/// Does NOT enforce Focus profile visibility rules (Open/Organized/
/// Protected) -- that filtering layer is a separate, not-yet-built gap,
/// split to items.id=175 (post-Release 1). See module header.
#[tauri::command]
#[specta::specta]
pub async fn list_outputs(
    user_id: String,
    persona_id: String,
    key_hex: String,
    focus_id: Option<String>,
    topic_id: Option<String>,
    output_type: Option<String>,
) -> Result<Vec<OutputInfo>, String> {
    let records = output_store::list_outputs(
        &user_id,
        &persona_id,
        &key_hex,
        focus_id.as_deref(),
        topic_id.as_deref(),
        output_type.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(records.into_iter().map(to_output_info).collect())
}

#[tauri::command]
#[specta::specta]
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

    Ok(to_output_info(record))
}

/// Deletes an output (items.id=91, part 2, complete 2026-07-26). Soft-delete
/// only -- content is zeroed and status set to 'deleted'; the row is never
/// hard-deleted (architecture Section 3.4, output_store::delete_output).
///
/// deep_purge: accepted for command-contract stability but NOT implemented.
/// Passing Some(true) returns Err("deep_purge_not_implemented"). See
/// output_store::delete_output's doc comment for why this is deliberately
/// out of scope for R1.
#[tauri::command]
#[specta::specta]
pub async fn delete_output(
    output_id: String,
    user_id: String,
    persona_id: String,
    key_hex: String,
    deep_purge: Option<bool>,
) -> Result<(), String> {
    output_store::delete_output(&user_id, &persona_id, &key_hex, &output_id, deep_purge)
        .await
        .map_err(|e| e.to_string())
}
