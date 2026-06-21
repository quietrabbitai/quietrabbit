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

use crate::conductor::types::{PersonalDBDecryptionError, PersonalField, PersonalTrack};

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
async fn open_personal_db(
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

    let rows = sqlx::query(
        "SELECT field_name, field_value, sensitivity, sensitivity_severity,
         source_id, abstraction_tier2, abstraction_tier3
         FROM personal_fields ORDER BY field_name",
    )
    .fetch_all(&mut conn)
    .await?;

    for row in rows {
        let field = PersonalField {
            field_name: row.try_get("field_name")?,
            field_value: row.try_get("field_value")?,
            sensitivity: row.try_get("sensitivity")?,
            sensitivity_severity: row.try_get::<i64, _>("sensitivity_severity")? as i32,
            source_id: row.try_get("source_id")?,
            abstraction_tier2: row.try_get("abstraction_tier2")?,
            abstraction_tier3: row.try_get("abstraction_tier3")?,
        };
        track.add_field(field).map_err(|e| PersonalStoreError::Validation(e.to_string()))?;
    }

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

pub async fn get_personal_field(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    field_name: &str,
) -> Result<Option<PersonalField>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT field_name, field_value, sensitivity, sensitivity_severity,
         source_id, abstraction_tier2, abstraction_tier3
         FROM personal_fields WHERE field_name = ?",
    )
    .bind(field_name)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(PersonalField {
            field_name: r.try_get("field_name")?,
            field_value: r.try_get("field_value")?,
            sensitivity: r.try_get("sensitivity")?,
            sensitivity_severity: r.try_get::<i64, _>("sensitivity_severity")? as i32,
            source_id: r.try_get("source_id")?,
            abstraction_tier2: r.try_get("abstraction_tier2")?,
            abstraction_tier3: r.try_get("abstraction_tier3")?,
        })),
    }
}

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
         source_id, abstraction_tier2, abstraction_tier3
         FROM personal_fields WHERE 1=1",
    );
    if let Some(sid) = source_id {
        qb.push(" AND source_id = ");
        qb.push_bind(sid);
    }
    if let Some(sens) = sensitivity {
        qb.push(" AND sensitivity = ");
        qb.push_bind(sens);
    }
    qb.push(" ORDER BY field_name");

    let rows = qb.build().fetch_all(&mut conn).await?;

    let mut fields = Vec::new();
    for r in rows {
        fields.push(PersonalField {
            field_name: r.try_get("field_name")?,
            field_value: r.try_get("field_value")?,
            sensitivity: r.try_get("sensitivity")?,
            sensitivity_severity: r.try_get::<i64, _>("sensitivity_severity")? as i32,
            source_id: r.try_get("source_id")?,
            abstraction_tier2: r.try_get("abstraction_tier2")?,
            abstraction_tier3: r.try_get("abstraction_tier3")?,
        });
    }
    Ok(fields)
}

// ---------------------------------------------------------------------------
// Personal fields (write)
// ---------------------------------------------------------------------------

/// Insert or update a personal field in personal.db.
/// Returns field id (UUID — existing id if field_name already exists).
///
/// Atomic rollback protection: SAVEPOINT wraps the SELECT + INSERT/UPDATE
/// so a partial write is rolled back on failure within this connection.
/// Does NOT serialize concurrent writers — two connections can still race
/// on INSERT. A UNIQUE(field_name) constraint + ON CONFLICT DO UPDATE would
/// solve this cleanly. Flag to Chat-PM for a future personal_fields migration.
///
/// sensitivity_severity is a GENERATED ALWAYS column — omitted from UPDATE.
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

    let metadata_json = serde_json::to_string(&extra_metadata.unwrap_or_default())
        .unwrap_or_else(|_| "{}".to_owned());
    let timestamp = crate::providers::utils::now();
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    sqlx::query("SAVEPOINT save_personal_field")
        .execute(&mut conn)
        .await?;

    let step: Result<String, sqlx::Error> = async {
        let existing = sqlx::query(
            "SELECT id FROM personal_fields WHERE field_name = ?",
        )
        .bind(field_name)
        .fetch_optional(&mut conn)
        .await?;

        let field_id = if let Some(row) = existing {
            let id: String = row.try_get("id")?;
            // sensitivity_severity intentionally omitted — GENERATED ALWAYS column.
            sqlx::query(
                "UPDATE personal_fields SET
                 field_value = ?, sensitivity = ?, source_id = ?,
                 ownership_scope = ?, abstraction_tier2 = ?, abstraction_tier3 = ?,
                 source = ?, updated_at = ?, extra_metadata = ?
                 WHERE id = ?",
            )
            .bind(field_value)
            .bind(sensitivity)
            .bind(source_id)
            .bind(ownership_scope)
            .bind(abstraction_tier2)
            .bind(abstraction_tier3)
            .bind(source)
            .bind(&timestamp)
            .bind(&metadata_json)
            .bind(&id)
            .execute(&mut conn)
            .await?;
            id
        } else {
            let new_id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO personal_fields
                 (id, source_id, field_name, field_value, sensitivity,
                  ownership_scope, abstraction_tier2, abstraction_tier3,
                  source, created_at, updated_at, extra_metadata)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&new_id)
            .bind(source_id)
            .bind(field_name)
            .bind(field_value)
            .bind(sensitivity)
            .bind(ownership_scope)
            .bind(abstraction_tier2)
            .bind(abstraction_tier3)
            .bind(source)
            .bind(&timestamp)
            .bind(&timestamp)
            .bind(&metadata_json)
            .execute(&mut conn)
            .await?;
            new_id
        };
        Ok(field_id)
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
/// Blanks the field_value then deletes the row, both under a SAVEPOINT.
/// Relies on SQLCipher file-level encryption for at-rest data protection —
/// does NOT guarantee zero-overwrite of underlying SQLite pages or WAL contents.
pub async fn delete_personal_field(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    field_name: &str,
) -> Result<bool, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    let timestamp = crate::providers::utils::now();

    let existing = sqlx::query(
        "SELECT id FROM personal_fields WHERE field_name = ?",
    )
    .bind(field_name)
    .fetch_optional(&mut conn)
    .await?;

    if existing.is_none() {
        return Ok(false);
    }

    sqlx::query("SAVEPOINT delete_personal_field")
        .execute(&mut conn)
        .await?;

    let step: Result<(), sqlx::Error> = async {
        // Step 1: blank the value before deletion (logical zeroing).
        sqlx::query(
            "UPDATE personal_fields SET field_value = '', updated_at = ?
             WHERE field_name = ?",
        )
        .bind(&timestamp)
        .bind(field_name)
        .execute(&mut conn)
        .await?;

        // Step 2: delete the now-blanked record.
        sqlx::query("DELETE FROM personal_fields WHERE field_name = ?")
            .bind(field_name)
            .execute(&mut conn)
            .await?;

        Ok(())
    }
    .await;

    match step {
        Ok(()) => {
            sqlx::query("RELEASE delete_personal_field")
                .execute(&mut conn)
                .await?;
            Ok(true)
        }
        Err(e) => {
            if let Err(rollback_err) = sqlx::query("ROLLBACK TO delete_personal_field")
                .execute(&mut conn)
                .await
            {
                log::error!(
                    "Savepoint rollback failed in delete_personal_field: {rollback_err}"
                );
            }
            let _ = sqlx::query("RELEASE delete_personal_field")
                .execute(&mut conn)
                .await;
            Err(PersonalStoreError::Database(e))
        }
    }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

pub async fn export_personal_fields(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    source_id: Option<&str>,
) -> Result<Vec<serde_json::Value>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT field_name, sensitivity, sensitivity_severity,
         source_id, abstraction_tier2, abstraction_tier3,
         ownership_scope, source, created_at, updated_at
         FROM personal_fields WHERE sensitivity_severity <= ",
    );
    qb.push_bind(EXPORT_SENSITIVITY_CEILING);
    if let Some(sid) = source_id {
        qb.push(" AND source_id = ");
        qb.push_bind(sid);
    }
    qb.push(" ORDER BY field_name");

    let rows = qb.build().fetch_all(&mut conn).await?;

    let mut result = Vec::new();
    for r in rows {
        result.push(serde_json::json!({
            "export_schema_version": EXPORT_SCHEMA_VERSION,
            "export_semantic": "metadata_only",
            "field_name": r.try_get::<String, _>("field_name")?,
            "sensitivity": r.try_get::<String, _>("sensitivity")?,
            "sensitivity_severity": r.try_get::<i64, _>("sensitivity_severity")? as i32,
            "source_id": r.try_get::<String, _>("source_id")?,
            "abstraction_tier2": r.try_get::<String, _>("abstraction_tier2")?,
            "abstraction_tier3": r.try_get::<String, _>("abstraction_tier3")?,
            "ownership_scope": r.try_get::<String, _>("ownership_scope")?,
            "source": r.try_get::<String, _>("source")?,
            "created_at": r.try_get::<String, _>("created_at")?,
            "updated_at": r.try_get::<String, _>("updated_at")?,
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
