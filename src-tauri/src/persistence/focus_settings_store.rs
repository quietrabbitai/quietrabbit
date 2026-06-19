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
// QUERY STYLE: runtime sqlx::query() only — no query!() macros.
// shared.db is unencrypted — no PRAGMA key required.
//
// CONNECTION MODEL: one connection per call (Phase 1 correctness implementation).
// Not the final architecture — shared.db will move to a connection pool
// under the actor model when the full providers layer is ported.

use std::path::PathBuf;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

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
    NotFound { persona_id: String, focus_id: String },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
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
    pub max_permitted_tier: i32,
    pub focus_profile: String,
    /// None if no voice override is set for this Focus.
    pub voice_override: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
}

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

fn get_shared_db_path() -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("instance")
        .join("shared.db")
}

// ---------------------------------------------------------------------------
// DB opener
// ---------------------------------------------------------------------------

/// Open shared.db (unencrypted). No PRAGMA key required.
/// Journal mode set per QR_NETWORK_STORAGE.
async fn open_shared_db() -> Result<SqliteConnection, FocusSettingsStoreError> {
    let db_path = get_shared_db_path();
    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let journal_mode = if network_storage { "DELETE" } else { "WAL" };

    let conn = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .pragma("journal_mode", journal_mode)
        .connect()
        .await?;

    Ok(conn)
}

// ---------------------------------------------------------------------------
// Row extraction
// ---------------------------------------------------------------------------

fn row_to_focus_settings(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<FocusSettings, sqlx::Error> {
    let voice_raw: Option<String> = row.try_get("voice_override")?;
    let voice_override: Option<serde_json::Value> = match voice_raw {
        None => None,
        Some(s) => {
            // TODO: log parse failure for forensic visibility
            serde_json::from_str(&s).ok()
        }
    };

    Ok(FocusSettings {
        persona_id: row.try_get("persona_id")?,
        focus_id: row.try_get("focus_id")?,
        context_flow: row.try_get("context_flow")?,
        library_visibility: row.try_get("library_visibility")?,
        privacy_tier: row.try_get::<i64, _>("privacy_tier")? as i32,
        max_permitted_tier: row.try_get::<i64, _>("max_permitted_tier")? as i32,
        focus_profile: row.try_get("focus_profile")?,
        voice_override,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate_settings(
    context_flow: &str,
    library_visibility: &str,
    privacy_tier: i32,
    max_permitted_tier: i32,
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
    if !(TIER_MIN..=TIER_MAX).contains(&max_permitted_tier) {
        return Err(FocusSettingsStoreError::Validation(format!(
            "max_permitted_tier must be between {TIER_MIN} and {TIER_MAX}, \
             got {max_permitted_tier}."
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
    persona_id: &str,
    focus_id: &str,
) -> Result<Option<FocusSettings>, FocusSettingsStoreError> {
    let mut conn = open_shared_db().await?;

    let row = sqlx::query(
        "SELECT persona_id, focus_id, context_flow, library_visibility,
         privacy_tier, max_permitted_tier, focus_profile, voice_override,
         created_at, updated_at
         FROM focus_settings WHERE persona_id = ? AND focus_id = ?",
    )
    .bind(persona_id)
    .bind(focus_id)
    .fetch_optional(&mut conn)
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
    persona_id: &str,
) -> Result<Vec<FocusSettings>, FocusSettingsStoreError> {
    let mut conn = open_shared_db().await?;

    let rows = sqlx::query(
        "SELECT persona_id, focus_id, context_flow, library_visibility,
         privacy_tier, max_permitted_tier, focus_profile, voice_override,
         created_at, updated_at
         FROM focus_settings WHERE persona_id = ?
         ORDER BY focus_id",
    )
    .bind(persona_id)
    .fetch_all(&mut conn)
    .await?;

    let mut result = Vec::new();
    for r in rows {
        result.push(
            row_to_focus_settings(&r).map_err(FocusSettingsStoreError::Database)?,
        );
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
pub async fn create_focus_settings(
    persona_id: &str,
    focus_id: &str,
    context_flow: &str,
    library_visibility: &str,
    privacy_tier: i32,
    max_permitted_tier: i32,
    focus_profile: &str,
    voice_override: Option<serde_json::Value>,
) -> Result<FocusSettings, FocusSettingsStoreError> {
    validate_settings(
        context_flow,
        library_visibility,
        privacy_tier,
        max_permitted_tier,
        focus_profile,
    )?;

    let created_at = crate::providers::utils::now();
    let voice_json: Option<String> = voice_override
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".to_owned()));

    let mut conn = open_shared_db().await?;

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
    .bind(max_permitted_tier)
    .bind(focus_profile)
    .bind(&voice_json)
    .bind(&created_at)
    .bind(&created_at)
    .execute(&mut conn)
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
        created_at: created_at.clone(),
        updated_at: created_at,
    })
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
pub async fn update_focus_settings(
    persona_id: &str,
    focus_id: &str,
    context_flow: Option<&str>,
    library_visibility: Option<&str>,
    privacy_tier: Option<i32>,
    max_permitted_tier: Option<i32>,
    focus_profile: Option<&str>,
    voice_override: Option<Option<serde_json::Value>>,
) -> Result<FocusSettings, FocusSettingsStoreError> {
    let existing =
        get_focus_settings(persona_id, focus_id)
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

    validate_settings(new_flow, new_vis, new_ptier, new_mtier, new_profile)?;

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

    let mut conn = open_shared_db().await?;

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
    .bind(new_mtier)
    .bind(new_profile)
    .bind(&new_voice_json)
    .bind(&updated_at)
    .bind(persona_id)
    .bind(focus_id)
    .execute(&mut conn)
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
        created_at: existing.created_at,
        updated_at,
    })
}
