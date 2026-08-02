// src-tauri/src/persistence/source_registry_store.rs
//
// cb-11 (Stage 1) — source-of-truth registry and record modification-state
// machine. Foundation block, items.id=128, decisions.id=502 (D6-460),
// decisions.id=621 §11.
//
// WHAT THIS MODULE OWNS:
//   source_registry  — one row per contributing source per Focus, plus its
//                      lifecycle (active / archived / pending_refresh).
//   entities.source_registry_id / source_url / modification_state — the
//                      edge from a record back to where it came from, and
//                      what a refresh is allowed to do to it.
//
// WHAT IT DOES NOT OWN (Stage 2, not built here):
//   dedup_candidates generation and resolution, the three match strategies,
//   the side-by-side field diff, and merge-policy enforcement. The table
//   exists in personal_002.sql; nothing writes to it yet.
//
// THE FRAMEWORK IS GENERIC BY DESIGN. decisions.id=502 defines six
// declarations a Focus must supply (content fields, match strategy, source
// types, usage-history field names, P0 question wording, user-generated
// fields). No Focus has recorded them yet — Cooking is status='designed',
// not built. So nothing here is Cooking-shaped: focus_slug and source_type
// are caller-supplied strings, and the declaration struct arrives in
// Stage 2 where the dedup engine actually needs it.
//
// SECOND ADOPTER: decisions.id=617's synced household grants reconcile a
// recipient instance against the owner's through this same framework, with
// each recipient modelled as one more source in the registry. That is why
// source_type is unconstrained TEXT — see personal_002.sql. The source_type
// value naming another QR instance is deliberately NOT invented here;
// instance-sharing architecture is items.id=147 item 4, a separate pass.
//
// THE INVARIANT THIS MODULE EXISTS TO PROTECT (decisions.id=502):
//   User-generated data always survives source transitions, refresh cycles,
//   deduplication resolution, and status changes. It is never overwritten
//   by source data. Every function below that touches a record on behalf of
//   a source honours modification_state before writing.
//
// QUERY STYLE: runtime sqlx::query() only — no query!() macros.

use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::SqliteConnection;

use crate::persistence::entity_store::{Entity, EntityUpdate};
use crate::persistence::personal_store::{open_personal_db, PersonalStoreError};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// source_registry.status values (personal_002.sql).
///   active           currently contributing
///   archived         user migrated away; history preserved
///   pending_refresh  import is stale and known to be
const VALID_SOURCE_STATUSES: &[&str] = &["active", "archived", "pending_refresh"];

/// source_type values enumerated by decisions.id=502. The column itself is
/// unconstrained TEXT so that decisions.id=617's household-share source type
/// can be added as data rather than as a migration — this list is advisory,
/// used only by `is_known_source_type` for caller diagnostics.
pub const KNOWN_SOURCE_TYPES: &[&str] = &[
    "mealie_live",
    "mealie_import",
    "paprika_import",
    "bookmark_import",
    "pdf_import",
    "url_ingestion",
    "user_created",
    "qr_generated",
];

/// True when source_type is one decisions.id=502 enumerated. A false result
/// is NOT an error — it is expected for source types added after that
/// decision was written. Callers may log it; nothing here rejects on it.
pub fn is_known_source_type(source_type: &str) -> bool {
    KNOWN_SOURCE_TYPES.contains(&source_type)
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One `source_registry` row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceRegistryEntry {
    pub id: String,
    pub persona_id: String,
    pub focus_slug: String,
    pub source_type: String,
    /// Live-source connection settings (post-R1). Never holds a credential —
    /// standing_rules.id=50 keeps secrets out of any store Claude reads or
    /// writes, and decisions.id=502 defers live sources past R1 anyway.
    pub connection_config: Option<serde_json::Value>,
    pub last_imported_at: Option<String>,
    pub last_synced_at: Option<String>,
    pub status: String,
    pub created_at: String,
    pub extra_metadata: serde_json::Value,
}

/// What a source refresh proposes to do to one record, after
/// modification_state has been consulted. decisions.id=502's refresh rules,
/// expressed as a return value rather than as a side effect — the caller
/// decides what to do with a Conflict, and the user resolves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshVerdict {
    /// pristine — no user edits. The source update may be applied directly.
    AutoAccept,
    /// user_modified — the user has edited this record. Surface a conflict
    /// for per-field resolution. Never auto-apply.
    Conflict,
    /// user_created — QR is authoritative. Source updates never apply.
    Ignore,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

const SOURCE_COLUMNS: &str =
    "id, persona_id, focus_slug, source_type, connection_config, \
     last_imported_at, last_synced_at, status, created_at, extra_metadata";

fn row_to_source(row: &sqlx::sqlite::SqliteRow) -> Result<SourceRegistryEntry, PersonalStoreError> {
    let id: String = row.try_get("id")?;

    let connection_config: Option<String> = row.try_get("connection_config")?;
    let connection_config = match connection_config {
        Some(raw) => Some(serde_json::from_str(&raw).map_err(|e| {
            PersonalStoreError::Validation(format!(
                "source_registry.connection_config for id '{id}' is not valid JSON: {e}"
            ))
        })?),
        None => None,
    };

    let metadata_raw: String = row.try_get("extra_metadata")?;
    let extra_metadata: serde_json::Value =
        serde_json::from_str(&metadata_raw).map_err(|e| {
            PersonalStoreError::Validation(format!(
                "source_registry.extra_metadata for id '{id}' is not valid JSON: {e}"
            ))
        })?;

    Ok(SourceRegistryEntry {
        id,
        persona_id: row.try_get("persona_id")?,
        focus_slug: row.try_get("focus_slug")?,
        source_type: row.try_get("source_type")?,
        connection_config,
        last_imported_at: row.try_get("last_imported_at")?,
        last_synced_at: row.try_get("last_synced_at")?,
        status: row.try_get("status")?,
        created_at: row.try_get("created_at")?,
        extra_metadata,
    })
}

fn validate_source_status(status: &str) -> Result<(), PersonalStoreError> {
    if !VALID_SOURCE_STATUSES.contains(&status) {
        return Err(PersonalStoreError::Validation(format!(
            "Unknown source status '{status}'. Must be one of: {}.",
            VALID_SOURCE_STATUSES.join(", ")
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Registry — create
// ---------------------------------------------------------------------------

/// Register a contributing source for a Focus. Returns the generated id.
///
/// Registering the same source_type twice for one Focus is allowed and not
/// an error: decisions.id=502 explicitly supports several sources of the
/// same kind (two Paprika exports from different years, or under
/// decisions.id=617, several household recipients each syncing separately).
/// Distinguishing them is the caller's job via extra_metadata.
#[allow(clippy::too_many_arguments)]
pub async fn register_source(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_slug: &str,
    source_type: &str,
    connection_config: Option<serde_json::Value>,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    register_source_conn(
        &mut conn,
        persona_id,
        focus_slug,
        source_type,
        connection_config,
        extra_metadata,
    )
    .await
}

pub(crate) async fn register_source_conn(
    conn: &mut SqliteConnection,
    persona_id: &str,
    focus_slug: &str,
    source_type: &str,
    connection_config: Option<serde_json::Value>,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, PersonalStoreError> {
    if focus_slug.trim().is_empty() {
        return Err(PersonalStoreError::Validation(
            "focus_slug is required and cannot be blank.".to_owned(),
        ));
    }
    if source_type.trim().is_empty() {
        return Err(PersonalStoreError::Validation(
            "source_type is required and cannot be blank.".to_owned(),
        ));
    }
    if !is_known_source_type(source_type) {
        // Not an error — decisions.id=617 needs a source type 502 never
        // enumerated. Logged so an actual typo is still visible.
        log::info!(
            "source_registry: registering source_type '{source_type}' \
             not enumerated in decisions.id=502"
        );
    }

    let new_id = uuid::Uuid::new_v4().to_string();
    let config_json = match &connection_config {
        Some(v) => Some(serde_json::to_string(v).map_err(|e| {
            PersonalStoreError::Validation(format!("connection_config not serializable: {e}"))
        })?),
        None => None,
    };
    let metadata_json = serde_json::to_string(&extra_metadata.unwrap_or(serde_json::json!({})))
        .map_err(|e| {
            PersonalStoreError::Validation(format!("extra_metadata not serializable: {e}"))
        })?;

    sqlx::query(
        "INSERT INTO source_registry
         (id, persona_id, focus_slug, source_type, connection_config,
          status, created_at, extra_metadata)
         VALUES (?, ?, ?, ?, ?, 'active', ?, ?)",
    )
    .bind(&new_id)
    .bind(persona_id)
    .bind(focus_slug)
    .bind(source_type)
    .bind(config_json)
    .bind(crate::providers::utils::now())
    .bind(&metadata_json)
    .execute(&mut *conn)
    .await?;

    Ok(new_id)
}

// ---------------------------------------------------------------------------
// Registry — read
// ---------------------------------------------------------------------------

/// Fetch one source. Ok(None) when it does not exist.
pub async fn get_source(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    source_id: &str,
) -> Result<Option<SourceRegistryEntry>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    get_source_conn(&mut conn, source_id).await
}

pub(crate) async fn get_source_conn(
    conn: &mut SqliteConnection,
    source_id: &str,
) -> Result<Option<SourceRegistryEntry>, PersonalStoreError> {
    let row = sqlx::query(&format!(
        "SELECT {SOURCE_COLUMNS} FROM source_registry WHERE id = ?"
    ))
    .bind(source_id)
    .fetch_optional(&mut *conn)
    .await?;

    match row {
        Some(r) => Ok(Some(row_to_source(&r)?)),
        None => Ok(None),
    }
}

/// List a Focus's sources, newest first. `status` None lists every status —
/// including archived, so a source-transition history stays visible.
pub async fn list_sources(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_slug: &str,
    status: Option<&str>,
) -> Result<Vec<SourceRegistryEntry>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    list_sources_conn(&mut conn, focus_slug, status).await
}

pub(crate) async fn list_sources_conn(
    conn: &mut SqliteConnection,
    focus_slug: &str,
    status: Option<&str>,
) -> Result<Vec<SourceRegistryEntry>, PersonalStoreError> {
    if let Some(s) = status {
        validate_source_status(s)?;
    }

    let rows = match status {
        Some(s) => {
            sqlx::query(&format!(
                "SELECT {SOURCE_COLUMNS} FROM source_registry \
                 WHERE focus_slug = ? AND status = ? ORDER BY created_at DESC"
            ))
            .bind(focus_slug)
            .bind(s)
            .fetch_all(&mut *conn)
            .await?
        }
        None => {
            sqlx::query(&format!(
                "SELECT {SOURCE_COLUMNS} FROM source_registry \
                 WHERE focus_slug = ? ORDER BY created_at DESC"
            ))
            .bind(focus_slug)
            .fetch_all(&mut *conn)
            .await?
        }
    };

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(row_to_source(row)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Registry — lifecycle
// ---------------------------------------------------------------------------

/// Move a source to a new lifecycle status.
///
/// decisions.id=502's source-transition rule (user migrates Paprika ->
/// Mealie: newer source becomes active, prior source archived) is expressed
/// as two explicit calls by the caller, not as an implicit side effect here.
/// Archiving is never destructive: the source's records keep their
/// source_registry_id, and its history stays queryable.
pub async fn set_source_status(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    source_id: &str,
    status: &str,
) -> Result<(), PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    set_source_status_conn(&mut conn, source_id, status).await
}

pub(crate) async fn set_source_status_conn(
    conn: &mut SqliteConnection,
    source_id: &str,
    status: &str,
) -> Result<(), PersonalStoreError> {
    validate_source_status(status)?;

    let result = sqlx::query("UPDATE source_registry SET status = ? WHERE id = ?")
        .bind(status)
        .bind(source_id)
        .execute(&mut *conn)
        .await?;

    if result.rows_affected() == 0 {
        return Err(PersonalStoreError::Validation(format!(
            "No source with id '{source_id}' — nothing was updated."
        )));
    }
    Ok(())
}

/// Record that an import from this source just completed: stamps
/// last_imported_at and returns the source to 'active' (clearing a
/// pending_refresh state). Refresh is user-triggered in R1 —
/// decisions.id=502 defers background auto-sync past R1 — so nothing calls
/// this on a timer.
pub async fn mark_source_imported(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    source_id: &str,
) -> Result<(), PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    mark_source_imported_conn(&mut conn, source_id).await
}

pub(crate) async fn mark_source_imported_conn(
    conn: &mut SqliteConnection,
    source_id: &str,
) -> Result<(), PersonalStoreError> {
    let result = sqlx::query(
        "UPDATE source_registry SET last_imported_at = ?, status = 'active' WHERE id = ?",
    )
    .bind(crate::providers::utils::now())
    .bind(source_id)
    .execute(&mut *conn)
    .await?;

    if result.rows_affected() == 0 {
        return Err(PersonalStoreError::Validation(format!(
            "No source with id '{source_id}' — nothing was updated."
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Records — the source edge and the modification-state machine
// ---------------------------------------------------------------------------

/// Create a record that came from a registered source.
///
/// Equivalent to entity_store::create_entity followed by stamping the three
/// columns this module owns — done under a SAVEPOINT so a record can never
/// exist with a half-written source edge. The record starts as `pristine`:
/// imported, unedited, and therefore safe to auto-update on the next
/// refresh (decisions.id=502).
///
/// Contrast with entity_store::create_entity, which leaves the record
/// `user_created` with no source — the correct state for manual entry and
/// QR generation.
#[allow(clippy::too_many_arguments)]
pub async fn import_record(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    source_id: &str,
    entity_type: &str,
    display_name: &str,
    aliases: &[String],
    source_url: Option<&str>,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    import_record_conn(
        &mut conn,
        source_id,
        entity_type,
        display_name,
        aliases,
        source_url,
        extra_metadata,
    )
    .await
}

pub(crate) async fn import_record_conn(
    conn: &mut SqliteConnection,
    source_id: &str,
    entity_type: &str,
    display_name: &str,
    aliases: &[String],
    source_url: Option<&str>,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, PersonalStoreError> {
    // Referential integrity by hand — PRAGMA foreign_keys is off codebase
    // wide, so a declared FK would not catch this.
    if get_source_conn(conn, source_id).await?.is_none() {
        return Err(PersonalStoreError::Validation(format!(
            "No source with id '{source_id}' — register the source before \
             importing records from it."
        )));
    }

    sqlx::query("SAVEPOINT import_record")
        .execute(&mut *conn)
        .await?;

    let step: Result<String, PersonalStoreError> = async {
        let new_id = crate::persistence::entity_store::create_entity_conn(
            conn,
            entity_type,
            display_name,
            aliases,
            None,
            extra_metadata,
        )
        .await?;

        sqlx::query(
            "UPDATE entities
             SET source_registry_id = ?, source_url = ?,
                 modification_state = 'pristine'
             WHERE id = ?",
        )
        .bind(source_id)
        .bind(source_url)
        .bind(&new_id)
        .execute(&mut *conn)
        .await?;

        Ok(new_id)
    }
    .await;

    match step {
        Ok(id) => {
            sqlx::query("RELEASE import_record")
                .execute(&mut *conn)
                .await?;
            Ok(id)
        }
        Err(e) => {
            if let Err(rollback_err) = sqlx::query("ROLLBACK TO import_record")
                .execute(&mut *conn)
                .await
            {
                log::error!("Savepoint rollback failed in import_record: {rollback_err}");
            }
            let _ = sqlx::query("RELEASE import_record")
                .execute(&mut *conn)
                .await;
            Err(e)
        }
    }
}

/// Ids of every record attributed to a source, in insertion-stable id order.
///
/// Returns ids rather than full Entity rows so the SELECT column list stays
/// owned by entity_store alone (P4) — callers wanting detail pass each id to
/// entity_store::get_entity. This is the query per-source refresh and source
/// transition both start from.
pub async fn list_record_ids_for_source(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    source_id: &str,
) -> Result<Vec<String>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    list_record_ids_for_source_conn(&mut conn, source_id).await
}

pub(crate) async fn list_record_ids_for_source_conn(
    conn: &mut SqliteConnection,
    source_id: &str,
) -> Result<Vec<String>, PersonalStoreError> {
    let rows = sqlx::query(
        "SELECT id FROM entities WHERE source_registry_id = ? ORDER BY id",
    )
    .bind(source_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(row.try_get("id")?);
    }
    Ok(out)
}

/// What a source refresh may do to this record, per decisions.id=502.
///
/// Read-only. It decides nothing and writes nothing — the caller applies an
/// AutoAccept, surfaces a Conflict to the user, and skips an Ignore. Keeping
/// the verdict separate from the action is what makes "user-generated data
/// is never overwritten by source data" checkable rather than aspirational.
///
/// Ok(None) when the record does not exist.
pub async fn refresh_verdict(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    entity_id: &str,
) -> Result<Option<RefreshVerdict>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    refresh_verdict_conn(&mut conn, entity_id).await
}

pub(crate) async fn refresh_verdict_conn(
    conn: &mut SqliteConnection,
    entity_id: &str,
) -> Result<Option<RefreshVerdict>, PersonalStoreError> {
    let record: Option<Entity> =
        crate::persistence::entity_store::get_entity_conn(conn, entity_id).await?;

    let record = match record {
        Some(r) => r,
        None => return Ok(None),
    };

    Ok(Some(match record.modification_state.as_str() {
        "pristine" => RefreshVerdict::AutoAccept,
        "user_modified" => RefreshVerdict::Conflict,
        "user_created" => RefreshVerdict::Ignore,
        other => {
            // The CHECK constraint makes this unreachable through any
            // supported write path. If it happens anyway, treat it as a
            // conflict: the safe direction is always "ask the user", never
            // "overwrite their data".
            log::warn!(
                "entities.modification_state '{other}' on record \
                 '{entity_id}' is not a known state — treating as Conflict"
            );
            RefreshVerdict::Conflict
        }
    }))
}

/// Record that the user has edited an imported record, moving it
/// pristine -> user_modified so future refreshes surface a conflict instead
/// of silently overwriting the edit.
///
/// A user_created record stays user_created — it was never source-derived,
/// so there is nothing for a refresh to conflict with. Calling this on one
/// is a no-op, not an error: an edit path should not have to ask where the
/// record came from before saving.
pub async fn mark_record_user_modified(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    entity_id: &str,
) -> Result<(), PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    mark_record_user_modified_conn(&mut conn, entity_id).await
}

pub(crate) async fn mark_record_user_modified_conn(
    conn: &mut SqliteConnection,
    entity_id: &str,
) -> Result<(), PersonalStoreError> {
    let result = sqlx::query(
        "UPDATE entities SET modification_state = 'user_modified'
         WHERE id = ? AND modification_state = 'pristine'",
    )
    .bind(entity_id)
    .execute(&mut *conn)
    .await?;

    if result.rows_affected() == 0 {
        // Either the record does not exist, or it is already user_modified,
        // or it is user_created. Distinguish the first from the other two —
        // a caller editing a nonexistent record has a bug.
        if crate::persistence::entity_store::get_entity_conn(conn, entity_id)
            .await?
            .is_none()
        {
            return Err(PersonalStoreError::Validation(format!(
                "No entity with id '{entity_id}' — nothing was updated."
            )));
        }
    }
    Ok(())
}

/// Mark records the source no longer has. decisions.id=502: QR keeps them,
/// excludes them from default views, and tells the user on refresh —
/// "[N] records were removed from [source] — they're still in QR and can be
/// deleted or kept." Recoverable, never a hard delete.
///
/// Returns how many records changed state. Records already in a non-active
/// status are left alone, so re-running a refresh does not disturb a
/// tombstone or an archive the user set deliberately.
pub async fn mark_records_deleted_in_source(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    source_id: &str,
    missing_entity_ids: &[String],
) -> Result<u64, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    mark_records_deleted_in_source_conn(&mut conn, source_id, missing_entity_ids).await
}

pub(crate) async fn mark_records_deleted_in_source_conn(
    conn: &mut SqliteConnection,
    source_id: &str,
    missing_entity_ids: &[String],
) -> Result<u64, PersonalStoreError> {
    if missing_entity_ids.is_empty() {
        return Ok(0);
    }

    let mut changed: u64 = 0;
    for entity_id in missing_entity_ids {
        // Scoped to the source that reported the absence: one source must
        // not be able to tombstone another source's records.
        let result = sqlx::query(
            "UPDATE entities SET status = 'deleted_in_source'
             WHERE id = ? AND source_registry_id = ? AND status = 'active'",
        )
        .bind(entity_id)
        .bind(source_id)
        .execute(&mut *conn)
        .await?;
        changed += result.rows_affected();
    }

    Ok(changed)
}

/// Apply a source update to a record, but only where decisions.id=502 says
/// it is safe to do so without asking. Returns the verdict that was acted
/// on, so the caller can surface Conflict and Ignore to the user rather than
/// discovering nothing happened.
///
/// This is the one place in the module that writes source data onto a
/// record, and it refuses to do so for anything but a pristine record.
pub async fn apply_source_update(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    entity_id: &str,
    update: &EntityUpdate,
) -> Result<RefreshVerdict, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    apply_source_update_conn(&mut conn, entity_id, update).await
}

pub(crate) async fn apply_source_update_conn(
    conn: &mut SqliteConnection,
    entity_id: &str,
    update: &EntityUpdate,
) -> Result<RefreshVerdict, PersonalStoreError> {
    let verdict = refresh_verdict_conn(conn, entity_id).await?.ok_or_else(|| {
        PersonalStoreError::Validation(format!(
            "No entity with id '{entity_id}' — nothing to refresh."
        ))
    })?;

    if verdict == RefreshVerdict::AutoAccept {
        crate::persistence::entity_store::update_entity_conn(conn, entity_id, update).await?;
    }

    Ok(verdict)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::entity_store::{
        create_entity_conn, get_entity_conn, retire_entity_conn, EntityUpdate,
    };
    use crate::persistence::migrations::parse_statements;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;

    const V1: &str = include_str!("../../schema/personal_001.sql");
    const V2: &str = include_str!("../../schema/personal_002.sql");
    const V3: &str = include_str!("../../schema/personal_003.sql");

    async fn test_db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for schema in [V1, V2, V3] {
            for stmt in parse_statements(schema) {
                sqlx::query(&stmt)
                    .execute(&mut conn)
                    .await
                    .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{stmt}"));
            }
        }
        conn
    }

    async fn a_source(conn: &mut SqliteConnection) -> String {
        register_source_conn(conn, "persona-1", "cooking", "paprika_import", None, None)
            .await
            .expect("register failed")
    }

    // -- registry -----------------------------------------------------------

    #[tokio::test]
    async fn register_then_get_round_trips() {
        let mut conn = test_db().await;
        let id = register_source_conn(
            &mut conn,
            "persona-1",
            "cooking",
            "mealie_import",
            Some(serde_json::json!({"base_url": "http://localhost"})),
            Some(serde_json::json!({"label": "2024 export"})),
        )
        .await
        .unwrap();

        let s = get_source_conn(&mut conn, &id).await.unwrap().unwrap();
        assert_eq!(s.persona_id, "persona-1");
        assert_eq!(s.focus_slug, "cooking");
        assert_eq!(s.source_type, "mealie_import");
        assert_eq!(s.status, "active", "a new source starts active");
        assert_eq!(
            s.connection_config,
            Some(serde_json::json!({"base_url": "http://localhost"}))
        );
        assert_eq!(s.extra_metadata, serde_json::json!({"label": "2024 export"}));
        assert!(s.last_imported_at.is_none());
        assert!(s.last_synced_at.is_none());
    }

    #[tokio::test]
    async fn get_missing_source_is_none_not_error() {
        let mut conn = test_db().await;
        assert!(get_source_conn(&mut conn, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn register_rejects_blank_required_fields() {
        let mut conn = test_db().await;
        assert!(
            register_source_conn(&mut conn, "p", "  ", "pdf_import", None, None)
                .await
                .is_err()
        );
        assert!(
            register_source_conn(&mut conn, "p", "cooking", "", None, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unenumerated_source_type_is_accepted_not_rejected() {
        // decisions.id=617's household-share source type is not in
        // decisions.id=502's list. Registering it must work.
        let mut conn = test_db().await;
        let id = register_source_conn(
            &mut conn,
            "persona-1",
            "cooking",
            "qr_household_share",
            None,
            None,
        )
        .await
        .expect("an unenumerated source_type must still register");

        let s = get_source_conn(&mut conn, &id).await.unwrap().unwrap();
        assert_eq!(s.source_type, "qr_household_share");
        assert!(!is_known_source_type("qr_household_share"));
        assert!(is_known_source_type("paprika_import"));
    }

    #[tokio::test]
    async fn list_sources_filters_by_focus_and_status() {
        let mut conn = test_db().await;
        let paprika = a_source(&mut conn).await;
        register_source_conn(&mut conn, "persona-1", "cooking", "mealie_import", None, None)
            .await
            .unwrap();
        register_source_conn(&mut conn, "persona-1", "travel", "pdf_import", None, None)
            .await
            .unwrap();

        let cooking = list_sources_conn(&mut conn, "cooking", None).await.unwrap();
        assert_eq!(cooking.len(), 2, "travel's source must not appear");

        set_source_status_conn(&mut conn, &paprika, "archived")
            .await
            .unwrap();

        let active = list_sources_conn(&mut conn, "cooking", Some("active"))
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        let archived = list_sources_conn(&mut conn, "cooking", Some("archived"))
            .await
            .unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, paprika);
    }

    #[tokio::test]
    async fn source_status_transitions_are_validated() {
        let mut conn = test_db().await;
        let id = a_source(&mut conn).await;
        for status in VALID_SOURCE_STATUSES {
            set_source_status_conn(&mut conn, &id, status)
                .await
                .unwrap_or_else(|e| panic!("status '{status}' rejected by the DB: {e}"));
        }
        assert!(set_source_status_conn(&mut conn, &id, "invented").await.is_err());
        assert!(set_source_status_conn(&mut conn, "no-such-id", "active").await.is_err());
    }

    #[tokio::test]
    async fn mark_source_imported_stamps_and_reactivates() {
        let mut conn = test_db().await;
        let id = a_source(&mut conn).await;
        set_source_status_conn(&mut conn, &id, "pending_refresh")
            .await
            .unwrap();

        mark_source_imported_conn(&mut conn, &id).await.unwrap();

        let s = get_source_conn(&mut conn, &id).await.unwrap().unwrap();
        assert!(s.last_imported_at.is_some(), "import must be stamped");
        assert_eq!(s.status, "active", "a completed import clears pending_refresh");
    }

    // -- record source edge -------------------------------------------------

    #[tokio::test]
    async fn imported_record_starts_pristine_and_carries_its_source() {
        let mut conn = test_db().await;
        let source = a_source(&mut conn).await;

        let id = import_record_conn(
            &mut conn,
            &source,
            "recipe",
            "Sourdough",
            &["pain au levain".to_owned()],
            Some("https://example.test/r/1"),
            None,
        )
        .await
        .unwrap();

        let e = get_entity_conn(&mut conn, &id).await.unwrap().unwrap();
        assert_eq!(e.modification_state, "pristine");
        assert_eq!(e.source_registry_id, Some(source));
        assert_eq!(e.source_url, Some("https://example.test/r/1".to_owned()));
        assert_eq!(e.status, "active");
        assert_eq!(e.aliases, vec!["pain au levain"]);
    }

    #[tokio::test]
    async fn manually_created_record_has_no_source_and_is_user_created() {
        let mut conn = test_db().await;
        let id = create_entity_conn(&mut conn, "recipe", "Scratch Recipe", &[], None, None)
            .await
            .unwrap();
        let e = get_entity_conn(&mut conn, &id).await.unwrap().unwrap();
        assert_eq!(e.modification_state, "user_created");
        assert!(e.source_registry_id.is_none());
        assert!(e.source_url.is_none());
    }

    #[tokio::test]
    async fn import_rejects_an_unregistered_source_and_writes_nothing() {
        let mut conn = test_db().await;
        assert!(
            import_record_conn(&mut conn, "ghost-source", "recipe", "X", &[], None, None)
                .await
                .is_err()
        );
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM entities")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count, 0, "a rejected import must leave no partial record");
    }

    #[tokio::test]
    async fn list_record_ids_is_scoped_to_one_source() {
        let mut conn = test_db().await;
        let a = a_source(&mut conn).await;
        let b = register_source_conn(&mut conn, "persona-1", "cooking", "mealie_import", None, None)
            .await
            .unwrap();

        let from_a = import_record_conn(&mut conn, &a, "recipe", "A1", &[], None, None)
            .await
            .unwrap();
        import_record_conn(&mut conn, &b, "recipe", "B1", &[], None, None)
            .await
            .unwrap();
        create_entity_conn(&mut conn, "recipe", "Manual", &[], None, None)
            .await
            .unwrap();

        let ids = list_record_ids_for_source_conn(&mut conn, &a).await.unwrap();
        assert_eq!(ids, vec![from_a], "only source A's records");
    }

    // -- modification-state machine -----------------------------------------

    #[tokio::test]
    async fn refresh_verdict_follows_modification_state() {
        let mut conn = test_db().await;
        let source = a_source(&mut conn).await;

        let imported = import_record_conn(&mut conn, &source, "recipe", "Imported", &[], None, None)
            .await
            .unwrap();
        assert_eq!(
            refresh_verdict_conn(&mut conn, &imported).await.unwrap(),
            Some(RefreshVerdict::AutoAccept),
            "pristine records accept source updates"
        );

        mark_record_user_modified_conn(&mut conn, &imported).await.unwrap();
        assert_eq!(
            refresh_verdict_conn(&mut conn, &imported).await.unwrap(),
            Some(RefreshVerdict::Conflict),
            "an edited record must surface a conflict, never auto-update"
        );

        let manual = create_entity_conn(&mut conn, "recipe", "Manual", &[], None, None)
            .await
            .unwrap();
        assert_eq!(
            refresh_verdict_conn(&mut conn, &manual).await.unwrap(),
            Some(RefreshVerdict::Ignore),
            "QR is authoritative for user_created records"
        );

        assert!(refresh_verdict_conn(&mut conn, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn marking_user_modified_never_downgrades_user_created() {
        let mut conn = test_db().await;
        let manual = create_entity_conn(&mut conn, "recipe", "Manual", &[], None, None)
            .await
            .unwrap();

        mark_record_user_modified_conn(&mut conn, &manual)
            .await
            .expect("editing a QR-origin record is not an error");

        let e = get_entity_conn(&mut conn, &manual).await.unwrap().unwrap();
        assert_eq!(
            e.modification_state, "user_created",
            "a record that was never source-derived stays QR-authoritative"
        );

        assert!(
            mark_record_user_modified_conn(&mut conn, "no-such-id")
                .await
                .is_err(),
            "but a nonexistent record is a caller bug"
        );
    }

    #[tokio::test]
    async fn source_update_applies_to_pristine_and_refuses_the_rest() {
        let mut conn = test_db().await;
        let source = a_source(&mut conn).await;
        let id = import_record_conn(&mut conn, &source, "recipe", "Original", &[], None, None)
            .await
            .unwrap();

        let rename = |name: &str| EntityUpdate {
            display_name: Some(name.to_owned()),
            ..Default::default()
        };

        let verdict = apply_source_update_conn(&mut conn, &id, &rename("From Source"))
            .await
            .unwrap();
        assert_eq!(verdict, RefreshVerdict::AutoAccept);
        assert_eq!(
            get_entity_conn(&mut conn, &id).await.unwrap().unwrap().display_name,
            "From Source"
        );

        // Once the user has edited it, the same call must not overwrite.
        mark_record_user_modified_conn(&mut conn, &id).await.unwrap();
        let verdict = apply_source_update_conn(&mut conn, &id, &rename("Clobbered"))
            .await
            .unwrap();
        assert_eq!(verdict, RefreshVerdict::Conflict);
        assert_eq!(
            get_entity_conn(&mut conn, &id).await.unwrap().unwrap().display_name,
            "From Source",
            "user-generated data is never overwritten by source data"
        );
    }

    // -- deleted in source --------------------------------------------------

    #[tokio::test]
    async fn deleted_in_source_is_recoverable_and_source_scoped() {
        let mut conn = test_db().await;
        let a = a_source(&mut conn).await;
        let b = register_source_conn(&mut conn, "persona-1", "cooking", "mealie_import", None, None)
            .await
            .unwrap();

        let from_a = import_record_conn(&mut conn, &a, "recipe", "A1", &[], None, None)
            .await
            .unwrap();
        let from_b = import_record_conn(&mut conn, &b, "recipe", "B1", &[], None, None)
            .await
            .unwrap();

        // Source A cannot tombstone source B's record.
        let changed = mark_records_deleted_in_source_conn(
            &mut conn,
            &a,
            &[from_a.clone(), from_b.clone()],
        )
        .await
        .unwrap();
        assert_eq!(changed, 1, "only A's own record may change");

        assert_eq!(
            get_entity_conn(&mut conn, &from_a).await.unwrap().unwrap().status,
            "deleted_in_source"
        );
        assert_eq!(
            get_entity_conn(&mut conn, &from_b).await.unwrap().unwrap().status,
            "active"
        );

        // The record is still there — recoverable, never hard-deleted.
        assert!(get_entity_conn(&mut conn, &from_a).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn refresh_does_not_disturb_a_status_the_user_set() {
        let mut conn = test_db().await;
        let source = a_source(&mut conn).await;
        let id = import_record_conn(&mut conn, &source, "recipe", "Tombstoned", &[], None, None)
            .await
            .unwrap();
        retire_entity_conn(&mut conn, &id, "user_deleted").await.unwrap();

        let changed =
            mark_records_deleted_in_source_conn(&mut conn, &source, &[id.clone()])
                .await
                .unwrap();
        assert_eq!(changed, 0, "a deliberate tombstone must survive a refresh");
        assert_eq!(
            get_entity_conn(&mut conn, &id).await.unwrap().unwrap().status,
            "user_deleted"
        );
    }

    #[tokio::test]
    async fn marking_an_empty_missing_list_is_a_no_op() {
        let mut conn = test_db().await;
        let source = a_source(&mut conn).await;
        assert_eq!(
            mark_records_deleted_in_source_conn(&mut conn, &source, &[])
                .await
                .unwrap(),
            0
        );
    }
}
