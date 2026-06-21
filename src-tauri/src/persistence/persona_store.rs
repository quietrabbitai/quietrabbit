// src-tauri/src/persistence/persona_store.rs
//
// Persona CRUD operations for shared.db (unencrypted).
// Replaces life_store.py as part of Phase C Persona model migration (D6-298).
//
// Persona is a personalization grouping only (D6-289, D6-291).
// No tier fields on the Persona record — tier enforcement is Focus-level.
// get_persona_for_user() performs membership validation only.
// Tier ceiling and privacy settings are read from focus_settings via
// focus_settings_store::get_focus_settings() at AUTHORIZE.
//
// floor_consent_preference is stored in personas.extra_metadata (D5-152).
//
// QUERY STYLE: runtime sqlx::query() only — no query!() macros.
// shared.db is unencrypted — no PRAGMA key required.
//
// CONNECTION MODEL: one connection per call (Phase 1 correctness implementation).

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum PersonaStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Persona '{0}' already exists")]
    AlreadyExists(String),
    #[error("Persona '{0}' not found")]
    NotFound(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Data type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Persona {
    pub id: String,
    pub display_name: String,
    pub persona_type: String,
    pub created_at: String,
    /// Stored as JSON TEXT in DB. Default is empty object {}.
    /// Includes floor_consent_preference when set (D5-152).
    pub extra_metadata: serde_json::Value,
}

// ---------------------------------------------------------------------------
// DB opener (shared.db — unencrypted)
// ---------------------------------------------------------------------------

async fn open_shared_db() -> Result<SqliteConnection, PersonaStoreError> {
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
        .connect()
        .await?;

    Ok(conn)
}

// ---------------------------------------------------------------------------
// Row extraction
// ---------------------------------------------------------------------------

fn row_to_persona(row: &sqlx::sqlite::SqliteRow) -> Result<Persona, sqlx::Error> {
    let raw: Option<String> = row.try_get("extra_metadata")?;
    let extra_metadata: serde_json::Value = match raw {
        None => serde_json::Value::Object(serde_json::Map::new()),
        Some(s) => {
            // TODO: log parse failure for forensic visibility
            serde_json::from_str(&s)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()))
        }
    };

    Ok(Persona {
        id: row.try_get("id")?,
        display_name: row.try_get("display_name")?,
        persona_type: row.try_get("persona_type")?,
        created_at: row.try_get("created_at")?,
        extra_metadata,
    })
}

// ---------------------------------------------------------------------------
// Constraint error classifier
// ---------------------------------------------------------------------------

/// Classify a sqlx database error as AlreadyExists (UNIQUE violation) or
/// propagate as Database.
///
/// SQLite base constraint code is 19 (SQLITE_CONSTRAINT). With extended result
/// codes enabled, UNIQUE violations may surface as 1555 (SQLITE_CONSTRAINT_PRIMARYKEY)
/// or 2067 (SQLITE_CONSTRAINT_UNIQUE). We check both the numeric code and the
/// error message to be safe across sqlx versions and SQLite build configurations.
fn classify_constraint_error(
    persona_id: &str,
    e: sqlx::Error,
) -> PersonaStoreError {
    if let Some(db_err) = e.as_database_error() {
        let code = db_err.code().unwrap_or_default();
        let msg = db_err.message().to_lowercase();
        let is_unique = matches!(code.as_ref(), "19" | "1555" | "2067")
            || msg.contains("unique constraint failed");
        if is_unique {
            return PersonaStoreError::AlreadyExists(persona_id.to_owned());
        }
    }
    PersonaStoreError::Database(e)
}

// ---------------------------------------------------------------------------
// Read operations
// ---------------------------------------------------------------------------

/// Fetch a persona by ID. Returns None if not found.
pub async fn get_persona(
    persona_id: &str,
) -> Result<Option<Persona>, PersonaStoreError> {
    let mut conn = open_shared_db().await?;

    let row = sqlx::query(
        "SELECT id, display_name, persona_type, created_at, extra_metadata
         FROM personas WHERE id = ?",
    )
    .bind(persona_id)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_persona(&r).map_err(PersonaStoreError::Database)?)),
    }
}

/// Fetch a persona only if the user has membership.
/// Returns None if not found or user is not a member.
/// Membership validation only — no tier data (D6-297).
/// Used by lifecycle AUTHORIZE to enforce access control.
pub async fn get_persona_for_user(
    user_id: &str,
    persona_id: &str,
) -> Result<Option<Persona>, PersonaStoreError> {
    let mut conn = open_shared_db().await?;

    let row = sqlx::query(
        "SELECT p.id, p.display_name, p.persona_type,
         p.created_at, p.extra_metadata
         FROM personas p
         JOIN user_personas up ON up.persona_id = p.id
         WHERE up.user_id = ? AND p.id = ?",
    )
    .bind(user_id)
    .bind(persona_id)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_persona(&r).map_err(PersonaStoreError::Database)?)),
    }
}

/// Return all personas accessible to a user, ordered by display_name.
pub async fn list_personas_for_user(
    user_id: &str,
) -> Result<Vec<Persona>, PersonaStoreError> {
    let mut conn = open_shared_db().await?;

    let rows = sqlx::query(
        "SELECT p.id, p.display_name, p.persona_type,
         p.created_at, p.extra_metadata
         FROM personas p
         JOIN user_personas up ON up.persona_id = p.id
         WHERE up.user_id = ?
         ORDER BY p.display_name",
    )
    .bind(user_id)
    .fetch_all(&mut conn)
    .await?;

    let mut personas = Vec::new();
    for r in rows {
        personas.push(row_to_persona(&r).map_err(PersonaStoreError::Database)?);
    }
    Ok(personas)
}

// ---------------------------------------------------------------------------
// Write operations
// ---------------------------------------------------------------------------

/// Create a new persona and atomically add the creator as a member.
/// Returns Err(AlreadyExists) if persona_id already exists.
/// Atomic: INSERT personas + INSERT user_personas under a SAVEPOINT.
/// No tier parameters — tier settings belong to focus_settings (D6-297).
pub async fn create_persona(
    persona_id: &str,
    display_name: &str,
    persona_type: &str,
    creator_user_id: &str,
) -> Result<Persona, PersonaStoreError> {
    let created_at = crate::providers::utils::now();
    let mut conn = open_shared_db().await?;

    sqlx::query("SAVEPOINT create_persona")
        .execute(&mut conn)
        .await?;

    let step: Result<(), sqlx::Error> = async {
        sqlx::query(
            "INSERT INTO personas
             (id, display_name, persona_type, created_at, extra_metadata)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(persona_id)
        .bind(display_name)
        .bind(persona_type)
        .bind(&created_at)
        .bind("{}")
        .execute(&mut conn)
        .await?;

        sqlx::query(
            "INSERT INTO user_personas (user_id, persona_id, joined_at)
             VALUES (?, ?, ?)",
        )
        .bind(creator_user_id)
        .bind(persona_id)
        .bind(&created_at)
        .execute(&mut conn)
        .await?;

        Ok(())
    }
    .await;

    match step {
        Ok(()) => {
            sqlx::query("RELEASE create_persona")
                .execute(&mut conn)
                .await?;
        }
        Err(e) => {
            // TODO: log rollback failure for forensic visibility.
            let _ = sqlx::query("ROLLBACK TO create_persona")
                .execute(&mut conn)
                .await;
            return Err(classify_constraint_error(persona_id, e));
        }
    }

    Ok(Persona {
        id: persona_id.to_owned(),
        display_name: display_name.to_owned(),
        persona_type: persona_type.to_owned(),
        created_at,
        extra_metadata: serde_json::Value::Object(serde_json::Map::new()),
    })
}

/// Delete a persona and all user_persona memberships (CASCADE).
/// Returns true if deleted, false if not found.
/// Does NOT delete per-persona databases (personal.db, outputs.db) —
/// those require explicit user confirmation and a separate cleanup operation.
pub async fn delete_persona(
    persona_id: &str,
) -> Result<bool, PersonaStoreError> {
    let mut conn = open_shared_db().await?;

    let result = sqlx::query("DELETE FROM personas WHERE id = ?")
        .bind(persona_id)
        .execute(&mut conn)
        .await?;

    Ok(result.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// User-persona membership
// ---------------------------------------------------------------------------

/// Add a user to a persona.
/// Returns true if added, false if already a member.
/// Returns Err(NotFound) if persona does not exist.
pub async fn add_user_to_persona(
    user_id: &str,
    persona_id: &str,
) -> Result<bool, PersonaStoreError> {
    // Verify persona exists first — mirrors Python LookupError behavior.
    if get_persona(persona_id).await?.is_none() {
        return Err(PersonaStoreError::NotFound(persona_id.to_owned()));
    }

    let mut conn = open_shared_db().await?;
    let timestamp = crate::providers::utils::now();

    let result = sqlx::query(
        "INSERT OR IGNORE INTO user_personas (user_id, persona_id, joined_at)
         VALUES (?, ?, ?)",
    )
    .bind(user_id)
    .bind(persona_id)
    .bind(&timestamp)
    .execute(&mut conn)
    .await?;

    // rows_affected == 0 means OR IGNORE fired — already a member.
    Ok(result.rows_affected() > 0)
}

/// Remove a user from a persona.
/// Returns true if removed, false if not a member.
pub async fn remove_user_from_persona(
    user_id: &str,
    persona_id: &str,
) -> Result<bool, PersonaStoreError> {
    let mut conn = open_shared_db().await?;

    let result = sqlx::query(
        "DELETE FROM user_personas WHERE user_id = ? AND persona_id = ?",
    )
    .bind(user_id)
    .bind(persona_id)
    .execute(&mut conn)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Check if a user is a member of a persona.
pub async fn is_user_in_persona(
    user_id: &str,
    persona_id: &str,
) -> Result<bool, PersonaStoreError> {
    let mut conn = open_shared_db().await?;

    let row = sqlx::query(
        "SELECT 1 FROM user_personas WHERE user_id = ? AND persona_id = ?",
    )
    .bind(user_id)
    .bind(persona_id)
    .fetch_optional(&mut conn)
    .await?;

    Ok(row.is_some())
}
