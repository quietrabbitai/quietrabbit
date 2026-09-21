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
use crate::conductor::tokens::ExternalAccess;
use crate::persistence::focus_settings_store;
use crate::persistence::output_store;
use crate::persistence::persona_store;

// ---------------------------------------------------------------------------
// PrivacyPreference (items.id=533)
// ---------------------------------------------------------------------------

/// IPC-boundary enum for focus_settings.privacy_tier (items.id=533).
/// Deliberately distinct from ExternalAccess (max_permitted_tier) -- CLAUDE.md:
/// never conflate focus_settings.privacy_tier with max_permitted_tier. Lives
/// here, not conductor/tokens.rs (ExternalAccess's home), to stay colocated
/// with the IPC structs it exists for and reinforce that it is not part of
/// the Conductor's own tier model.
///
/// IPC BOUNDARY ONLY: the DB column, focus_settings_store.rs, persona_store.rs,
/// and lifecycle.rs's focus_privacy_tier.min(execution_tier) arithmetic all
/// stay a plain i32 -- out of scope for this item. Conversion happens only in
/// this file and consent.rs, at the point a FocusSettings (store, i32) value
/// crosses into or out of an IPC struct.
///
/// Explicit discriminants double as the numeric mapping (Red=1/Yellow=2/
/// Green=3, matching focus_settings_store.rs's header comment and
/// decisions.id=649) -- derive(Ord) on a fieldless enum orders by
/// discriminant value, so Red < Yellow < Green requires the discriminants
/// to stay in ascending declaration order (verified against rustc 1.97.1).
/// This exactly matches the existing "numeric increase loosens privacy"
/// direction this module's TIER DIRECTION NOTE (above) already documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyPreference {
    Red = 1,
    Yellow = 2,
    Green = 3,
}

impl PrivacyPreference {
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

impl TryFrom<i32> for PrivacyPreference {
    type Error = String;

    fn try_from(v: i32) -> Result<Self, Self::Error> {
        match v {
            1 => Ok(Self::Red),
            2 => Ok(Self::Yellow),
            3 => Ok(Self::Green),
            other => Err(format!(
                "invalid privacy_tier {other}: must be 1 (red), 2 (yellow), or 3 (green)"
            )),
        }
    }
}

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
    pub privacy_tier: PrivacyPreference,
    pub max_permitted_tier: ExternalAccess,
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
    pub privacy_tier: Option<PrivacyPreference>,
    pub max_permitted_tier: Option<ExternalAccess>,
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
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct FrictionGateDetail {
    pub persona_id: String,
    pub focus_id: String,
    pub requested_privacy_tier: Option<PrivacyPreference>,
    pub requested_focus_profile: Option<String>,
    pub requested_max_permitted_tier: Option<ExternalAccess>,
    pub existing_privacy_tier: PrivacyPreference,
    pub existing_focus_profile: String,
    pub existing_max_permitted_tier: ExternalAccess,
    /// True when privacy_tier would numerically increase (loosen -- see
    /// module header's TIER DIRECTION NOTE). False when only focus_profile
    /// moving to 'protected' tripped the gate.
    pub privacy_would_loosen: bool,
    /// True when focus_profile would move to 'protected'.
    pub moves_to_protected: bool,
    /// items.id=321: true when max_permitted_tier would numerically
    /// increase -- same loosening direction as privacy_tier.
    pub max_permitted_tier_would_loosen: bool,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn list_personas(
    user_id: String,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<Vec<PersonaInfo>, String> {
    let personas = persona_store::list_personas_for_user(&pool, &user_id)
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
    pool: State<'_, sqlx::SqlitePool>,
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
        &pool,
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
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<Vec<FocusInfo>, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let settings = focus_settings_store::list_focus_settings_for_persona(&pool, &persona_id)
        .await
        .map_err(|e| e.to_string())?;

    let last_used_map = output_store::get_last_used_map(&user_id, &persona_id, &key_hex_str).await;

    // Collected into a Result, not a bare Vec via .map() -- privacy_tier's
    // i32 -> PrivacyPreference conversion (items.id=533) is fallible (a
    // corrupt DB value is a real, if unlikely, failure mode), so a single
    // bad row must fail the whole call rather than being silently dropped
    // or defaulted.
    settings
        .into_iter()
        .map(|s| {
            let last_used = last_used_map.get(&s.focus_id).cloned();
            Ok(FocusInfo {
                focus_id: s.focus_id,
                focus_profile: s.focus_profile,
                context_flow: s.context_flow,
                library_visibility: s.library_visibility,
                privacy_tier: PrivacyPreference::try_from(s.privacy_tier)?,
                max_permitted_tier: s.max_permitted_tier,
                updated_at: s.updated_at,
                last_used,
            })
        })
        .collect()
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
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<FocusInfo, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let s = focus_settings_store::get_focus_settings(&pool, &persona_id, &focus_id)
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
        privacy_tier: PrivacyPreference::try_from(s.privacy_tier)?,
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
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<FocusInfo, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    // No manual bounds check needed for either tier field -- items.id=448
    // retyped max_permitted_tier to ExternalAccess and items.id=533 retyped
    // privacy_tier to PrivacyPreference, so an out-of-range value is
    // unrepresentable by construction for both (same reasoning already
    // applied to FailureHandler::new(), conductor/failure.rs).

    // Friction gate check (items.id=92). privacy_tier is red (most
    // restrictive) .. green (least restrictive) -- see module header's
    // TIER DIRECTION NOTE. A PrivacyPreference increase LOOSENS privacy;
    // existing.privacy_tier is the store's own i32 (out of scope for
    // items.id=533), so the comparison converts request.privacy_tier to i32
    // rather than converting existing.privacy_tier to PrivacyPreference --
    // cheaper and this comparison never needs to construct the enum value.
    let existing =
        focus_settings_store::get_focus_settings(&pool, &request.persona_id, &request.focus_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "not_found".to_string())?;

    let privacy_would_loosen = request
        .privacy_tier
        .map(|t| t.as_i32() > existing.privacy_tier)
        .unwrap_or(false);
    let moves_to_protected = request
        .focus_profile
        .as_deref()
        .map(|p| p == "protected" && existing.focus_profile != "protected")
        .unwrap_or(false);
    let max_permitted_tier_would_loosen = request
        .max_permitted_tier
        .map(|t| t > existing.max_permitted_tier)
        .unwrap_or(false);

    if privacy_would_loosen || moves_to_protected || max_permitted_tier_would_loosen {
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
            requested_max_permitted_tier: if max_permitted_tier_would_loosen {
                request.max_permitted_tier
            } else {
                None
            },
            existing_privacy_tier: PrivacyPreference::try_from(existing.privacy_tier)?,
            existing_focus_profile: existing.focus_profile.clone(),
            existing_max_permitted_tier: existing.max_permitted_tier,
            privacy_would_loosen,
            moves_to_protected,
            max_permitted_tier_would_loosen,
        };
        let detail_json =
            serde_json::to_string(&detail).unwrap_or_else(|_| "friction_gate_blocked".to_owned());
        return Err(detail_json);
    }

    let s = focus_settings_store::update_focus_settings(
        &pool,
        &request.persona_id,
        &request.focus_id,
        request.context_flow.as_deref(),
        request.library_visibility.as_deref(),
        request.privacy_tier.map(PrivacyPreference::as_i32),
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
        privacy_tier: PrivacyPreference::try_from(s.privacy_tier)?,
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
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
        pool: sqlx::SqlitePool,
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
        let lock = ENV_MUTEX.lock().await;
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

        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool must connect");

        crate::auth::user_store::create_user(
            &pool,
            USER_ID,
            "Persona Test User",
            "user",
            false,
            &[0u8; crate::auth::kdf::SALT_LEN],
            crate::auth::kdf::DEFAULT_ARGON2_MEMORY_KIB,
            crate::auth::kdf::DEFAULT_ARGON2_ITERATIONS,
            crate::auth::kdf::DEFAULT_ARGON2_PARALLELISM,
            &[0u8; 32],
        )
        .await
        .expect("create_user must succeed in test setup");

        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
            pool,
        }
    }

    #[tokio::test]
    async fn list_personas_returns_real_color() {
        let _env = setup().await;
        persona_store::create_persona(
            &_env.pool,
            PERSONA_ID,
            "Color Test Persona",
            "personal",
            USER_ID,
            Some("indigo"),
        )
        .await
        .expect("create_persona must succeed");

        let app = mock_app_with_registry(_env.pool.clone());
        let pool = app.state::<sqlx::SqlitePool>();
        let personas = list_personas(USER_ID.to_owned(), pool)
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
        persona_store::create_persona(
            &_env.pool,
            PERSONA_ID,
            "No Color Persona",
            "personal",
            USER_ID,
            None,
        )
        .await
        .expect("create_persona must succeed");

        let app = mock_app_with_registry(_env.pool.clone());
        let pool = app.state::<sqlx::SqlitePool>();
        let personas = list_personas(USER_ID.to_owned(), pool)
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
        persona_store::create_persona(
            &_env.pool,
            PERSONA_ID,
            "Focus Count Persona",
            "personal",
            USER_ID,
            None,
        )
        .await
        .expect("create_persona must succeed");

        let app = mock_app_with_registry(_env.pool.clone());
        let pool = app.state::<sqlx::SqlitePool>();
        let before = list_personas(USER_ID.to_owned(), pool.clone())
            .await
            .expect("list_personas must succeed");
        assert_eq!(
            before[0].focus_count as usize,
            persona_store::SEEDED_FOCUS_IDS.len(),
            "create_persona must seed focus_settings rows for the default Focuses"
        );

        for focus_id in ["role-assessment", "some-other-focus"] {
            focus_settings_store::create_focus_settings(
                &_env.pool,
                PERSONA_ID,
                focus_id,
                "bidirectional",
                "shared",
                2,
                ExternalAccess::AnonymousRequired,
                "open",
                None,
            )
            .await
            .expect("create_focus_settings must succeed");
        }

        let after = list_personas(USER_ID.to_owned(), pool)
            .await
            .expect("list_personas must succeed");
        assert_eq!(
            after[0].focus_count as usize,
            persona_store::SEEDED_FOCUS_IDS.len() + 2,
            "focus_count must reflect the real number of focus_settings rows, not a default"
        );
    }

    /// items.id (this fix): the seed bug that shipped unnoticed -- every
    /// persona after the first-created one got zero focus_settings rows,
    /// because shared_001.sql's original seed only ever ran once, against
    /// whatever personas existed at migration time. Guards
    /// persona_store::create_persona's explicit per-persona provisioning
    /// (SEEDED_FOCUS_IDS) by asserting a SECOND persona -- not the first --
    /// gets working focus_settings rows for all three affected Focuses
    /// immediately on creation.
    #[tokio::test]
    async fn create_persona_seeds_focus_settings_for_a_second_persona() {
        let _env = setup().await;
        const SECOND_PERSONA_ID: &str = "persona-persona-test-second";

        persona_store::create_persona(
            &_env.pool,
            PERSONA_ID,
            "First Persona",
            "personal",
            USER_ID,
            None,
        )
        .await
        .expect("create_persona must succeed for the first persona");

        persona_store::create_persona(
            &_env.pool,
            SECOND_PERSONA_ID,
            "Second Persona",
            "personal",
            USER_ID,
            None,
        )
        .await
        .expect("create_persona must succeed for the second persona");

        for focus_id in persona_store::SEEDED_FOCUS_IDS {
            let settings =
                focus_settings_store::get_focus_settings(&_env.pool, SECOND_PERSONA_ID, focus_id)
                    .await
                    .expect("get_focus_settings must succeed")
                    .unwrap_or_else(|| {
                        panic!(
                            "second persona must have a focus_settings row for '{focus_id}' -- \
                             this is exactly the gap where only the first-created persona was \
                             seeded"
                        )
                    });

            assert_eq!(settings.context_flow, "bidirectional");
            assert_eq!(settings.library_visibility, "shared");
            assert_eq!(settings.privacy_tier, 2);
            assert_eq!(
                settings.max_permitted_tier,
                ExternalAccess::AnonymousRequired
            );
            assert_eq!(settings.focus_profile, "open");
        }
    }

    #[tokio::test]
    async fn get_focus_settings_last_used_is_none_before_any_run() {
        let _env = setup().await;
        persona_store::create_persona(
            &_env.pool,
            PERSONA_ID,
            "Last Used Persona",
            "personal",
            USER_ID,
            None,
        )
        .await
        .expect("create_persona must succeed");
        // create_persona seeds a "quick-ask" focus_settings row automatically
        // (persona_store::SEEDED_FOCUS_IDS) -- no separate create_focus_settings
        // call needed here.

        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, USER_ID, MASTER_KEY).await;
        let pool = app.state::<sqlx::SqlitePool>();

        let info = get_focus_settings(
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            registry,
            "quick-ask".to_owned(),
            pool,
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
        persona_store::create_persona(
            &_env.pool,
            PERSONA_ID,
            "Last Used Persona 2",
            "personal",
            USER_ID,
            None,
        )
        .await
        .expect("create_persona must succeed");
        // create_persona seeds a "quick-ask" focus_settings row automatically
        // (persona_store::SEEDED_FOCUS_IDS) -- no separate create_focus_settings
        // call needed here.
        output_store::test_seed_focus_run(
            USER_ID,
            PERSONA_ID,
            &key_hex(&MASTER_KEY),
            "run-1",
            "quick-ask",
        )
        .await
        .expect("test_seed_focus_run must succeed");

        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, USER_ID, MASTER_KEY).await;
        let pool = app.state::<sqlx::SqlitePool>();

        let info = get_focus_settings(
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            registry,
            "quick-ask".to_owned(),
            pool,
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
        persona_store::create_persona(
            &_env.pool,
            PERSONA_ID,
            "Batch Persona",
            "personal",
            USER_ID,
            None,
        )
        .await
        .expect("create_persona must succeed");
        // create_persona seeds a "quick-ask" focus_settings row automatically
        // (persona_store::SEEDED_FOCUS_IDS) -- no separate create_focus_settings
        // call needed here.
        output_store::test_seed_focus_run(
            USER_ID,
            PERSONA_ID,
            &key_hex(&MASTER_KEY),
            "run-1",
            "quick-ask",
        )
        .await
        .expect("test_seed_focus_run must succeed");

        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, USER_ID, MASTER_KEY).await;
        let pool = app.state::<sqlx::SqlitePool>();

        let focuses = list_focuses(USER_ID.to_owned(), PERSONA_ID.to_owned(), registry, pool)
            .await
            .expect("list_focuses must succeed");

        assert_eq!(
            focuses.len(),
            persona_store::SEEDED_FOCUS_IDS.len(),
            "create_persona seeds one focus_settings row per SEEDED_FOCUS_IDS entry"
        );
        let quick_ask = focuses
            .iter()
            .find(|f| f.focus_id == "quick-ask")
            .expect("quick-ask must be among the seeded Focuses");
        assert!(
            quick_ask.last_used.is_some(),
            "list_focuses' batched last_used map must surface the same real value \
             get_focus_settings' single-focus lookup does"
        );
    }

    #[tokio::test]
    async fn update_focus_settings_max_permitted_tier_loosen_trips_gate() {
        let _env = setup().await;
        persona_store::create_persona(
            &_env.pool,
            PERSONA_ID,
            "Ceiling Gate Persona",
            "personal",
            USER_ID,
            None,
        )
        .await
        .expect("create_persona must succeed");
        // create_persona seeds a "quick-ask" focus_settings row automatically
        // (persona_store::SEEDED_FOCUS_IDS) -- no separate create_focus_settings
        // call needed here.

        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, USER_ID, MASTER_KEY).await;
        let pool = app.state::<sqlx::SqlitePool>();

        let err = update_focus_settings(
            USER_ID.to_owned(),
            registry,
            UpdateFocusSettingsRequest {
                persona_id: PERSONA_ID.to_owned(),
                focus_id: "quick-ask".to_owned(),
                context_flow: None,
                library_visibility: None,
                privacy_tier: None,
                max_permitted_tier: Some(ExternalAccess::Unrestricted),
                focus_profile: None,
            },
            pool,
        )
        .await
        .expect_err("raising max_permitted_tier alone must trip the friction gate");

        let detail: FrictionGateDetail =
            serde_json::from_str(&err).expect("gate error must be FrictionGateDetail JSON");

        assert!(detail.max_permitted_tier_would_loosen);
        assert_eq!(
            detail.requested_max_permitted_tier,
            Some(ExternalAccess::Unrestricted)
        );
        assert_eq!(
            detail.existing_max_permitted_tier,
            ExternalAccess::AnonymousRequired
        );
        assert!(
            !detail.privacy_would_loosen && !detail.moves_to_protected,
            "only max_permitted_tier changed -- the other two flags must stay false"
        );
    }

    // -----------------------------------------------------------------------
    // PrivacyPreference (items.id=533)
    // -----------------------------------------------------------------------

    /// UpdateFocusSettingsRequest derives Deserialize only (frontend -> backend,
    /// never serialized in production code), so this is not a bidirectional
    /// round trip -- it locks the wire shape the frontend is expected to send
    /// and asserts it deserializes into the right PrivacyPreference variant.
    /// This is what actually catches a frontend/backend spelling mismatch
    /// (e.g. frictionGateDetail.ts or FocusSettingsControls.tsx ever sending
    /// "Green" or "privacy-green" instead of "green").
    #[test]
    fn update_focus_settings_request_json_shape() {
        let json = r#"{
            "persona_id": "p1",
            "focus_id": "f1",
            "context_flow": null,
            "library_visibility": null,
            "privacy_tier": "green",
            "max_permitted_tier": null,
            "focus_profile": null
        }"#;
        let req: UpdateFocusSettingsRequest =
            serde_json::from_str(json).expect("valid privacy_tier spelling must deserialize");
        assert_eq!(req.privacy_tier, Some(PrivacyPreference::Green));

        let json_null = r#"{
            "persona_id": "p1",
            "focus_id": "f1",
            "context_flow": null,
            "library_visibility": null,
            "privacy_tier": null,
            "max_permitted_tier": null,
            "focus_profile": null
        }"#;
        let req_null: UpdateFocusSettingsRequest =
            serde_json::from_str(json_null).expect("null privacy_tier must deserialize to None");
        assert_eq!(req_null.privacy_tier, None);
    }

    /// Negative case: a wire value that isn't one of PrivacyPreference's three
    /// snake_case spellings must fail deserialization, not silently coerce or
    /// panic -- this is the exact "wire drift fails visibly" requirement
    /// items.id=533 exists to satisfy. Covers both an unknown-spelling string
    /// and a bare number (the OLD wire shape, pre-items.id=533).
    #[test]
    fn update_focus_settings_request_rejects_invalid_privacy_tier() {
        let bad_spelling = r#"{
            "persona_id": "p1",
            "focus_id": "f1",
            "context_flow": null,
            "library_visibility": null,
            "privacy_tier": "Green",
            "max_permitted_tier": null,
            "focus_profile": null
        }"#;
        assert!(
            serde_json::from_str::<UpdateFocusSettingsRequest>(bad_spelling).is_err(),
            "PascalCase spelling must not silently deserialize"
        );

        let old_numeric = r#"{
            "persona_id": "p1",
            "focus_id": "f1",
            "context_flow": null,
            "library_visibility": null,
            "privacy_tier": 3,
            "max_permitted_tier": null,
            "focus_profile": null
        }"#;
        assert!(
            serde_json::from_str::<UpdateFocusSettingsRequest>(old_numeric).is_err(),
            "the old pre-items.id=533 bare-number wire shape must be rejected, not silently \
             accepted -- a frontend that hasn't migrated must fail loudly"
        );
    }

    /// FrictionGateDetail derives both Serialize and Deserialize (serialized
    /// on the way out as update_focus_settings' Err(String) payload,
    /// deserialized back in update_focus_settings_max_permitted_tier_loosen_
    /// trips_gate above). This test locks the exact JSON text
    /// frictionGateDetail.ts depends on for its two PrivacyPreference fields,
    /// then confirms the round trip is lossless.
    #[test]
    fn friction_gate_detail_json_round_trip() {
        let detail = FrictionGateDetail {
            persona_id: "p1".to_owned(),
            focus_id: "f1".to_owned(),
            requested_privacy_tier: Some(PrivacyPreference::Green),
            requested_focus_profile: None,
            requested_max_permitted_tier: None,
            existing_privacy_tier: PrivacyPreference::Yellow,
            existing_focus_profile: "open".to_owned(),
            existing_max_permitted_tier: ExternalAccess::AnonymousRequired,
            privacy_would_loosen: true,
            moves_to_protected: false,
            max_permitted_tier_would_loosen: false,
        };

        let json = serde_json::to_string(&detail).expect("FrictionGateDetail must serialize");
        assert!(
            json.contains(r#""requested_privacy_tier":"green""#),
            "wire text must spell the variant \"green\", got: {json}"
        );
        assert!(
            json.contains(r#""existing_privacy_tier":"yellow""#),
            "wire text must spell the variant \"yellow\", got: {json}"
        );

        let round_tripped: FrictionGateDetail =
            serde_json::from_str(&json).expect("serialized FrictionGateDetail must deserialize");
        assert_eq!(round_tripped, detail);
    }

    /// Exhaustive 3x3 matrix, not just the three monotonic Ord assertions
    /// ExternalAccess's own test uses (conductor/tokens.rs) -- this module's
    /// own header already documents one prior tier-direction naming bug
    /// (variable names once said the opposite of what the numeric comparison
    /// actually did); this test locks the actual comparison the friction
    /// gate runs (`requested > existing`) for every pair, not just Ord in
    /// the abstract.
    #[test]
    fn privacy_preference_ordering_matrix() {
        use PrivacyPreference::{Green, Red, Yellow};

        let variants = [Red, Yellow, Green];
        let expected_loosens = [
            // (existing, requested) -> requested > existing
            (Red, Red, false),
            (Red, Yellow, true),
            (Red, Green, true),
            (Yellow, Red, false),
            (Yellow, Yellow, false),
            (Yellow, Green, true),
            (Green, Red, false),
            (Green, Yellow, false),
            (Green, Green, false),
        ];
        assert_eq!(expected_loosens.len(), variants.len() * variants.len());

        for (existing, requested, expect_loosen) in expected_loosens {
            assert_eq!(
                requested > existing,
                expect_loosen,
                "requested={requested:?} existing={existing:?}: expected loosen={expect_loosen}"
            );
            // Ordinal cross-check: PrivacyPreference's Ord must agree with
            // its own as_i32() mapping, the invariant the whole enum exists
            // to preserve alongside the DB's numeric storage.
            assert_eq!(
                requested.as_i32() > existing.as_i32(),
                expect_loosen,
                "as_i32() comparison disagreed with derived Ord for \
                 requested={requested:?} existing={existing:?}"
            );
        }
    }
}
