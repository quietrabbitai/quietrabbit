// src-tauri/src/persistence/output_store.rs
//
// Output record persistence for outputs.db — per-user, per-persona, SQLCipher encrypted.
// Path: /users/{user_id}/personas/{persona_id}/outputs.db
//
// Responsibility boundary:
//   output_store  — output record persistence + read-only run status for UI polling
//   conductor/lifecycle  — all focus_run state transitions (create, promote, status updates)
//
// get_focus_run_status() is a documented exception to the output-only boundary:
// the UI polling endpoint needs run status without importing lifecycle machinery.
// Revisit when a service layer is introduced in Layer 8+.
//
// delete_output: full zero-then-delete sequence deferred to Layer 5+.
// Architecture Section 3.4 deletion sequence:
//   1. Zero content:  UPDATE outputs SET content = '' WHERE id = ?
//   2. FTS5 update:   handled by COALESCE trigger in schema
//   3. Mark deleted:  UPDATE outputs SET status = 'deleted', deleted_at = ? WHERE id = ?
// Row is never deleted — audit record preserved permanently.
//
// QUERY STYLE: runtime sqlx::query() only — no query!() macros.
// PRAGMA key applied via SqliteConnectOptions (D6-346).
// Caller supplies bare hex; store wraps it in SQLCipher x'...' syntax.

use std::path::PathBuf;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Canonical sensitivity values — must match sensitivity_levels.yaml and
/// lifecycle output_sensitivity(). Reject anything outside this set at write time.
const VALID_SENSITIVITY: &[&str] = &["general", "personal", "medical", "financial"];

// ---------------------------------------------------------------------------
// OutputRecord
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct OutputRecord {
    pub id: String,
    pub focus_run_id: String,
    pub output_type: String,
    pub content: String,
    pub sensitivity: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum OutputStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Run not found: {0}")]
    RunNotFound(String),
}

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

fn get_outputs_db_path(user_id: &str, persona_id: &str) -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("outputs.db")
}

// ---------------------------------------------------------------------------
// DB opener
// ---------------------------------------------------------------------------

/// Open outputs.db with SQLCipher key.
/// Caller supplies bare hex; store wraps it in SQLCipher x'...' syntax.
/// PRAGMA key fires before journal_mode via SqliteConnectOptions (D6-346).
/// busy_timeout=5000ms guards against transient SQLITE_BUSY during concurrent
/// UI polling and Conductor writes.
async fn open_outputs_db(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, OutputStoreError> {
    let db_path = get_outputs_db_path(user_id, persona_id);

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

// ---------------------------------------------------------------------------
// Row mapping helper
// ---------------------------------------------------------------------------

fn row_to_output_record(r: &sqlx::sqlite::SqliteRow) -> Result<OutputRecord, sqlx::Error> {
    Ok(OutputRecord {
        id: r.try_get("id")?,
        focus_run_id: r.try_get("focus_run_id")?,
        output_type: r.try_get("output_type")?,
        content: r.try_get("content")?,
        sensitivity: r.try_get("sensitivity")?,
        status: r.try_get("status")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Write a completed output to outputs.db. Returns the output id.
/// FTS5 index updated automatically via schema trigger on insert.
/// sensitivity must be one of: general, personal, medical, financial.
///
/// sensitivity_severity is a GENERATED ALWAYS column in the outputs table —
/// omitted from INSERT; SQLite computes it automatically.
pub async fn save_output(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
    output_type: &str,
    content: &str,
    sensitivity: &str,
    output_id: Option<&str>,
) -> Result<String, OutputStoreError> {
    if !VALID_SENSITIVITY.contains(&sensitivity) {
        return Err(OutputStoreError::Validation(format!(
            "Invalid sensitivity '{}'. Must be one of: {}",
            sensitivity,
            VALID_SENSITIVITY.join(", ")
        )));
    }

    let oid = output_id
        .map(|s| s.to_owned())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let timestamp = crate::providers::utils::now();
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    sqlx::query(
        "INSERT INTO outputs
         (id, focus_run_id, output_type, content, sensitivity,
          status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'active', ?, ?)",
    )
    .bind(&oid)
    .bind(focus_run_id)
    .bind(output_type)
    .bind(content)
    .bind(sensitivity)
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(&mut conn)
    .await?;

    Ok(oid)
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// Fetch a single active output by id.
/// Returns None if not found or not active.
pub async fn get_output(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    output_id: &str,
) -> Result<Option<OutputRecord>, OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT id, focus_run_id, output_type, content,
                sensitivity, status, created_at, updated_at
         FROM outputs
         WHERE id = ? AND status = 'active'",
    )
    .bind(output_id)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(
            row_to_output_record(&r).map_err(OutputStoreError::Database)?,
        )),
    }
}

/// Fetch the most recent active output for a focus run.
/// Returns None if no active output exists.
/// Used by UI output display endpoint.
pub async fn get_output_for_run(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
) -> Result<Option<OutputRecord>, OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT id, focus_run_id, output_type, content,
                sensitivity, status, created_at, updated_at
         FROM outputs
         WHERE focus_run_id = ? AND status = 'active'
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(focus_run_id)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(
            row_to_output_record(&r).map_err(OutputStoreError::Database)?,
        )),
    }
}

/// Fetch the current status of a focus run.
/// Returns status string or None if focus_run_id not found.
/// Used by UI polling endpoint.
///
/// Note: reads focus_runs, which is lifecycle state. This is a documented
/// exception — the UI polling endpoint needs run status without importing
/// lifecycle machinery. Revisit when a service layer is introduced in Layer 8+.
///
/// PERFORMANCE NOTE: Connection-per-call (Phase 1) causes SQLCipher key
/// derivation on every poll. Target for shared connection in Layer 8+
/// persistence performance pass.
pub async fn get_focus_run_status(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
) -> Result<Option<String>, OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query("SELECT status FROM focus_runs WHERE id = ?")
        .bind(focus_run_id)
        .fetch_optional(&mut conn)
        .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(r.try_get("status").map_err(OutputStoreError::Database)?)),
    }
}

// ---------------------------------------------------------------------------
// Delete (deferred — Layer 5+)
// ---------------------------------------------------------------------------

/// Delete an output. Full sequence deferred to Layer 5+.
/// Correct deletion sequence (architecture Section 3.4):
///   1. Zero content:  UPDATE outputs SET content = '' WHERE id = ?
///   2. FTS5 update:   handled by COALESCE trigger in schema
///   3. Mark deleted:  UPDATE outputs SET status = 'deleted', deleted_at = ? WHERE id = ?
/// Row is never deleted — audit record preserved permanently.
pub async fn delete_output(
    _user_id: &str,
    _persona_id: &str,
    _key_hex: &str,
    _output_id: &str,
) -> Result<(), OutputStoreError> {
    unimplemented!(
        "delete_output: full zero-then-delete sequence implemented in Layer 5+"
    )
}

// ---------------------------------------------------------------------------
// Consent decisions (D6-352)
// ---------------------------------------------------------------------------

/// Mark a focus run as cancelled.
/// No-op if the run is already in a terminal state (complete/cancelled/failed).
/// Returns RunNotFound if run_id does not exist in this outputs.db.
pub async fn cancel_focus_run(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    run_id: &str,
) -> Result<(), OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    // Check whether the run exists at all.
    let exists: bool = sqlx::query(
        "SELECT 1 FROM focus_runs WHERE id = ? LIMIT 1",
    )
    .bind(run_id)
    .fetch_optional(&mut conn)
    .await?
    .is_some();

    if !exists {
        return Err(OutputStoreError::RunNotFound(run_id.to_string()));
    }

    // Update only if not already terminal — no-op on complete/cancelled/failed.
    sqlx::query(
        "UPDATE focus_runs SET status = 'cancelled'
         WHERE id = ? AND status NOT IN ('complete','cancelled','failed')",
    )
    .bind(run_id)
    .execute(&mut conn)
    .await?;

    Ok(())
}

/// Record a Gate 3 consent decision for a paused focus run (D6-352).
/// decision: "approved" | "declined"
/// Validated by consent_decisions CHECK constraint in outputs_006.sql.
pub async fn write_consent_decision(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    run_id: &str,
    decision: &str,
) -> Result<(), OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::providers::utils::now();

    sqlx::query(
        "INSERT INTO consent_decisions
             (id, focus_run_id, decision_type, decision,
              abstraction_tier, save_preference, created_at)
         VALUES (?, ?, 'gate3', ?, NULL, NULL, ?)",
    )
    .bind(&id)
    .bind(run_id)
    .bind(decision)
    .bind(&now)
    .execute(&mut conn)
    .await?;

    Ok(())
}

/// Record a floor consent decision for a paused focus run (D6-352).
/// decision: "proceed" | "cancel"
/// save_preference: if true, caller writes floor_consent_preference to
///   personas.extra_metadata in shared.db (D5-152) — not done here.
/// Validated by consent_decisions CHECK constraint in outputs_006.sql.
pub async fn write_floor_consent_decision(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    run_id: &str,
    abstraction_tier: i32,
    decision: &str,
    save_preference: bool,
) -> Result<(), OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::providers::utils::now();
    let save_pref_val = if save_preference { 1i32 } else { 0i32 };

    sqlx::query(
        "INSERT INTO consent_decisions
             (id, focus_run_id, decision_type, decision,
              abstraction_tier, save_preference, created_at)
         VALUES (?, ?, 'floor', ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(run_id)
    .bind(decision)
    .bind(abstraction_tier)
    .bind(save_pref_val)
    .bind(&now)
    .execute(&mut conn)
    .await?;

    Ok(())
}
