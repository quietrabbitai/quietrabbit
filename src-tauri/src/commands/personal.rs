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
// key_hex: derived server-side from KeyRegistry (items.id=268) -- no longer
// passed per-call from the frontend. See auth::registry::key_hex.

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::persistence::personal_store;

// ---------------------------------------------------------------------------
// Response structs
// ---------------------------------------------------------------------------

/// IPC-safe projection of PersonalField. field_value is intentionally absent.
#[derive(Debug, Serialize, Type)]
pub struct PersonalFieldInfo {
    pub field_name: String,
    pub sensitivity: String,
    /// Abstracted display value for cloud_anonymous routing contexts.
    pub abstraction_tier2: String,
    /// Abstracted display value for cloud_frontier routing contexts.
    pub abstraction_tier3: String,
}

#[derive(Debug, Deserialize, Type)]
pub struct UpdatePersonalFieldRequest {
    pub persona_id: String,
    pub user_id: String,
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
#[specta::specta]
pub async fn get_personal_fields(
    persona_id: String,
    user_id: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<Vec<PersonalFieldInfo>, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.data_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let fields =
        personal_store::list_personal_fields(&user_id, &persona_id, &key_hex_str, None, None)
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

/// The write half of update_personal_field, split out so it is testable
/// against a real personal.db without a Tauri State (items.id=930).
async fn store_user_field(
    request: &UpdatePersonalFieldRequest,
    key_hex_str: &str,
) -> Result<(), String> {
    let sensitivity = request.sensitivity.as_deref().unwrap_or("personal");

    personal_store::save_personal_field(
        &request.user_id,
        &request.persona_id,
        key_hex_str,
        &request.field_name,
        &request.value,
        sensitivity,
        "user", // source_id: user-entered fields always have source "user"
        "self", // ownership_scope: default per-user ownership
        // items.id=930: "" violated the entity_facts CHECK. A valid default; the
        // disclosure policy is fact_disclosure_policy, not these columns.
        personal_store::default_abstraction_for(sensitivity),
        personal_store::default_abstraction_for(sensitivity),
        "ipc", // source: provenance tag for this write path
        None,  // extra_metadata
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn update_personal_field(
    request: UpdatePersonalFieldRequest,
    key_registry: State<'_, KeyRegistry>,
) -> Result<PersonalFieldInfo, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.data_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    store_user_field(&request, &key_hex_str).await?;

    // Re-fetch to return current state (includes any gate-computed abstractions).
    let field = personal_store::get_personal_field(
        &request.user_id,
        &request.persona_id,
        &key_hex_str,
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
#[specta::specta]
pub async fn get_voice_profile(
    persona_id: String,
    user_id: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<VoiceProfileInfo, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.data_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let profile = personal_store::load_voice_profile(&user_id, &persona_id, &key_hex_str)
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    const KEY: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb";

    async fn read_row(user: &str, persona: &str, field: &str) -> (String, String, String) {
        let mut conn = personal_store::open_personal_db(user, persona, KEY)
            .await
            .unwrap();
        let r = sqlx::query(
            "SELECT field_value, abstraction_tier2, abstraction_tier3 FROM entity_facts
             WHERE entity_id IS NULL AND field_name = ? AND valid_until IS NULL",
        )
        .bind(field)
        .fetch_one(&mut conn)
        .await
        .unwrap();
        (r.get(0), r.get(1), r.get(2))
    }

    // items.id=930 stage 1: the first tests this writer has had, against the real schema.
    #[tokio::test]
    async fn update_personal_field_writes_succeed_against_the_real_schema() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved = std::env::var("QR_DATA_ROOT").ok();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("QR_DATA_ROOT", dir.path());
        let (u, p) = ("pf-user", "pf-persona");

        for (field, sens, want) in [
            ("home_city", None, "pass"),
            ("employer", Some("general"), "pass"),
            ("diagnosis", Some("medical"), "not_permitted"),
            ("income", Some("financial"), "not_permitted"),
        ] {
            let req = UpdatePersonalFieldRequest {
                persona_id: p.to_owned(),
                user_id: u.to_owned(),
                field_name: field.to_owned(),
                value: "v1".to_owned(),
                sensitivity: sens.map(str::to_owned),
            };
            store_user_field(&req, KEY)
                .await
                .unwrap_or_else(|e| panic!("{field}: {e}"));
            let (v, t2, t3) = read_row(u, p, field).await;
            assert_eq!((v.as_str(), t2.as_str(), t3.as_str()), ("v1", want, want));
        }

        // An unknown sensitivity is still refused by the store.
        let bad = UpdatePersonalFieldRequest {
            persona_id: p.to_owned(),
            user_id: u.to_owned(),
            field_name: "x".to_owned(),
            value: "v".to_owned(),
            sensitivity: Some("secret".to_owned()),
        };
        assert!(store_user_field(&bad, KEY).await.is_err());

        match saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }
}
