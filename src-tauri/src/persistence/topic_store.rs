// src-tauri/src/persistence/topic_store.rs
//
// Topic CRUD, run history, classification preferences, and topic storage
// location registry. All backed by outputs.db (per-user, per-persona, encrypted)
// and shared.db (instance-level, unencrypted) for topic_index mirror writes.
//
// Responsibility boundary:
//   topic_store    -- topic lifecycle state, run history, classification prefs,
//                     storage location registry, topic_index mirror writes
//   lifecycle      -- focus_run state transitions (create, promote, status)
//   domain_context_store -- domain_context.db reads and writes
//   plan_state_store     -- plan_state.db reads and writes
//
// topic_index in shared.db (unencrypted) mirrors topic metadata for the
// dashboard. topic_store writes to both outputs.db and shared.db where possible.
// On conflict, outputs.db is authoritative. Mirror writes are non-fatal.
//
// topic_storage_locations is a DISCOVERY INDEX -- not the authoritative source
// of truth for filesystem state. Boot Check uses it as primary lookup.
// Filesystem existence takes precedence on backup restore or manual recovery.
//
// Path helpers (ensure_focus_dirs, get_domain_context_path, get_plan_state_path)
// are exported as pub from this module as the canonical source.
// plan_state_store.rs and migrations.rs contain private duplicates pending
// the Layer 8+ path unification TODO.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros.
// PRAGMA key applied via SqliteConnectOptions (D6-346).
// Caller supplies bare hex; store wraps it in SQLCipher x'...' syntax.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

fn run_history_retention_days() -> i64 {
    std::env::var("QR_RUN_HISTORY_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(90)
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Topic {
    pub id: String,
    pub focus_id: String,
    pub user_id: String,
    pub persona_id: String,
    pub lifecycle_state: String,
    pub placeholder_name: String,
    pub created_at: String,
    pub updated_at: String,
    pub name: Option<String>,
    pub dormant_since: Option<String>,
    pub closed_at: Option<String>,
    pub extra_metadata: serde_json::Value,
}

impl Topic {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.placeholder_name)
    }
}

#[derive(Debug, Clone)]
pub struct ClassificationPreference {
    pub id: String,
    pub focus_id: String,
    pub persona_id: String,
    pub content_type: String,
    pub visibility_scope: String,
    pub transformation: String,
    pub user_calibrated: bool,
    pub confidence: f64,
    pub created_at: String,
    pub updated_at: String,
    pub sensitivity_preset: Option<String>,
    pub last_applied_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PromotableRun {
    pub id: String,
    pub focus_run_id: String,
    pub focus_id: String,
    pub output_id: Option<String>,
    pub output_type: Option<String>,
    pub created_at: String,
    pub promote_window_expires_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum TopicStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Path helpers (public -- canonical source for focus/topic directory structure)
// ---------------------------------------------------------------------------

/// Lazily create the directory structure for a focus and optionally a topic.
/// Called at Phase 3 INITIALIZE before opening domain_context.db or plan_state.db.
/// Returns (focus_dir, topic_dir). topic_dir is None if topic_id is not provided.
pub fn ensure_focus_dirs(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: Option<&str>,
) -> Result<(PathBuf, Option<PathBuf>), TopicStoreError> {
    let focus_dir = crate::persistence::migrations::get_data_root()
        .join("users").join(user_id)
        .join("personas").join(persona_id)
        .join("focuses").join(focus_id);
    std::fs::create_dir_all(&focus_dir)?;

    let topic_dir = if let Some(tid) = topic_id {
        let td = focus_dir.join("topics").join(tid);
        std::fs::create_dir_all(&td)?;
        Some(td)
    } else {
        None
    };

    Ok((focus_dir, topic_dir))
}

/// Return the canonical path for a focus's domain_context.db.
pub fn get_domain_context_path(user_id: &str, persona_id: &str, focus_id: &str) -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("users").join(user_id)
        .join("personas").join(persona_id)
        .join("focuses").join(focus_id)
        .join("domain_context.db")
}

/// Return the canonical path for a topic's plan_state.db.
pub fn get_plan_state_path(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
) -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("users").join(user_id)
        .join("personas").join(persona_id)
        .join("focuses").join(focus_id)
        .join("topics").join(topic_id)
        .join("plan_state.db")
}

// ---------------------------------------------------------------------------
// Row mapping helpers
// ---------------------------------------------------------------------------

fn row_to_topic(r: &sqlx::sqlite::SqliteRow) -> Result<Topic, sqlx::Error> {
    // JSON parse fallback: returns json!({}) on malformed extra_metadata.
    // Intentional resilience -- a crashed read is worse than missing metadata.
    // If diagnostic tests surface unexpected empty metadata, check DB integrity.
    let extra_metadata: serde_json::Value = serde_json::from_str(
        &r.try_get::<String, _>("extra_metadata")
            .unwrap_or_else(|_| "{}".to_owned()),
    )
    .unwrap_or_else(|_| serde_json::json!({}));

    Ok(Topic {
        id:               r.try_get("id")?,
        focus_id:         r.try_get("focus_id")?,
        user_id:          r.try_get("user_id")?,
        persona_id:       r.try_get("persona_id")?,
        lifecycle_state:  r.try_get("lifecycle_state")?,
        placeholder_name: r.try_get("placeholder_name")?,
        created_at:       r.try_get("created_at")?,
        updated_at:       r.try_get("updated_at")?,
        name:             r.try_get("name")?,
        dormant_since:    r.try_get("dormant_since")?,
        closed_at:        r.try_get("closed_at")?,
        extra_metadata,
    })
}

fn row_to_classification_preference(
    r: &sqlx::sqlite::SqliteRow,
) -> Result<ClassificationPreference, sqlx::Error> {
    Ok(ClassificationPreference {
        id:                 r.try_get("id")?,
        focus_id:           r.try_get("focus_id")?,
        persona_id:         r.try_get("persona_id")?,
        content_type:       r.try_get("content_type")?,
        visibility_scope:   r.try_get("visibility_scope")?,
        transformation:     r.try_get("transformation")?,
        sensitivity_preset: r.try_get("sensitivity_preset")?,
        user_calibrated:    r.try_get::<i64, _>("user_calibrated")? != 0,
        confidence:         r.try_get("confidence")?,
        last_applied_at:    r.try_get("last_applied_at")?,
        created_at:         r.try_get("created_at")?,
        updated_at:         r.try_get("updated_at")?,
    })
}

// ---------------------------------------------------------------------------
// DB openers
// ---------------------------------------------------------------------------

/// Open outputs.db (encrypted, per-user per-persona).
async fn open_outputs_db(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, TopicStoreError> {
    let db_path = crate::persistence::migrations::get_data_root()
        .join("users").join(user_id)
        .join("personas").join(persona_id)
        .join("outputs.db");

    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let journal_mode = if network_storage { "DELETE" } else { "WAL" };

    let conn = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .pragma("key", format!("x'{key_hex}'"))
        .pragma("journal_mode", journal_mode)
        .pragma("busy_timeout", "5000")
        .connect()
        .await?;

    Ok(conn)
}

/// Open shared.db (unencrypted, instance-level, topic_index mirror).
/// Non-fatal context: callers discard errors from mirror operations.
async fn open_shared_db() -> Result<SqliteConnection, TopicStoreError> {
    let db_path = crate::persistence::migrations::get_data_root()
        .join("instance")
        .join("shared.db");

    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let journal_mode = if network_storage { "DELETE" } else { "WAL" };

    let conn = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .pragma("journal_mode", journal_mode)
        .pragma("busy_timeout", "5000")
        .connect()
        .await?;

    Ok(conn)
}

// ---------------------------------------------------------------------------
// Topic CRUD
// ---------------------------------------------------------------------------

/// Create a new topic in outputs.db and register it in shared.db topic_index.
/// Also registers plan_state.db path in topic_storage_locations.
/// placeholder_name generated from focus_id + timestamp if not provided.
/// name: user-assigned. None = unnamed (naming offered on resume).
pub async fn create_topic(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_id: &str,
    name: Option<&str>,
    placeholder_name: Option<&str>,
) -> Result<Topic, TopicStoreError> {
    let topic_id = uuid::Uuid::new_v4().to_string();
    let timestamp = crate::providers::utils::now();
    // Placeholder format: "focus_id -- YYYY-MM-DD HH:MM"
    // Uses chrono format directly -- avoids brittle string-slice indexing.
    let ph_name = placeholder_name
        .map(|s| s.to_owned())
        .unwrap_or_else(|| {
            let formatted = Utc::now().format("%Y-%m-%d %H:%M").to_string();
            format!("{} \u{2014} {}", focus_id, formatted)
        });
    let plan_state_path = get_plan_state_path(user_id, persona_id, focus_id, &topic_id)
        .to_string_lossy()
        .to_string();

    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    // SAVEPOINT: topics INSERT and topic_storage_locations INSERT are a logical unit.
    // If the storage location INSERT fails, the topic row must not persist orphaned.
    // ROLLBACK TO used to match the SAVEPOINT pattern in migrations.rs.
    sqlx::query("SAVEPOINT create_topic_sp").execute(&mut conn).await?;

    let topic_result = sqlx::query(
        "INSERT INTO topics
         (id, focus_id, user_id, persona_id, name, placeholder_name,
          lifecycle_state, created_at, updated_at, extra_metadata)
         VALUES (?, ?, ?, ?, ?, ?, 'active', ?, ?, '{}')",
    )
    .bind(&topic_id).bind(focus_id).bind(user_id).bind(persona_id)
    .bind(name).bind(&ph_name).bind(&timestamp).bind(&timestamp)
    .execute(&mut conn).await;

    if let Err(e) = topic_result {
        let _ = sqlx::query("ROLLBACK TO create_topic_sp").execute(&mut conn).await;
        return Err(TopicStoreError::Database(e));
    }

    let storage_result = sqlx::query(
        "INSERT INTO topic_storage_locations (topic_id, db_path, created_at)
         VALUES (?, ?, ?)",
    )
    .bind(&topic_id).bind(&plan_state_path).bind(&timestamp)
    .execute(&mut conn).await;

    if let Err(e) = storage_result {
        let _ = sqlx::query("ROLLBACK TO create_topic_sp").execute(&mut conn).await;
        return Err(TopicStoreError::Database(e));
    }

    sqlx::query("RELEASE create_topic_sp").execute(&mut conn).await?;

    // Mirror to shared.db topic_index -- non-fatal.
    let _ = mirror_topic_index(
        &topic_id, persona_id, focus_id,
        name.unwrap_or(&ph_name), "active", &timestamp, 0, &timestamp,
    ).await;

    Ok(Topic {
        id: topic_id, focus_id: focus_id.to_owned(), user_id: user_id.to_owned(),
        persona_id: persona_id.to_owned(), name: name.map(|s| s.to_owned()),
        placeholder_name: ph_name, lifecycle_state: "active".to_owned(),
        created_at: timestamp.clone(), updated_at: timestamp,
        dormant_since: None, closed_at: None,
        extra_metadata: serde_json::json!({}),
    })
}

/// Fetch a topic by id. Returns None if not found.
/// Filters on user_id and persona_id for defensive tenant isolation.
pub async fn get_topic(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    topic_id: &str,
) -> Result<Option<Topic>, TopicStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT id, focus_id, user_id, persona_id, name, placeholder_name,
                lifecycle_state, dormant_since, created_at, updated_at,
                closed_at, extra_metadata
         FROM topics WHERE id = ? AND user_id = ? AND persona_id = ?",
    )
    .bind(topic_id).bind(user_id).bind(persona_id)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_topic(&r).map_err(TopicStoreError::Database)?)),
    }
}

/// List topics with optional filters. Ordered by updated_at DESC.
/// Always filters on user_id and persona_id for tenant isolation.
pub async fn list_topics(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_id: Option<&str>,
    lifecycle_state: Option<&str>,
) -> Result<Vec<Topic>, TopicStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, focus_id, user_id, persona_id, name, placeholder_name,
                lifecycle_state, dormant_since, created_at, updated_at,
                closed_at, extra_metadata FROM topics WHERE user_id = ",
    );
    qb.push_bind(user_id);
    qb.push(" AND persona_id = ");
    qb.push_bind(persona_id);

    if let Some(fid) = focus_id {
        qb.push(" AND focus_id = ");
        qb.push_bind(fid);
    }
    if let Some(state) = lifecycle_state {
        qb.push(" AND lifecycle_state = ");
        qb.push_bind(state);
    }
    qb.push(" ORDER BY updated_at DESC");

    let rows = qb.build().fetch_all(&mut conn).await?;

    rows.iter()
        .map(|r| row_to_topic(r).map_err(TopicStoreError::Database))
        .collect()
}

/// Update topic lifecycle state. Returns true if found and updated.
/// Automatically sets closed_at when transitioning to complete or closed.
///
/// Completion authority invariant (ADR-013 Section 8.9):
/// topics.lifecycle_state NEVER set to 'complete' by system autonomously.
/// This function does not enforce that -- callers must not pass 'complete'
/// from system code. Only user-initiated calls may pass 'complete'.
pub async fn update_topic_state(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    topic_id: &str,
    lifecycle_state: &str,
    dormant_since: Option<&str>,
) -> Result<bool, TopicStoreError> {
    let timestamp = crate::providers::utils::now();
    let closed_at: Option<&str> =
        if lifecycle_state == "complete" || lifecycle_state == "closed" {
            Some(&timestamp)
        } else {
            None
        };

    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let result = sqlx::query(
        "UPDATE topics SET lifecycle_state = ?, dormant_since = ?,
         closed_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind(lifecycle_state).bind(dormant_since).bind(closed_at)
    .bind(&timestamp).bind(topic_id)
    .execute(&mut conn).await?;

    let updated = result.rows_affected() > 0;
    if updated {
        let _ = update_topic_index_state(topic_id, lifecycle_state, &timestamp).await;
    }
    Ok(updated)
}

/// Set or update the user-assigned name for a topic.
/// Also updates the topic_index display_name mirror. Returns true if found and updated.
pub async fn name_topic(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    topic_id: &str,
    name: &str,
) -> Result<bool, TopicStoreError> {
    let timestamp = crate::providers::utils::now();
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let result = sqlx::query(
        "UPDATE topics SET name = ?, updated_at = ? WHERE id = ?",
    )
    .bind(name).bind(&timestamp).bind(topic_id)
    .execute(&mut conn).await?;

    let updated = result.rows_affected() > 0;
    if updated {
        let _ = update_topic_index_display_name(topic_id, name, &timestamp).await;
    }
    Ok(updated)
}

/// Increment session_count on topic_index in shared.db.
/// Called at Phase 3 INITIALIZE. Returns new session_count, 0 if not found.
/// Note: touches shared.db only -- user_id/persona_id/key_hex are not needed.
pub async fn increment_topic_session_count(
    topic_id: &str,
) -> Result<i32, TopicStoreError> {
    let timestamp = crate::providers::utils::now();
    let mut conn = open_shared_db().await?;

    sqlx::query(
        "UPDATE topic_index SET session_count = session_count + 1,
         last_active_at = ?, updated_at = ? WHERE topic_id = ?",
    )
    .bind(&timestamp).bind(&timestamp).bind(topic_id)
    .execute(&mut conn).await?;

    let row = sqlx::query(
        "SELECT session_count FROM topic_index WHERE topic_id = ?",
    )
    .bind(topic_id)
    .fetch_optional(&mut conn).await?;

    match row {
        None => Ok(0),
        Some(r) => Ok(r.try_get::<i64, _>("session_count")? as i32),
    }
}

// ---------------------------------------------------------------------------
// Topic storage location registry
// ---------------------------------------------------------------------------

/// Retrieve the registered plan_state.db path from the discovery index.
/// Returns None if not registered.
/// Boot Check uses this as primary lookup -- filesystem scan is orphan fallback only.
pub async fn get_plan_state_db_path(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    topic_id: &str,
) -> Result<Option<String>, TopicStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT db_path FROM topic_storage_locations WHERE topic_id = ?",
    )
    .bind(topic_id)
    .fetch_optional(&mut conn).await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(r.try_get("db_path")?)),
    }
}

/// Mark a topic's plan_state.db as verified at current time.
pub async fn mark_storage_location_verified(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    topic_id: &str,
) -> Result<(), TopicStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    sqlx::query(
        "UPDATE topic_storage_locations SET verified_at = ?, orphaned = 0
         WHERE topic_id = ?",
    )
    .bind(crate::providers::utils::now()).bind(topic_id)
    .execute(&mut conn).await?;
    Ok(())
}

/// Mark a topic's plan_state.db as orphaned (file missing at Boot Check).
/// Boot Check never auto-deletes -- surfaces as dashboard notification.
pub async fn mark_storage_location_orphaned(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    topic_id: &str,
) -> Result<(), TopicStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    sqlx::query(
        "UPDATE topic_storage_locations SET orphaned = 1, verified_at = ?
         WHERE topic_id = ?",
    )
    .bind(crate::providers::utils::now()).bind(topic_id)
    .execute(&mut conn).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Run history
// ---------------------------------------------------------------------------

/// Create a run_history entry for a focus run.
///
/// Quick Ask invariant: promote_window_expires_at is always NULL for Quick Ask.
/// Named runs (topic_id non-null) also have no promote window.
/// 90-day promote window applies only to unnamed non-Quick Ask runs.
///
/// Returns the run_history entry id.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn create_run_history_entry(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
    focus_id: &str,
    is_quick_ask: bool,
    topic_id: Option<&str>,
    output_id: Option<&str>,
    output_type: Option<&str>,
) -> Result<String, TopicStoreError> {
    let entry_id = uuid::Uuid::new_v4().to_string();
    let timestamp = crate::providers::utils::now();

    let promote_expires: Option<String> = if !is_quick_ask && topic_id.is_none() {
        let expiry = Utc::now() + Duration::days(run_history_retention_days());
        Some(expiry.to_rfc3339())
    } else {
        None
    };

    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    sqlx::query(
        "INSERT INTO run_history
         (id, focus_run_id, focus_id, persona_id, topic_id,
          output_id, output_type, is_quick_ask,
          promote_window_expires_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&entry_id).bind(focus_run_id).bind(focus_id).bind(persona_id)
    .bind(topic_id).bind(output_id).bind(output_type)
    .bind(if is_quick_ask { 1i64 } else { 0i64 })
    .bind(promote_expires.as_deref()).bind(&timestamp)
    .execute(&mut conn).await?;

    Ok(entry_id)
}

/// Set output_id to NULL in run_history when a Library output is deleted.
/// Entry retained for audit unless user explicitly purges.
pub async fn nullify_run_history_output(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    output_id: &str,
) -> Result<(), TopicStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    sqlx::query("UPDATE run_history SET output_id = NULL WHERE output_id = ?")
        .bind(output_id).execute(&mut conn).await?;
    Ok(())
}

/// List unnamed non-Quick Ask runs within their promote window.
/// Used for "Promote to topic" UI (90-day window).
pub async fn list_promotable_runs(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_id: Option<&str>,
) -> Result<Vec<PromotableRun>, TopicStoreError> {
    let current_time = crate::providers::utils::now();
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, focus_run_id, focus_id, output_id, output_type,
                created_at, promote_window_expires_at
         FROM run_history
         WHERE topic_id IS NULL AND is_quick_ask = 0
         AND promote_window_expires_at > ",
    );
    qb.push_bind(&current_time);

    if let Some(fid) = focus_id {
        qb.push(" AND focus_id = ");
        qb.push_bind(fid);
    }
    qb.push(" ORDER BY created_at DESC");

    let rows = qb.build().fetch_all(&mut conn).await?;

    let mut result = Vec::new();
    for r in rows {
        result.push(PromotableRun {
            id:                        r.try_get("id")?,
            focus_run_id:              r.try_get("focus_run_id")?,
            focus_id:                  r.try_get("focus_id")?,
            output_id:                 r.try_get("output_id")?,
            output_type:               r.try_get("output_type")?,
            created_at:                r.try_get("created_at")?,
            promote_window_expires_at: r.try_get("promote_window_expires_at")?,
        });
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Classification preferences
// ---------------------------------------------------------------------------

/// Fetch the classification preference for a content type within a focus.
/// Returns None if no preference established -- Mode 2 should fire.
pub async fn get_classification_preference(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_id: &str,
    content_type: &str,
) -> Result<Option<ClassificationPreference>, TopicStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT id, focus_id, persona_id, content_type, visibility_scope,
                transformation, sensitivity_preset, user_calibrated,
                confidence, last_applied_at, created_at, updated_at
         FROM classification_preferences
         WHERE focus_id = ? AND persona_id = ? AND content_type = ?",
    )
    .bind(focus_id).bind(persona_id).bind(content_type)
    .fetch_optional(&mut conn).await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(
            row_to_classification_preference(&r).map_err(TopicStoreError::Database)?,
        )),
    }
}

/// Insert or update a classification preference. Returns the preference id.
/// Mode 2 response: user_calibrated=true. Mode 1 inference: user_calibrated=false.
///
/// Preset-to-dimensions mapping:
///   standard  = tier2_permitted  + generalize_ok
///   sensitive = anonymous_tier2  + anonymize_ok
///   private   = tier_1_only      + generalize_ok
///   locked    = tier_1_only      + no_generalize
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn upsert_classification_preference(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_id: &str,
    content_type: &str,
    visibility_scope: &str,
    transformation: &str,
    sensitivity_preset: Option<&str>,
    user_calibrated: bool,
    confidence: f64,
) -> Result<String, TopicStoreError> {
    let timestamp = crate::providers::utils::now();
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let existing_id: Option<String> = sqlx::query(
        "SELECT id FROM classification_preferences
         WHERE focus_id = ? AND persona_id = ? AND content_type = ?",
    )
    .bind(focus_id).bind(persona_id).bind(content_type)
    .fetch_optional(&mut conn).await?
    .map(|r| r.try_get("id"))
    .transpose()
    .map_err(TopicStoreError::Database)?;

    if let Some(ref eid) = existing_id {
        sqlx::query(
            "UPDATE classification_preferences SET
             visibility_scope = ?, transformation = ?,
             sensitivity_preset = ?, user_calibrated = ?,
             confidence = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(visibility_scope).bind(transformation).bind(sensitivity_preset)
        .bind(if user_calibrated { 1i64 } else { 0i64 })
        .bind(confidence).bind(&timestamp).bind(eid)
        .execute(&mut conn).await?;

        Ok(eid.clone())
    } else {
        let pref_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO classification_preferences
             (id, focus_id, persona_id, content_type, visibility_scope,
              transformation, sensitivity_preset, user_calibrated,
              confidence, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&pref_id).bind(focus_id).bind(persona_id).bind(content_type)
        .bind(visibility_scope).bind(transformation).bind(sensitivity_preset)
        .bind(if user_calibrated { 1i64 } else { 0i64 })
        .bind(confidence).bind(&timestamp).bind(&timestamp)
        .execute(&mut conn).await?;

        Ok(pref_id)
    }
}

/// Update last_applied_at timestamp when a Mode 1 preference is used.
pub async fn record_preference_applied(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_id: &str,
    content_type: &str,
) -> Result<(), TopicStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    sqlx::query(
        "UPDATE classification_preferences SET last_applied_at = ?
         WHERE focus_id = ? AND persona_id = ? AND content_type = ?",
    )
    .bind(crate::providers::utils::now()).bind(focus_id).bind(persona_id).bind(content_type)
    .execute(&mut conn).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Topic index mirrors (private -- callers use `let _ = fn().await`)
// ---------------------------------------------------------------------------

/// Write or update the topic_index row in shared.db.
/// Non-fatal if shared.db write fails -- outputs.db is authoritative.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
async fn mirror_topic_index(
    topic_id: &str,
    persona_id: &str,
    focus_id: &str,
    display_name: &str,
    lifecycle_state: &str,
    last_active_at: &str,
    session_count: i64,
    created_at: &str,
) -> Result<(), TopicStoreError> {
    let mut conn = open_shared_db().await?;
    sqlx::query(
        "INSERT OR REPLACE INTO topic_index
         (topic_id, persona_id, focus_id, display_name, lifecycle_state,
          last_active_at, session_count, content_summary, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)",
    )
    .bind(topic_id).bind(persona_id).bind(focus_id).bind(display_name)
    .bind(lifecycle_state).bind(last_active_at).bind(session_count)
    .bind(created_at).bind(created_at)
    .execute(&mut conn).await?;
    Ok(())
}

/// Update lifecycle_state and last_active_at in topic_index. Non-fatal.
async fn update_topic_index_state(
    topic_id: &str,
    lifecycle_state: &str,
    timestamp: &str,
) -> Result<(), TopicStoreError> {
    let mut conn = open_shared_db().await?;
    sqlx::query(
        "UPDATE topic_index SET lifecycle_state = ?,
         last_active_at = ?, updated_at = ? WHERE topic_id = ?",
    )
    .bind(lifecycle_state).bind(timestamp).bind(timestamp).bind(topic_id)
    .execute(&mut conn).await?;
    Ok(())
}

/// Update display_name in topic_index when user names a topic. Non-fatal.
async fn update_topic_index_display_name(
    topic_id: &str,
    display_name: &str,
    timestamp: &str,
) -> Result<(), TopicStoreError> {
    let mut conn = open_shared_db().await?;
    sqlx::query(
        "UPDATE topic_index SET display_name = ?, updated_at = ? WHERE topic_id = ?",
    )
    .bind(display_name).bind(timestamp).bind(topic_id)
    .execute(&mut conn).await?;
    Ok(())
}
