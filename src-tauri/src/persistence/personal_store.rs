// src-tauri/src/persistence/personal_store.rs
//
// Personal field and voice profile data access layer.
// Operates on personal.db — per-user, per-persona, SQLCipher encrypted.
// Path: /users/{user_id}/personas/{persona_id}/personal.db
//
// Field encryption note:
//   The entire DB is SQLCipher-encrypted at file level — no plaintext on disk.
//   HKDF per-field encryption (additional layer) activates in Layer 8.
//   The store API is encryption-agnostic — callers pass field_value as str.
//
// Ownership scopes:
//   self:     written and read by this user only (default)
//   group:    shared with a context group (Release 2 UX)
//   instance: instance-wide; general/personal sensitivity only.
//             Enforced at write time — medical/financial blocked here.
//
// Short-field warning:
//   Gate2 uses MIN_MATCH_LENGTH = 4 for substring scanning.
//   save_personal_field() warns (via log) when a medical/financial field
//   has a short value — Gate2 cannot detect it in model responses.
//
// Voice profile value validation (D5-151):
//   save_voice_profile_entry() validates values at write time.
//   Rejects values containing PII patterns or exceeding word-count ceiling.
//
// disclosure_log is NEVER deleted — permanent audit trail (D6-198).
// delete_disclosure_log does NOT exist in this module. Do not add it.
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

use crate::conductor::types::{
    EntityFact, PersonalDBDecryptionError, PersonalField, PersonalTrack,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Gate2 minimum match length — fields shorter than this cannot be detected
/// in model responses by substring scan.
const MIN_MATCH_LENGTH: usize = 4;

/// Export sensitivity ceiling — fields above this severity are never exported.
/// general=1, personal=2, medical=3, financial=4.
const EXPORT_SENSITIVITY_CEILING: i32 = 2;

/// Export schema version string — matches Python oracle.
/// Kept as string per export contract; downstream consumers parse it as-is.
const EXPORT_SCHEMA_VERSION: &str = "1.0";

/// Instance-scope sensitivity ceiling (general and personal only).
const INSTANCE_SCOPE_MAX_SEVERITY: i32 = 2;

/// Voice profile word count ceiling (D5-151).
const VOICE_VALUE_MAX_WORDS: usize = 12;

/// Voice profile precedence levels (personal_001.sql: BETWEEN 1 AND 5).
/// Lower value = lower precedence. Higher value overwrites for same attribute.
const VOICE_PRECEDENCE_MODEL_BASELINE: i32 = 1;
#[allow(dead_code)]
const VOICE_PRECEDENCE_SPECIALIST_DEFAULTS: i32 = 2;
/// Global precedence — entries stored with persona_id = NULL.
const VOICE_PRECEDENCE_GLOBAL: i32 = 3;
#[allow(dead_code)]
const VOICE_PRECEDENCE_PERSONA: i32 = 4;
/// Writing context — applied at Step 8, not loaded at INITIALIZE.
const VOICE_PRECEDENCE_WRITING_CONTEXT: i32 = 5;

const VOICE_VALUE_REJECTION_MSG: &str =
    "We couldn't save that voice preference — it looks like it contains \
     personal details. Voice preferences describe how you communicate, \
     not who you are. Try something like 'professional and direct' instead.";

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum PersonalStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Decryption error: {0}")]
    Decryption(#[from] PersonalDBDecryptionError),
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

fn get_personal_db_path(user_id: &str, persona_id: &str) -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("personal.db")
}

// ---------------------------------------------------------------------------
// DB opener
// ---------------------------------------------------------------------------

/// Open personal.db with SQLCipher key.
/// Caller supplies bare hex; store wraps it in SQLCipher x'...' syntax.
/// PRAGMA key fires before journal_mode via SqliteConnectOptions (D6-346).
pub(crate) async fn open_personal_db(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, PersonalStoreError> {
    let db_path = get_personal_db_path(user_id, persona_id);

    if !db_path.exists() {
        return Err(PersonalStoreError::Decryption(PersonalDBDecryptionError {
            plain_language: "Quiet Rabbit couldn't open your personal information. \
                             Your session may have expired. Please log in again."
                .to_owned(),
        }));
    }

    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let journal_mode = if network_storage { "DELETE" } else { "WAL" };

    let conn = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .pragma("key", format!("x'{key_hex}'"))
        .pragma("journal_mode", journal_mode)
        .connect()
        .await
        .map_err(|e| {
            let msg = e.to_string().to_lowercase();
            if msg.contains("not a database") || msg.contains("file is not a database") {
                PersonalStoreError::Decryption(PersonalDBDecryptionError {
                    plain_language: "Quiet Rabbit couldn't open your personal information. \
                                     Your session may have expired. Please log in again."
                        .to_owned(),
                })
            } else {
                PersonalStoreError::Database(e)
            }
        })?;

    Ok(conn)
}

// ---------------------------------------------------------------------------
// Load (Phase 3 INITIALIZE)
// ---------------------------------------------------------------------------

/// Load all personal fields and voice profile for a user+persona.
/// Returns an unsealed PersonalTrack — caller (lifecycle) seals it.
/// Called during Phase 3 INITIALIZE.
pub async fn load_personal_track(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<PersonalTrack, PersonalStoreError> {
    if key_hex.is_empty() {
        return Err(PersonalStoreError::Decryption(PersonalDBDecryptionError {
            plain_language: "Your session has expired. Please log in again.".to_owned(),
        }));
    }

    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    let mut track = PersonalTrack::new();
    // items.id=170 fix (2026-07-26): personal_fields was dropped by the
    // entity-model migration; this function used to populate track.fields
    // by reading that dead table via track.add_field(). track.fields is now
    // left empty at INITIALIZE (PersonalField/PersonalTrack.fields is kept
    // as a live type, not retired — it remains the IPC-facing shape for
    // commands::personal -- but nothing currently needs it populated here).
    // PersonalTrack.entity_facts (populated by build_personal_track() via
    // load_entity_facts_for_context(), called separately right after this
    // function returns) already carries every singleton fact a fresh
    // install has. Populating both fields and entity_facts from the same
    // underlying rows would be redundant duplication, not a fix. See
    // items.id=170 handoff for the full verification trail (ownership_scope
    // enforcement confirmed as a real write-time privacy gate,
    // abstraction_tier2/tier3 domain match confirmed identical between
    // personal_fields and entity_facts) behind this call.

    let profile = resolve_voice_profile_conn(&mut conn, persona_id).await?;
    track
        .set_voice_profile(profile)
        .map_err(|e| PersonalStoreError::Validation(e.to_string()))?;

    // life_context is empty at INITIALIZE — legacy name retained per standing rule.
    track
        .set_life_context(indexmap::IndexMap::new())
        .map_err(|e| PersonalStoreError::Validation(e.to_string()))?;

    Ok(track)
}

// ---------------------------------------------------------------------------
// Voice profile (read)
// ---------------------------------------------------------------------------

async fn resolve_voice_profile_conn(
    conn: &mut SqliteConnection,
    persona_id: &str,
) -> Result<indexmap::IndexMap<String, String>, PersonalStoreError> {
    // ORDER BY precedence ASC — lower-precedence rows processed first,
    // higher-precedence rows overwrite for same attribute key.
    // VOICE_PRECEDENCE_WRITING_CONTEXT (5) applied at Step 8 — not loaded here.
    let rows = sqlx::query(
        "SELECT attribute, value FROM voice_profiles
         WHERE persona_id = ? OR persona_id IS NULL
         ORDER BY precedence ASC",
    )
    .bind(persona_id)
    .fetch_all(conn)
    .await?;

    let mut profile: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
    for row in rows {
        profile.insert(row.try_get("attribute")?, row.try_get("value")?);
    }
    Ok(profile)
}

pub async fn load_voice_profile(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<indexmap::IndexMap<String, String>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    resolve_voice_profile_conn(&mut conn, persona_id).await
}

// ---------------------------------------------------------------------------
// Personal fields (read)
// ---------------------------------------------------------------------------

/// Map one singleton entity_facts row (entity_id IS NULL) onto PersonalField.
/// Shared by every personal-field read path below (items.id=170 fix,
/// 2026-07-26) so the extra_metadata source_id recovery logic lives in one
/// place rather than being repeated per function.
///
/// source_id: entity_facts has no first-class source_id column (unlike the
/// retired personal_fields table). save_personal_field (below) writes it
/// into extra_metadata under the "source_id" key; this reads it back out.
/// Falls back to "" if absent/malformed rather than erroring -- source_id
/// was never validated as non-empty on the old table either, and a missing
/// value here should surface as an empty string to callers, not fail the
/// whole read.
fn row_to_personal_field(r: &sqlx::sqlite::SqliteRow) -> Result<PersonalField, PersonalStoreError> {
    let metadata_json: String = r.try_get("extra_metadata")?;
    let source_id = serde_json::from_str::<serde_json::Value>(&metadata_json)
        .ok()
        .and_then(|v| v.get("source_id").and_then(|s| s.as_str().map(str::to_owned)))
        .unwrap_or_default();

    Ok(PersonalField {
        field_name: r.try_get("field_name")?,
        field_value: r.try_get("field_value")?,
        sensitivity: r.try_get("sensitivity")?,
        sensitivity_severity: r.try_get::<i64, _>("sensitivity_severity")? as i32,
        source_id,
        abstraction_tier2: r.try_get("abstraction_tier2")?,
        abstraction_tier3: r.try_get("abstraction_tier3")?,
    })
}

/// items.id=170 fix (2026-07-26): personal_fields does not exist post-
/// entity-model migration. Reads the equivalent singleton entity_facts row
/// (entity_id IS NULL, one active row per field_name enforced by
/// idx_entity_facts_singleton_field) and maps it onto PersonalField.
///
/// source_id is recovered from extra_metadata (JSON key "source_id") --
/// entity_facts has no first-class source_id column. Falls back to "" if
/// absent (rows written before this fix, or written by a path that never
/// set it) rather than erroring, since source_id was never validated as
/// non-empty on the old table either.
pub async fn get_personal_field(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    field_name: &str,
) -> Result<Option<PersonalField>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT field_name, field_value, sensitivity, sensitivity_severity,
         abstraction_tier2, abstraction_tier3, extra_metadata
         FROM entity_facts
         WHERE entity_id IS NULL AND valid_until IS NULL AND field_name = ?",
    )
    .bind(field_name)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_personal_field(&r)?)),
    }
}

/// items.id=170 fix (2026-07-26): personal_fields does not exist post-
/// entity-model migration. Lists singleton entity_facts rows (entity_id IS
/// NULL, valid_until IS NULL) instead.
///
/// source_id filter matches against extra_metadata's "source_id" JSON key
/// via json_extract, since entity_facts has no first-class source_id
/// column -- see row_to_personal_field's doc comment for why that value
/// lives in extra_metadata rather than a dedicated column.
pub async fn list_personal_fields(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    source_id: Option<&str>,
    sensitivity: Option<&str>,
) -> Result<Vec<PersonalField>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT field_name, field_value, sensitivity, sensitivity_severity,
         abstraction_tier2, abstraction_tier3, extra_metadata
         FROM entity_facts
         WHERE entity_id IS NULL AND valid_until IS NULL",
    );
    if let Some(sid) = source_id {
        qb.push(" AND json_extract(extra_metadata, '$.source_id') = ");
        qb.push_bind(sid);
    }
    if let Some(sens) = sensitivity {
        qb.push(" AND sensitivity = ");
        qb.push_bind(sens);
    }
    qb.push(" ORDER BY field_name");

    let rows = qb.build().fetch_all(&mut conn).await?;

    let mut fields = Vec::new();
    for r in &rows {
        fields.push(row_to_personal_field(r)?);
    }
    Ok(fields)
}

// ---------------------------------------------------------------------------
// Personal fields (write)
// ---------------------------------------------------------------------------

/// Insert a personal field as a singleton entity_facts row (entity_id IS NULL).
/// Returns the new row's id (UUID) -- always a fresh id, never the id of a
/// prior version, since entity_facts rows are immutable (items.id=170 fix,
/// 2026-07-26; see module note above and personal_001.sql's entity_facts
/// comment: "Facts are immutable — updates are new rows. No updated_at
/// column."). This is a real behavioral change from the retired
/// personal_fields table, which allowed true UPDATE-in-place and returned
/// the SAME id across repeated writes to the same field_name -- callers
/// that cached a field's id across writes expecting it to stay stable no
/// longer can. No live caller does this (checked commands/personal.rs,
/// conductor/extract.rs) but it is a real contract change worth flagging.
///
/// Atomic rollback protection: SAVEPOINT wraps the supersede-then-insert
/// sequence, mirroring create_entity_fact_with_provenance's pattern (same
/// table, same immutability rule) rather than reinventing it.
///
/// source_persona_id (required by entity_facts' NOT-NULL-on-insert trigger,
/// trg_entity_facts_provenance_required) is this function's own persona_id
/// parameter -- the Persona instance this fact is being written into, same
/// meaning create_entity_fact_with_provenance gives it. cross_persona_export
/// defaults to false / origin_persona_id to None: every save_personal_field
/// write is native to its own Persona, never a cross-Persona copy -- that
/// path (if one is ever needed here) would need its own function, matching
/// how create_entity_fact_with_provenance requires an explicit flag rather
/// than inferring cross-Persona status.
///
/// source_id and ownership_scope: entity_facts has no first-class columns
/// for either (unlike the retired personal_fields table). Both are written
/// into extra_metadata under "source_id" / "ownership_scope" keys instead --
/// preserved for audit/export, but no longer structurally queryable via a
/// dedicated column since nothing in the live codebase reads either back
/// out except this module's own export path (checked every caller before
/// making this call; see items.id=170 handoff for the full trail).
/// ownership_scope's write-time enforcement (below) is unaffected by this --
/// the validation runs against the caller-supplied parameter, not a stored
/// value, exactly as it always has.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn save_personal_field(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    field_name: &str,
    field_value: &str,
    sensitivity: &str,
    source_id: &str,
    ownership_scope: &str,
    abstraction_tier2: &str,
    abstraction_tier3: &str,
    source: &str,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, PersonalStoreError> {
    let sensitivity_severity: i32 = match sensitivity {
        "general" => 1,
        "personal" => 2,
        "medical" => 3,
        "financial" => 4,
        other => {
            return Err(PersonalStoreError::Validation(format!(
                "Unknown sensitivity '{other}'. \
                 Must be general, personal, medical, or financial."
            )))
        }
    };

    if ownership_scope == "instance" && sensitivity_severity > INSTANCE_SCOPE_MAX_SEVERITY {
        return Err(PersonalStoreError::Validation(format!(
            "Instance-scoped fields may not have sensitivity '{sensitivity}'. \
             Only 'general' or 'personal' are permitted at instance scope."
        )));
    }

    if field_value.len() < MIN_MATCH_LENGTH && sensitivity_severity >= 3 {
        log::warn!(
            "short-field write: field='{}' sensitivity='{}' len={}. \
             Gate2 cannot detect short values in model responses.",
            field_name, sensitivity, field_value.len()
        );
    }

    // source_id / ownership_scope preserved in extra_metadata -- see doc
    // comment above for why entity_facts carries these here rather than in
    // dedicated columns. Caller-supplied extra_metadata keys are preserved
    // alongside them; a caller that also sets "source_id"/"ownership_scope"
    // in extra_metadata directly would have those overwritten below, but no
    // live caller does (checked commands/personal.rs, conductor/extract.rs
    // -- both pass extra_metadata=None).
    let mut metadata = extra_metadata.unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert("source_id".to_owned(), serde_json::json!(source_id));
        obj.insert("ownership_scope".to_owned(), serde_json::json!(ownership_scope));
    }
    let metadata_json = serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_owned());

    let timestamp = crate::providers::utils::now();
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    sqlx::query("SAVEPOINT save_personal_field")
        .execute(&mut conn)
        .await?;

    let step: Result<String, sqlx::Error> = async {
        // Supersede any existing active singleton fact for this field_name --
        // mirrors create_entity_fact_with_provenance's supersede-then-insert
        // sequence. entity_facts rows are immutable; this is a new version,
        // not an in-place UPDATE.
        sqlx::query(
            "UPDATE entity_facts SET valid_until = ?
             WHERE entity_id IS NULL AND field_name = ? AND valid_until IS NULL",
        )
        .bind(&timestamp)
        .bind(field_name)
        .execute(&mut conn)
        .await?;

        let new_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO entity_facts
             (id, entity_id, field_name, field_value, sensitivity, source,
              created_at, extra_metadata, abstraction_tier2, abstraction_tier3,
              source_persona_id, cross_persona_export, origin_persona_id)
             VALUES (?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, NULL)",
        )
        .bind(&new_id)
        .bind(field_name)
        .bind(field_value)
        .bind(sensitivity)
        .bind(source)
        .bind(&timestamp)
        .bind(&metadata_json)
        .bind(abstraction_tier2)
        .bind(abstraction_tier3)
        .bind(persona_id)
        .execute(&mut conn)
        .await?;

        Ok(new_id)
    }
    .await;

    match step {
        Ok(id) => {
            sqlx::query("RELEASE save_personal_field")
                .execute(&mut conn)
                .await?;
            Ok(id)
        }
        Err(e) => {
            if let Err(rollback_err) = sqlx::query("ROLLBACK TO save_personal_field")
                .execute(&mut conn)
                .await
            {
                log::error!(
                    "Savepoint rollback failed in save_personal_field: {rollback_err}"
                );
            }
            let _ = sqlx::query("RELEASE save_personal_field")
                .execute(&mut conn)
                .await;
            Err(PersonalStoreError::Database(e))
        }
    }
}

/// Logical deletion of a personal field.
///
/// items.id=170 fix (2026-07-26): rewritten to supersede the active
/// singleton entity_facts row (set valid_until) rather than the retired
/// personal_fields table's blank-then-hard-DELETE sequence. This is a
/// deliberate behavior change, not a mechanical port: entity_facts rows are
/// immutable by design (personal_001.sql: "Facts are immutable — updates
/// are new rows"; every other writer in this module -- save_personal_field
/// above, create_entity_fact_with_provenance in this same file -- follows
/// supersede-then-insert, never a hard DELETE). A hard DELETE here would
/// contradict that design and destroy history the table exists to keep.
/// Checked before making this call: delete_personal_field has no live
/// caller anywhere in the codebase (no IPC command, not referenced by
/// commands/personal.rs or conductor/extract.rs), so no existing contract
/// requires the old hard-delete return semantics -- following the table's
/// own design intent is the correct default here, not a guess.
///
/// A superseded row (valid_until IS NOT NULL) is excluded from every read
/// path in this module (get_personal_field, list_personal_fields,
/// load_personal_track, export_personal_fields all filter on
/// valid_until IS NULL) -- functionally invisible to every caller, exactly
/// as a deleted personal_fields row was, but recoverable from history
/// rather than destroyed.
pub async fn delete_personal_field(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    field_name: &str,
) -> Result<bool, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    let timestamp = crate::providers::utils::now();

    let existing = sqlx::query(
        "SELECT id FROM entity_facts
         WHERE entity_id IS NULL AND field_name = ? AND valid_until IS NULL",
    )
    .bind(field_name)
    .fetch_optional(&mut conn)
    .await?;

    if existing.is_none() {
        return Ok(false);
    }

    sqlx::query(
        "UPDATE entity_facts SET valid_until = ?
         WHERE entity_id IS NULL AND field_name = ? AND valid_until IS NULL",
    )
    .bind(&timestamp)
    .bind(field_name)
    .execute(&mut conn)
    .await?;

    Ok(true)
}

// ---------------------------------------------------------------------------
// Entity facts (write) — Cross-Persona Data Provenance
// decisions.id=546, items.id=27. Schema: personal_001.sql (entity_facts,
// consolidated 2026-07-24 from the entity-model migration + provenance
// columns, items.id=169).
// ---------------------------------------------------------------------------

/// Insert a new entity_facts row with mandatory, immutable provenance.
///
/// Facts are immutable — this ALWAYS inserts a new row; it never updates an
/// existing one. If an active fact already exists for this (entity_id,
/// field_name) — or this singleton field_name when entity_id is None — the
/// prior active row is superseded by setting its valid_until to this write's
/// timestamp before the new row is inserted, preserving history rather than
/// overwriting it. This matches the partial-unique-index design in
/// personal_001.sql (one active row per entity+field, WHERE valid_until IS NULL)
/// and requires no schema change.
///
/// source_persona_id is REQUIRED — the DB-level trigger
/// trg_entity_facts_provenance_required rejects any insert where it's NULL,
/// but validation happens here first for a clean application-layer error
/// instead of a raw SQL trigger abort surfacing to the caller.
///
/// cross_persona_export / origin_persona_id: cross_persona_export defaults
/// to false (0) for native/forked facts. When true, origin_persona_id is
/// required (mirrors the DB CHECK constraint: origin_persona_id IS NULL OR
/// cross_persona_export = 1). This function does NOT implement the
/// per-session re-confirmation UI or the context-assembly enforcement in
/// decisions.id=424 — those are separate, not yet built (see items.id=27
/// remaining scope, flagged in the Chat-DEV handoff).
///
/// Immutability after insert (source_persona_id, cross_persona_export,
/// origin_persona_id) is enforced by trg_entity_facts_provenance_immutable
/// at the DB level — no application-layer UPDATE path for these three
/// fields exists or should be added.
///
/// Atomic: SAVEPOINT wraps the supersede-then-insert sequence.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn create_entity_fact_with_provenance(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    entity_id: Option<&str>,
    field_name: &str,
    field_value: &str,
    sensitivity: &str,
    source: &str,
    source_persona_id: &str,
    cross_persona_export: bool,
    origin_persona_id: Option<&str>,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, PersonalStoreError> {
    if !matches!(sensitivity, "general" | "personal" | "medical" | "financial") {
        return Err(PersonalStoreError::Validation(format!(
            "Unknown sensitivity '{sensitivity}'. \
             Must be general, personal, medical, or financial."
        )));
    }

    if source_persona_id.is_empty() {
        return Err(PersonalStoreError::Validation(
            "source_persona_id is required for every entity_facts write \
             (decisions.id=546)."
                .to_owned(),
        ));
    }

    if cross_persona_export && origin_persona_id.is_none() {
        return Err(PersonalStoreError::Validation(
            "origin_persona_id is required when cross_persona_export is true \
             (decisions.id=546)."
                .to_owned(),
        ));
    }
    if !cross_persona_export && origin_persona_id.is_some() {
        return Err(PersonalStoreError::Validation(
            "origin_persona_id must be omitted when cross_persona_export is false \
             (decisions.id=546)."
                .to_owned(),
        ));
    }

    let metadata_json = serde_json::to_string(&extra_metadata.unwrap_or_default())
        .unwrap_or_else(|_| "{}".to_owned());
    let timestamp = crate::providers::utils::now();
    let cross_persona_export_flag: i32 = if cross_persona_export { 1 } else { 0 };
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    sqlx::query("SAVEPOINT create_entity_fact_with_provenance")
        .execute(&mut conn)
        .await?;

    let step: Result<String, sqlx::Error> = async {
        // Supersede any existing active fact for this entity+field (or
        // singleton field_name when entity_id is None) — facts are
        // immutable, so history is preserved via valid_until rather than
        // overwritten.
        sqlx::query(
            "UPDATE entity_facts SET valid_until = ?
             WHERE field_name = ? AND valid_until IS NULL
             AND (entity_id = ? OR (entity_id IS NULL AND ? IS NULL))",
        )
        .bind(&timestamp)
        .bind(field_name)
        .bind(entity_id)
        .bind(entity_id)
        .execute(&mut conn)
        .await?;

        let new_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO entity_facts
             (id, entity_id, field_name, field_value, sensitivity, source,
              created_at, extra_metadata,
              source_persona_id, cross_persona_export, origin_persona_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&new_id)
        .bind(entity_id)
        .bind(field_name)
        .bind(field_value)
        .bind(sensitivity)
        .bind(source)
        .bind(&timestamp)
        .bind(&metadata_json)
        .bind(source_persona_id)
        .bind(cross_persona_export_flag)
        .bind(origin_persona_id)
        .execute(&mut conn)
        .await?;

        Ok(new_id)
    }
    .await;

    match step {
        Ok(id) => {
            sqlx::query("RELEASE create_entity_fact_with_provenance")
                .execute(&mut conn)
                .await?;
            Ok(id)
        }
        Err(e) => {
            if let Err(rollback_err) = sqlx::query("ROLLBACK TO create_entity_fact_with_provenance")
                .execute(&mut conn)
                .await
            {
                log::error!(
                    "Savepoint rollback failed in create_entity_fact_with_provenance: {rollback_err}"
                );
            }
            let _ = sqlx::query("RELEASE create_entity_fact_with_provenance")
                .execute(&mut conn)
                .await;
            Err(PersonalStoreError::Database(e))
        }
    }
}

// ---------------------------------------------------------------------------
// Entity facts (read) — Cross-Persona Data Provenance
// decisions.id=546, items.id=27. Read path for context assembly. This
// function loads EntityFact rows for PersonalTrack — it does NOT implement
// the decisions.id=424 enforcement check (same-Persona facts include
// normally, cross_persona_export=true facts require per-session
// confirmation, mismatched source_persona_id/cross_persona_export=false is
// a hard block flagged as a system integrity error). That check runs on
// the data this function returns; it is separate, not-yet-built scope.
// ---------------------------------------------------------------------------

/// Load all active entity_facts rows (valid_until IS NULL) for a
/// user+persona. Returns rows in an unspecified order — caller (lifecycle,
/// via PersonalTrack::add_entity_fact) is responsible for ordering if
/// order-sensitive presentation is ever needed; none of R1's consumers are.
///
/// Called during Phase 3 INITIALIZE, alongside load_personal_track().
/// Mirrors load_personal_track()'s decryption-key and row-mapping pattern.
pub async fn load_entity_facts_for_context(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<Vec<EntityFact>, PersonalStoreError> {
    if key_hex.is_empty() {
        return Err(PersonalStoreError::Decryption(PersonalDBDecryptionError {
            plain_language: "Your session has expired. Please log in again.".to_owned(),
        }));
    }

    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    let rows = sqlx::query(
        "SELECT id, entity_id, field_name, field_value, sensitivity, sensitivity_severity,
         source_persona_id, cross_persona_export, origin_persona_id
         FROM entity_facts WHERE valid_until IS NULL
         ORDER BY field_name",
    )
    .fetch_all(&mut conn)
    .await?;

    let mut facts = Vec::with_capacity(rows.len());
    for row in rows {
        let cross_persona_export: i64 = row.try_get("cross_persona_export")?;
        facts.push(EntityFact {
            id: row.try_get("id")?,
            entity_id: row.try_get("entity_id")?,
            field_name: row.try_get("field_name")?,
            field_value: row.try_get("field_value")?,
            sensitivity: row.try_get("sensitivity")?,
            sensitivity_severity: row.try_get::<i64, _>("sensitivity_severity")? as i32,
            source_persona_id: row.try_get("source_persona_id")?,
            cross_persona_export: cross_persona_export != 0,
            origin_persona_id: row.try_get("origin_persona_id")?,
        });
    }

    Ok(facts)
}

/// Load active (valid_until IS NULL) entity_facts rows with
/// cross_persona_export = 1 for a user+persona — the set of facts that
/// require per-session user confirmation before a Focus run may include
/// them (decisions.id=546, decisions.id=639, items.id=27).
///
/// Called by the pre-Focus-start IPC query
/// (commands::consent::get_pending_cross_persona_confirmations), entirely
/// outside FocusRun — decisions.id=639's design keeps this check ahead of
/// FocusRun::new(), not inside Conductor's INITIALIZE phase. Mirrors
/// load_entity_facts_for_context()'s decryption-key and row-mapping
/// pattern; a strict subset of the same rows.
pub async fn list_pending_cross_persona_exports(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<Vec<EntityFact>, PersonalStoreError> {
    if key_hex.is_empty() {
        return Err(PersonalStoreError::Decryption(PersonalDBDecryptionError {
            plain_language: "Your session has expired. Please log in again.".to_owned(),
        }));
    }

    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    let rows = sqlx::query(
        "SELECT id, entity_id, field_name, field_value, sensitivity, sensitivity_severity,
         source_persona_id, cross_persona_export, origin_persona_id
         FROM entity_facts WHERE valid_until IS NULL AND cross_persona_export = 1
         ORDER BY field_name",
    )
    .fetch_all(&mut conn)
    .await?;

    let mut facts = Vec::with_capacity(rows.len());
    for row in rows {
        let cross_persona_export: i64 = row.try_get("cross_persona_export")?;
        facts.push(EntityFact {
            id: row.try_get("id")?,
            entity_id: row.try_get("entity_id")?,
            field_name: row.try_get("field_name")?,
            field_value: row.try_get("field_value")?,
            sensitivity: row.try_get("sensitivity")?,
            sensitivity_severity: row.try_get::<i64, _>("sensitivity_severity")? as i32,
            source_persona_id: row.try_get("source_persona_id")?,
            cross_persona_export: cross_persona_export != 0,
            origin_persona_id: row.try_get("origin_persona_id")?,
        });
    }

    Ok(facts)
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// items.id=170 fix (2026-07-26): personal_fields does not exist post-
/// entity-model migration. Exports active singleton entity_facts rows
/// (entity_id IS NULL, valid_until IS NULL) instead.
///
/// Checked before making this call: export_personal_fields has no live
/// caller anywhere in the codebase (no IPC command, not referenced
/// elsewhere) -- unlike the other five functions in this fix, no existing
/// contract constrains this rewrite. Field shape is kept identical to the
/// old export payload regardless, since a Moving Day / export consumer is
/// exactly the kind of caller that would arrive later expecting the
/// documented shape, not a caller that already exists to check.
///
/// source_id / ownership_scope: recovered from extra_metadata, same as
/// row_to_personal_field (see that function's doc comment) -- not read via
/// row_to_personal_field itself because the export payload also carries
/// ownership_scope, which PersonalField's struct shape does not have room
/// for and was never asked to.
///
/// updated_at: entity_facts has no updated_at column (immutable rows have
/// only created_at -- a "new version" is a new row with its own
/// created_at, per personal_001.sql's design). Exported as equal to
/// created_at rather than omitted, since that is the honest value for an
/// immutable row -- not a placeholder standing in for a missing column.
pub async fn export_personal_fields(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    source_id: Option<&str>,
) -> Result<Vec<serde_json::Value>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT field_name, sensitivity, sensitivity_severity,
         abstraction_tier2, abstraction_tier3, source, created_at, extra_metadata
         FROM entity_facts
         WHERE entity_id IS NULL AND valid_until IS NULL AND sensitivity_severity <= ",
    );
    qb.push_bind(EXPORT_SENSITIVITY_CEILING);
    if let Some(sid) = source_id {
        qb.push(" AND json_extract(extra_metadata, '$.source_id') = ");
        qb.push_bind(sid);
    }
    qb.push(" ORDER BY field_name");

    let rows = qb.build().fetch_all(&mut conn).await?;

    let mut result = Vec::new();
    for r in rows {
        let metadata_json: String = r.try_get("extra_metadata")?;
        let metadata: serde_json::Value =
            serde_json::from_str(&metadata_json).unwrap_or(serde_json::json!({}));
        let source_id_out = metadata
            .get("source_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let ownership_scope_out = metadata
            .get("ownership_scope")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let created_at: String = r.try_get("created_at")?;

        result.push(serde_json::json!({
            "export_schema_version": EXPORT_SCHEMA_VERSION,
            "export_semantic": "metadata_only",
            "field_name": r.try_get::<String, _>("field_name")?,
            "sensitivity": r.try_get::<String, _>("sensitivity")?,
            "sensitivity_severity": r.try_get::<i64, _>("sensitivity_severity")? as i32,
            "source_id": source_id_out,
            "abstraction_tier2": r.try_get::<String, _>("abstraction_tier2")?,
            "abstraction_tier3": r.try_get::<String, _>("abstraction_tier3")?,
            "ownership_scope": ownership_scope_out,
            "source": r.try_get::<String, _>("source")?,
            "created_at": created_at.clone(),
            "updated_at": created_at,
        }));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Voice profile value validation (D5-151)
// ---------------------------------------------------------------------------

fn validate_voice_profile_value(
    attribute: &str,
    value: &str,
) -> Result<(), PersonalStoreError> {
    let normalized = value.trim();
    let word_count = normalized.split_whitespace().count();

    if word_count > VOICE_VALUE_MAX_WORDS {
        log::warn!(
            "voice_profile write rejected: value too long attribute='{}' word_count={}",
            attribute, word_count
        );
        return Err(PersonalStoreError::Validation(
            VOICE_VALUE_REJECTION_MSG.to_owned(),
        ));
    }

    let has_email = normalized.contains('@') && {
        let parts: Vec<&str> = normalized.splitn(2, '@').collect();
        parts.len() == 2 && parts[1].contains('.')
    };
    let has_url = normalized.contains("http://")
        || normalized.contains("https://")
        || normalized.contains("www.");
    // Phone detection: counts raw digit characters.
    // Phase 1 stub — produces more false negatives and fewer false positives
    // than the Python regex (e.g. spelled-out numbers bypass detection).
    // TODO: replace with regex crate pattern match for full Python parity.
    let digit_count = normalized.chars().filter(|c| c.is_ascii_digit()).count();
    let has_phone = digit_count >= 8;

    if has_email || has_url || has_phone {
        let reason = if has_email {
            "email address"
        } else if has_url {
            "URL"
        } else {
            "phone number"
        };
        log::warn!(
            "voice_profile write rejected: {} detected attribute='{}'",
            reason, attribute
        );
        return Err(PersonalStoreError::Validation(
            VOICE_VALUE_REJECTION_MSG.to_owned(),
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Voice profile (write)
// ---------------------------------------------------------------------------

/// Write a voice profile entry at the specified precedence level.
/// VOICE_PRECEDENCE_GLOBAL (3) entries store persona_id = NULL.
/// Upserts on composite key: (stored_persona_id, source_id, precedence, attribute).
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn save_voice_profile_entry(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    attribute: &str,
    value: &str,
    precedence: i32,
    source_id: Option<&str>,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, PersonalStoreError> {
    validate_voice_profile_value(attribute, value)?;

    if !(VOICE_PRECEDENCE_MODEL_BASELINE..=VOICE_PRECEDENCE_WRITING_CONTEXT)
        .contains(&precedence)
    {
        return Err(PersonalStoreError::Validation(format!(
            "Voice profile precedence must be {}-{}, got {precedence}.",
            VOICE_PRECEDENCE_MODEL_BASELINE, VOICE_PRECEDENCE_WRITING_CONTEXT
        )));
    }

    let stored_persona_id: Option<&str> = if precedence == VOICE_PRECEDENCE_GLOBAL {
        None
    } else {
        Some(persona_id)
    };

    let metadata_json = serde_json::to_string(&extra_metadata.unwrap_or_default())
        .unwrap_or_else(|_| "{}".to_owned());
    let timestamp = crate::providers::utils::now();
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    let existing = sqlx::query(
        "SELECT id FROM voice_profiles
         WHERE (persona_id = ? OR (persona_id IS NULL AND ? IS NULL))
         AND (source_id = ? OR (source_id IS NULL AND ? IS NULL))
         AND precedence = ? AND attribute = ?",
    )
    .bind(stored_persona_id)
    .bind(stored_persona_id)
    .bind(source_id)
    .bind(source_id)
    .bind(precedence)
    .bind(attribute)
    .fetch_optional(&mut conn)
    .await?;

    let entry_id = if let Some(row) = existing {
        let id: String = row.try_get("id")?;
        sqlx::query(
            "UPDATE voice_profiles SET value = ?, updated_at = ?,
             extra_metadata = ? WHERE id = ?",
        )
        .bind(value)
        .bind(&timestamp)
        .bind(&metadata_json)
        .bind(&id)
        .execute(&mut conn)
        .await?;
        id
    } else {
        let new_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO voice_profiles
             (id, persona_id, source_id, precedence,
              attribute, value, created_at, updated_at, extra_metadata)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&new_id)
        .bind(stored_persona_id)
        .bind(source_id)
        .bind(precedence)
        .bind(attribute)
        .bind(value)
        .bind(&timestamp)
        .bind(&timestamp)
        .bind(&metadata_json)
        .execute(&mut conn)
        .await?;
        new_id
    };

    Ok(entry_id)
}

pub async fn delete_voice_profile_entry(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    attribute: &str,
    precedence: i32,
    source_id: Option<&str>,
) -> Result<bool, PersonalStoreError> {
    let stored_persona_id: Option<&str> = if precedence == VOICE_PRECEDENCE_GLOBAL {
        None
    } else {
        Some(persona_id)
    };

    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    let existing = sqlx::query(
        "SELECT id FROM voice_profiles
         WHERE (persona_id = ? OR (persona_id IS NULL AND ? IS NULL))
         AND (source_id = ? OR (source_id IS NULL AND ? IS NULL))
         AND precedence = ? AND attribute = ?",
    )
    .bind(stored_persona_id)
    .bind(stored_persona_id)
    .bind(source_id)
    .bind(source_id)
    .bind(precedence)
    .bind(attribute)
    .fetch_optional(&mut conn)
    .await?;

    match existing {
        None => Ok(false),
        Some(row) => {
            let id: String = row.try_get("id")?;
            sqlx::query("DELETE FROM voice_profiles WHERE id = ?")
                .bind(&id)
                .execute(&mut conn)
                .await?;
            Ok(true)
        }
    }
}

// ---------------------------------------------------------------------------
// Disclosure log (write-only — D6-198)
// ---------------------------------------------------------------------------

/// Write a disclosure log entry. NEVER deleted — permanent audit trail (D6-198).
/// Returns the new entry id.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn write_disclosure_log(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
    step_id: &str,
    routing_tier: i32,
    provider: Option<&str>,
    fields_shared: &[String],
    fields_abstracted: &serde_json::Value,
    fields_withheld: &[String],
    override_declined: bool,
    declined_at: Option<&str>,
    execution_tier: Option<i32>,
    abstraction_tier: Option<i32>,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, PersonalStoreError> {
    let entry_id = uuid::Uuid::new_v4().to_string();
    let timestamp = crate::providers::utils::now();
    let shared_json = serde_json::to_string(fields_shared)
        .unwrap_or_else(|_| "[]".to_owned());
    let abstracted_json = serde_json::to_string(fields_abstracted)
        .unwrap_or_else(|_| "{}".to_owned());
    let withheld_json = serde_json::to_string(fields_withheld)
        .unwrap_or_else(|_| "[]".to_owned());
    let metadata_json = serde_json::to_string(&extra_metadata.unwrap_or_default())
        .unwrap_or_else(|_| "{}".to_owned());
    let override_flag: i32 = if override_declined { 1 } else { 0 };

    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    sqlx::query(
        "INSERT INTO disclosure_log
         (id, user_id, persona_id, focus_run_id, step_id, routing_tier,
          provider, fields_shared, fields_abstracted, fields_withheld,
          override_declined, declined_at, created_at, extra_metadata,
          execution_tier, abstraction_tier)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&entry_id)
    .bind(user_id)
    .bind(persona_id)
    .bind(focus_run_id)
    .bind(step_id)
    .bind(routing_tier)
    .bind(provider)
    .bind(&shared_json)
    .bind(&abstracted_json)
    .bind(&withheld_json)
    .bind(override_flag)
    .bind(declined_at)
    .bind(&timestamp)
    .bind(&metadata_json)
    .bind(execution_tier)
    .bind(abstraction_tier)
    .execute(&mut conn)
    .await?;

    Ok(entry_id)
}

/// Read disclosure log entries for a focus run. Read-only — no delete (D6-198).
/// JSON TEXT columns deserialized to serde_json::Value on read.
pub async fn get_disclosure_log_for_run(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
) -> Result<Vec<serde_json::Value>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    let rows = sqlx::query(
        "SELECT id, user_id, persona_id, focus_run_id, step_id,
         routing_tier, provider, fields_shared, fields_abstracted,
         fields_withheld, override_declined, declined_at, created_at,
         extra_metadata, execution_tier, abstraction_tier
         FROM disclosure_log WHERE focus_run_id = ?
         ORDER BY created_at",
    )
    .bind(focus_run_id)
    .fetch_all(&mut conn)
    .await?;

    let mut entries = Vec::new();
    for r in rows {
        // Deserialize JSON TEXT columns to Value on read — callers receive
        // parsed structures, not raw JSON strings.
        let fields_shared: serde_json::Value = serde_json::from_str(
            &r.try_get::<String, _>("fields_shared")?,
        )
        .unwrap_or(serde_json::json!([]));
        let fields_abstracted: serde_json::Value = serde_json::from_str(
            &r.try_get::<String, _>("fields_abstracted")?,
        )
        .unwrap_or(serde_json::json!({}));
        let fields_withheld: serde_json::Value = serde_json::from_str(
            &r.try_get::<String, _>("fields_withheld")?,
        )
        .unwrap_or(serde_json::json!([]));
        let extra_metadata: serde_json::Value = serde_json::from_str(
            &r.try_get::<String, _>("extra_metadata")?,
        )
        .unwrap_or(serde_json::json!({}));

        entries.push(serde_json::json!({
            "id": r.try_get::<String, _>("id")?,
            "user_id": r.try_get::<String, _>("user_id")?,
            "persona_id": r.try_get::<String, _>("persona_id")?,
            "focus_run_id": r.try_get::<String, _>("focus_run_id")?,
            "step_id": r.try_get::<String, _>("step_id")?,
            "routing_tier": r.try_get::<i64, _>("routing_tier")? as i32,
            "provider": r.try_get::<Option<String>, _>("provider")?,
            "fields_shared": fields_shared,
            "fields_abstracted": fields_abstracted,
            "fields_withheld": fields_withheld,
            "override_declined": r.try_get::<i64, _>("override_declined")? != 0,
            "declined_at": r.try_get::<Option<String>, _>("declined_at")?,
            "created_at": r.try_get::<String, _>("created_at")?,
            "extra_metadata": extra_metadata,
            "execution_tier": r.try_get::<Option<i64>, _>("execution_tier")?.map(|v| v as i32),
            "abstraction_tier": r.try_get::<Option<i64>, _>("abstraction_tier")?.map(|v| v as i32),
        }));
    }
    Ok(entries)
}
