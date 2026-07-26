// src-tauri/src/commands/persona.rs
//
// Group 4 — Persona and Focus management.
// Commands: list_personas, create_persona, list_focuses,
//           get_focus_settings, update_focus_settings.
//
// Friction gate (HANDOFF_IPC_SURFACE.md — implemented items.id=92, 2026-07-26):
//   update_focus_settings enforces the friction gate for any change that
//   loosens privacy_tier (numerically increases -- see note below) or moves
//   a Focus to Protected profile. Per HANDOFF_IPC_SURFACE.md: "The gate is
//   surfaced to the user before the command completes" and "Backend
//   enforces it, frontend responds to the result" -- both satisfied by a
//   structured FrictionGateBlocked error returned from this same command
//   (not a second round trip), which the frontend uses to show a
//   confirm/cancel prompt and then calls commands::consent::
//   submit_friction_gate_decision with the user's choice.
//
//   TIER DIRECTION NOTE: privacy_tier is 1 (red, most restrictive) through
//   3 (green, least restrictive) -- see focus_settings_store.rs header and
//   conductor/lifecycle.rs's tier-ceiling check (a step "requires" a tier;
//   higher tier = more external routing permitted = less private). A
//   numeric tier *increase* therefore LOOSENS privacy, it does not
//   restrict it. An earlier version of this comment and the code's own
//   variable names had this backwards (calling a tier increase "privacy
//   restriction increasing") -- fixed here; the underlying gate condition
//   (t > existing.privacy_tier) was always correct, only the naming lied
//   about what it meant.
//
//   submit_friction_gate_decision (commands/consent.rs) is the actor that
//   both records the decision (focus_settings_friction_decisions,
//   shared_002.sql) AND applies the settings change on 'proceed' -- mirrors
//   submit_extract_confirm's shape (record + follow-through in one command)
//   rather than adding a confirm flag back onto update_focus_settings,
//   which would create two different code paths to the same mutation.
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

/// Structured detail for a friction-gate-blocked update_focus_settings call
/// (items.id=92). The frontend uses this to build a confirm/cancel prompt,
/// then calls commands::consent::submit_friction_gate_decision with the
/// user's choice -- see that command's doc comment for the full flow.
///
/// requested_privacy_tier / requested_focus_profile: whichever of the two
/// actually tripped the gate. Both are echoed even though only one may be
/// gate-relevant, so the frontend can show the complete requested state
/// without a second get_focus_settings round trip.
#[derive(Debug, Serialize, Type)]
pub struct FrictionGateDetail {
    pub persona_id: String,
    pub focus_id: String,
    pub requested_privacy_tier: Option<i32>,
    pub requested_focus_profile: Option<String>,
    pub existing_privacy_tier: i32,
    pub existing_focus_profile: String,
    /// True when privacy_tier would numerically increase (loosen -- see
    /// module header's TIER DIRECTION NOTE). False when only focus_profile
    /// moving to 'protected' tripped the gate.
    pub privacy_would_loosen: bool,
    /// True when focus_profile would move to 'protected'.
    pub moves_to_protected: bool,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
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
#[specta::specta]
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
#[specta::specta]
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
#[specta::specta]
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

/// Applies a Focus settings change directly, UNLESS the change would loosen
/// privacy_tier or move focus_profile to 'protected' -- in which case this
/// returns Err(json-serialized FrictionGateDetail) instead of applying
/// anything, and the frontend must route the user through
/// commands::consent::submit_friction_gate_decision to either apply the
/// change (decision="proceed") or drop it (decision="cancel"). See module
/// header for the full flow and the tier-direction note.
///
/// The error string is JSON (FrictionGateDetail serialized), not a plain
/// message -- distinguishable from every other error this command can
/// return (validation failures, not_found) by attempting a JSON parse.
/// A frontend that doesn't parse it still gets a readable-enough string,
/// but the structured shape is what submit_friction_gate_decision expects
/// to be built from.
#[tauri::command]
#[specta::specta]
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

    // Friction gate check (items.id=92). privacy_tier is 1=red (most
    // restrictive) .. 3=green (least restrictive) -- see module header's
    // TIER DIRECTION NOTE. A numeric increase LOOSENS privacy.
    let existing = focus_settings_store::get_focus_settings(
        &request.persona_id,
        &request.focus_id,
    )
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "not_found".to_string())?;

    let privacy_would_loosen = request
        .privacy_tier
        .map(|t| t > existing.privacy_tier)
        .unwrap_or(false);
    let moves_to_protected = request
        .focus_profile
        .as_deref()
        .map(|p| p == "protected" && existing.focus_profile != "protected")
        .unwrap_or(false);

    if privacy_would_loosen || moves_to_protected {
        let detail = FrictionGateDetail {
            persona_id: request.persona_id.clone(),
            focus_id: request.focus_id.clone(),
            requested_privacy_tier: if privacy_would_loosen { request.privacy_tier } else { None },
            requested_focus_profile: if moves_to_protected {
                request.focus_profile.clone()
            } else {
                None
            },
            existing_privacy_tier: existing.privacy_tier,
            existing_focus_profile: existing.focus_profile.clone(),
            privacy_would_loosen,
            moves_to_protected,
        };
        let detail_json = serde_json::to_string(&detail)
            .unwrap_or_else(|_| "friction_gate_blocked".to_owned());
        return Err(detail_json);
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
