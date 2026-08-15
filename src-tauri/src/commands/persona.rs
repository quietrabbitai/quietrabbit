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
// list_personas IPC gap -- items.id=237, color/focus_count CLOSED:
//   PersonaInfo now returns color (personas.extra_metadata.color, written by
//   create_persona) and focus_count (LEFT JOIN focus_settings, persona_store.rs).
//   "privacy defaults" from the IPC surface's row 14 is still open -- no
//   decision names what a Persona-level privacy default even is (privacy
//   settings are Focus-level per D6-297); not in items.id=237's scope.
//
// list_focuses IPC gap -- items.id=237, last_used CLOSED:
//   FocusInfo.last_used is now a real value: MAX(started_at) from outputs.db's
//   focus_runs (persistence::output_store::get_focus_last_used/get_last_used_map),
//   which is why list_focuses/get_focus_settings/update_focus_settings now take
//   user_id and access outputs.db -- the same per-persona encrypted DB access
//   pattern already used by commands::active_board::get_active_board/get_topic_list.
//   key_hex itself is derived server-side from KeyRegistry (items.id=268), not
//   accepted as an IPC parameter -- see auth::registry::key_hex.
//   dormancy_state is NOT part of this fix -- split to items.id=256. The
//   dispatch for this item assumed an existing Persona-level Hibernate/Archive
//   lifecycle model (items.id=20) could be reused for it; investigation found
//   items.id=20 is a design description only (QUIET_RABBIT_DESIGN.md), with no
//   personas.status column, enum, or commands anywhere in this repo -- nothing
//   to reuse, and the exact value set is a real design decision, not something
//   to invent mid-build.
//
// get_focus_settings takes (persona_id, focus_id) — the store key is composite.
//   The IPC surface spec lists focus_id only, written at a higher level of
//   abstraction. persona_id is required for the DB lookup and must be supplied.

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::persistence::focus_settings_store;
use crate::persistence::output_store;
use crate::persistence::persona_store;

// ---------------------------------------------------------------------------
// Response structs
// ---------------------------------------------------------------------------

/// IPC gap: privacy defaults still missing (post-Release 1, not this item's
/// scope -- see module header). color/focus_count closed by items.id=237.
#[derive(Debug, Serialize, Type)]
pub struct PersonaInfo {
    pub id: String,
    pub display_name: String,
    pub persona_type: String,
    pub created_at: String,
    pub color: Option<String>,
    /// i32, not Persona.focus_count's i64 -- specta forbids exporting
    /// BigInt-style types (i64/u64/...) to TypeScript.
    pub focus_count: i32,
}

#[derive(Debug, Deserialize, Type)]
pub struct CreatePersonaRequest {
    pub user_id: String,
    pub name: String,
    pub color: Option<String>,
    pub persona_type: Option<String>,
}

#[derive(Debug, Serialize, Type)]
pub struct CreatePersonaResponse {
    pub persona_id: String,
}

/// IPC gap: dormancy_state still missing -- split to items.id=256 (see
/// module header). last_used closed by items.id=237.
#[derive(Debug, Serialize, Type)]
pub struct FocusInfo {
    pub focus_id: String,
    pub focus_profile: String,
    pub context_flow: String,
    pub library_visibility: String,
    pub privacy_tier: i32,
    pub max_permitted_tier: i32,
    pub updated_at: String,
    /// Most recent focus_runs.started_at for this Focus (outputs.db), or
    /// None if it has never run or outputs.db isn't reachable with the
    /// supplied key_hex. NOT the same as updated_at (settings-edit time).
    pub last_used: Option<String>,
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
        .map(|p| {
            let color = p
                .extra_metadata
                .get("color")
                .and_then(|v| v.as_str())
                .map(String::from);
            PersonaInfo {
                id: p.id,
                display_name: p.display_name,
                persona_type: p.persona_type,
                created_at: p.created_at,
                color,
                focus_count: p.focus_count as i32,
            }
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
        request.color.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(CreatePersonaResponse {
        persona_id: persona.id,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn list_focuses(
    user_id: String,
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<Vec<FocusInfo>, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let settings = focus_settings_store::list_focus_settings_for_persona(&persona_id)
        .await
        .map_err(|e| e.to_string())?;

    let last_used_map = output_store::get_last_used_map(&user_id, &persona_id, &key_hex_str).await;

    Ok(settings
        .into_iter()
        .map(|s| {
            let last_used = last_used_map.get(&s.focus_id).cloned();
            FocusInfo {
                focus_id: s.focus_id,
                focus_profile: s.focus_profile,
                context_flow: s.context_flow,
                library_visibility: s.library_visibility,
                privacy_tier: s.privacy_tier,
                max_permitted_tier: s.max_permitted_tier,
                updated_at: s.updated_at,
                last_used,
            }
        })
        .collect())
}

/// get_focus_settings takes both persona_id and focus_id — the store key is
/// composite. The IPC spec lists focus_id only (higher-level abstraction).
#[tauri::command]
#[specta::specta]
pub async fn get_focus_settings(
    user_id: String,
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
    focus_id: String,
) -> Result<FocusInfo, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let s = focus_settings_store::get_focus_settings(&persona_id, &focus_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not_found".to_string())?;

    let last_used =
        output_store::get_focus_last_used(&user_id, &persona_id, &key_hex_str, &focus_id).await;

    Ok(FocusInfo {
        focus_id: s.focus_id,
        focus_profile: s.focus_profile,
        context_flow: s.context_flow,
        library_visibility: s.library_visibility,
        privacy_tier: s.privacy_tier,
        max_permitted_tier: s.max_permitted_tier,
        updated_at: s.updated_at,
        last_used,
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
    user_id: String,
    key_registry: State<'_, KeyRegistry>,
    request: UpdateFocusSettingsRequest,
) -> Result<FocusInfo, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

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
    let existing = focus_settings_store::get_focus_settings(&request.persona_id, &request.focus_id)
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
            requested_privacy_tier: if privacy_would_loosen {
                request.privacy_tier
            } else {
                None
            },
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
        let detail_json =
            serde_json::to_string(&detail).unwrap_or_else(|_| "friction_gate_blocked".to_owned());
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

    let last_used = output_store::get_focus_last_used(
        &user_id,
        &request.persona_id,
        &key_hex_str,
        &request.focus_id,
    )
    .await;

    Ok(FocusInfo {
        focus_id: s.focus_id,
        focus_profile: s.focus_profile,
        context_flow: s.context_flow,
        library_visibility: s.library_visibility,
        privacy_tier: s.privacy_tier,
        max_permitted_tier: s.max_permitted_tier,
        updated_at: s.updated_at,
        last_used,
    })
}

// ---------------------------------------------------------------------------
// Tests (items.id=237)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::output_store;
    use crate::test_support::{mock_app_with_registry, populate_registry, ENV_MUTEX};
    use tauri::Manager;

    const USER_ID: &str = "user-persona-test";
    const PERSONA_ID: &str = "persona-persona-test";
    const MASTER_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
        [0xABu8; crate::auth::kdf::MASTER_KEY_LEN];

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    /// Real shared.db + real encrypted outputs.db via the actual migration
    /// path -- mirrors commands::library's setup() (library.rs:294-366).
    /// Does NOT create a persona -- each test creates its own via
    /// persona_store::create_persona so color/focus_count can vary per test.
    async fn setup() -> TestEnv {
        let lock = ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();

        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration must succeed in test setup");
        crate::persistence::migrations::migrate_outputs_db(
            USER_ID,
            PERSONA_ID,
            &key_hex(&MASTER_KEY),
        )
        .await
        .expect("outputs.db migration must succeed in test setup");

        crate::auth::user_store::create_user(
            USER_ID,
            "Persona Test User",
            "user",
            false,
            &[0u8; crate::auth::kdf::SALT_LEN],
            crate::auth::kdf::DEFAULT_ARGON2_MEMORY_KIB,
            crate::auth::kdf::DEFAULT_ARGON2_ITERATIONS,
            crate::auth::kdf::DEFAULT_ARGON2_PARALLELISM,
        )
        .await
        .expect("create_user must succeed in test setup");

        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
        }
    }

    #[tokio::test]
    async fn list_personas_returns_real_color() {
        let _env = setup().await;
        persona_store::create_persona(
            PERSONA_ID,
            "Color Test Persona",
            "personal",
            USER_ID,
            Some("indigo"),
        )
        .await
        .expect("create_persona must succeed");

        let personas = list_personas(USER_ID.to_owned())
            .await
            .expect("list_personas must succeed");

        assert_eq!(personas.len(), 1);
        assert_eq!(
            personas[0].color,
            Some("indigo".to_owned()),
            "color must round-trip through extra_metadata, not be a placeholder"
        );
    }

    #[tokio::test]
    async fn list_personas_color_is_none_when_unset() {
        let _env = setup().await;
        persona_store::create_persona(PERSONA_ID, "No Color Persona", "personal", USER_ID, None)
            .await
            .expect("create_persona must succeed");

        let personas = list_personas(USER_ID.to_owned())
            .await
            .expect("list_personas must succeed");

        assert_eq!(
            personas[0].color, None,
            "schema default for a persona created without color is None, not a placeholder string"
        );
    }

    #[tokio::test]
    async fn list_personas_focus_count_reflects_real_focus_settings_rows() {
        let _env = setup().await;
        persona_store::create_persona(PERSONA_ID, "Focus Count Persona", "personal", USER_ID, None)
            .await
            .expect("create_persona must succeed");

        let before = list_personas(USER_ID.to_owned())
            .await
            .expect("list_personas must succeed");
        assert_eq!(before[0].focus_count, 0, "a fresh persona has no Focuses yet");

        for focus_id in ["quick-ask", "writing-assistant"] {
            focus_settings_store::create_focus_settings(
                PERSONA_ID,
                focus_id,
                "bidirectional",
                "shared",
                2,
                2,
                "open",
                None,
            )
            .await
            .expect("create_focus_settings must succeed");
        }

        let after = list_personas(USER_ID.to_owned())
            .await
            .expect("list_personas must succeed");
        assert_eq!(
            after[0].focus_count, 2,
            "focus_count must reflect the real number of focus_settings rows, not a default"
        );
    }

    #[tokio::test]
    async fn get_focus_settings_last_used_is_none_before_any_run() {
        let _env = setup().await;
        persona_store::create_persona(PERSONA_ID, "Last Used Persona", "personal", USER_ID, None)
            .await
            .expect("create_persona must succeed");
        focus_settings_store::create_focus_settings(
            PERSONA_ID,
            "quick-ask",
            "bidirectional",
            "shared",
            2,
            2,
            "open",
            None,
        )
        .await
        .expect("create_focus_settings must succeed");

        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, USER_ID, MASTER_KEY).await;

        let info = get_focus_settings(
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            registry,
            "quick-ask".to_owned(),
        )
        .await
        .expect("get_focus_settings must succeed");

        assert_eq!(
            info.last_used, None,
            "a Focus with zero focus_runs must report last_used=None, not a placeholder"
        );
    }

    #[tokio::test]
    async fn get_focus_settings_last_used_is_real_after_a_run() {
        let _env = setup().await;
        persona_store::create_persona(PERSONA_ID, "Last Used Persona 2", "personal", USER_ID, None)
            .await
            .expect("create_persona must succeed");
        focus_settings_store::create_focus_settings(
            PERSONA_ID,
            "quick-ask",
            "bidirectional",
            "shared",
            2,
            2,
            "open",
            None,
        )
        .await
        .expect("create_focus_settings must succeed");
        output_store::test_seed_focus_run(
            USER_ID,
            PERSONA_ID,
            &key_hex(&MASTER_KEY),
            "run-1",
            "quick-ask",
        )
        .await
        .expect("test_seed_focus_run must succeed");

        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, USER_ID, MASTER_KEY).await;

        let info = get_focus_settings(
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            registry,
            "quick-ask".to_owned(),
        )
        .await
        .expect("get_focus_settings must succeed");

        assert!(
            info.last_used.is_some(),
            "last_used must be a real MAX(started_at) value once a focus_run exists, not None"
        );
    }

    #[tokio::test]
    async fn list_focuses_last_used_matches_get_focus_settings_via_batch_map() {
        let _env = setup().await;
        persona_store::create_persona(PERSONA_ID, "Batch Persona", "personal", USER_ID, None)
            .await
            .expect("create_persona must succeed");
        focus_settings_store::create_focus_settings(
            PERSONA_ID,
            "quick-ask",
            "bidirectional",
            "shared",
            2,
            2,
            "open",
            None,
        )
        .await
        .expect("create_focus_settings must succeed");
        output_store::test_seed_focus_run(
            USER_ID,
            PERSONA_ID,
            &key_hex(&MASTER_KEY),
            "run-1",
            "quick-ask",
        )
        .await
        .expect("test_seed_focus_run must succeed");

        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, USER_ID, MASTER_KEY).await;

        let focuses = list_focuses(USER_ID.to_owned(), PERSONA_ID.to_owned(), registry)
            .await
            .expect("list_focuses must succeed");

        assert_eq!(focuses.len(), 1);
        assert!(
            focuses[0].last_used.is_some(),
            "list_focuses' batched last_used map must surface the same real value \
             get_focus_settings' single-focus lookup does"
        );
    }
}
