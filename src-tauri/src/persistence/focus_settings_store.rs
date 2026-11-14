// src-tauri/src/persistence/focus_settings_store.rs
//
// Focus-level settings CRUD for shared.db (unencrypted).
// focus_settings lives in shared.db so Privacy Guardian can read Focus
// settings before opening encrypted per-user stores. Settings are
// behavioral configuration, not personal data.
//
// PK is (persona_id, focus_id) — full PK required for all reads.
// lifecycle.rs calls get_focus_settings(persona_id, focus_id) at AUTHORIZE.
// Conductor asserts non-None at AUTHORIZE — missing row is a hard error.
//
// Three independent Focus settings per D6-291:
//   context_flow:       bidirectional | receive_only | isolated
//   library_visibility: shared | persona_visible | persona_hidden
//   privacy_tier:       1 (red) | 2 (yellow) | 3 (green)
// max_permitted_tier: hard Tier ceiling for this Focus (D6-297).
// focus_profile:      convenience label for the three settings (D6-294).
// voice_override:     Focus-level voice JSON or None (D6-302).
//
// COLLECTION CONSENT (items.id=920, decisions.id=944, shared_044.sql):
// collection_consent / collection_consent_version are the Persona's state on a
// Focus's silent collection. They are deliberately NOT part of
// update_focus_settings() or create_focus_settings(): the ONLY writer is
// set_collection_consent(), called by stage 2's command and by nothing else, so
// neither a model, the settings command nor the friction gate can move them.
//
// QUERY STYLE: runtime sqlx::query() only — no query!() macros.
// shared.db is unencrypted — no PRAGMA key required.
//
// CONNECTION MODEL: pooled (items.id=483) -- callers pass a &sqlx::SqlitePool
// in; every fn here does pool.acquire().

use sqlx::Row;
use thiserror::Error;

use crate::conductor::tokens::ExternalAccess;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const VALID_CONTEXT_FLOWS: &[&str] = &["bidirectional", "receive_only", "isolated"];
const VALID_LIBRARY_VISIBILITY: &[&str] = &["shared", "persona_visible", "persona_hidden"];
const VALID_FOCUS_PROFILES: &[&str] = &["open", "organized", "protected"];
const TIER_MIN: i32 = 1;
const TIER_MAX: i32 = 3;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum FocusSettingsStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Not found: focus_settings for persona='{persona_id}' focus='{focus_id}'")]
    NotFound {
        persona_id: String,
        focus_id: String,
    },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Collection consent (items.id=920)
// ---------------------------------------------------------------------------

/// Where a Persona stands on one Focus's silent collection. Stored as the
/// CHECK-constrained text of focus_settings.collection_consent (shared_044.sql).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum CollectionConsent {
    NotAsked,
    Granted,
    Declined,
    Withdrawn,
}

impl CollectionConsent {
    pub fn as_str(self) -> &'static str {
        match self {
            CollectionConsent::NotAsked => "not_asked",
            CollectionConsent::Granted => "granted",
            CollectionConsent::Declined => "declined",
            CollectionConsent::Withdrawn => "withdrawn",
        }
    }

    pub fn parse(s: &str) -> Option<CollectionConsent> {
        match s {
            "not_asked" => Some(CollectionConsent::NotAsked),
            "granted" => Some(CollectionConsent::Granted),
            "declined" => Some(CollectionConsent::Declined),
            "withdrawn" => Some(CollectionConsent::Withdrawn),
            _ => None,
        }
    }
}

/// The consent state and the text version it was given for. The current text
/// version is data (conductor::collection_consent_text, block_data/
/// collection_consent_text.json).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectionConsentState {
    pub consent: CollectionConsent,
    pub version: i64,
}

/// The version of the collection-consent text a Persona must have agreed to. It
/// is data, not a constant (items.id=920 stage 2): see
/// conductor::collection_consent_text.
pub fn current_text_version() -> i64 {
    crate::conductor::collection_consent_text::current_version()
}

impl CollectionConsentState {
    /// True only for a grant of the CURRENT text. Anything else, an old
    /// version included, is not a grant.
    pub fn is_current_grant(&self) -> bool {
        self.consent == CollectionConsent::Granted && self.version == current_text_version()
    }
}

// ---------------------------------------------------------------------------
// Data type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FocusSettings {
    pub persona_id: String,
    pub focus_id: String,
    pub context_flow: String,
    pub library_visibility: String,
    pub privacy_tier: i32,
    pub max_permitted_tier: ExternalAccess,
    pub focus_profile: String,
    /// None if no voice override is set for this Focus.
    pub voice_override: Option<serde_json::Value>,
    /// items.id=920: read-only here. Only set_collection_consent() writes it.
    pub collection_consent: CollectionConsent,
    pub collection_consent_version: i64,
    pub created_at: String,
    pub updated_at: String,
}

// ---------------------------------------------------------------------------
// Row extraction
// ---------------------------------------------------------------------------

fn row_to_focus_settings(row: &sqlx::sqlite::SqliteRow) -> Result<FocusSettings, sqlx::Error> {
    let persona_id: String = row.try_get("persona_id")?;
    let focus_id: String = row.try_get("focus_id")?;
    let voice_raw: Option<String> = row.try_get("voice_override")?;
    let voice_override: Option<serde_json::Value> = match voice_raw {
        None => None,
        Some(s) => match serde_json::from_str(&s) {
            Ok(v) => Some(v),
            Err(e) => {
                log::warn!(
                    "focus_settings voice_override failed to parse as JSON for \
                     persona='{persona_id}' focus='{focus_id}', defaulting to None: {e}"
                );
                None
            }
        },
    };

    Ok(FocusSettings {
        persona_id,
        focus_id,
        context_flow: row.try_get("context_flow")?,
        library_visibility: row.try_get("library_visibility")?,
        privacy_tier: row.try_get::<i64, _>("privacy_tier")? as i32,
        // items.id=529: schema CHECK (max_permitted_tier IN (...),
        // shared_020.sql) guarantees this is always one of the 4 known
        // strings, so parse() failing here means that check was bypassed --
        // a bug to surface as a decode error, not silently misroute.
        max_permitted_tier: row
            .try_get::<String, _>("max_permitted_tier")?
            .parse::<ExternalAccess>()
            .map_err(|e: String| sqlx::Error::Decode(e.into()))?,
        focus_profile: row.try_get("focus_profile")?,
        voice_override,
        // shared_044.sql's CHECK guarantees a known string, so a failed parse
        // means the check was bypassed: a decode error, never a default that
        // could read as a grant.
        collection_consent: {
            let raw: String = row.try_get("collection_consent")?;
            CollectionConsent::parse(&raw).ok_or_else(|| {
                sqlx::Error::Decode(format!("unknown collection_consent '{raw}'").into())
            })?
        },
        collection_consent_version: row.try_get("collection_consent_version")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

// max_permitted_tier has no range check here -- items.id=448 retyped it to
// ExternalAccess, so an invalid value is unrepresentable (same reasoning
// already applied to FailureHandler::new(), conductor/failure.rs).
fn validate_settings(
    context_flow: &str,
    library_visibility: &str,
    privacy_tier: i32,
    focus_profile: &str,
) -> Result<(), FocusSettingsStoreError> {
    if !VALID_CONTEXT_FLOWS.contains(&context_flow) {
        return Err(FocusSettingsStoreError::Validation(format!(
            "context_flow must be one of {:?}, got '{context_flow}'.",
            VALID_CONTEXT_FLOWS
        )));
    }
    if !VALID_LIBRARY_VISIBILITY.contains(&library_visibility) {
        return Err(FocusSettingsStoreError::Validation(format!(
            "library_visibility must be one of {:?}, got '{library_visibility}'.",
            VALID_LIBRARY_VISIBILITY
        )));
    }
    if !(TIER_MIN..=TIER_MAX).contains(&privacy_tier) {
        return Err(FocusSettingsStoreError::Validation(format!(
            "privacy_tier must be between {TIER_MIN} and {TIER_MAX}, \
             got {privacy_tier}."
        )));
    }
    if !VALID_FOCUS_PROFILES.contains(&focus_profile) {
        return Err(FocusSettingsStoreError::Validation(format!(
            "focus_profile must be one of {:?}, got '{focus_profile}'.",
            VALID_FOCUS_PROFILES
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Read operations
// ---------------------------------------------------------------------------

/// Fetch focus settings by full PK (persona_id, focus_id).
/// Returns None if not found.
/// Primary read path — called by lifecycle AUTHORIZE and tier ceiling check.
/// Conductor asserts non-None at AUTHORIZE — missing row is a hard error.
pub async fn get_focus_settings(
    pool: &sqlx::SqlitePool,
    persona_id: &str,
    focus_id: &str,
) -> Result<Option<FocusSettings>, FocusSettingsStoreError> {
    let mut conn = pool.acquire().await?;

    let row = sqlx::query(
        "SELECT persona_id, focus_id, context_flow, library_visibility,
         privacy_tier, max_permitted_tier, focus_profile, voice_override,
         collection_consent, collection_consent_version,
         created_at, updated_at
         FROM focus_settings WHERE persona_id = ? AND focus_id = ?",
    )
    .bind(persona_id)
    .bind(focus_id)
    .fetch_optional(&mut *conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(
            row_to_focus_settings(&r).map_err(FocusSettingsStoreError::Database)?,
        )),
    }
}

/// Return all focus settings rows for a persona, ordered by focus_id.
pub async fn list_focus_settings_for_persona(
    pool: &sqlx::SqlitePool,
    persona_id: &str,
) -> Result<Vec<FocusSettings>, FocusSettingsStoreError> {
    let mut conn = pool.acquire().await?;

    let rows = sqlx::query(
        "SELECT persona_id, focus_id, context_flow, library_visibility,
         privacy_tier, max_permitted_tier, focus_profile, voice_override,
         collection_consent, collection_consent_version,
         created_at, updated_at
         FROM focus_settings WHERE persona_id = ?
         ORDER BY focus_id",
    )
    .bind(persona_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut result = Vec::new();
    for r in rows {
        result.push(row_to_focus_settings(&r).map_err(FocusSettingsStoreError::Database)?);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Write operations
// ---------------------------------------------------------------------------

/// Create a focus_settings row for the given persona + focus.
/// Returns Err(Validation) on invalid field values.
/// Duplicate PK propagates as Database error — duplicate is an application
/// logic error (AUTHORIZE assertion catches missing rows first).
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn create_focus_settings(
    pool: &sqlx::SqlitePool,
    persona_id: &str,
    focus_id: &str,
    context_flow: &str,
    library_visibility: &str,
    privacy_tier: i32,
    max_permitted_tier: ExternalAccess,
    focus_profile: &str,
    voice_override: Option<serde_json::Value>,
) -> Result<FocusSettings, FocusSettingsStoreError> {
    validate_settings(
        context_flow,
        library_visibility,
        privacy_tier,
        focus_profile,
    )?;

    let created_at = crate::providers::utils::now();
    let voice_json: Option<String> = voice_override
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".to_owned()));

    let mut conn = pool.acquire().await?;

    sqlx::query(
        "INSERT INTO focus_settings
         (persona_id, focus_id, context_flow, library_visibility,
          privacy_tier, max_permitted_tier, focus_profile, voice_override,
          created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(persona_id)
    .bind(focus_id)
    .bind(context_flow)
    .bind(library_visibility)
    .bind(privacy_tier)
    .bind(max_permitted_tier.as_str())
    .bind(focus_profile)
    .bind(&voice_json)
    .bind(&created_at)
    .bind(&created_at)
    .execute(&mut *conn)
    .await?;

    Ok(FocusSettings {
        persona_id: persona_id.to_owned(),
        focus_id: focus_id.to_owned(),
        context_flow: context_flow.to_owned(),
        library_visibility: library_visibility.to_owned(),
        privacy_tier,
        max_permitted_tier,
        focus_profile: focus_profile.to_owned(),
        voice_override,
        // A new row is always not_asked (the column default).
        collection_consent: CollectionConsent::NotAsked,
        collection_consent_version: 0,
        created_at: created_at.clone(),
        updated_at: created_at,
    })
}

/// Create a focus_settings row for `(persona_id, focus_id)` only if none
/// exists, with the Open-profile defaults every seeded Focus gets (bidirectional,
/// shared, privacy_tier 2) and the given ceiling. Returns true when it created
/// the row, false when one was already there. An existing row is NEVER touched:
/// a ceiling the user raised through the friction gate survives every later
/// call. Idempotent, so it can run at bind time and again at every send
/// (items.id=716, the lesson of items.id=318: a Persona with no row hits a hard
/// AUTHORIZE failure). The Persona row must exist (foreign key).
pub async fn ensure_focus_settings_default(
    pool: &sqlx::SqlitePool,
    persona_id: &str,
    focus_id: &str,
    max_permitted_tier: ExternalAccess,
) -> Result<bool, FocusSettingsStoreError> {
    let created_at = crate::providers::utils::now();
    let mut conn = pool.acquire().await?;

    let done = sqlx::query(
        "INSERT OR IGNORE INTO focus_settings
         (persona_id, focus_id, context_flow, library_visibility,
          privacy_tier, max_permitted_tier, focus_profile, voice_override,
          created_at, updated_at)
         VALUES (?, ?, 'bidirectional', 'shared', 2, ?, 'open', NULL, ?, ?)",
    )
    .bind(persona_id)
    .bind(focus_id)
    .bind(max_permitted_tier.as_str())
    .bind(&created_at)
    .bind(&created_at)
    .execute(&mut *conn)
    .await?;

    Ok(done.rows_affected() > 0)
}

/// Update one or more fields on an existing focus_settings row.
/// Only provided (non-None) fields are updated.
///
/// voice_override uses Option<Option<serde_json::Value>> to distinguish:
///   None              = no change (leave existing voice_override intact)
///   Some(None)        = clear the voice override (SQL NULL)
///   Some(Some(value)) = set to new JSON value
///
/// Some(None) → new_voice_json = None → binds as SQL NULL.
/// The NULL vs JSON distinction is preserved at the DB boundary.
///
/// Returns Err(NotFound) if row not found.
/// Returns Err(Validation) on invalid field values.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn update_focus_settings(
    pool: &sqlx::SqlitePool,
    persona_id: &str,
    focus_id: &str,
    context_flow: Option<&str>,
    library_visibility: Option<&str>,
    privacy_tier: Option<i32>,
    max_permitted_tier: Option<ExternalAccess>,
    focus_profile: Option<&str>,
    voice_override: Option<Option<serde_json::Value>>,
) -> Result<FocusSettings, FocusSettingsStoreError> {
    let existing = get_focus_settings(pool, persona_id, focus_id)
        .await?
        .ok_or_else(|| FocusSettingsStoreError::NotFound {
            persona_id: persona_id.to_owned(),
            focus_id: focus_id.to_owned(),
        })?;

    let new_flow = context_flow.unwrap_or(&existing.context_flow);
    let new_vis = library_visibility.unwrap_or(&existing.library_visibility);
    let new_ptier = privacy_tier.unwrap_or(existing.privacy_tier);
    let new_mtier = max_permitted_tier.unwrap_or(existing.max_permitted_tier);
    let new_profile = focus_profile.unwrap_or(&existing.focus_profile);

    validate_settings(new_flow, new_vis, new_ptier, new_profile)?;

    // voice_override tri-state: None = no change, Some(None) = clear, Some(Some(v)) = set.
    // Some(None) produces new_voice_json = None, which binds as SQL NULL — correct.
    let new_voice: Option<serde_json::Value> = match voice_override {
        None => existing.voice_override.clone(),
        Some(v) => v,
    };
    let new_voice_json: Option<String> = new_voice
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".to_owned()));

    let updated_at = crate::providers::utils::now();

    let mut conn = pool.acquire().await?;

    sqlx::query(
        "UPDATE focus_settings SET
         context_flow = ?, library_visibility = ?, privacy_tier = ?,
         max_permitted_tier = ?, focus_profile = ?, voice_override = ?,
         updated_at = ?
         WHERE persona_id = ? AND focus_id = ?",
    )
    .bind(new_flow)
    .bind(new_vis)
    .bind(new_ptier)
    .bind(new_mtier.as_str())
    .bind(new_profile)
    .bind(&new_voice_json)
    .bind(&updated_at)
    .bind(persona_id)
    .bind(focus_id)
    .execute(&mut *conn)
    .await?;

    Ok(FocusSettings {
        persona_id: persona_id.to_owned(),
        focus_id: focus_id.to_owned(),
        context_flow: new_flow.to_owned(),
        library_visibility: new_vis.to_owned(),
        privacy_tier: new_ptier,
        max_permitted_tier: new_mtier,
        focus_profile: new_profile.to_owned(),
        voice_override: new_voice,
        // Untouched: update_focus_settings never writes the consent columns.
        collection_consent: existing.collection_consent,
        collection_consent_version: existing.collection_consent_version,
        created_at: existing.created_at,
        updated_at,
    })
}

// ---------------------------------------------------------------------------
// Collection consent (items.id=920)
// ---------------------------------------------------------------------------

/// The Persona's collection-consent state for one Focus, read fresh from the
/// pool. Ok(None) when the Focus has no focus_settings row. The writer gate
/// (conductor::lifecycle) calls this immediately before EACH store write, so a
/// withdrawal takes effect on the very next op.
pub async fn get_collection_consent(
    pool: &sqlx::SqlitePool,
    persona_id: &str,
    focus_id: &str,
) -> Result<Option<CollectionConsentState>, FocusSettingsStoreError> {
    let mut conn = pool.acquire().await?;
    let row = sqlx::query(
        "SELECT collection_consent, collection_consent_version
         FROM focus_settings WHERE persona_id = ? AND focus_id = ?",
    )
    .bind(persona_id)
    .bind(focus_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let raw: String = row.try_get("collection_consent")?;
    let consent = CollectionConsent::parse(&raw).ok_or_else(|| {
        FocusSettingsStoreError::Validation(format!("unknown collection_consent '{raw}'"))
    })?;
    Ok(Some(CollectionConsentState {
        consent,
        version: row.try_get("collection_consent_version")?,
    }))
}

/// THE ONLY WRITER of focus_settings.collection_consent and
/// collection_consent_version (items.id=920). The user's own confirm (stage 2's
/// commands) is its only caller; nothing in the conductor, no model path,
/// update_focus_settings and submit_friction_gate_decision never reach it.
/// `version` is the consent text version the user was shown. It is stored for
/// Granted AND Declined (both must name a version of 1 or more, so a changed text
/// asks again) and stored as 0 for NotAsked and Withdrawn. The gate is unchanged:
/// it opens only for Granted at the current text version. Returns Err(NotFound)
/// when the row does not exist.
pub async fn set_collection_consent(
    pool: &sqlx::SqlitePool,
    persona_id: &str,
    focus_id: &str,
    consent: CollectionConsent,
    version: i64,
) -> Result<(), FocusSettingsStoreError> {
    let keeps_version = matches!(
        consent,
        CollectionConsent::Granted | CollectionConsent::Declined
    );
    if keeps_version && version < 1 {
        return Err(FocusSettingsStoreError::Validation(
            "a grant or a decline must name the consent text version shown, 1 or more".to_owned(),
        ));
    }
    let stored_version = if keeps_version { version } else { 0 };
    let updated_at = crate::providers::utils::now();
    let mut conn = pool.acquire().await?;
    let done = sqlx::query(
        "UPDATE focus_settings
         SET collection_consent = ?, collection_consent_version = ?, updated_at = ?
         WHERE persona_id = ? AND focus_id = ?",
    )
    .bind(consent.as_str())
    .bind(stored_version)
    .bind(&updated_at)
    .bind(persona_id)
    .bind(focus_id)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() == 0 {
        return Err(FocusSettingsStoreError::NotFound {
            persona_id: persona_id.to_owned(),
            focus_id: focus_id.to_owned(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Friction gate decisions (items.id=92)
// ---------------------------------------------------------------------------

/// Record a user's decision to proceed with (or cancel) a privacy-loosening
/// Focus settings change that tripped the friction gate.
///
/// Lives in focus_settings_friction_decisions (shared_002.sql), a sibling
/// table to focus_settings rather than a row in outputs.db's
/// consent_decisions -- see shared_002.sql's header comment for the full
/// reasoning (consent_decisions.focus_run_id is NOT NULL, and this decision
/// has no FocusRun to anchor to).
///
/// requested_privacy_tier / requested_focus_profile: pass None for whichever
/// dimension did not trip the gate -- mirrors update_focus_settings itself
/// treating every field as independently optional. At least one must be
/// Some, enforced by the table's own CHECK constraint (a request with
/// neither would mean the gate fired for no reason).
///
/// existing_privacy_tier / existing_focus_profile: the settings row's values
/// at the moment the gate fired, for audit purposes (see shared_002.sql).
/// Caller supplies these rather than this function re-reading
/// focus_settings, since the caller (update_focus_settings) already has
/// them in hand from its own gate check and a second read here could race
/// against a concurrent write.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn record_friction_gate_decision(
    pool: &sqlx::SqlitePool,
    persona_id: &str,
    focus_id: &str,
    decision: &str,
    requested_privacy_tier: Option<i32>,
    requested_focus_profile: Option<&str>,
    requested_max_permitted_tier: Option<ExternalAccess>,
    existing_privacy_tier: i32,
    existing_focus_profile: &str,
    existing_max_permitted_tier: Option<ExternalAccess>,
) -> Result<String, FocusSettingsStoreError> {
    if !matches!(decision, "proceed" | "cancel") {
        return Err(FocusSettingsStoreError::Validation(format!(
            "friction gate decision must be 'proceed' or 'cancel', got '{decision}'."
        )));
    }
    if requested_privacy_tier.is_none()
        && requested_focus_profile.is_none()
        && requested_max_permitted_tier.is_none()
    {
        return Err(FocusSettingsStoreError::Validation(
            "friction gate decision requires at least one of \
             requested_privacy_tier / requested_focus_profile / \
             requested_max_permitted_tier -- a gate trip with none of them \
             means nothing actually triggered it."
                .to_owned(),
        ));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let created_at = crate::providers::utils::now();
    let mut conn = pool.acquire().await?;

    sqlx::query(
        "INSERT INTO focus_settings_friction_decisions
         (id, persona_id, focus_id, decision, requested_privacy_tier,
          requested_focus_profile, requested_max_permitted_tier,
          existing_privacy_tier, existing_focus_profile, existing_max_permitted_tier,
          created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(persona_id)
    .bind(focus_id)
    .bind(decision)
    .bind(requested_privacy_tier)
    .bind(requested_focus_profile)
    .bind(requested_max_permitted_tier.map(|t| t.as_str()))
    .bind(existing_privacy_tier)
    .bind(existing_focus_profile)
    .bind(existing_max_permitted_tier.map(|t| t.as_str()))
    .bind(&created_at)
    .execute(&mut *conn)
    .await?;

    Ok(id)
}

// ---------------------------------------------------------------------------
// Tests (items.id=920: collection consent)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{insert_persona_anchor, ENV_MUTEX};

    const PERSONA: &str = "p-consent";
    const FOCUS: &str = "tech-support";

    struct Env {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved: Option<String>,
        _dir: tempfile::TempDir,
        pool: sqlx::SqlitePool,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            match &self.saved {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    /// A real shared.db (all migrations, so shared_044 is in) with one Persona
    /// anchor and one focus_settings row.
    async fn env() -> Env {
        let lock = ENV_MUTEX.lock().await;
        let saved = std::env::var("QR_DATA_ROOT").ok();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", dir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration");
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool");
        insert_persona_anchor(&pool, PERSONA).await;
        create_focus_settings(
            &pool,
            PERSONA,
            FOCUS,
            "bidirectional",
            "shared",
            2,
            ExternalAccess::LocalOnly,
            "open",
            None,
        )
        .await
        .expect("focus_settings row");
        Env {
            _lock: lock,
            saved,
            _dir: dir,
            pool,
        }
    }

    #[test]
    fn collection_consent_strings_round_trip() {
        for c in [
            CollectionConsent::NotAsked,
            CollectionConsent::Granted,
            CollectionConsent::Declined,
            CollectionConsent::Withdrawn,
        ] {
            assert_eq!(CollectionConsent::parse(c.as_str()), Some(c));
        }
        assert_eq!(CollectionConsent::parse("yes"), None);
    }

    #[tokio::test]
    async fn a_new_row_is_not_asked_and_never_a_grant() {
        let e = env().await;
        let state = get_collection_consent(&e.pool, PERSONA, FOCUS)
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(state.consent, CollectionConsent::NotAsked);
        assert_eq!(state.version, 0);
        assert!(!state.is_current_grant());
        let full = get_focus_settings(&e.pool, PERSONA, FOCUS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(full.collection_consent, CollectionConsent::NotAsked);
        assert_eq!(full.collection_consent_version, 0);
        // A row that does not exist is None, which the gate reads as not granted.
        assert!(get_collection_consent(&e.pool, PERSONA, "no-such-focus")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn only_a_grant_of_the_current_text_version_is_a_current_grant() {
        let e = env().await;
        set_collection_consent(
            &e.pool,
            PERSONA,
            FOCUS,
            CollectionConsent::Granted,
            crate::conductor::collection_consent_text::current_version(),
        )
        .await
        .unwrap();
        let s = get_collection_consent(&e.pool, PERSONA, FOCUS)
            .await
            .unwrap()
            .unwrap();
        assert!(s.is_current_grant());

        // A grant of an older text is not current.
        set_collection_consent(&e.pool, PERSONA, FOCUS, CollectionConsent::Granted, 99)
            .await
            .unwrap();
        let s = get_collection_consent(&e.pool, PERSONA, FOCUS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.consent, CollectionConsent::Granted);
        assert_eq!(s.version, 99);
        assert!(!s.is_current_grant());

        // A decline stores the version shown, and is never a grant.
        set_collection_consent(&e.pool, PERSONA, FOCUS, CollectionConsent::Declined, 1)
            .await
            .unwrap();
        let s = get_collection_consent(&e.pool, PERSONA, FOCUS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((s.consent, s.version), (CollectionConsent::Declined, 1));
        assert!(!s.is_current_grant());

        // Withdrawn and NotAsked store 0 whatever version is passed.
        for c in [CollectionConsent::Withdrawn, CollectionConsent::NotAsked] {
            set_collection_consent(&e.pool, PERSONA, FOCUS, c, 1)
                .await
                .unwrap();
            let s = get_collection_consent(&e.pool, PERSONA, FOCUS)
                .await
                .unwrap()
                .unwrap();
            assert_eq!((s.consent, s.version), (c, 0));
            assert!(!s.is_current_grant());
        }
    }

    #[tokio::test]
    async fn the_setter_validates_and_reports_a_missing_row() {
        let e = env().await;
        for c in [CollectionConsent::Granted, CollectionConsent::Declined] {
            for bad in [0, -1] {
                assert!(
                    matches!(
                        set_collection_consent(&e.pool, PERSONA, FOCUS, c, bad).await,
                        Err(FocusSettingsStoreError::Validation(_))
                    ),
                    "{c:?} v{bad}"
                );
            }
        }
        assert!(matches!(
            set_collection_consent(
                &e.pool,
                PERSONA,
                "no-such-focus",
                CollectionConsent::Granted,
                1
            )
            .await,
            Err(FocusSettingsStoreError::NotFound { .. })
        ));
        let s = get_collection_consent(&e.pool, PERSONA, FOCUS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            s.consent,
            CollectionConsent::NotAsked,
            "nothing was written"
        );
    }

    /// update_focus_settings writes the other columns and never the consent
    /// ones, whatever it is asked to change.
    #[tokio::test]
    async fn update_focus_settings_never_moves_the_consent_columns() {
        let e = env().await;
        set_collection_consent(&e.pool, PERSONA, FOCUS, CollectionConsent::Granted, 1)
            .await
            .unwrap();
        let updated = update_focus_settings(
            &e.pool,
            PERSONA,
            FOCUS,
            Some("isolated"),
            Some("persona_hidden"),
            Some(3),
            Some(ExternalAccess::Unrestricted),
            Some("protected"),
            Some(Some(serde_json::json!({"tone": "x"}))),
        )
        .await
        .unwrap();
        assert_eq!(updated.collection_consent, CollectionConsent::Granted);
        assert_eq!(updated.collection_consent_version, 1);
        let s = get_collection_consent(&e.pool, PERSONA, FOCUS)
            .await
            .unwrap()
            .unwrap();
        assert!(s.is_current_grant(), "the stored state is untouched too");

        // And a declined state stays declined through the same call.
        set_collection_consent(&e.pool, PERSONA, FOCUS, CollectionConsent::Declined, 1)
            .await
            .unwrap();
        update_focus_settings(
            &e.pool,
            PERSONA,
            FOCUS,
            None,
            None,
            Some(1),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let s = get_collection_consent(&e.pool, PERSONA, FOCUS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.consent, CollectionConsent::Declined);
    }

    /// The only SQL in this module that names the consent columns in a write is
    /// set_collection_consent's UPDATE. update_focus_settings, create and the
    /// friction-gate writer never do, and the IPC request struct has no such field.
    #[test]
    fn only_set_collection_consent_writes_the_consent_columns() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let store = std::fs::read_to_string(
            std::path::Path::new(manifest).join("src/persistence/focus_settings_store.rs"),
        )
        .unwrap();
        let production = store.split("#[cfg(test)]").next().unwrap();
        let writes: Vec<&str> = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| {
                let l = l.to_lowercase();
                l.contains("collection_consent =") || l.contains("collection_consent_version =")
            })
            .collect();
        // The one UPDATE assigns both columns, on two lines.
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert!(
            writes[0].contains("SET collection_consent = ?"),
            "{writes:?}"
        );

        let persona =
            std::fs::read_to_string(std::path::Path::new(manifest).join("src/commands/persona.rs"))
                .unwrap();
        let start = persona
            .find("pub struct UpdateFocusSettingsRequest")
            .expect("request struct");
        let body = &persona[start..start + persona[start..].find("\n}\n").unwrap()];
        assert!(
            !body.contains("collection") && !body.contains("consent"),
            "UpdateFocusSettingsRequest must not carry consent: {body}"
        );
    }
}
