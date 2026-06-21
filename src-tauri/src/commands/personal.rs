// src-tauri/src/commands/personal.rs
//
// Group 6 — Personal context.
// Commands: get_personal_fields, update_personal_field, get_voice_profile.
//
// SECURITY INVARIANT: Raw PersonalTrack values must never leave this module.
// get_personal_fields returns field_name, sensitivity, and abstraction display
// values (abstraction_tier2, abstraction_tier3) only. field_value is excluded
// from the response struct -- PersonalField.field_value is #[serde(skip)] in
// the domain type, and PersonalFieldInfo below does not include it at all.
// This is the primary IPC enforcement point for the no-raw-values contract.
//
// Sensitivity validation: save_personal_field() in personal_store rejects
// unknown sensitivity values with a Validation error. No duplicate check
// needed at the IPC boundary.
//
// key_hex: passed per-call in Release 1 (no auth layer yet). Layer 8 will
// move session key management into tauri::State and eliminate per-call passing.

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::persistence::personal_store;

// ---------------------------------------------------------------------------
// Response structs
// ---------------------------------------------------------------------------

/// IPC-safe projection of PersonalField. field_value is intentionally absent.
#[derive(Debug, Serialize, Type)]
pub struct PersonalFieldInfo {
    pub field_name: String,
    pub sensitivity: String,
    /// Abstracted display value for Tier 2 routing contexts.
    pub abstraction_tier2: String,
    /// Abstracted display value for Tier 3 routing contexts.
    pub abstraction_tier3: String,
}

#[derive(Debug, Deserialize, Type)]
pub struct UpdatePersonalFieldRequest {
    pub persona_id: String,
    pub user_id: String,
    pub key_hex: String,
    pub field_name: String,
    pub value: String,
    /// Sensitivity of the field value. Defaults to "personal" if omitted.
    /// Validation (general/personal/medical/financial) is enforced by the store.
    pub sensitivity: Option<String>,
}

/// IPC projection of voice profile. The store may contain additional attributes
/// beyond these three; they are intentionally dropped at the IPC boundary.
/// The IPC surface specifies tone, formality, length_preference only.
#[derive(Debug, Serialize, Type)]
pub struct VoiceProfileInfo {
    pub tone: String,
    pub formality: String,
    pub length_preference: String,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_personal_fields(
    persona_id: String,
    user_id: String,
    key_hex: String,
) -> Result<Vec<PersonalFieldInfo>, String> {
    let fields =
        personal_store::list_personal_fields(&user_id, &persona_id, &key_hex, None, None)
            .await
            .map_err(|e| e.to_string())?;

    // Project to IPC-safe struct. field_value is never included.
    Ok(fields
        .into_iter()
        .map(|f| PersonalFieldInfo {
            field_name: f.field_name,
            sensitivity: f.sensitivity,
            abstraction_tier2: f.abstraction_tier2,
            abstraction_tier3: f.abstraction_tier3,
        })
        .collect())
}

#[tauri::command]
pub async fn update_personal_field(
    request: UpdatePersonalFieldRequest,
) -> Result<PersonalFieldInfo, String> {
    let sensitivity = request.sensitivity.as_deref().unwrap_or("personal");

    personal_store::save_personal_field(
        &request.user_id,
        &request.persona_id,
        &request.key_hex,
        &request.field_name,
        &request.value,
        sensitivity,
        "user",  // source_id: user-entered fields always have source "user"
        "self",  // ownership_scope: default per-user ownership
        "",      // abstraction_tier2: empty -- set by Gate logic, not user
        "",      // abstraction_tier3: empty -- set by Gate logic, not user
        "ipc",   // source: provenance tag for this write path
        None,    // extra_metadata
    )
    .await
    .map_err(|e| e.to_string())?;

    // Re-fetch to return current state (includes any gate-computed abstractions).
    let field = personal_store::get_personal_field(
        &request.user_id,
        &request.persona_id,
        &request.key_hex,
        &request.field_name,
    )
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "not_found".to_string())?;

    Ok(PersonalFieldInfo {
        field_name: field.field_name,
        sensitivity: field.sensitivity,
        abstraction_tier2: field.abstraction_tier2,
        abstraction_tier3: field.abstraction_tier3,
    })
}

#[tauri::command]
pub async fn get_voice_profile(
    persona_id: String,
    user_id: String,
    key_hex: String,
) -> Result<VoiceProfileInfo, String> {
    let profile =
        personal_store::load_voice_profile(&user_id, &persona_id, &key_hex)
            .await
            .map_err(|e| e.to_string())?;

    Ok(VoiceProfileInfo {
        tone: profile.get("tone").cloned().unwrap_or_default(),
        formality: profile.get("formality").cloned().unwrap_or_default(),
        length_preference: profile
            .get("length_preference")
            .cloned()
            .unwrap_or_default(),
    })
}
