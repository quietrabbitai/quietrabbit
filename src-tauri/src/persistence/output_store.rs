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
// delete_output: soft-delete only (items.id=91 part 2, complete 2026-07-26).
// Architecture Section 3.4 deletion sequence:
//   1. Zero content:  UPDATE outputs SET content = '' WHERE id = ?
//   2. FTS5 update:   automatic via outputs_fts_update trigger (outputs_001.sql)
//   3. Mark deleted:  UPDATE outputs SET status = 'deleted', updated_at = ? WHERE id = ?
// Row is never hard-deleted — audit record preserved permanently.
// deep_purge parameter accepted but not implemented — Some(true) returns
// Err("deep_purge_not_implemented"). See delete_output's own doc comment.
//
// QUERY STYLE: runtime sqlx::query() only — no query!() macros.
// PRAGMA key applied via SqliteConnectOptions (D6-346).
// Caller supplies bare hex; store wraps it in SQLCipher x'...' syntax.

use std::path::PathBuf;

use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

use crate::conductor::privacy::types::{ElementDecision, ElementDecisionKind};

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

    let conn = crate::providers::utils::connect_options_encrypted(&db_path, key_hex)
        .create_if_missing(false)
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
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
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
        Some(r) => Ok(Some(
            r.try_get("status").map_err(OutputStoreError::Database)?,
        )),
    }
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

/// List active outputs, optionally filtered by focus_id, topic_id, and/or
/// output_type. Joins through focus_runs for focus_id/topic_id since those
/// columns live there, not on outputs itself.
///
/// Ordered most-recent-first (outputs.created_at DESC) — the Library's
/// natural browse order.
///
/// Does NOT enforce Focus profile visibility rules (Open/Organized/
/// Protected) — that filtering layer is a separate, not-yet-built gap,
/// split to items.id=175 (post-Release 1). Callers needing that enforcement
/// must apply it on top of this function's results until it lands in the store.
pub async fn list_outputs(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_id: Option<&str>,
    topic_id: Option<&str>,
    output_type: Option<&str>,
) -> Result<Vec<OutputRecord>, OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT o.id, o.focus_run_id, o.output_type, o.content,
                o.sensitivity, o.status, o.created_at, o.updated_at
         FROM outputs o
         JOIN focus_runs r ON r.id = o.focus_run_id
         WHERE o.status = 'active'",
    );
    if let Some(fid) = focus_id {
        qb.push(" AND r.focus_id = ");
        qb.push_bind(fid);
    }
    if let Some(tid) = topic_id {
        qb.push(" AND r.topic_id = ");
        qb.push_bind(tid);
    }
    if let Some(otype) = output_type {
        qb.push(" AND o.output_type = ");
        qb.push_bind(otype);
    }
    qb.push(" ORDER BY o.created_at DESC");

    let rows = qb.build().fetch_all(&mut conn).await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(row_to_output_record(r).map_err(OutputStoreError::Database)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// Delete-sequence core, operating on an already-open connection.
/// Testable directly against an in-memory SQLite connection (see tests
/// module) without requiring a real SQLCipher-encrypted outputs.db file.
///
/// Deletion sequence (architecture Section 3.4):
///   1. Zero content:  UPDATE outputs SET content = '' WHERE id = ?
///   2. FTS5 update:   automatic — outputs_fts_update trigger (outputs_001.sql)
///      fires on this UPDATE and removes the old content from the FTS5 index
///      as part of the same statement. No separate step is issued here.
///   3. Mark deleted:  UPDATE outputs SET status = 'deleted', updated_at = ?
///      WHERE id = ?
///
/// Row is never hard-deleted — audit record preserved permanently. Both
/// UPDATEs are unconditional on id match; deleting an already-deleted or
/// nonexistent id is a no-op (0 rows affected), not an error — matches the
/// idempotent-delete convention used elsewhere in this file (e.g.
/// cancel_focus_run's no-op-on-terminal-state pattern).
async fn delete_output_conn(
    conn: &mut SqliteConnection,
    output_id: &str,
) -> Result<(), OutputStoreError> {
    let timestamp = crate::providers::utils::now();

    sqlx::query("UPDATE outputs SET content = '' WHERE id = ?")
        .bind(output_id)
        .execute(&mut *conn)
        .await?;

    sqlx::query("UPDATE outputs SET status = 'deleted', updated_at = ? WHERE id = ?")
        .bind(&timestamp)
        .bind(output_id)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

/// Delete an output (items.id=91, part 2).
///
/// deep_purge: accepted for Tauri command-contract stability but NOT
/// implemented. Passing Some(true) returns Err("deep_purge_not_implemented")
/// rather than silently ignoring the flag or guessing at behavior. Deep
/// purge has no specification for outputs — the nearest analog,
/// decisions.id=242 ("Deep purge option at Plan deletion"), covers a
/// different object type (Plan → Domain Context provenance cleanup via an
/// interactive review flow) and does not transfer to a delete-call boolean
/// here. This is deliberately out of scope for R1, consistent with this
/// module's "row is never hard-deleted" architecture — a true purge would
/// mean actually removing the row, which the schema and this function do
/// not do. None or Some(false) proceed with the standard soft-delete
/// sequence below.
pub async fn delete_output(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    output_id: &str,
    deep_purge: Option<bool>,
) -> Result<(), OutputStoreError> {
    if deep_purge == Some(true) {
        return Err(OutputStoreError::Validation(
            "deep_purge_not_implemented".to_string(),
        ));
    }

    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    delete_output_conn(&mut conn, output_id).await
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
    let exists: bool = sqlx::query("SELECT 1 FROM focus_runs WHERE id = ? LIMIT 1")
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

/// Update focus_runs.status to an arbitrary value.
/// Used by submit_extract_confirm to set status='complete' after all
/// candidate decisions are written and verified.
pub async fn set_focus_run_status(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
    status: &str,
) -> Result<(), OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    sqlx::query("UPDATE focus_runs SET status = ? WHERE id = ?")
        .bind(status)
        .bind(focus_run_id)
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

/// Record per-element Privacy Guardian consent decisions for a paused focus run
/// (D6-362, items.id=37). One row per ElementDecision -- see outputs_001.sql's
/// consent_decisions header for why this is a fan-out, not a single row
/// holding a JSON blob.
///
/// decisions_json: JSON-serialized Vec<ElementDecision> from the Privacy Guardian
///   modal. The caller (consent.rs) is responsible for serialization; this
///   function deserializes it (D6-362 IPC boundary rule keeps consent.rs's
///   command layer from importing conductor types).
///   Expected JSON shape per element:
///     { "span_id": string, "decision": "generalize"|"keep_private"|"release_original",
///       "suggestion_text": string|null, "user_modified_text": string|null }
///
/// All rows in one call share a single created_at timestamp (one user
/// submission -> N rows, same instant) and are written inside a SAVEPOINT --
/// a mid-batch failure leaves no partial rows.
pub async fn write_element_consent_decisions(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    run_id: &str,
    decisions_json: &str,
) -> Result<(), OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    write_element_consent_decisions_conn(&mut conn, run_id, decisions_json).await
}

async fn write_element_consent_decisions_conn(
    conn: &mut SqliteConnection,
    run_id: &str,
    decisions_json: &str,
) -> Result<(), OutputStoreError> {
    let decisions: Vec<ElementDecision> = serde_json::from_str(decisions_json).map_err(|e| {
        OutputStoreError::Validation(format!("decisions_json parse error: {e}"))
    })?;

    if decisions.is_empty() {
        return Err(OutputStoreError::Validation(
            "decisions_json must contain at least one element decision".to_owned(),
        ));
    }

    let now = crate::providers::utils::now();

    sqlx::query("SAVEPOINT write_element_consent_sp")
        .execute(&mut *conn)
        .await?;

    for d in &decisions {
        let decision_str = match d.decision {
            ElementDecisionKind::Generalize => "generalize",
            ElementDecisionKind::KeepPrivate => "keep_private",
            ElementDecisionKind::ReleaseOriginal => "release_original",
        };
        let id = uuid::Uuid::new_v4().to_string();

        let result = sqlx::query(
            "INSERT INTO consent_decisions
                 (id, focus_run_id, decision_type, decision, abstraction_tier,
                  save_preference, span_id, suggestion_text, user_modified_text, created_at)
             VALUES (?, ?, 'element_consent', ?, NULL, NULL, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(run_id)
        .bind(decision_str)
        .bind(&d.span_id)
        .bind(&d.suggestion_text)
        .bind(&d.user_modified_text)
        .bind(&now)
        .execute(&mut *conn)
        .await;

        if let Err(e) = result {
            let _ = sqlx::query("ROLLBACK TO write_element_consent_sp")
                .execute(&mut *conn)
                .await;
            return Err(OutputStoreError::Database(e));
        }
    }

    sqlx::query("RELEASE write_element_consent_sp")
        .execute(&mut *conn)
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::migrations::parse_statements;
    use sqlx::sqlite::SqliteConnectOptions;

    const OUTPUTS_SCHEMA: &str = include_str!("../../schema/outputs_001.sql");

    async fn test_db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for stmt in parse_statements(OUTPUTS_SCHEMA) {
            sqlx::query(&stmt)
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{stmt}"));
        }
        conn
    }

    /// Insert a focus_run and one active output row directly, bypassing
    /// save_output (which requires a real outputs.db path). Returns the
    /// output id.
    async fn seed_output(conn: &mut SqliteConnection, content: &str) -> String {
        let run_id = uuid::Uuid::new_v4().to_string();
        let output_id = uuid::Uuid::new_v4().to_string();
        let now = "2026-07-26T00:00:00Z";

        sqlx::query(
            "INSERT INTO focus_runs (id, focus_id, status, started_at)
             VALUES (?, 'focus-1', 'complete', ?)",
        )
        .bind(&run_id)
        .bind(now)
        .execute(&mut *conn)
        .await
        .expect("focus_runs insert failed");

        sqlx::query(
            "INSERT INTO outputs
             (id, focus_run_id, output_type, content, sensitivity,
              status, created_at, updated_at)
             VALUES (?, ?, 'note', ?, 'general', 'active', ?, ?)",
        )
        .bind(&output_id)
        .bind(&run_id)
        .bind(content)
        .bind(now)
        .bind(now)
        .execute(&mut *conn)
        .await
        .expect("outputs insert failed");

        output_id
    }

    /// Insert a bare focus_run row directly. Returns the run id.
    async fn seed_focus_run(conn: &mut SqliteConnection) -> String {
        let run_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO focus_runs (id, focus_id, status, started_at)
             VALUES (?, 'focus-1', 'complete', '2026-08-03T00:00:00Z')",
        )
        .bind(&run_id)
        .execute(&mut *conn)
        .await
        .expect("focus_runs insert failed");
        run_id
    }

    #[tokio::test]
    async fn delete_output_conn_zeroes_content_and_marks_deleted_without_removing_row() {
        let mut conn = test_db().await;
        let output_id = seed_output(&mut conn, "sensitive output text").await;

        delete_output_conn(&mut conn, &output_id)
            .await
            .expect("delete_output_conn failed");

        let row = sqlx::query("SELECT content, status FROM outputs WHERE id = ?")
            .bind(&output_id)
            .fetch_optional(&mut conn)
            .await
            .expect("query failed")
            .expect("row must still exist — never hard-deleted");

        let content: String = row.try_get("content").unwrap();
        let status: String = row.try_get("status").unwrap();
        assert_eq!(content, "", "content must be zeroed");
        assert_eq!(status, "deleted", "status must be marked deleted");
    }

    #[tokio::test]
    async fn delete_output_conn_removes_row_from_fts_index() {
        let mut conn = test_db().await;
        let output_id = seed_output(&mut conn, "findable via fts search term").await;

        // Sanity check: findable before delete.
        let before =
            sqlx::query("SELECT rowid FROM outputs_fts WHERE outputs_fts MATCH 'findable'")
                .fetch_all(&mut conn)
                .await
                .expect("fts query failed");
        assert!(
            !before.is_empty(),
            "seeded output must be findable before delete"
        );

        delete_output_conn(&mut conn, &output_id)
            .await
            .expect("delete_output_conn failed");

        let after = sqlx::query("SELECT rowid FROM outputs_fts WHERE outputs_fts MATCH 'findable'")
            .fetch_all(&mut conn)
            .await
            .expect("fts query failed");
        assert!(
            after.is_empty(),
            "deleted output's content must no longer be searchable"
        );
    }

    #[tokio::test]
    async fn delete_output_conn_on_nonexistent_id_is_a_noop_not_an_error() {
        let mut conn = test_db().await;
        let result = delete_output_conn(&mut conn, "does-not-exist").await;
        assert!(result.is_ok(), "deleting a nonexistent id must not error");
    }

    #[tokio::test]
    async fn write_element_consent_decisions_conn_inserts_one_row_per_element() {
        let mut conn = test_db().await;
        let run_id = seed_focus_run(&mut conn).await;

        // Third element deliberately omits suggestion_text/user_modified_text --
        // the fully-NULL-optionals shape must be accepted, not just the
        // all-fields-populated case.
        let decisions_json = r#"[
            {"span_id": "span-1", "decision": "generalize",
             "suggestion_text": "[person]", "user_modified_text": null},
            {"span_id": "span-2", "decision": "release_original",
             "suggestion_text": null, "user_modified_text": "edited value"},
            {"span_id": "span-3", "decision": "keep_private",
             "suggestion_text": null, "user_modified_text": null}
        ]"#;

        write_element_consent_decisions_conn(&mut conn, &run_id, decisions_json)
            .await
            .expect("write_element_consent_decisions_conn failed");

        let rows = sqlx::query(
            "SELECT decision, span_id, suggestion_text, user_modified_text,
                    abstraction_tier, save_preference
             FROM consent_decisions WHERE focus_run_id = ? ORDER BY span_id",
        )
        .bind(&run_id)
        .fetch_all(&mut conn)
        .await
        .expect("query failed");

        assert_eq!(rows.len(), 3, "must insert one row per element");

        let decision: String = rows[0].try_get("decision").unwrap();
        let span_id: String = rows[0].try_get("span_id").unwrap();
        let suggestion_text: Option<String> = rows[0].try_get("suggestion_text").unwrap();
        assert_eq!(decision, "generalize");
        assert_eq!(span_id, "span-1");
        assert_eq!(suggestion_text.as_deref(), Some("[person]"));

        let abstraction_tier: Option<i64> = rows[2].try_get("abstraction_tier").unwrap();
        let save_preference: Option<i64> = rows[2].try_get("save_preference").unwrap();
        let suggestion_text_3: Option<String> = rows[2].try_get("suggestion_text").unwrap();
        let user_modified_text_3: Option<String> = rows[2].try_get("user_modified_text").unwrap();
        assert!(abstraction_tier.is_none(), "abstraction_tier must be NULL");
        assert!(save_preference.is_none(), "save_preference must be NULL");
        assert!(
            suggestion_text_3.is_none() && user_modified_text_3.is_none(),
            "fully-NULL optional fields must be accepted"
        );
    }

    #[tokio::test]
    async fn write_element_consent_decisions_conn_rejects_empty_array() {
        let mut conn = test_db().await;
        let run_id = seed_focus_run(&mut conn).await;

        let result = write_element_consent_decisions_conn(&mut conn, &run_id, "[]").await;

        match result.unwrap_err() {
            OutputStoreError::Validation(msg) => {
                assert!(msg.contains("at least one"), "unexpected message: {msg}")
            }
            other => panic!("expected Validation variant, got: {other:?}"),
        }

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM consent_decisions")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "no rows may be written on rejection");
    }

    #[tokio::test]
    async fn write_element_consent_decisions_conn_rejects_malformed_json() {
        let mut conn = test_db().await;
        let run_id = seed_focus_run(&mut conn).await;

        let result =
            write_element_consent_decisions_conn(&mut conn, &run_id, "not valid json").await;

        assert!(matches!(result, Err(OutputStoreError::Validation(_))));
    }

    #[tokio::test]
    async fn consent_decisions_check_rejects_element_consent_row_with_invalid_decision() {
        let mut conn = test_db().await;
        let run_id = seed_focus_run(&mut conn).await;

        let result = sqlx::query(
            "INSERT INTO consent_decisions
                 (id, focus_run_id, decision_type, decision, span_id, created_at)
             VALUES ('id-1', ?, 'element_consent', 'not_a_real_decision', 'span-1', 'now')",
        )
        .bind(&run_id)
        .execute(&mut conn)
        .await;

        assert!(
            result.is_err(),
            "CHECK constraint must reject an unrecognized element_consent decision value"
        );
    }

    #[tokio::test]
    async fn consent_decisions_check_rejects_element_consent_row_with_null_span_id() {
        let mut conn = test_db().await;
        let run_id = seed_focus_run(&mut conn).await;

        let result = sqlx::query(
            "INSERT INTO consent_decisions
                 (id, focus_run_id, decision_type, decision, span_id, created_at)
             VALUES ('id-1', ?, 'element_consent', 'generalize', NULL, 'now')",
        )
        .bind(&run_id)
        .execute(&mut conn)
        .await;

        assert!(
            result.is_err(),
            "CHECK constraint must require span_id for element_consent rows"
        );
    }

    #[tokio::test]
    async fn consent_decisions_check_still_accepts_valid_gate3_and_floor_rows() {
        let mut conn = test_db().await;
        let run_id = seed_focus_run(&mut conn).await;

        sqlx::query(
            "INSERT INTO consent_decisions (id, focus_run_id, decision_type, decision, created_at)
             VALUES ('id-gate3', ?, 'gate3', 'approved', 'now')",
        )
        .bind(&run_id)
        .execute(&mut conn)
        .await
        .expect("gate3 row must still satisfy the updated 3-branch CHECK");

        sqlx::query(
            "INSERT INTO consent_decisions
                 (id, focus_run_id, decision_type, decision, abstraction_tier, created_at)
             VALUES ('id-floor', ?, 'floor', 'proceed', 2, 'now')",
        )
        .bind(&run_id)
        .execute(&mut conn)
        .await
        .expect("floor row must still satisfy the updated 3-branch CHECK");
    }
}
