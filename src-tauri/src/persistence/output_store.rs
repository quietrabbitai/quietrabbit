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
//   3. Mark deleted:  UPDATE outputs SET deleted_at = ?, updated_at = ? WHERE id = ?
// deleted_at (outputs_007.sql, items.id=559) is independent of the four-state
// status column (decisions.id=421) -- deletion is an audit/visibility fact
// layered on top of, not a value within, the lifecycle enum. See
// delete_output_conn's own doc comment for the full reasoning.
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

use crate::conductor::privacy::output_scan::OutputScanResult;
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
    /// The owning focus_runs.focus_id -- present on every row via the
    /// NOT NULL REFERENCES focus_runs(id) FK, so this join can never drop a
    /// row. Needed by commands/library.rs to look up focus_settings.
    /// focus_profile for Library visibility enforcement (items.id=230).
    ///
    /// For an ingested row (source='external_ingested') this is the
    /// "system-ingest" pseudo-Focus sentinel (see create_ingest_focus_run),
    /// NOT the real Focus the document was filed under -- callers doing
    /// Focus-profile visibility checks must use `focus_slug` instead for
    /// those rows. See commands/library.rs's is_protected call sites.
    pub focus_id: String,
    pub output_type: String,
    /// NULL for an ingested document stored as opaque bytes (storage_path
    /// holds the real file) -- see items.id=383. Non-NULL for every
    /// qr_generated output (unchanged) and for ingested uploads whose
    /// content was plain-text-decodable (mirrored here for FTS5
    /// searchability).
    pub content: Option<String>,
    pub sensitivity: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    /// 'qr_generated' (default, existing behavior) | 'external_ingested'
    /// (items.id=383, decisions.id=486).
    pub source: String,
    pub project_entity_id: Option<String>,
    /// The REAL Focus an ingested document was filed under -- see the
    /// `focus_id` doc comment above. NULL for qr_generated rows (the real
    /// Focus is already reachable via focus_id) and for an ingested
    /// document not filed under any specific Focus.
    pub focus_slug: Option<String>,
    /// Path to the current version's encrypted blob on disk
    /// (persistence/ingest_blob.rs). NULL for qr_generated rows.
    pub storage_path: Option<String>,
    pub storage_version: i32,
    pub original_filename: Option<String>,
    /// items.id=416 (decisions.id=767): the real Privacy Guardian
    /// (output_scan::scan_output, ScanIntensity::Full) result, cached at
    /// creation time -- distinct from `sensitivity` above, which is an
    /// earlier, separate in-run classification, left untouched. NULL/false
    /// defaults mean "never scanned" (a row created before this cache
    /// existed, or one that never went through lifecycle.rs::output(), e.g.
    /// an ingested document) -- see pg_scan_completed_at.
    pub pg_scan_blocked: bool,
    pub pg_scan_timed_out: bool,
    pub pg_scan_plain_language: Option<String>,
    /// JSON array of OutputScanFinding, or NULL if never scanned.
    pub pg_scan_findings_json: Option<String>,
    /// NULL = never scanned (legacy row, or a creation path that doesn't
    /// call scan_output) -- the signal callers use to decide whether
    /// pg_scan_blocked is trustworthy cache or just an unset default.
    pub pg_scan_completed_at: Option<String>,
    /// One of prime/update/fork/reference/continue_draft (decisions.id=422,
    /// outputs_007.sql). Defaults to 'prime' for qr_generated rows and
    /// 'reference' for external_ingested rows (see save_ingested_output) --
    /// both defaults, not hard restrictions; any of the five values is
    /// assignable regardless of `source` (decisions.id=826).
    pub document_relationship: String,
    /// Set for document_relationship='fork' rows. NULL otherwise.
    pub parent_output_id: Option<String>,
    /// Set on the prior/previously-active record when
    /// update_active_document() supersedes it with a newer one -- points at
    /// the new record's id. NULL until superseded.
    pub superseded_by: Option<String>,
    /// Set by export_output() on the finalized->potentially-stale
    /// transition (decisions.id=421/826), cleared by
    /// return_output_from_export() on the way back to finalized. NULL means
    /// never exported (or already returned).
    pub exported_at: Option<String>,
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
    #[error("Migration error: {0}")]
    Migration(#[from] crate::persistence::migrations::MigrationError),
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
///
/// key_hex boundary (items.id=268): this fn and every public fn in this file
/// keep taking key_hex: &str -- that hasn't changed, and shouldn't, since
/// these are plain library functions (not #[tauri::command]s) also called
/// directly from this file's own tests and from background tasks. What
/// changed is who calls them: every command-layer caller now derives
/// key_hex from auth::registry::KeyRegistry server-side (see
/// commands/qr_hosted.rs for the reference pattern) instead of accepting it as
/// a bare IPC parameter from the frontend.
// items.id=406: bumped from private to pub(crate) -- the fact-identity
// persistence cascade (conductor/privacy/gate3.rs) needs its own outputs.db
// connection for consent_decisions prior-decision lookups and
// pf_fact_mentions reads/writes, matching personal_store::open_personal_db's
// existing pub(crate) visibility (already cross-imported by 3+ modules).
pub(crate) async fn open_outputs_db(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, OutputStoreError> {
    let db_path = get_outputs_db_path(user_id, persona_id);

    // BUG FOUND + FIXED (items.id=412 live verification pass, same class as
    // items.id=384 slice 7's message_store.rs fix and items.id=389's
    // personal_store.rs fix): this used to be `if !db_path.exists()`, which
    // only ever ran migrations against a brand-new file. Any outputs.db
    // created before outputs_004.sql/outputs_005.sql shipped (source,
    // project_entity_id, focus_slug, storage_path, storage_version,
    // original_filename, plus the Privacy Guardian persistence-cascade
    // columns) stays stuck on whatever version it was created at forever,
    // no matter how many times the app reopens it -- confirmed live
    // (Jason, 2026-09-04) as "no such column: o.source" from
    // output_store::list_outputs against a real pre-existing dev account.
    //
    // Fix: always call migrate_outputs_db, relying on run_migrations' own
    // idempotent/pending-only behavior (schema_version-tracked) rather than
    // a file-existence guess about whether anything is pending.
    crate::persistence::migrations::migrate_outputs_db(user_id, persona_id, key_hex).await?;

    let conn = crate::providers::utils::connect_options_encrypted(&db_path, key_hex)
        .create_if_missing(false)
        .pragma("busy_timeout", "5000")
        .connect()
        .await?;

    Ok(conn)
}

/// Test-only seed helper: inserts a minimal focus_runs row into the real
/// encrypted outputs.db at the given user/persona/key path (status=complete),
/// so commands::library's tests can seed a save_output()-referenceable
/// focus_run without duplicating this module's schema/path knowledge.
/// Not used by any non-test code path.
#[cfg(test)]
pub(crate) async fn test_seed_focus_run(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
    focus_id: &str,
) -> Result<(), OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    sqlx::query(
        "INSERT INTO focus_runs (id, focus_id, status, started_at)
         VALUES (?, ?, 'complete', ?)",
    )
    .bind(focus_run_id)
    .bind(focus_id)
    .bind(crate::providers::utils::now())
    .execute(&mut conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Row mapping helper
// ---------------------------------------------------------------------------

fn row_to_output_record(r: &sqlx::sqlite::SqliteRow) -> Result<OutputRecord, sqlx::Error> {
    Ok(OutputRecord {
        id: r.try_get("id")?,
        focus_run_id: r.try_get("focus_run_id")?,
        focus_id: r.try_get("focus_id")?,
        output_type: r.try_get("output_type")?,
        content: r.try_get("content")?,
        sensitivity: r.try_get("sensitivity")?,
        status: r.try_get("status")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
        source: r.try_get("source")?,
        project_entity_id: r.try_get("project_entity_id")?,
        focus_slug: r.try_get("focus_slug")?,
        storage_path: r.try_get("storage_path")?,
        storage_version: r.try_get("storage_version")?,
        original_filename: r.try_get("original_filename")?,
        pg_scan_blocked: r.try_get::<i64, _>("pg_scan_blocked")? != 0,
        pg_scan_timed_out: r.try_get::<i64, _>("pg_scan_timed_out")? != 0,
        pg_scan_plain_language: r.try_get("pg_scan_plain_language")?,
        pg_scan_findings_json: r.try_get("pg_scan_findings_json")?,
        pg_scan_completed_at: r.try_get("pg_scan_completed_at")?,
        document_relationship: r.try_get("document_relationship")?,
        parent_output_id: r.try_get("parent_output_id")?,
        superseded_by: r.try_get("superseded_by")?,
        exported_at: r.try_get("exported_at")?,
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
///
/// scan_result (items.id=416, decisions.id=767): the real Privacy Guardian
/// scan (output_scan::scan_output, ScanIntensity::Full) computed by the
/// caller at creation time, persisted alongside so later reads (e.g.
/// commands::library::prepare_clipboard_copy) never need to re-scan
/// immutable content. lifecycle.rs::output() always passes Some(&result) --
/// None is reserved for callers with no creation-time scan to report (test
/// seeding not exercising the cache, or a future non-Focus-run creation
/// path); a None row simply reads back as "never scanned"
/// (pg_scan_completed_at NULL), which callers must treat as cache-miss, not
/// as scan_result.blocked == false.
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
    scan_result: Option<&OutputScanResult>,
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

    let (
        pg_scan_blocked,
        pg_scan_timed_out,
        pg_scan_plain_language,
        pg_scan_findings_json,
        pg_scan_completed_at,
    ) = match scan_result {
        Some(r) => (
            r.blocked,
            r.timed_out,
            r.plain_language.clone(),
            Some(
                serde_json::to_string(
                    &r.findings
                        .iter()
                        .map(|f| {
                            serde_json::json!({
                                "start_byte": f.start_byte,
                                "end_byte": f.end_byte,
                                "label": f.label,
                                "score": f.score,
                            })
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_else(|_| "[]".to_string()),
            ),
            Some(timestamp.clone()),
        ),
        None => (false, false, None, None, None),
    };

    sqlx::query(
        "INSERT INTO outputs
         (id, focus_run_id, output_type, content, sensitivity,
          status, created_at, updated_at,
          pg_scan_blocked, pg_scan_timed_out, pg_scan_plain_language,
          pg_scan_findings_json, pg_scan_completed_at)
         VALUES (?, ?, ?, ?, ?, 'draft', ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&oid)
    .bind(focus_run_id)
    .bind(output_type)
    .bind(content)
    .bind(sensitivity)
    .bind(&timestamp)
    .bind(&timestamp)
    .bind(pg_scan_blocked)
    .bind(pg_scan_timed_out)
    .bind(pg_scan_plain_language)
    .bind(pg_scan_findings_json)
    .bind(pg_scan_completed_at)
    .execute(&mut conn)
    .await?;

    Ok(oid)
}

/// items.id=416: backfills the pg_scan_* columns for a row that was created
/// before this cache existed (or via a creation path that doesn't call
/// scan_output, e.g. ingestion) and just got its first live copy-time scan
/// in commands::library::prepare_clipboard_copy's fallback branch -- so the
/// *next* copy of the same immutable output becomes a cache hit too, per
/// decisions.id=767's "classify once" principle, converging every output
/// onto the creation-time model over time rather than re-scanning it forever.
pub async fn backfill_scan_result(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    output_id: &str,
    scan_result: &OutputScanResult,
) -> Result<(), OutputStoreError> {
    let findings_json = serde_json::to_string(
        &scan_result
            .findings
            .iter()
            .map(|f| {
                serde_json::json!({
                    "start_byte": f.start_byte,
                    "end_byte": f.end_byte,
                    "label": f.label,
                    "score": f.score,
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_string());
    let timestamp = crate::providers::utils::now();
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    sqlx::query(
        "UPDATE outputs
         SET pg_scan_blocked = ?, pg_scan_timed_out = ?, pg_scan_plain_language = ?,
             pg_scan_findings_json = ?, pg_scan_completed_at = ?
         WHERE id = ?",
    )
    .bind(scan_result.blocked)
    .bind(scan_result.timed_out)
    .bind(&scan_result.plain_language)
    .bind(&findings_json)
    .bind(&timestamp)
    .bind(output_id)
    .execute(&mut conn)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// Fetch a single output by id.
/// Returns None if not found or deleted.
pub async fn get_output(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    output_id: &str,
) -> Result<Option<OutputRecord>, OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT o.id, o.focus_run_id, r.focus_id, o.output_type, o.content,
                o.sensitivity, o.status, o.created_at, o.updated_at,
                o.source, o.project_entity_id, o.focus_slug, o.storage_path,
                o.storage_version, o.original_filename,
                o.pg_scan_blocked, o.pg_scan_timed_out, o.pg_scan_plain_language,
                o.pg_scan_findings_json, o.pg_scan_completed_at,
                o.document_relationship, o.parent_output_id, o.superseded_by,
                o.exported_at
         FROM outputs o
         JOIN focus_runs r ON r.id = o.focus_run_id
         WHERE o.id = ? AND o.deleted_at IS NULL",
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

/// Fetch the most recent non-deleted output for a focus run.
/// Returns None if no such output exists.
/// Used by UI output display endpoint.
pub async fn get_output_for_run(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
) -> Result<Option<OutputRecord>, OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT o.id, o.focus_run_id, r.focus_id, o.output_type, o.content,
                o.sensitivity, o.status, o.created_at, o.updated_at,
                o.source, o.project_entity_id, o.focus_slug, o.storage_path,
                o.storage_version, o.original_filename,
                o.pg_scan_blocked, o.pg_scan_timed_out, o.pg_scan_plain_language,
                o.pg_scan_findings_json, o.pg_scan_completed_at,
                o.document_relationship, o.parent_output_id, o.superseded_by,
                o.exported_at
         FROM outputs o
         JOIN focus_runs r ON r.id = o.focus_run_id
         WHERE o.focus_run_id = ? AND o.deleted_at IS NULL
         ORDER BY o.created_at DESC
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

/// Fetch the routing_tier_used (the tier the run actually executed under) for
/// a focus run. Returns None if the run isn't found or the column is NULL
/// (routing_tier_used is nullable -- e.g. a run that never completed).
/// Same connection-per-call shape as get_focus_run_status above.
pub async fn get_focus_run_routing_tier(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
) -> Result<Option<i32>, OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query("SELECT routing_tier_used FROM focus_runs WHERE id = ?")
        .bind(focus_run_id)
        .fetch_optional(&mut conn)
        .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(r
            .try_get::<Option<i64>, _>("routing_tier_used")
            .map_err(OutputStoreError::Database)?
            .map(|v| v as i32)),
    }
}

// ---------------------------------------------------------------------------
// Last-used (items.id=237)
// ---------------------------------------------------------------------------

/// Best-effort last-used timestamp for a single Focus (MAX(started_at) over
/// its focus_runs). Returns None on ANY failure -- missing outputs.db (a
/// persona that has never run this Focus, or ever been unlocked this
/// session), bad/empty key_hex, or a genuine query error -- because this is
/// a display value for commands::persona::get_focus_settings/
/// update_focus_settings, not something that should ever break the rest of
/// a Focus's settings from being returned.
///
/// Only logs (warn) when outputs.db exists on disk but still failed to open
/// or query -- a persona whose outputs.db was never created is the common,
/// expected case (empty key_hex pre-Layer-8, or a brand-new Focus) and is
/// not worth a log line every call.
pub async fn get_focus_last_used(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_id: &str,
) -> Option<String> {
    if key_hex.is_empty() || !get_outputs_db_path(user_id, persona_id).exists() {
        return None;
    }

    let mut conn = match open_outputs_db(user_id, persona_id, key_hex).await {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "get_focus_last_used: could not open outputs.db for \
                 persona='{persona_id}' focus='{focus_id}': {e}"
            );
            return None;
        }
    };

    match sqlx::query("SELECT MAX(started_at) AS last_used FROM focus_runs WHERE focus_id = ?")
        .bind(focus_id)
        .fetch_one(&mut conn)
        .await
        .and_then(|r| r.try_get::<Option<String>, _>("last_used"))
    {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "get_focus_last_used: query failed for persona='{persona_id}' \
                 focus='{focus_id}': {e}"
            );
            None
        }
    }
}

/// Batched form of get_focus_last_used for a whole persona's Focus list
/// (commands::persona::list_focuses) -- one connection covering every
/// focus_id via GROUP BY, instead of one connection per Focus. Same
/// best-effort/no-error-propagation contract: returns an empty map on any
/// failure rather than an error.
pub async fn get_last_used_map(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> std::collections::HashMap<String, String> {
    if key_hex.is_empty() || !get_outputs_db_path(user_id, persona_id).exists() {
        return std::collections::HashMap::new();
    }

    let mut conn = match open_outputs_db(user_id, persona_id, key_hex).await {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "get_last_used_map: could not open outputs.db for persona='{persona_id}': {e}"
            );
            return std::collections::HashMap::new();
        }
    };

    let rows = match sqlx::query(
        "SELECT focus_id, MAX(started_at) AS last_used FROM focus_runs GROUP BY focus_id",
    )
    .fetch_all(&mut conn)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            log::warn!("get_last_used_map: query failed for persona='{persona_id}': {e}");
            return std::collections::HashMap::new();
        }
    };

    let mut map = std::collections::HashMap::with_capacity(rows.len());
    for r in rows {
        let focus_id: String = match r.try_get("focus_id") {
            Ok(v) => v,
            Err(_) => continue,
        };
        let last_used: Option<String> = match r.try_get("last_used") {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(last_used) = last_used {
            map.insert(focus_id, last_used);
        }
    }
    map
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

/// List library-visible outputs, optionally filtered by focus_id, topic_id,
/// and/or output_type. Joins through focus_runs for focus_id/topic_id since
/// those columns live there, not on outputs itself.
///
/// Excludes soft-deleted rows (deleted_at IS NOT NULL) and archived rows
/// (status = 'archived') -- decisions.id=421 defines archived as "excluded
/// from default library views," and this function is that default view.
///
/// Ordered most-recent-first (outputs.created_at DESC) — the Library's
/// natural browse order.
///
/// Does NOT enforce Focus profile visibility rules (Open/Organized/
/// Protected) — that's a separate field (focus_settings.focus_profile, a
/// shared.db lookup, not something this outputs.db query can join against)
/// and is enforced by the caller. See commands::library::list_outputs
/// (items.id=230), which applies it on top of this function's results.
///
/// `source`: None = no filter (matches focus_id/topic_id/output_type's own
/// convention). The "default to Library view only" business rule --
/// defaulting to source='qr_generated' when the frontend passes nothing --
/// is deliberately NOT this function's job; it lives in
/// commands::library::list_outputs (items.id=383), same layering as the
/// focus_profile visibility check above.
pub async fn list_outputs(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_id: Option<&str>,
    topic_id: Option<&str>,
    output_type: Option<&str>,
    source: Option<&str>,
) -> Result<Vec<OutputRecord>, OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT o.id, o.focus_run_id, r.focus_id, o.output_type, o.content,
                o.sensitivity, o.status, o.created_at, o.updated_at,
                o.source, o.project_entity_id, o.focus_slug, o.storage_path,
                o.storage_version, o.original_filename,
                o.pg_scan_blocked, o.pg_scan_timed_out, o.pg_scan_plain_language,
                o.pg_scan_findings_json, o.pg_scan_completed_at,
                o.document_relationship, o.parent_output_id, o.superseded_by,
                o.exported_at
         FROM outputs o
         JOIN focus_runs r ON r.id = o.focus_run_id
         WHERE o.deleted_at IS NULL AND o.status != 'archived'",
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
    if let Some(src) = source {
        qb.push(" AND o.source = ");
        qb.push_bind(src);
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
///   3. Mark deleted:  UPDATE outputs SET deleted_at = ?, updated_at = ?
///      WHERE id = ?
///
/// deleted_at (outputs_007.sql) is independent of status -- deletion is an
/// audit/visibility fact layered on top of the four-state document lifecycle
/// (decisions.id=421), not a lifecycle state itself, so this no longer
/// mutates status. The row's status at time of delete is left exactly as it
/// was.
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

    sqlx::query("UPDATE outputs SET deleted_at = ?, updated_at = ? WHERE id = ?")
        .bind(&timestamp)
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
    let decisions = write_element_consent_decisions_conn(&mut conn, run_id, decisions_json).await?;

    // items.id=406 (decisions.id=756): "remember this for [Persona]" --
    // explicit, off-by-default opt-in (D5-152 pattern reuse). Written here,
    // after the primary consent_decisions rows are safely committed, same
    // ordering `submit_floor_consent_decision` already uses for its own
    // secondary write. Silently skipped (not an error) when fact_key is
    // None -- there is nothing stable to key a standing preference on for a
    // fact none of the three deterministic layers resolved.
    for d in &decisions {
        if !d.save_for_persona {
            continue;
        }
        let Some(fact_key) = &d.fact_key else {
            continue;
        };
        let decision_str = match d.decision {
            ElementDecisionKind::Generalize => "generalize",
            ElementDecisionKind::KeepPrivate => "keep_private",
            ElementDecisionKind::ReleaseOriginal => "release_original",
        };
        if let Err(e) = crate::persistence::personal_store::write_standing_preference(
            user_id,
            persona_id,
            key_hex,
            fact_key,
            &d.category,
            decision_str,
            d.suggestion_text.as_deref(),
            d.user_modified_text.as_deref(),
        )
        .await
        {
            log::warn!("failed to write standing preference for fact_key={fact_key}: {e}");
        }
    }

    Ok(())
}

async fn write_element_consent_decisions_conn(
    conn: &mut SqliteConnection,
    run_id: &str,
    decisions_json: &str,
) -> Result<Vec<ElementDecision>, OutputStoreError> {
    let decisions: Vec<ElementDecision> = serde_json::from_str(decisions_json)
        .map_err(|e| OutputStoreError::Validation(format!("decisions_json parse error: {e}")))?;

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
                  save_preference, span_id, suggestion_text, user_modified_text, created_at,
                  category, fact_key)
             VALUES (?, ?, 'element_consent', ?, NULL, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(run_id)
        .bind(decision_str)
        // items.id=406: save_preference now carries ElementDecision's own
        // save_for_persona flag for element_consent rows (previously always
        // NULL here -- 'floor' rows are the only other user of this column
        // and are unaffected). The actual standing-preference write happens
        // in write_element_consent_decisions (the caller of this function),
        // after this SAVEPOINT commits -- this bound value is audit-trail
        // parity, not the write path itself.
        .bind(d.save_for_persona as i64)
        .bind(&d.span_id)
        .bind(&d.suggestion_text)
        .bind(&d.user_modified_text)
        .bind(&now)
        .bind(&d.category)
        .bind(&d.fact_key)
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

    Ok(decisions)
}

// ---------------------------------------------------------------------------
// items.id=406 (decisions.id=756/757) -- Privacy Guardian persistence
// cascade: conversation-scoped prior-decision lookup, auto-reapplication
// audit rows, and within-conversation coreference state. All outputs.db --
// consent_decisions/pf_fact_mentions live here, not personal.db.
// ---------------------------------------------------------------------------

/// A previously-recorded decision for a resolved fact, as read back for
/// silent reapplication. Deliberately narrower than the full
/// `consent_decisions` row -- only what gate3 needs to reapply a decision
/// without a frontend round-trip.
#[derive(Debug, Clone)]
pub struct PriorFactDecision {
    /// "generalize" | "keep_private" | "release_original"
    pub decision: String,
    pub suggestion_text: Option<String>,
    pub user_modified_text: Option<String>,
}

/// Conversation-scoped prior-decision query (decisions.id=756's default,
/// no-opt-in-required tier): has `fact_key` already been decided earlier in
/// THIS `focus_run_id`? Latest decision wins if (implausibly) more than one
/// exists for the same fact_key within a run. `None` -- including on a
/// gracefully-degraded empty `key_hex` -- means "ask the user", the safe
/// fallback (this is an optimization layer, not a privacy control).
pub async fn find_consent_decision_for_fact(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
    fact_key: &str,
) -> Result<Option<PriorFactDecision>, OutputStoreError> {
    if key_hex.is_empty() {
        return Ok(None);
    }
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT decision, suggestion_text, user_modified_text
         FROM consent_decisions
         WHERE focus_run_id = ? AND fact_key = ? AND decision_type = 'element_consent'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(focus_run_id)
    .bind(fact_key)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(PriorFactDecision {
            decision: r.try_get("decision")?,
            suggestion_text: r.try_get("suggestion_text")?,
            user_modified_text: r.try_get("user_modified_text")?,
        })),
    }
}

/// Writes the audit-trail row for a fact silently reapplied without a
/// frontend round-trip (gate3.rs's partition_by_prior_decision). Mirrors
/// write_element_consent_decisions_conn's INSERT shape but for exactly one
/// row, with a freshly-generated span_id (this invocation's own ephemeral
/// id, same convention as an interactively-reviewed row) and a real
/// original_text -- available here because gate3 already has the entity in
/// hand, unlike the interactive path where the frontend doesn't echo it back.
#[allow(clippy::too_many_arguments)]
pub async fn write_auto_reapplied_consent_decision(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
    decision: &str,
    suggestion_text: Option<&str>,
    user_modified_text: Option<&str>,
    category: &str,
    fact_key: &str,
    original_text: &str,
) -> Result<(), OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let span_id = uuid::Uuid::new_v4().to_string();
    let now = crate::providers::utils::now();

    sqlx::query(
        "INSERT INTO consent_decisions
             (id, focus_run_id, decision_type, decision, abstraction_tier,
              save_preference, span_id, suggestion_text, user_modified_text, created_at,
              category, fact_key, original_text)
         VALUES (?, ?, 'element_consent', ?, NULL, NULL, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(focus_run_id)
    .bind(decision)
    .bind(&span_id)
    .bind(suggestion_text)
    .bind(user_modified_text)
    .bind(&now)
    .bind(category)
    .bind(fact_key)
    .bind(original_text)
    .execute(&mut conn)
    .await?;

    Ok(())
}

/// Records one span mention for Layer 2 (within-conversation coreference,
/// coref.rs) -- inserted regardless of whether this span ended up needing
/// interactive review, so a LATER span in the same conversation can coref
/// against it even before this one has a decision. Only called when a
/// `fact_key` was actually resolved (Layer 1 or Layer 2 itself) -- an
/// unresolved span contributes nothing for a future span to match against.
pub async fn insert_fact_mention(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
    category: &str,
    fact_key: &str,
    original_text: &str,
) -> Result<(), OutputStoreError> {
    if key_hex.is_empty() {
        return Ok(());
    }
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::providers::utils::now();

    sqlx::query(
        "INSERT INTO pf_fact_mentions (id, focus_run_id, category, fact_key, original_text, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(focus_run_id)
    .bind(category)
    .bind(fact_key)
    .bind(original_text)
    .bind(&now)
    .execute(&mut conn)
    .await?;

    Ok(())
}

/// Loads every span mention recorded so far in this conversation, for Layer
/// 2's coreference resolution (coref.rs::resolve_within_conversation).
/// Empty (including on a gracefully-degraded empty `key_hex`) simply means
/// Layer 2 has nothing to resolve against yet -- not an error.
pub async fn load_fact_mentions_for_run(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
) -> Result<Vec<crate::conductor::privacy::coref::PfFactMention>, OutputStoreError> {
    if key_hex.is_empty() {
        return Ok(vec![]);
    }
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let rows = sqlx::query(
        "SELECT category, fact_key, original_text FROM pf_fact_mentions
         WHERE focus_run_id = ? ORDER BY created_at ASC",
    )
    .bind(focus_run_id)
    .fetch_all(&mut conn)
    .await?;

    let mut mentions = Vec::with_capacity(rows.len());
    for r in rows {
        mentions.push(crate::conductor::privacy::coref::PfFactMention {
            category: r.try_get("category")?,
            fact_key: r.try_get("fact_key")?,
            original_text: r.try_get("original_text")?,
        });
    }
    Ok(mentions)
}

// ---------------------------------------------------------------------------
// Ingestion (items.id=383, decisions.id=486)
// ---------------------------------------------------------------------------

/// Fixed system pseudo-Focus id for ingest-only focus_runs. There is no
/// "active Focus" concept anywhere in this codebase (confirmed: no
/// current_focus/active_focus tracking exists -- Active Board is a list of
/// topic-cards, not a single-selection pointer) and focus_runs.focus_id has
/// no FK, so this sentinel is both the only workable attachment point and
/// free to invent. Real Focus association for an ingested document is
/// carried separately on outputs.focus_slug, not through this run's
/// focus_id -- see OutputRecord's own doc comment.
pub const INGEST_PSEUDO_FOCUS_ID: &str = "system-ingest";

/// Create a lightweight ingest-only focus_run to satisfy outputs.
/// focus_run_id's NOT NULL REFERENCES focus_runs(id) constraint for a
/// document that didn't come from running a Focus. One new row per upload
/// (not a shared singleton) -- keeps a clean 1:1 audit trail between an
/// ingest event and its resulting output row.
///
/// Deliberately bypasses conductor::lifecycle::FocusRun::authorize()
/// entirely -- that path loads a .focus file and requires a matching
/// focus_settings row for (persona_id, focus_id), neither of which exists
/// for INGEST_PSEUDO_FOCUS_ID. Direct INSERT instead, same shape as this
/// file's own test-only test_seed_focus_run helper. status='complete'
/// immediately -- an ingest-only run never actually executes.
pub async fn create_ingest_focus_run(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<String, OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    let run_id = uuid::Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO focus_runs (id, focus_id, status, started_at)
         VALUES (?, ?, 'complete', ?)",
    )
    .bind(&run_id)
    .bind(INGEST_PSEUDO_FOCUS_ID)
    .bind(crate::providers::utils::now())
    .execute(&mut conn)
    .await?;

    Ok(run_id)
}

/// Write an ingested document's row to outputs.db. source='external_ingested',
/// storage_version=1. `content` is the plain-text mirror for FTS5
/// searchability when the upload was text-decodable -- NULL for opaque
/// binary formats whose real bytes live only at `storage_path`.
///
/// `output_id` is caller-supplied (unlike save_output's optional id) --
/// the caller (commands/ingest.rs) needs the id before this call, to derive
/// `storage_path` via ingest_blob::ingest_blob_path() and write the
/// encrypted blob before the DB row referencing that path exists.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn save_ingested_output(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    output_id: &str,
    focus_run_id: &str,
    output_type: &str,
    sensitivity: &str,
    focus_slug: &str,
    project_entity_id: Option<&str>,
    storage_path: &str,
    original_filename: &str,
    content: Option<&str>,
) -> Result<String, OutputStoreError> {
    if !VALID_SENSITIVITY.contains(&sensitivity) {
        return Err(OutputStoreError::Validation(format!(
            "Invalid sensitivity '{}'. Must be one of: {}",
            sensitivity,
            VALID_SENSITIVITY.join(", ")
        )));
    }

    let timestamp = crate::providers::utils::now();
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    // document_relationship explicitly 'reference' (decisions.id=826), not
    // left to the schema's own DEFAULT 'prime' -- 'reference' is the
    // intended default for an ingested document (decisions.id=422: "inspired
    // by existing document, fully independent content"), while remaining
    // reassignable to any of the five types afterward (e.g. via
    // update_active_document below) regardless of `source`.
    sqlx::query(
        "INSERT INTO outputs
         (id, focus_run_id, output_type, content, sensitivity,
          status, created_at, updated_at, source, focus_slug,
          project_entity_id, storage_path, storage_version, original_filename,
          document_relationship)
         VALUES (?, ?, ?, ?, ?, 'finalized', ?, ?, 'external_ingested', ?, ?, ?, 1, ?, 'reference')",
    )
    .bind(output_id)
    .bind(focus_run_id)
    .bind(output_type)
    .bind(content)
    .bind(sensitivity)
    .bind(&timestamp)
    .bind(&timestamp)
    .bind(focus_slug)
    .bind(project_entity_id)
    .bind(storage_path)
    .bind(original_filename)
    .execute(&mut conn)
    .await?;

    Ok(output_id.to_string())
}

/// Record an edited version of an ingested document: bumps storage_version
/// and repoints storage_path at the new file. Prior version files are NOT
/// deleted here -- the caller (commands/ingest.rs) writes the new encrypted
/// blob to its own v{n}.enc path before calling this; old version files
/// stay on disk as history. Returns the new storage_version.
pub async fn bump_ingested_document_version(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    output_id: &str,
    new_storage_path: &str,
) -> Result<i32, OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    let timestamp = crate::providers::utils::now();

    let row = sqlx::query(
        "UPDATE outputs
         SET storage_version = storage_version + 1,
             storage_path = ?,
             updated_at = ?
         WHERE id = ? AND source = 'external_ingested'
         RETURNING storage_version",
    )
    .bind(new_storage_path)
    .bind(&timestamp)
    .bind(output_id)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        Some(r) => Ok(r.try_get::<i64, _>("storage_version")? as i32),
        None => Err(OutputStoreError::Validation(format!(
            "no ingested document with id '{output_id}' to version"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Document relationship + export lifecycle (items.id=556, decisions.id=826)
// ---------------------------------------------------------------------------

/// Wires the Library "Update active document" action (items.id=557):
/// `new_output_id` becomes the canonical version, `previous_output_id` is
/// the record it supersedes. Sets `document_relationship='update'` on
/// `new_output_id` and `superseded_by=new_output_id` on `previous_output_id`
/// -- exactly decisions.id=422's own definition of the `update` relationship
/// type ("prior version remains finalized but gets superseded_by set; new
/// version becomes canonical"). `parent_output_id` is untouched -- that
/// field belongs to `fork`, not `update` (decisions.id=422).
///
/// No `source` check anywhere here: works in either direction (an ingested
/// document can supersede a qr_generated one and vice versa), the
/// decoupling decisions.id=826 asked for.
pub async fn update_active_document(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    new_output_id: &str,
    previous_output_id: &str,
) -> Result<(), OutputStoreError> {
    if new_output_id == previous_output_id {
        return Err(OutputStoreError::Validation(
            "new_output_id and previous_output_id must differ".to_string(),
        ));
    }

    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    let timestamp = crate::providers::utils::now();

    sqlx::query("SAVEPOINT update_active_document_sp")
        .execute(&mut conn)
        .await?;

    let new_row = sqlx::query(
        "UPDATE outputs SET document_relationship = 'update', updated_at = ?
         WHERE id = ? RETURNING id",
    )
    .bind(&timestamp)
    .bind(new_output_id)
    .fetch_optional(&mut conn)
    .await?;

    if new_row.is_none() {
        let _ = sqlx::query("ROLLBACK TO update_active_document_sp")
            .execute(&mut conn)
            .await;
        return Err(OutputStoreError::Validation(format!(
            "no such output: '{new_output_id}'"
        )));
    }

    let prev_row = sqlx::query(
        "UPDATE outputs SET superseded_by = ?, updated_at = ?
         WHERE id = ? RETURNING id",
    )
    .bind(new_output_id)
    .bind(&timestamp)
    .bind(previous_output_id)
    .fetch_optional(&mut conn)
    .await?;

    if prev_row.is_none() {
        let _ = sqlx::query("ROLLBACK TO update_active_document_sp")
            .execute(&mut conn)
            .await;
        return Err(OutputStoreError::Validation(format!(
            "no such output: '{previous_output_id}'"
        )));
    }

    sqlx::query("RELEASE update_active_document_sp")
        .execute(&mut conn)
        .await?;

    Ok(())
}

/// Wires the Library Export action (decisions.id=421/826): fires the
/// finalized -> potentially-stale transition and stamps `exported_at`.
/// Rejects any other starting status -- this is a defined single-arrow
/// transition, not an idempotent no-op like delete/cancel_focus_run.
pub async fn export_output(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    output_id: &str,
) -> Result<(), OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    let timestamp = crate::providers::utils::now();

    let status: Option<String> = sqlx::query("SELECT status FROM outputs WHERE id = ?")
        .bind(output_id)
        .fetch_optional(&mut conn)
        .await?
        .map(|r| r.try_get("status"))
        .transpose()?;

    match status.as_deref() {
        None => {
            return Err(OutputStoreError::Validation(format!(
                "no such output: '{output_id}'"
            )))
        }
        Some("finalized") => {}
        Some(other) => {
            return Err(OutputStoreError::Validation(format!(
                "cannot export output in status '{other}': must be finalized"
            )))
        }
    }

    sqlx::query(
        "UPDATE outputs SET status = 'potentially-stale', exported_at = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(&timestamp)
    .bind(&timestamp)
    .bind(output_id)
    .execute(&mut conn)
    .await?;

    Ok(())
}

/// Reverse of export_output (decisions.id=421/826): fires the
/// potentially-stale -> finalized transition on return/re-import and clears
/// `exported_at`. Standalone primitive -- this codebase has no
/// `import_external_draft` flow yet to wire it into (out of this item's
/// scope; see items.id=559), but the column-clearing contract holds
/// regardless of what eventually calls it.
pub async fn return_output_from_export(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    output_id: &str,
) -> Result<(), OutputStoreError> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    let timestamp = crate::providers::utils::now();

    let status: Option<String> = sqlx::query("SELECT status FROM outputs WHERE id = ?")
        .bind(output_id)
        .fetch_optional(&mut conn)
        .await?
        .map(|r| r.try_get("status"))
        .transpose()?;

    match status.as_deref() {
        None => {
            return Err(OutputStoreError::Validation(format!(
                "no such output: '{output_id}'"
            )))
        }
        Some("potentially-stale") => {}
        Some(other) => {
            return Err(OutputStoreError::Validation(format!(
                "cannot return output in status '{other}': must be potentially-stale"
            )))
        }
    }

    sqlx::query(
        "UPDATE outputs SET status = 'finalized', exported_at = NULL, updated_at = ?
         WHERE id = ?",
    )
    .bind(&timestamp)
    .bind(output_id)
    .execute(&mut conn)
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
    // items.id=406: consent_decisions.category/fact_key/original_text and
    // pf_fact_mentions are added in outputs_005.sql, not outputs_001.sql --
    // this in-memory test DB must apply both (005 only touches tables 001
    // already defines, so no need for 002/003 in between).
    const OUTPUTS_SCHEMA_V5: &str = include_str!("../../schema/outputs_005.sql");
    // items.id=559: outputs_007.sql's table rebuild (INSERT INTO outputs_new
    // ... SELECT ... FROM outputs) selects source/project_entity_id/
    // focus_slug/storage_path/storage_version/original_filename (004) and
    // pg_scan_* (006) unconditionally -- unlike 005, these are hard
    // prerequisites for 007 to apply cleanly here, not just tables it
    // happens to also touch. 002/003 remain unnecessary (extract_confirm_
    // candidates.source and a focus_runs index, neither read by 007).
    const OUTPUTS_SCHEMA_V4: &str = include_str!("../../schema/outputs_004.sql");
    const OUTPUTS_SCHEMA_V6: &str = include_str!("../../schema/outputs_006.sql");
    const OUTPUTS_SCHEMA_V7: &str = include_str!("../../schema/outputs_007.sql");
    // items.id=556: exported_at is a plain additive column on top of 007's
    // rebuilt table -- no new hard prerequisite beyond 007 itself.
    const OUTPUTS_SCHEMA_V8: &str = include_str!("../../schema/outputs_008.sql");

    async fn test_db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for stmt in parse_statements(OUTPUTS_SCHEMA)
            .into_iter()
            .chain(parse_statements(OUTPUTS_SCHEMA_V4))
            .chain(parse_statements(OUTPUTS_SCHEMA_V5))
            .chain(parse_statements(OUTPUTS_SCHEMA_V6))
            .chain(parse_statements(OUTPUTS_SCHEMA_V7))
            .chain(parse_statements(OUTPUTS_SCHEMA_V8))
        {
            sqlx::query(&stmt)
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{stmt}"));
        }
        conn
    }

    /// Insert a focus_run and one output row directly, bypassing
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
             VALUES (?, ?, 'note', ?, 'general', 'draft', ?, ?)",
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

        let row = sqlx::query("SELECT content, deleted_at FROM outputs WHERE id = ?")
            .bind(&output_id)
            .fetch_optional(&mut conn)
            .await
            .expect("query failed")
            .expect("row must still exist — never hard-deleted");

        let content: String = row.try_get("content").unwrap();
        let deleted_at: Option<String> = row.try_get("deleted_at").unwrap();
        assert_eq!(content, "", "content must be zeroed");
        assert!(deleted_at.is_some(), "deleted_at must be set");
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
    async fn outputs_007_migration_preserves_fts_and_backfills_status_correctly() {
        // Builds the outputs.db schema as it existed immediately before
        // outputs_007.sql (001+004+005+006 -- the two-state active/deleted
        // status world), seeds one 'active' row and one 'deleted' row
        // directly against that schema, then applies 007 on top. This is
        // the only way to exercise the table rebuild's rowid reassignment
        // against the outputs_fts external-content index -- test_db() above
        // builds the fully-migrated schema fresh, so it never has a
        // pre-existing row at migration time and would not catch a rowid
        // desync regression, nor the value-mapped backfill.
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for stmt in parse_statements(OUTPUTS_SCHEMA)
            .into_iter()
            .chain(parse_statements(OUTPUTS_SCHEMA_V4))
            .chain(parse_statements(OUTPUTS_SCHEMA_V5))
            .chain(parse_statements(OUTPUTS_SCHEMA_V6))
        {
            sqlx::query(&stmt)
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("pre-007 schema statement failed: {e}\n{stmt}"));
        }

        let run_id = uuid::Uuid::new_v4().to_string();
        let now = "2026-07-26T00:00:00Z";
        sqlx::query(
            "INSERT INTO focus_runs (id, focus_id, status, started_at)
             VALUES (?, 'focus-1', 'complete', ?)",
        )
        .bind(&run_id)
        .bind(now)
        .execute(&mut conn)
        .await
        .expect("focus_runs insert failed");

        let active_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO outputs
             (id, focus_run_id, output_type, content, sensitivity,
              status, created_at, updated_at)
             VALUES (?, ?, 'note', 'findable pre-migration content', 'general',
                     'active', ?, ?)",
        )
        .bind(&active_id)
        .bind(&run_id)
        .bind(now)
        .bind(now)
        .execute(&mut conn)
        .await
        .expect("active-row outputs insert failed");

        let deleted_id = uuid::Uuid::new_v4().to_string();
        let deleted_at_ts = "2026-08-01T00:00:00Z";
        sqlx::query(
            "INSERT INTO outputs
             (id, focus_run_id, output_type, content, sensitivity,
              status, created_at, updated_at)
             VALUES (?, ?, 'note', '', 'general', 'deleted', ?, ?)",
        )
        .bind(&deleted_id)
        .bind(&run_id)
        .bind(now)
        .bind(deleted_at_ts)
        .execute(&mut conn)
        .await
        .expect("deleted-row outputs insert failed");

        for stmt in parse_statements(OUTPUTS_SCHEMA_V7) {
            sqlx::query(&stmt)
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("outputs_007.sql statement failed: {e}\n{stmt}"));
        }

        let found = sqlx::query("SELECT rowid FROM outputs_fts WHERE outputs_fts MATCH 'findable'")
            .fetch_all(&mut conn)
            .await
            .expect("fts query failed");
        assert!(
            !found.is_empty(),
            "pre-existing row's content must remain findable via FTS after the outputs_007 rebuild"
        );

        let active_row = sqlx::query("SELECT status, deleted_at FROM outputs WHERE id = ?")
            .bind(&active_id)
            .fetch_one(&mut conn)
            .await
            .expect("active row must survive the migration");
        let active_status: String = active_row.try_get("status").unwrap();
        let active_deleted_at: Option<String> = active_row.try_get("deleted_at").unwrap();
        assert_eq!(
            active_status, "finalized",
            "pre-existing 'active' row must be backfilled to 'finalized'"
        );
        assert!(
            active_deleted_at.is_none(),
            "pre-existing 'active' row must not gain a deleted_at"
        );

        let deleted_row = sqlx::query("SELECT status, deleted_at FROM outputs WHERE id = ?")
            .bind(&deleted_id)
            .fetch_one(&mut conn)
            .await
            .expect("deleted row must survive the migration");
        let deleted_status: String = deleted_row.try_get("status").unwrap();
        let deleted_deleted_at: Option<String> = deleted_row.try_get("deleted_at").unwrap();
        assert_eq!(
            deleted_status, "archived",
            "pre-existing 'deleted' row must be backfilled to 'archived'"
        );
        assert_eq!(
            deleted_deleted_at.as_deref(),
            Some(deleted_at_ts),
            "pre-existing 'deleted' row's deleted_at must be backfilled from its updated_at"
        );
    }

    #[tokio::test]
    async fn write_element_consent_decisions_conn_inserts_one_row_per_element() {
        let mut conn = test_db().await;
        let run_id = seed_focus_run(&mut conn).await;

        // Third element deliberately omits suggestion_text/user_modified_text/
        // fact_key -- the fully-NULL-optionals shape must be accepted, not
        // just the all-fields-populated case. save_for_persona relies on its
        // #[serde(default)] (false) when omitted, same reasoning.
        let decisions_json = r#"[
            {"span_id": "span-1", "decision": "generalize", "category": "private_person",
             "suggestion_text": "[person]", "user_modified_text": null, "fact_key": null},
            {"span_id": "span-2", "decision": "release_original", "category": "private_email",
             "suggestion_text": null, "user_modified_text": "edited value",
             "fact_key": "abc123", "save_for_persona": true},
            {"span_id": "span-3", "decision": "keep_private", "category": "secret",
             "suggestion_text": null, "user_modified_text": null}
        ]"#;

        write_element_consent_decisions_conn(&mut conn, &run_id, decisions_json)
            .await
            .expect("write_element_consent_decisions_conn failed");

        let rows = sqlx::query(
            "SELECT decision, span_id, suggestion_text, user_modified_text,
                    abstraction_tier, save_preference, category, fact_key
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
        let category_1: String = rows[0].try_get("category").unwrap();
        let fact_key_1: Option<String> = rows[0].try_get("fact_key").unwrap();
        assert_eq!(decision, "generalize");
        assert_eq!(span_id, "span-1");
        assert_eq!(suggestion_text.as_deref(), Some("[person]"));
        assert_eq!(category_1, "private_person");
        assert!(
            fact_key_1.is_none(),
            "span-1 omitted fact_key -- must persist as NULL"
        );

        // items.id=406: save_preference now carries ElementDecision's own
        // save_for_persona flag for element_consent rows (previously always
        // NULL) -- span-2 opted in, span-3 didn't specify (defaults false).
        let save_preference_2: Option<i64> = rows[1].try_get("save_preference").unwrap();
        let fact_key_2: Option<String> = rows[1].try_get("fact_key").unwrap();
        assert_eq!(
            save_preference_2,
            Some(1),
            "span-2 set save_for_persona: true"
        );
        assert_eq!(fact_key_2.as_deref(), Some("abc123"));

        let abstraction_tier: Option<i64> = rows[2].try_get("abstraction_tier").unwrap();
        let save_preference: Option<i64> = rows[2].try_get("save_preference").unwrap();
        let suggestion_text_3: Option<String> = rows[2].try_get("suggestion_text").unwrap();
        let user_modified_text_3: Option<String> = rows[2].try_get("user_modified_text").unwrap();
        assert!(abstraction_tier.is_none(), "abstraction_tier must be NULL");
        assert_eq!(
            save_preference,
            Some(0),
            "span-3 omitted save_for_persona -- defaults false, not NULL"
        );
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

    /// Regression test for items.id=278: open_outputs_db must self-heal a
    /// never-yet-created outputs.db by running migrate_outputs_db() itself,
    /// rather than hard-failing with SQLITE_CANTOPEN -- the same bug class
    /// items.id=275 fixed for shared.db (a single eager call in main.rs's
    /// setup()), applied here at the connection layer instead since
    /// outputs.db is per-user-per-persona, not a single app-wide file
    /// main.rs can migrate eagerly at boot. Deliberately does NOT
    /// pre-create the file or call migrate_outputs_db() anywhere in this
    /// test -- that absence is exactly the fresh-persona, first-access
    /// case under test. Also proves the healed file has a real migrated
    /// schema (not just an empty file) by querying a real table.
    #[tokio::test]
    async fn open_outputs_db_self_heals_a_never_created_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        let user_id = "self-heal-user";
        let persona_id = "self-heal-persona";
        let key_hex = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";

        let db_path = get_outputs_db_path(user_id, persona_id);
        assert!(
            !db_path.exists(),
            "test setup must start with no db file -- that's the fresh-persona case under test"
        );

        let result = open_outputs_db(user_id, persona_id, key_hex).await;

        {
            let mut conn = result.expect(
                "open_outputs_db must self-heal a fresh persona's never-created \
                 outputs.db, not hard-fail",
            );
            let row = sqlx::query("SELECT COUNT(*) AS n FROM outputs")
                .fetch_one(&mut conn)
                .await
                .expect("outputs table must exist after self-heal migration");
            let n: i64 = row.try_get("n").unwrap();
            assert_eq!(n, 0);
        }

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    /// Regression test for items.id=412 (live verification pass, same bug
    /// class as message_store.rs's open_messages_db_heals_a_pre_existing_v1_
    /// only_database and personal_store.rs's open_personal_db_heals_a_pre_
    /// existing_v1_only_database): open_outputs_db used to call
    /// migrate_outputs_db ONLY when the file didn't exist yet
    /// ("if !db_path.exists()"), so an outputs.db created before
    /// outputs_004.sql's `source` column landed stayed stuck on its
    /// original schema forever, no matter how many times the app reopened
    /// it -- confirmed live against a real pre-existing dev account
    /// ("no such column: o.source" from list_outputs). Hand-builds that
    /// stale pre-outputs_004 shape directly against a real encrypted file
    /// (SCHEMA_FILES is a compile-time static that always includes v4+ now,
    /// so migrate_outputs_db itself can't produce a deliberately-stale
    /// fixture).
    #[tokio::test]
    async fn open_outputs_db_heals_a_pre_existing_database_missing_the_source_column() {
        const OUTPUTS_001_SCHEMA: &str = include_str!("../../schema/outputs_001.sql");

        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "stale-pre-source-user";
        let persona_id = "stale-pre-source-persona";
        let key_hex = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";
        let db_path = get_outputs_db_path(user_id, persona_id);
        std::fs::create_dir_all(db_path.parent().unwrap())
            .expect("failed to create parent dirs for test db");

        {
            let mut conn = SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true)
                .pragma("key", format!("\"x'{key_hex}'\""))
                .connect()
                .await
                .expect("stale fixture connect failed");
            for stmt in crate::persistence::migrations::parse_statements(OUTPUTS_001_SCHEMA) {
                sqlx::query(&stmt)
                    .execute(&mut conn)
                    .await
                    .unwrap_or_else(|e| {
                        panic!("stale fixture schema statement failed: {e}\n{stmt}")
                    });
            }
        }
        assert!(db_path.exists(), "stale pre-source fixture file must exist");

        // The real function under test -- must heal the stale file, not
        // just successfully connect to it as-is.
        let heal_result = open_outputs_db(user_id, persona_id, key_hex).await;
        assert!(
            heal_result.is_ok(),
            "open_outputs_db must heal the stale pre-outputs_004 database, not error: {heal_result:?}"
        );

        let mut verify_conn = open_outputs_db(user_id, persona_id, key_hex)
            .await
            .expect("re-open must succeed");
        let row_err = sqlx::query("SELECT source FROM outputs LIMIT 0")
            .fetch_optional(&mut verify_conn)
            .await
            .err()
            .map(|e| e.to_string());

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert!(
            row_err.is_none(),
            "outputs.source (added in outputs_004.sql) must be queryable after opening a \
             pre-existing pre-outputs_004 outputs.db -- open_outputs_db must run pending \
             migrations on every open, not only when the file doesn't exist yet: {row_err:?}"
        );
    }

    // -- Ingestion (items.id=383, decisions.id=486) -------------------------

    const INGEST_KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";

    /// Real on-disk outputs.db via the actual migration path (matches
    /// open_outputs_db_self_heals_a_never_created_file's own real-file
    /// pattern) -- create_ingest_focus_run/save_ingested_output both
    /// self-heal via open_outputs_db exactly like every other public fn in
    /// this file, so no separate schema bootstrap is needed here.
    #[tokio::test]
    async fn create_ingest_focus_run_then_save_ingested_output_round_trips() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "ingest-user";
        let persona_id = "ingest-persona";

        let verify = async {
            let focus_run_id = create_ingest_focus_run(user_id, persona_id, INGEST_KEY_HEX)
                .await
                .expect("create_ingest_focus_run must satisfy outputs.focus_run_id's FK");

            let status = get_focus_run_status(user_id, persona_id, INGEST_KEY_HEX, &focus_run_id)
                .await
                .expect("query must succeed")
                .expect("the ingest-only run must exist");
            assert_eq!(
                status, "complete",
                "an ingest-only run never executes -- it starts and stays complete"
            );

            let output_id = save_ingested_output(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                "output-1",
                &focus_run_id,
                "ingested_document",
                "general",
                "travel",
                Some("entity-1"),
                "/fake/storage/path/v1.enc",
                "ryanair-confirmation.pdf",
                None, // opaque binary upload -- no text mirror
            )
            .await
            .expect("save_ingested_output must succeed with a real focus_run_id");
            assert_eq!(output_id, "output-1");

            let record = get_output(user_id, persona_id, INGEST_KEY_HEX, "output-1")
                .await
                .expect("query must succeed")
                .expect("the ingested row must be readable back via get_output");

            assert_eq!(record.source, "external_ingested");
            assert_eq!(record.focus_id, INGEST_PSEUDO_FOCUS_ID);
            assert_eq!(record.focus_slug.as_deref(), Some("travel"));
            assert_eq!(record.project_entity_id.as_deref(), Some("entity-1"));
            assert_eq!(
                record.storage_path.as_deref(),
                Some("/fake/storage/path/v1.enc")
            );
            assert_eq!(record.storage_version, 1);
            assert_eq!(
                record.original_filename.as_deref(),
                Some("ryanair-confirmation.pdf")
            );
            assert_eq!(
                record.content, None,
                "opaque binary uploads have no text mirror"
            );
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn each_ingest_upload_gets_its_own_focus_run_not_a_shared_singleton() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "ingest-user-2";
        let persona_id = "ingest-persona-2";

        let verify = async {
            let run_a = create_ingest_focus_run(user_id, persona_id, INGEST_KEY_HEX)
                .await
                .unwrap();
            let run_b = create_ingest_focus_run(user_id, persona_id, INGEST_KEY_HEX)
                .await
                .unwrap();
            assert_ne!(
                run_a, run_b,
                "one new ingest-only focus_run per upload, not a shared singleton"
            );
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn bump_ingested_document_version_increments_and_repoints_storage_path() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "ingest-user-3";
        let persona_id = "ingest-persona-3";

        let verify = async {
            let focus_run_id = create_ingest_focus_run(user_id, persona_id, INGEST_KEY_HEX)
                .await
                .unwrap();
            save_ingested_output(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                "output-v",
                &focus_run_id,
                "ingested_document",
                "general",
                "travel",
                None,
                "/fake/v1.enc",
                "doc.txt",
                Some("v1 text"),
            )
            .await
            .unwrap();

            let new_version = bump_ingested_document_version(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                "output-v",
                "/fake/v2.enc",
            )
            .await
            .expect("bump must succeed for an existing ingested row");
            assert_eq!(new_version, 2);

            let record = get_output(user_id, persona_id, INGEST_KEY_HEX, "output-v")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(record.storage_version, 2);
            assert_eq!(record.storage_path.as_deref(), Some("/fake/v2.enc"));
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn bump_ingested_document_version_rejects_a_nonexistent_id() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "ingest-user-4";
        let persona_id = "ingest-persona-4";

        let verify = async {
            let result = bump_ingested_document_version(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                "does-not-exist",
                "/fake/v2.enc",
            )
            .await;
            assert!(matches!(result, Err(OutputStoreError::Validation(_))));
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn list_outputs_source_filter_isolates_ingested_from_qr_generated() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "ingest-user-5";
        let persona_id = "ingest-persona-5";

        let verify = async {
            let qr_run_id = "run-qr";
            test_seed_focus_run(user_id, persona_id, INGEST_KEY_HEX, qr_run_id, "focus-1")
                .await
                .unwrap();
            save_output(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                qr_run_id,
                "note",
                "a qr-generated note",
                "general",
                None,
                None,
            )
            .await
            .unwrap();

            let ingest_run_id = create_ingest_focus_run(user_id, persona_id, INGEST_KEY_HEX)
                .await
                .unwrap();
            save_ingested_output(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                "output-ingested",
                &ingest_run_id,
                "ingested_document",
                "general",
                "focus-1",
                None,
                "/fake/v1.enc",
                "doc.txt",
                Some("ingested text"),
            )
            .await
            .unwrap();

            let qr_only = list_outputs(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                None,
                None,
                None,
                Some("qr_generated"),
            )
            .await
            .unwrap();
            assert_eq!(qr_only.len(), 1);
            assert_eq!(qr_only[0].source, "qr_generated");

            let ingested_only = list_outputs(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                None,
                None,
                None,
                Some("external_ingested"),
            )
            .await
            .unwrap();
            assert_eq!(ingested_only.len(), 1);
            assert_eq!(ingested_only[0].source, "external_ingested");

            let all = list_outputs(user_id, persona_id, INGEST_KEY_HEX, None, None, None, None)
                .await
                .unwrap();
            assert_eq!(
                all.len(),
                2,
                "source: None must mean no filter, same convention as focus_id/topic_id/output_type"
            );
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    // -- items.id=406: fact-identity persistence cascade ---------------------

    #[tokio::test]
    async fn fact_mentions_and_prior_decision_round_trip_against_a_real_encrypted_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "fact-cascade-user";
        let persona_id = "fact-cascade-persona";
        let key_hex = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";
        let focus_run_id = "run-fact-cascade-1";

        test_seed_focus_run(user_id, persona_id, key_hex, focus_run_id, "quick-ask")
            .await
            .expect("seed focus_run must succeed");

        // load_fact_mentions_for_run on an empty conversation: no mentions yet.
        let empty = load_fact_mentions_for_run(user_id, persona_id, key_hex, focus_run_id)
            .await
            .expect("query must succeed even with zero rows");
        assert!(empty.is_empty());

        // Record one mention, then read it back.
        insert_fact_mention(
            user_id,
            persona_id,
            key_hex,
            focus_run_id,
            "private_email",
            "hash-abc",
            "jane@example.com",
        )
        .await
        .expect("insert_fact_mention must succeed");

        let mentions = load_fact_mentions_for_run(user_id, persona_id, key_hex, focus_run_id)
            .await
            .expect("query must succeed");
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].category, "private_email");
        assert_eq!(mentions[0].fact_key, "hash-abc");
        assert_eq!(mentions[0].original_text, "jane@example.com");

        // No decision exists yet for this fact_key.
        let none_yet =
            find_consent_decision_for_fact(user_id, persona_id, key_hex, focus_run_id, "hash-abc")
                .await
                .expect("query must succeed");
        assert!(none_yet.is_none());

        // Auto-reapply writes a consent_decisions row keyed by the SAME
        // fact_key -- a later call in this same conversation must find it.
        write_auto_reapplied_consent_decision(
            user_id,
            persona_id,
            key_hex,
            focus_run_id,
            "generalize",
            Some("[email address]"),
            None,
            "private_email",
            "hash-abc",
            "jane@example.com",
        )
        .await
        .expect("write_auto_reapplied_consent_decision must succeed");

        let found =
            find_consent_decision_for_fact(user_id, persona_id, key_hex, focus_run_id, "hash-abc")
                .await
                .expect("query must succeed")
                .expect("prior decision must now be found");
        assert_eq!(found.decision, "generalize");
        assert_eq!(found.suggestion_text.as_deref(), Some("[email address]"));
        assert!(found.user_modified_text.is_none());

        // A DIFFERENT fact_key in the same run must not match.
        let different = find_consent_decision_for_fact(
            user_id,
            persona_id,
            key_hex,
            focus_run_id,
            "hash-unrelated",
        )
        .await
        .expect("query must succeed");
        assert!(different.is_none());

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn find_consent_decision_for_fact_is_scoped_to_focus_run_id() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "fact-scope-user";
        let persona_id = "fact-scope-persona";
        let key_hex = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";

        test_seed_focus_run(user_id, persona_id, key_hex, "run-a", "quick-ask")
            .await
            .unwrap();
        test_seed_focus_run(user_id, persona_id, key_hex, "run-b", "quick-ask")
            .await
            .unwrap();

        write_auto_reapplied_consent_decision(
            user_id,
            persona_id,
            key_hex,
            "run-a",
            "keep_private",
            None,
            None,
            "private_person",
            "hash-person-1",
            "Jane Doe",
        )
        .await
        .unwrap();

        // Same fact_key, but a DIFFERENT conversation -- conversation-scoped
        // persistence must not leak across focus_run_id (decisions.id=756:
        // cross-conversation reuse requires the explicit Persona-scoped
        // opt-in, a separate table entirely -- never this one).
        let cross_run =
            find_consent_decision_for_fact(user_id, persona_id, key_hex, "run-b", "hash-person-1")
                .await
                .unwrap();
        assert!(
            cross_run.is_none(),
            "consent_decisions lookup must be scoped to focus_run_id, not leak across conversations"
        );

        let same_run =
            find_consent_decision_for_fact(user_id, persona_id, key_hex, "run-a", "hash-person-1")
                .await
                .unwrap();
        assert!(same_run.is_some());

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    // -----------------------------------------------------------------
    // items.id=556 -- document relationship + export lifecycle
    // -----------------------------------------------------------------

    /// Test-only: flips an output's status directly (bypassing the
    /// lifecycle state machine, which is out of this item's scope) so
    /// export_output/return_output_from_export have a row in the right
    /// starting state to act on.
    async fn set_status_for_test(
        user_id: &str,
        persona_id: &str,
        key_hex: &str,
        output_id: &str,
        status: &str,
    ) {
        let mut conn = open_outputs_db(user_id, persona_id, key_hex).await.unwrap();
        sqlx::query("UPDATE outputs SET status = ? WHERE id = ?")
            .bind(status)
            .bind(output_id)
            .execute(&mut conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn save_ingested_output_defaults_document_relationship_to_reference() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "docrel-user-1";
        let persona_id = "docrel-persona-1";

        let verify = async {
            let focus_run_id = create_ingest_focus_run(user_id, persona_id, INGEST_KEY_HEX)
                .await
                .unwrap();
            save_ingested_output(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                "ingested-1",
                &focus_run_id,
                "ingested_document",
                "general",
                "travel",
                None,
                "/fake/v1.enc",
                "doc.txt",
                Some("v1 text"),
            )
            .await
            .unwrap();

            let record = get_output(user_id, persona_id, INGEST_KEY_HEX, "ingested-1")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                record.document_relationship, "reference",
                "an ingested document must default to 'reference', not the \
                 schema's own 'prime' default -- decisions.id=826"
            );
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn update_active_document_lets_an_ingested_document_supersede_a_qr_generated_one() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "docrel-user-2";
        let persona_id = "docrel-persona-2";

        let verify = async {
            let qr_run_id = "run-qr";
            test_seed_focus_run(user_id, persona_id, INGEST_KEY_HEX, qr_run_id, "focus-1")
                .await
                .unwrap();
            save_output(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                qr_run_id,
                "note",
                "the original qr-generated note",
                "general",
                Some("qr-doc"),
                None,
            )
            .await
            .unwrap();

            let ingest_run_id = create_ingest_focus_run(user_id, persona_id, INGEST_KEY_HEX)
                .await
                .unwrap();
            save_ingested_output(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                "ingested-doc",
                &ingest_run_id,
                "ingested_document",
                "general",
                "focus-1",
                None,
                "/fake/v1.enc",
                "doc.txt",
                Some("newer text"),
            )
            .await
            .unwrap();

            // An ingested document (source=external_ingested) supersedes a
            // qr_generated one -- the decoupled direction decisions.id=826
            // specifically calls out.
            update_active_document(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                "ingested-doc",
                "qr-doc",
            )
            .await
            .expect("update_active_document must succeed across source types");

            let new_record = get_output(user_id, persona_id, INGEST_KEY_HEX, "ingested-doc")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(new_record.document_relationship, "update");

            let prev_record = get_output(user_id, persona_id, INGEST_KEY_HEX, "qr-doc")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(prev_record.superseded_by.as_deref(), Some("ingested-doc"));
            assert!(
                prev_record.parent_output_id.is_none(),
                "parent_output_id belongs to 'fork', not 'update' -- must stay untouched"
            );
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn update_active_document_rejects_matching_ids() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "docrel-user-3";
        let persona_id = "docrel-persona-3";

        let verify = async {
            let result =
                update_active_document(user_id, persona_id, INGEST_KEY_HEX, "same-id", "same-id")
                    .await;
            assert!(matches!(result, Err(OutputStoreError::Validation(_))));
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn update_active_document_rolls_back_when_previous_output_id_does_not_exist() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "docrel-user-4";
        let persona_id = "docrel-persona-4";

        let verify = async {
            let run_id = "run-qr";
            test_seed_focus_run(user_id, persona_id, INGEST_KEY_HEX, run_id, "focus-1")
                .await
                .unwrap();
            save_output(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                run_id,
                "note",
                "a lone qr-generated note",
                "general",
                Some("qr-doc-2"),
                None,
            )
            .await
            .unwrap();

            let result = update_active_document(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                "qr-doc-2",
                "does-not-exist",
            )
            .await;
            assert!(matches!(result, Err(OutputStoreError::Validation(_))));

            // Rolled back -- the first UPDATE must not have stuck.
            let record = get_output(user_id, persona_id, INGEST_KEY_HEX, "qr-doc-2")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                record.document_relationship, "prime",
                "a failed update_active_document call must leave document_relationship untouched"
            );
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn export_output_transitions_finalized_to_potentially_stale_and_stamps_exported_at() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "export-user-1";
        let persona_id = "export-persona-1";

        let verify = async {
            let run_id = "run-1";
            test_seed_focus_run(user_id, persona_id, INGEST_KEY_HEX, run_id, "focus-1")
                .await
                .unwrap();
            save_output(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                run_id,
                "note",
                "a finished note",
                "general",
                Some("exp-doc"),
                None,
            )
            .await
            .unwrap();
            set_status_for_test(user_id, persona_id, INGEST_KEY_HEX, "exp-doc", "finalized").await;

            export_output(user_id, persona_id, INGEST_KEY_HEX, "exp-doc")
                .await
                .expect("export must succeed from finalized");

            let record = get_output(user_id, persona_id, INGEST_KEY_HEX, "exp-doc")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(record.status, "potentially-stale");
            assert!(record.exported_at.is_some());
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn export_output_rejects_a_non_finalized_status() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "export-user-2";
        let persona_id = "export-persona-2";

        let verify = async {
            let run_id = "run-1";
            test_seed_focus_run(user_id, persona_id, INGEST_KEY_HEX, run_id, "focus-1")
                .await
                .unwrap();
            // save_output leaves the row in status='draft'.
            save_output(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                run_id,
                "note",
                "still a draft",
                "general",
                Some("draft-doc"),
                None,
            )
            .await
            .unwrap();

            let result = export_output(user_id, persona_id, INGEST_KEY_HEX, "draft-doc").await;
            assert!(matches!(result, Err(OutputStoreError::Validation(_))));
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn return_output_from_export_transitions_back_and_clears_exported_at() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "export-user-3";
        let persona_id = "export-persona-3";

        let verify = async {
            let run_id = "run-1";
            test_seed_focus_run(user_id, persona_id, INGEST_KEY_HEX, run_id, "focus-1")
                .await
                .unwrap();
            save_output(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                run_id,
                "note",
                "a finished note",
                "general",
                Some("return-doc"),
                None,
            )
            .await
            .unwrap();
            set_status_for_test(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                "return-doc",
                "finalized",
            )
            .await;
            export_output(user_id, persona_id, INGEST_KEY_HEX, "return-doc")
                .await
                .unwrap();

            return_output_from_export(user_id, persona_id, INGEST_KEY_HEX, "return-doc")
                .await
                .expect("return must succeed from potentially-stale");

            let record = get_output(user_id, persona_id, INGEST_KEY_HEX, "return-doc")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(record.status, "finalized");
            assert!(record.exported_at.is_none());
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn return_output_from_export_rejects_a_non_potentially_stale_status() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "export-user-4";
        let persona_id = "export-persona-4";

        let verify = async {
            let run_id = "run-1";
            test_seed_focus_run(user_id, persona_id, INGEST_KEY_HEX, run_id, "focus-1")
                .await
                .unwrap();
            save_output(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                run_id,
                "note",
                "already finalized, never exported",
                "general",
                Some("never-exported-doc"),
                None,
            )
            .await
            .unwrap();
            set_status_for_test(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                "never-exported-doc",
                "finalized",
            )
            .await;

            let result = return_output_from_export(
                user_id,
                persona_id,
                INGEST_KEY_HEX,
                "never-exported-doc",
            )
            .await;
            assert!(matches!(result, Err(OutputStoreError::Validation(_))));
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }
}
