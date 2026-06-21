// src-tauri/src/commands/persona.rs
//
// Group 4 — Persona and Focus management.
// Commands: list_personas, create_persona, list_focuses,
//           get_focus_settings, update_focus_settings.
//
// Friction gate (HANDOFF_IPC_SURFACE.md — post-Release 1):
//   update_focus_settings must enforce the friction gate for any change that
//   increases privacy restriction or moves a Focus to Protected profile.
//   The gate is a Conductor concern, not a frontend concern (IPC surface spec).
//   Current state: privacy-restricting changes are blocked with
//   Err("friction_gate_not_implemented") until the Conductor gate is wired.
//   Non-restricting changes proceed immediately.
//
// list_personas IPC gap (post-Release 1):
//   IPC surface specifies color, focus_count, and privacy defaults.
//   PersonaInfo currently returns id, display_name, persona_type, created_at only.
//   color and privacy defaults are in personas.extra_metadata (not yet parsed).
//   focus_count requires a join not present in persona_store.
//
// list_focuses IPC gap (post-Release 1):
//   IPC surface specifies dormancy state and last_used.
//   FocusInfo uses updated_at as a proxy; dormancy state is not in focus_settings.
//
// get_focus_settings takes (persona_id, focus_id) — the store key is composite.
//   The IPC surface spec lists focus_id only, written at a higher level of
//   abstraction. persona_id is required for the DB lookup and must be supplied.

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::persistence::focus_settings_store;
use crate::persistence::persona_store;

// ---------------------------------------------------------------------------
// Response structs
// ---------------------------------------------------------------------------

/// IPC gap: missing color, focus_count, privacy defaults (post-Release 1).
#[derive(Debug, Serialize, Type)]
pub struct PersonaInfo {
    pub id: String,
    pub display_name: String,
    pub persona_type: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize, Type)]
pub struct CreatePersonaRequest {
    pub user_id: String,
    pub name: String,
    // color omitted: persona_store::create_persona has no extra_metadata param.
    // Add when persistence supports it.
    pub persona_type: Option<String>,
}

#[derive(Debug, Serialize, Type)]
pub struct CreatePersonaResponse {
    pub persona_id: String,
}

/// IPC gap: missing dormancy_state, last_used (post-Release 1).
#[derive(Debug, Serialize, Type)]
pub struct FocusInfo {
    pub focus_id: String,
    pub focus_profile: String,
    pub context_flow: String,
    pub library_visibility: String,
    pub privacy_tier: i32,
    pub max_permitted_tier: i32,
    pub updated_at: String,
}

#[derive(Debug, Deserialize, Type)]
pub struct UpdateFocusSettingsRequest {
    pub persona_id: String,
    pub focus_id: String,
    pub context_flow: Option<String>,
    pub library_visibility: Option<String>,
    pub privacy_tier: Option<i32>,
    pub max_permitted_tier: Option<i32>,
    pub focus_profile: Option<String>,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_personas(user_id: String) -> Result<Vec<PersonaInfo>, String> {
    let personas = persona_store::list_personas_for_user(&user_id)
        .await
        .map_err(|e| e.to_string())?;

    Ok(personas
        .into_iter()
        .map(|p| PersonaInfo {
            id: p.id,
            display_name: p.display_name,
            persona_type: p.persona_type,
            created_at: p.created_at,
        })
        .collect())
}

#[tauri::command]
pub async fn create_persona(
    request: CreatePersonaRequest,
) -> Result<CreatePersonaResponse, String> {
    if request.name.trim().is_empty() {
        return Err("persona name cannot be empty".to_string());
    }

    let persona_type = match request.persona_type.as_deref().unwrap_or("standard") {
        t @ ("standard" | "protected") => t,
        other => {
            return Err(format!("invalid persona_type: {other}"));
        }
    };

    let persona_id = uuid::Uuid::new_v4().to_string();

    let persona = persona_store::create_persona(
        &persona_id,
        &request.name,
        persona_type,
        &request.user_id,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(CreatePersonaResponse { persona_id: persona.id })
}

#[tauri::command]
pub async fn list_focuses(persona_id: String) -> Result<Vec<FocusInfo>, String> {
    let settings = focus_settings_store::list_focus_settings_for_persona(&persona_id)
        .await
        .map_err(|e| e.to_string())?;

    Ok(settings
        .into_iter()
        .map(|s| FocusInfo {
            focus_id: s.focus_id,
            focus_profile: s.focus_profile,
            context_flow: s.context_flow,
            library_visibility: s.library_visibility,
            privacy_tier: s.privacy_tier,
            max_permitted_tier: s.max_permitted_tier,
            updated_at: s.updated_at,
        })
        .collect())
}

/// get_focus_settings takes both persona_id and focus_id — the store key is
/// composite. The IPC spec lists focus_id only (higher-level abstraction).
#[tauri::command]
pub async fn get_focus_settings(
    persona_id: String,
    focus_id: String,
) -> Result<FocusInfo, String> {
    let s = focus_settings_store::get_focus_settings(&persona_id, &focus_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not_found".to_string())?;

    Ok(FocusInfo {
        focus_id: s.focus_id,
        focus_profile: s.focus_profile,
        context_flow: s.context_flow,
        library_visibility: s.library_visibility,
        privacy_tier: s.privacy_tier,
        max_permitted_tier: s.max_permitted_tier,
        updated_at: s.updated_at,
    })
}

#[tauri::command]
pub async fn update_focus_settings(
    request: UpdateFocusSettingsRequest,
) -> Result<FocusInfo, String> {
    // Tier bounds check: valid tiers are 1-3.
    for (name, val) in [
        ("privacy_tier", request.privacy_tier),
        ("max_permitted_tier", request.max_permitted_tier),
    ] {
        if let Some(v) = val {
            if !(1..=3).contains(&v) {
                return Err(format!("{name} must be between 1 and 3, got {v}"));
            }
        }
    }

    // Friction gate guard (post-Release 1: Conductor enforcement not yet wired).
    // Block changes that increase privacy restriction or move to Protected profile
    // rather than silently bypassing the gate.
    let existing = focus_settings_store::get_focus_settings(
        &request.persona_id,
        &request.focus_id,
    )
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "not_found".to_string())?;

    let privacy_increases = request
        .privacy_tier
        .map(|t| t > existing.privacy_tier)
        .unwrap_or(false);
    let moves_to_protected = request
        .focus_profile
        .as_deref()
        .map(|p| p == "protected" && existing.focus_profile != "protected")
        .unwrap_or(false);

    if privacy_increases || moves_to_protected {
        return Err("friction_gate_not_implemented".to_string());
    }

    let s = focus_settings_store::update_focus_settings(
        &request.persona_id,
        &request.focus_id,
        request.context_flow.as_deref(),
        request.library_visibility.as_deref(),
        request.privacy_tier,
        request.max_permitted_tier,
        request.focus_profile.as_deref(),
        None, // voice_override: not exposed in IPC surface v1
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(FocusInfo {
        focus_id: s.focus_id,
        focus_profile: s.focus_profile,
        context_flow: s.context_flow,
        library_visibility: s.library_visibility,
        privacy_tier: s.privacy_tier,
        max_permitted_tier: s.max_permitted_tier,
        updated_at: s.updated_at,
    })
}
