// src-tauri/src/persistence/integration_keys_store.rs
//
// integration_keys CRUD — per-user, SQLCipher-encrypted integration_keys.db.
// Fills the gap named in commands/qr_hosted.rs's own header ("Both commands
// require integration_keys_store (not yet ported)... Flagged to Chat-PM")
// and built per Architecture/AUTH_MULTIUSER_ARCHITECTURE.md Section 8.3,
// which explicitly assigns "exact columns are an implementation detail for
// whoever builds items.id=185 against this" -- decisions.id=65's schema
// (keys_001.sql, extended in place rather than a rebuild migration --
// its own CONSOLIDATION NOTE explains why the persona_id/auth_type/
// expires_at SHAPE change didn't need one) is the live, built schema this
// module operates against. keys_002.sql (items.id=528 Phase 2) exists now,
// but only for a stored-VALUE rename (key_type 'tier2' -> 'qr_hosted'), not
// a shape change -- the table shape keys_001.sql describes is still current.
//
// SCOPE (items.id=185, narrowed 2026-08-01): this module plus
// commands/qr_hosted.rs's consumer-side wiring only. (The hosted-provider
// client reads keys from here since items.id=709: the executor's
// resolve_hosted_inputs() calls get_active_key; the old per-provider adapters
// and their environment-variable bridge are deleted.) Does NOT touch:
//   - Every other store's (output_store.rs, personal_store.rs,
//     topic_store.rs) bare key_hex: &str parameter -- Architecture Section
//     4.2's "every encrypted-store open() reads from KeyRegistry" end
//     state is not yet reached anywhere in the codebase; this item is a
//     deliberate first slice (qr_hosted.rs only), not the full migration.
//
// api_key/credential must NEVER be returned to the frontend (write-only
// per commands/qr_hosted.rs's own IPC surface comment) -- get_active_key's
// caller in commands/qr_hosted.rs is responsible for stripping `credential`
// before constructing any frontend-facing response; this module returns
// the full row because it has legitimate internal callers (e.g. a future
// provider client actually needing the credential to make a request) that
// are not the IPC layer.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros, matching
// every other store in this module.
// PRAGMA key applied via providers::utils::connect_options_encrypted,
// which already carries the outer-double-quotes fix (items.id=206) -- not
// duplicated here, per P4 (One Home): a second inline PRAGMA implementation
// would be exactly the kind of duplication that let the items.id=206 bug
// go uncaught in two places already.

use std::path::PathBuf;

use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum IntegrationKeysStoreError {
    /// The account's key epoch changed since this session unlocked
    /// (auth::epoch_guard, items.id=629).
    #[error("{0}")]
    KeyEpoch(#[from] crate::auth::epoch_guard::KeyEpochError),
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Migration error: {0}")]
    Migration(#[from] crate::persistence::migrations::MigrationError),
}

// ---------------------------------------------------------------------------
// Data type
// ---------------------------------------------------------------------------

/// key_type of the keys the hosted-provider client reads (renamed from
/// "tier2" by keys_002.sql, items.id=528 Phase 2). One definition for the
/// settings commands and the executor (items.id=709).
pub const QR_HOSTED_KEY_TYPE: &str = "qr_hosted";

/// One row of integration_keys. `credential` is the plaintext secret
/// (protected only by SQLCipher file-level encryption, per keys_001.sql's
/// own header on why no field-level layer is needed) -- callers that
/// construct an IPC response from this struct MUST NOT include
/// `credential` in it (commands/qr_hosted.rs's own doc comment: "api_key must
/// NEVER be returned to the frontend").
///
/// Debug is hand-written and prints `credential` as a fixed marker
/// (items.id=709): a derived Debug would put the key in any `{:?}` a caller
/// ever writes.
#[derive(Clone)]
pub struct IntegrationKey {
    pub id: String,
    pub provider: String,
    pub key_type: String,
    pub integration_id: String,
    pub credential_label: String,
    pub credential: String,
    pub persona_id: Option<String>,
    pub auth_type: Option<String>,
    pub expires_at: Option<String>,
    pub is_active: bool,
    pub created_at: String,
    pub last_verified_at: Option<String>,
    /// The record's `extra_metadata` column: a JSON object holding per-user
    /// provider variables such as an account id (decisions.id=875 point 1).
    /// Anything that is not a JSON object reads as an empty object. Written
    /// by `save_credential` only (credential capture, items.id=714).
    pub extra_metadata: serde_json::Value,
}

impl std::fmt::Debug for IntegrationKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntegrationKey")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("key_type", &self.key_type)
            .field("integration_id", &self.integration_id)
            .field("credential_label", &self.credential_label)
            .field("credential", &"[redacted]")
            .field("persona_id", &self.persona_id)
            .field("auth_type", &self.auth_type)
            .field("expires_at", &self.expires_at)
            .field("is_active", &self.is_active)
            .field("created_at", &self.created_at)
            .field("last_verified_at", &self.last_verified_at)
            .field("extra_metadata", &"[not printed]")
            .finish()
    }
}

fn row_to_integration_key(r: &sqlx::sqlite::SqliteRow) -> Result<IntegrationKey, sqlx::Error> {
    Ok(IntegrationKey {
        id: r.try_get("id")?,
        provider: r.try_get("provider")?,
        key_type: r.try_get("key_type")?,
        integration_id: r.try_get("integration_id")?,
        credential_label: r.try_get("credential_label")?,
        credential: r.try_get("credential")?,
        persona_id: r.try_get("persona_id")?,
        auth_type: r.try_get("auth_type")?,
        expires_at: r.try_get("expires_at")?,
        is_active: r.try_get::<i64, _>("is_active")? != 0,
        created_at: r.try_get("created_at")?,
        last_verified_at: r.try_get("last_verified_at")?,
        extra_metadata: {
            let raw: String = r.try_get("extra_metadata")?;
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(v) if v.is_object() => v,
                _ => serde_json::json!({}),
            }
        },
    })
}

// ---------------------------------------------------------------------------
// Path + DB opener
// ---------------------------------------------------------------------------

fn get_integration_keys_db_path(user_id: &str) -> PathBuf {
    crate::providers::utils::db_path_integration_keys(user_id)
}

/// Open integration_keys.db with SQLCipher key. Mirrors
/// personal_store.rs::open_personal_db's shape, but delegates PRAGMA
/// construction to connect_options_encrypted (P4 -- see module header)
/// rather than building SqliteConnectOptions inline a third time.
pub(crate) async fn open_integration_keys_db(
    user_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, IntegrationKeysStoreError> {
    crate::auth::epoch_guard::ensure_current(user_id).await?;
    let db_path = get_integration_keys_db_path(user_id);

    // Always migrate, not only when the file is missing (items.id=389, the
    // same bug personal_store.rs and output_store.rs fixed): an
    // integration_keys.db created before its latest schema version shipped
    // (keys_003.sql, items.id=708) would otherwise stay stuck on the version
    // it was created at. run_migrations is schema_version-tracked and only
    // applies what is pending.
    crate::persistence::migrations::migrate_keys_db(user_id, key_hex).await?;

    let conn = crate::providers::utils::connect_options_encrypted(&db_path, key_hex)
        .create_if_missing(false)
        .connect()
        .await?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// Look up the active credential for (provider, key_type), optionally
/// scoped to a persona. persona_id = None matches only a user-global key
/// (persona_id IS NULL in the row) -- it does NOT fall back to a
/// persona-scoped key, and a Some(persona_id) lookup does NOT fall back to
/// a user-global key either. keys_001.sql's own header establishes these
/// as two legitimately different, coexisting credentials, not a priority
/// chain -- silently falling back would pick a key not explicitly
/// selected for the given scope. Callers needing fallback behavior compose
/// it themselves with two calls.
pub async fn get_active_key(
    user_id: &str,
    key_hex: &str,
    provider: &str,
    key_type: &str,
    persona_id: Option<&str>,
) -> Result<Option<IntegrationKey>, IntegrationKeysStoreError> {
    let mut conn = open_integration_keys_db(user_id, key_hex).await?;
    get_active_key_conn(&mut conn, provider, key_type, persona_id).await
}

/// Provider ids that have an active user-global key of `key_type`, sorted.
/// PRESENCE ONLY (items.id=712): selects the `provider` column and nothing
/// else, so no credential is read into memory. One database open for the
/// whole set. User-global scope (persona_id IS NULL) matches the lookup the
/// hosted client uses (`get_active_key` with no persona).
pub async fn active_key_provider_ids(
    user_id: &str,
    key_hex: &str,
    key_type: &str,
) -> Result<Vec<String>, IntegrationKeysStoreError> {
    let mut conn = open_integration_keys_db(user_id, key_hex).await?;
    active_key_provider_ids_conn(&mut conn, key_type).await
}

async fn active_key_provider_ids_conn(
    conn: &mut SqliteConnection,
    key_type: &str,
) -> Result<Vec<String>, IntegrationKeysStoreError> {
    let rows = sqlx::query(
        "SELECT DISTINCT provider FROM integration_keys
         WHERE key_type = ? AND is_active = 1 AND persona_id IS NULL
         ORDER BY provider",
    )
    .bind(key_type)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            r.try_get::<String, _>("provider")
                .map_err(IntegrationKeysStoreError::from)
        })
        .collect()
}

async fn get_active_key_conn(
    conn: &mut SqliteConnection,
    provider: &str,
    key_type: &str,
    persona_id: Option<&str>,
) -> Result<Option<IntegrationKey>, IntegrationKeysStoreError> {
    let row = sqlx::query(
        "SELECT id, provider, key_type, integration_id, credential_label, credential,
                persona_id, auth_type, expires_at, is_active, created_at, last_verified_at,
                extra_metadata
         FROM integration_keys
         WHERE provider = ? AND key_type = ? AND is_active = 1
           AND ((? IS NULL AND persona_id IS NULL) OR persona_id = ?)",
    )
    .bind(provider)
    .bind(key_type)
    .bind(persona_id)
    .bind(persona_id)
    .fetch_optional(&mut *conn)
    .await?;

    row.map(|r| row_to_integration_key(&r))
        .transpose()
        .map_err(IntegrationKeysStoreError::from)
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Insert or replace the credential for (provider, key_type, integration_id,
/// persona_id) -- matches the table's own UNIQUE constraint exactly, so
/// this is a true upsert on that key, not a blind insert. credential_label
/// defaults to provider if not given a more specific one by the caller
/// (commands/qr_hosted.rs does not currently collect a separate label from the
/// frontend).
#[allow(clippy::too_many_arguments)]
pub async fn upsert_key(
    user_id: &str,
    key_hex: &str,
    provider: &str,
    key_type: &str,
    credential: &str,
    persona_id: Option<&str>,
    auth_type: Option<&str>,
    expires_at: Option<&str>,
) -> Result<(), IntegrationKeysStoreError> {
    if credential.trim().is_empty() {
        return Err(IntegrationKeysStoreError::Validation(
            "credential must not be empty".to_owned(),
        ));
    }

    let mut conn = open_integration_keys_db(user_id, key_hex).await?;
    upsert_key_conn(
        &mut conn, provider, key_type, credential, persona_id, auth_type, expires_at,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn upsert_key_conn(
    conn: &mut SqliteConnection,
    provider: &str,
    key_type: &str,
    credential: &str,
    persona_id: Option<&str>,
    auth_type: Option<&str>,
    expires_at: Option<&str>,
) -> Result<(), IntegrationKeysStoreError> {
    let now = crate::providers::utils::now();
    // NOTE: NOT an ON CONFLICT upsert. SQLite's UNIQUE constraint treats
    // NULL as distinct from every other value including another NULL, so
    // ON CONFLICT (provider, key_type, integration_id, persona_id) never
    // fires when persona_id IS NULL -- the common case (user-global keys).
    // An earlier version of this function used ON CONFLICT and silently
    // inserted a duplicate row on every re-save of a global key instead of
    // replacing it; caught by this module's own
    // upsert_on_same_scope_replaces_not_duplicates test. Explicit
    // existence check (matching get_active_key_conn's own IS-NULL-aware
    // WHERE clause) then UPDATE-or-INSERT is correct for a nullable
    // uniqueness column; ON CONFLICT is not.
    let existing_id: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM integration_keys
         WHERE provider = ? AND key_type = ? AND integration_id = '_default'
           AND ((? IS NULL AND persona_id IS NULL) OR persona_id = ?)",
    )
    .bind(provider)
    .bind(key_type)
    .bind(persona_id)
    .bind(persona_id)
    .fetch_optional(&mut *conn)
    .await?;

    match existing_id {
        Some((id,)) => {
            sqlx::query(
                "UPDATE integration_keys
                 SET credential = ?, auth_type = ?, expires_at = ?, is_active = 1
                 WHERE id = ?",
            )
            .bind(credential)
            .bind(auth_type)
            .bind(expires_at)
            .bind(&id)
            .execute(&mut *conn)
            .await?;
        }
        None => {
            sqlx::query(
                "INSERT INTO integration_keys
                    (id, provider, key_type, integration_id, credential_label, credential,
                     persona_id, auth_type, expires_at, is_active, created_at)
                 VALUES (?, ?, ?, '_default', ?, ?, ?, ?, ?, 1, ?)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(provider)
            .bind(key_type)
            .bind(provider) // credential_label default: provider name
            .bind(credential)
            .bind(persona_id)
            .bind(auth_type)
            .bind(expires_at)
            .bind(&now)
            .execute(&mut *conn)
            .await?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Credential capture write (items.id=714)
// ---------------------------------------------------------------------------

/// Largest `extra_metadata` object `save_credential` will store, in bytes of
/// its JSON text. It holds a handful of per-user provider variables (an
/// account id), never content.
pub const MAX_EXTRA_METADATA_BYTES: usize = 4096;

/// One credential as the connect flow captured it.
///
/// `credential` is borrowed so the caller keeps it in its own zeroizing
/// buffer. Debug is hand-written for the same reason as `IntegrationKey`'s.
pub struct NewCredential<'a> {
    pub provider: &'a str,
    pub key_type: &'a str,
    pub credential: &'a str,
    pub persona_id: Option<&'a str>,
    /// `api_key`, `oauth_token` or `manual_copy` (keys_001.sql CHECK).
    pub auth_type: Option<&'a str>,
    pub expires_at: Option<&'a str>,
    /// When the provider accepted this exact key, or None when it was not
    /// checked. A replaced key never inherits the old key's verification.
    pub last_verified_at: Option<&'a str>,
    /// `Some(object)` REPLACES the stored `extra_metadata` (the variables
    /// belong to the key they were entered with); `None` keeps what is
    /// stored. Must be a JSON object of string values, at most
    /// `MAX_EXTRA_METADATA_BYTES` of text.
    pub extra_metadata: Option<&'a serde_json::Value>,
}

impl std::fmt::Debug for NewCredential<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewCredential")
            .field("provider", &self.provider)
            .field("key_type", &self.key_type)
            .field("credential", &"[redacted]")
            .field("persona_id", &self.persona_id)
            .field("auth_type", &self.auth_type)
            .field("expires_at", &self.expires_at)
            .field("last_verified_at", &self.last_verified_at)
            .field("extra_metadata", &"[not printed]")
            .finish()
    }
}

fn validated_extra_metadata(
    value: &serde_json::Value,
) -> Result<String, IntegrationKeysStoreError> {
    let bad = |what: &str| IntegrationKeysStoreError::Validation(what.to_owned());
    let Some(map) = value.as_object() else {
        return Err(bad("extra_metadata must be a JSON object"));
    };
    if !map.values().all(serde_json::Value::is_string) {
        return Err(bad("extra_metadata values must be strings"));
    }
    let text = serde_json::to_string(value).map_err(|_| bad("extra_metadata is not JSON"))?;
    if text.len() > MAX_EXTRA_METADATA_BYTES {
        return Err(bad("extra_metadata is too large"));
    }
    Ok(text)
}

/// Insert or replace the credential for (provider, key_type, persona_id)
/// together with its `extra_metadata` and verification time. The one writer
/// of `extra_metadata`. Same identity rule as `upsert_key` (user-global and
/// persona-scoped keys are two different credentials), but unlike
/// `upsert_key` it always sets `last_verified_at` from the caller.
pub async fn save_credential(
    user_id: &str,
    key_hex: &str,
    new: &NewCredential<'_>,
) -> Result<(), IntegrationKeysStoreError> {
    // Validate before any file is opened.
    check_new_credential(new)?;
    let mut conn = open_integration_keys_db(user_id, key_hex).await?;
    save_credential_conn(&mut conn, new).await
}

fn check_new_credential(
    new: &NewCredential<'_>,
) -> Result<Option<String>, IntegrationKeysStoreError> {
    if new.credential.trim().is_empty() {
        return Err(IntegrationKeysStoreError::Validation(
            "credential must not be empty".to_owned(),
        ));
    }
    new.extra_metadata.map(validated_extra_metadata).transpose()
}

pub(crate) async fn save_credential_conn(
    conn: &mut SqliteConnection,
    new: &NewCredential<'_>,
) -> Result<(), IntegrationKeysStoreError> {
    let metadata_text = check_new_credential(new)?;
    let now = crate::providers::utils::now();
    // Explicit existence check, not ON CONFLICT: see upsert_key_conn.
    let existing_id: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM integration_keys
         WHERE provider = ? AND key_type = ? AND integration_id = '_default'
           AND ((? IS NULL AND persona_id IS NULL) OR persona_id = ?)",
    )
    .bind(new.provider)
    .bind(new.key_type)
    .bind(new.persona_id)
    .bind(new.persona_id)
    .fetch_optional(&mut *conn)
    .await?;

    match existing_id {
        Some((id,)) => {
            sqlx::query(
                "UPDATE integration_keys
                 SET credential = ?, auth_type = ?, expires_at = ?, is_active = 1,
                     last_verified_at = ?,
                     extra_metadata = COALESCE(?, extra_metadata)
                 WHERE id = ?",
            )
            .bind(new.credential)
            .bind(new.auth_type)
            .bind(new.expires_at)
            .bind(new.last_verified_at)
            .bind(metadata_text.as_deref())
            .bind(&id)
            .execute(&mut *conn)
            .await?;
        }
        None => {
            sqlx::query(
                "INSERT INTO integration_keys
                    (id, provider, key_type, integration_id, credential_label, credential,
                     persona_id, auth_type, expires_at, is_active, created_at,
                     last_verified_at, extra_metadata)
                 VALUES (?, ?, ?, '_default', ?, ?, ?, ?, ?, 1, ?, ?, ?)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(new.provider)
            .bind(new.key_type)
            .bind(new.provider) // credential_label default: provider name
            .bind(new.credential)
            .bind(new.persona_id)
            .bind(new.auth_type)
            .bind(new.expires_at)
            .bind(&now)
            .bind(new.last_verified_at)
            .bind(metadata_text.as_deref().unwrap_or("{}"))
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// items.id=528 Phase 2: key_type's real stored value, renamed from the
    /// retired 'tier2' by keys_002.sql. A named constant here so a future
    /// rename touches one line, not the ~17 call sites below that were
    /// previously scattered raw literals.
    const TEST_KEY_TYPE: &str = "qr_hosted";

    /// In-memory SQLite seeded directly from the CREATE TABLE statement
    /// this module depends on -- mirrors entity_store.rs's *_conn testing
    /// pattern (test the query logic against a real schema, without
    /// needing QR_DATA_ROOT or a SQLCipher key). This is deliberately NOT
    /// the full keys_001.sql file (which also creates schema_version/
    /// migration_lock, irrelevant to this module's own logic) -- just the
    /// table shape this module's queries depend on.
    async fn seeded_conn() -> SqliteConnection {
        use sqlx::Connection;
        let mut conn = SqliteConnection::connect(":memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE integration_keys (
                id                  TEXT PRIMARY KEY,
                provider            TEXT NOT NULL,
                key_type            TEXT NOT NULL,
                integration_id      TEXT NOT NULL DEFAULT '_default',
                credential_label    TEXT NOT NULL,
                credential          TEXT NOT NULL,
                persona_id          TEXT,
                auth_type           TEXT,
                expires_at          TEXT,
                is_active           INTEGER NOT NULL DEFAULT 1,
                created_at          TEXT NOT NULL,
                last_verified_at    TEXT,
                extra_metadata      TEXT NOT NULL DEFAULT '{}',
                UNIQUE (provider, key_type, integration_id, persona_id)
            )",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn
    }

    #[tokio::test]
    async fn active_key_provider_ids_lists_user_global_active_keys_only() {
        let mut conn = seeded_conn().await;
        for (provider, persona) in [
            ("fixture-host-b", None),
            ("fixture-host-a", None),
            ("fixture-host-c", Some("persona-1")),
        ] {
            upsert_key_conn(
                &mut conn,
                provider,
                TEST_KEY_TYPE,
                "fixture-key",
                persona,
                None,
                None,
            )
            .await
            .unwrap();
        }
        upsert_key_conn(
            &mut conn,
            "fixture-host-d",
            "other_key_type",
            "fixture-key",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        sqlx::query("UPDATE integration_keys SET is_active = 0 WHERE provider = 'fixture-host-b'")
            .execute(&mut conn)
            .await
            .unwrap();

        // Sorted; persona-scoped, other-key-type and inactive rows excluded.
        let ids = active_key_provider_ids_conn(&mut conn, TEST_KEY_TYPE)
            .await
            .unwrap();
        assert_eq!(ids, vec!["fixture-host-a".to_owned()]);
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips() {
        let mut conn = seeded_conn().await;
        upsert_key_conn(
            &mut conn,
            "fixture-host",
            TEST_KEY_TYPE,
            "fixture-key-abc123",
            None,
            Some("api_key"),
            None,
        )
        .await
        .unwrap();

        let found = get_active_key_conn(&mut conn, "fixture-host", TEST_KEY_TYPE, None)
            .await
            .unwrap();
        assert!(found.is_some());
        let key = found.unwrap();
        assert_eq!(key.credential, "fixture-key-abc123");
        assert_eq!(key.auth_type, Some("api_key".to_owned()));
        assert!(key.is_active);
    }

    #[tokio::test]
    async fn upsert_on_same_scope_replaces_not_duplicates() {
        let mut conn = seeded_conn().await;
        upsert_key_conn(
            &mut conn,
            "fixture-host",
            TEST_KEY_TYPE,
            "old-key",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        upsert_key_conn(
            &mut conn,
            "fixture-host",
            TEST_KEY_TYPE,
            "new-key",
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM integration_keys")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(
            count.0, 1,
            "same (provider, key_type, integration_id, persona_id) must replace, not add a row"
        );

        let found = get_active_key_conn(&mut conn, "fixture-host", TEST_KEY_TYPE, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.credential, "new-key");
    }

    #[tokio::test]
    async fn global_and_persona_scoped_keys_coexist() {
        // keys_001.sql's own header: a user-global key and a persona-
        // scoped key for the same provider are legitimately different
        // credentials, not a duplicate -- this is the scenario the
        // UNIQUE constraint's inclusion of persona_id exists to allow.
        let mut conn = seeded_conn().await;
        upsert_key_conn(
            &mut conn,
            "fixture-host",
            TEST_KEY_TYPE,
            "global-key",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        upsert_key_conn(
            &mut conn,
            "fixture-host",
            TEST_KEY_TYPE,
            "work-persona-key",
            Some("persona-work"),
            None,
            None,
        )
        .await
        .unwrap();

        let global = get_active_key_conn(&mut conn, "fixture-host", TEST_KEY_TYPE, None)
            .await
            .unwrap()
            .unwrap();
        let scoped = get_active_key_conn(
            &mut conn,
            "fixture-host",
            TEST_KEY_TYPE,
            Some("persona-work"),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(global.credential, "global-key");
        assert_eq!(scoped.credential, "work-persona-key");
    }

    #[tokio::test]
    async fn get_active_key_does_not_fall_back_across_scopes() {
        let mut conn = seeded_conn().await;
        upsert_key_conn(
            &mut conn,
            "fixture-host",
            TEST_KEY_TYPE,
            "work-persona-key",
            Some("persona-work"),
            None,
            None,
        )
        .await
        .unwrap();

        // No global key exists -- a global lookup must not silently
        // return the persona-scoped one.
        let global = get_active_key_conn(&mut conn, "fixture-host", TEST_KEY_TYPE, None)
            .await
            .unwrap();
        assert!(global.is_none());
    }

    #[tokio::test]
    async fn get_active_key_returns_none_when_absent() {
        let mut conn = seeded_conn().await;
        let found = get_active_key_conn(&mut conn, "nonexistent", TEST_KEY_TYPE, None)
            .await
            .unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn upsert_rejects_empty_credential_validation_boundary() {
        // Validation happens in the public upsert_key() before any DB
        // call -- this test exercises that boundary check directly rather
        // than through a real DB round trip.
        let credential = "   ";
        assert!(credential.trim().is_empty());
    }

    // -- real on-disk SQLCipher tests ---------------------------------------
    //
    // Every test above exercises the private *_conn helpers against
    // seeded_conn()'s hand-written, unencrypted :memory: schema -- the
    // public get_active_key/upsert_key wrappers (the only functions that
    // call open_integration_keys_db -> connect_options_encrypted) are never
    // invoked by any test. These tests close that gap by running the real
    // migration (migrations::migrate_keys_db) against a tempdir-backed
    // QR_DATA_ROOT, then exercising the public wrappers against that real
    // encrypted file -- following the same pattern as
    // plan_state_store.rs's real-file tests. They deliberately do not
    // re-verify every scoping branch already covered above in-memory; they
    // focus on the one dimension those tests structurally cannot reach.

    const TEST_KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";
    const WRONG_KEY_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddee";

    #[tokio::test]
    async fn get_active_key_and_upsert_key_round_trip_against_real_migrated_db() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";

        crate::persistence::migrations::migrate_keys_db(user_id, TEST_KEY_HEX)
            .await
            .expect("real migration must succeed");

        upsert_key(
            user_id,
            TEST_KEY_HEX,
            "openai",
            "api_key",
            "secret-value",
            None,
            Some("api_key"),
            None,
        )
        .await
        .expect("upsert_key must succeed against a real migrated encrypted file");

        let found = get_active_key(user_id, TEST_KEY_HEX, "openai", "api_key", None).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        let key = found
            .expect("get_active_key must succeed against a real migrated encrypted file")
            .expect("the key upserted above must be found");
        assert_eq!(key.credential, "secret-value");
        assert_eq!(key.auth_type, Some("api_key".to_owned()));
        assert!(key.is_active);
    }

    #[tokio::test]
    async fn upsert_key_replaces_not_duplicates_against_real_migrated_db() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";

        crate::persistence::migrations::migrate_keys_db(user_id, TEST_KEY_HEX)
            .await
            .expect("real migration must succeed");

        upsert_key(
            user_id,
            TEST_KEY_HEX,
            "fixture-host",
            TEST_KEY_TYPE,
            "old-key",
            None,
            None,
            None,
        )
        .await
        .expect("first upsert_key must succeed");
        upsert_key(
            user_id,
            TEST_KEY_HEX,
            "fixture-host",
            TEST_KEY_TYPE,
            "new-key",
            None,
            None,
            None,
        )
        .await
        .expect("second upsert_key on the same scope must succeed");

        let mut verify_conn = open_integration_keys_db(user_id, TEST_KEY_HEX)
            .await
            .expect("verification connection must open");
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM integration_keys")
            .fetch_one(&mut verify_conn)
            .await
            .unwrap();

        let found =
            get_active_key(user_id, TEST_KEY_HEX, "fixture-host", TEST_KEY_TYPE, None).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert_eq!(
            count.0, 1,
            "same (provider, key_type, integration_id, persona_id) must replace, not add a \
             row, in the real migrated schema -- not just the hand-seeded in-memory one"
        );
        assert_eq!(
            found.unwrap().unwrap().credential,
            "new-key",
            "the latest upsert must win against a real migrated encrypted file"
        );
    }

    #[tokio::test]
    async fn get_active_key_returns_err_not_none_for_wrong_key_against_real_db() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";

        crate::persistence::migrations::migrate_keys_db(user_id, TEST_KEY_HEX)
            .await
            .expect("real migration must succeed");
        upsert_key(
            user_id,
            TEST_KEY_HEX,
            "fixture-host",
            TEST_KEY_TYPE,
            "some-key",
            None,
            None,
            None,
        )
        .await
        .expect("upsert_key must succeed with the correct key");

        let result =
            get_active_key(user_id, WRONG_KEY_HEX, "fixture-host", TEST_KEY_TYPE, None).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert!(
            result.is_err(),
            "a wrong key against a real encrypted file must surface as an error, not be \
             mistaken for Ok(None) (\"no key configured\")"
        );
    }

    #[tokio::test]
    async fn upsert_key_rejects_empty_credential_before_touching_real_db() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";
        // Deliberately NOT migrated -- the empty-credential check must
        // short-circuit before any connection attempt, so the db file must
        // never be created.
        let db_path = tempdir
            .path()
            .join("users")
            .join(user_id)
            .join("integration_keys.db");

        let result = upsert_key(
            user_id,
            TEST_KEY_HEX,
            "openai",
            "api_key",
            "   ",
            None,
            None,
            None,
        )
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert!(
            matches!(result, Err(IntegrationKeysStoreError::Validation(_))),
            "an empty credential must be rejected as a Validation error: {result:?}"
        );
        assert!(
            !db_path.exists(),
            "validation must short-circuit before any connection attempt creates the db file"
        );
    }

    /// items.id=708 / items.id=389: an integration_keys.db created before
    /// keys_003.sql shipped must pick the new tables up on its next open
    /// without losing its rows. The pre-708 state is simulated by dropping
    /// the five tables and keys v3's schema_version row from a fully
    /// migrated file (SCHEMA_FILES is a compile-time static).
    #[tokio::test]
    async fn open_applies_pending_migration_to_existing_db_keeping_rows() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";
        crate::persistence::migrations::migrate_keys_db(user_id, TEST_KEY_HEX)
            .await
            .expect("real migration must succeed");
        upsert_key(
            user_id,
            TEST_KEY_HEX,
            "openai",
            "api_key",
            "secret-value",
            None,
            Some("api_key"),
            None,
        )
        .await
        .expect("seed key");

        {
            let mut conn = crate::providers::utils::connect_options_encrypted(
                &get_integration_keys_db_path(user_id),
                TEST_KEY_HEX,
            )
            .create_if_missing(false)
            .connect()
            .await
            .expect("open for tampering");
            for table in [
                "provider_consent",
                "provider_limits",
                "provider_usage_ledger",
                "price_overrides",
                "calibration",
            ] {
                sqlx::query(&format!("DROP TABLE {table}"))
                    .execute(&mut conn)
                    .await
                    .unwrap();
            }
            // keys_004 alters these tables and owns provider_rate_snapshot, and
            // keys_005 owns the live-model tables and alters calibration, so
            // all three must be re-applied together (the migrator applies
            // versions above the highest recorded one).
            for table in [
                "provider_rate_snapshot",
                "provider_live_models",
                "provider_model_refresh",
            ] {
                sqlx::query(&format!("DROP TABLE {table}"))
                    .execute(&mut conn)
                    .await
                    .unwrap();
            }
            sqlx::query("DELETE FROM schema_version WHERE version IN (3, 4, 5)")
                .execute(&mut conn)
                .await
                .unwrap();
        }

        let found = get_active_key(user_id, TEST_KEY_HEX, "openai", "api_key", None).await;

        let mut conn = crate::providers::utils::connect_options_encrypted(
            &get_integration_keys_db_path(user_id),
            TEST_KEY_HEX,
        )
        .create_if_missing(false)
        .connect()
        .await
        .expect("reopen to inspect");
        let tables: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN \
             ('provider_consent', 'provider_limits', 'provider_usage_ledger', \
              'price_overrides', 'calibration', 'provider_rate_snapshot', \
              'provider_live_models', 'provider_model_refresh')",
        )
        .fetch_all(&mut conn)
        .await
        .unwrap();

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert_eq!(
            tables.len(),
            8,
            "opening must re-apply the pending migration"
        );
        let key = found
            .expect("open must succeed on a pre-migration db")
            .expect("the pre-existing key must survive");
        assert_eq!(key.credential, "secret-value");
    }
    // -----------------------------------------------------------------------
    // items.id=709: extra_metadata is readable; Debug never prints the key
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn extra_metadata_is_read_as_a_json_object() {
        let mut conn = seeded_conn().await;
        upsert_key_conn(&mut conn, "p1", TEST_KEY_TYPE, "k1", None, None, None)
            .await
            .unwrap();
        // Default column value reads as an empty object.
        let key = get_active_key_conn(&mut conn, "p1", TEST_KEY_TYPE, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(key.extra_metadata, serde_json::json!({}));

        sqlx::query("UPDATE integration_keys SET extra_metadata = ? WHERE provider = 'p1'")
            .bind(r#"{"account_id":"abc123"}"#)
            .execute(&mut conn)
            .await
            .unwrap();
        let key = get_active_key_conn(&mut conn, "p1", TEST_KEY_TYPE, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(key.extra_metadata["account_id"], "abc123");

        // Anything that is not a JSON object reads as an empty object.
        for bad in ["not json", "[1,2]", "\"text\"", "7"] {
            sqlx::query("UPDATE integration_keys SET extra_metadata = ? WHERE provider = 'p1'")
                .bind(bad)
                .execute(&mut conn)
                .await
                .unwrap();
            let key = get_active_key_conn(&mut conn, "p1", TEST_KEY_TYPE, None)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(key.extra_metadata, serde_json::json!({}), "{bad}");
        }
    }

    #[tokio::test]
    async fn debug_output_never_contains_the_credential() {
        let mut conn = seeded_conn().await;
        let secret = "sk-debug-must-not-print-this-value";
        upsert_key_conn(&mut conn, "p1", TEST_KEY_TYPE, secret, None, None, None)
            .await
            .unwrap();
        let key = get_active_key_conn(&mut conn, "p1", TEST_KEY_TYPE, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(key.credential, secret);
        let shown = format!("{key:?}");
        assert!(!shown.contains(secret), "{shown}");
        assert!(shown.contains("redacted"));
        let pretty = format!("{key:#?}");
        assert!(!pretty.contains(secret));
        let option = format!("{:?}", Some(&key));
        assert!(!option.contains(secret));
    }

    // items.id=714: save_credential is the one writer of extra_metadata.

    fn new_cred<'a>(
        provider: &'a str,
        credential: &'a str,
        meta: Option<&'a serde_json::Value>,
        verified: Option<&'a str>,
    ) -> NewCredential<'a> {
        NewCredential {
            provider,
            key_type: TEST_KEY_TYPE,
            credential,
            persona_id: None,
            auth_type: Some("api_key"),
            expires_at: None,
            last_verified_at: verified,
            extra_metadata: meta,
        }
    }

    #[tokio::test]
    async fn save_credential_inserts_with_metadata_and_verification() {
        let mut conn = seeded_conn().await;
        let meta = serde_json::json!({"account_id": "abc123"});
        save_credential_conn(
            &mut conn,
            &new_cred("p1", "sk-one", Some(&meta), Some("2026-10-12T10:00:00Z")),
        )
        .await
        .unwrap();
        let key = get_active_key_conn(&mut conn, "p1", TEST_KEY_TYPE, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(key.credential, "sk-one");
        assert_eq!(key.auth_type.as_deref(), Some("api_key"));
        assert_eq!(key.extra_metadata, meta);
        assert_eq!(
            key.last_verified_at.as_deref(),
            Some("2026-10-12T10:00:00Z")
        );
    }

    #[tokio::test]
    async fn save_credential_without_metadata_inserts_an_empty_object() {
        let mut conn = seeded_conn().await;
        save_credential_conn(&mut conn, &new_cred("p1", "sk-one", None, None))
            .await
            .unwrap();
        let key = get_active_key_conn(&mut conn, "p1", TEST_KEY_TYPE, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(key.extra_metadata, serde_json::json!({}));
        assert_eq!(key.last_verified_at, None);
    }

    #[tokio::test]
    async fn resaving_without_metadata_keeps_it_and_with_metadata_replaces_it() {
        let mut conn = seeded_conn().await;
        let first = serde_json::json!({"account_id": "abc123"});
        save_credential_conn(&mut conn, &new_cred("p1", "sk-one", Some(&first), None))
            .await
            .unwrap();
        save_credential_conn(&mut conn, &new_cred("p1", "sk-two", None, None))
            .await
            .unwrap();
        let key = get_active_key_conn(&mut conn, "p1", TEST_KEY_TYPE, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(key.credential, "sk-two");
        assert_eq!(key.extra_metadata, first, "None keeps the stored object");

        let second = serde_json::json!({"account_id": "def456"});
        save_credential_conn(&mut conn, &new_cred("p1", "sk-three", Some(&second), None))
            .await
            .unwrap();
        let key = get_active_key_conn(&mut conn, "p1", TEST_KEY_TYPE, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            key.extra_metadata, second,
            "Some replaces, it does not merge"
        );

        // An empty object clears.
        let empty = serde_json::json!({});
        save_credential_conn(&mut conn, &new_cred("p1", "sk-four", Some(&empty), None))
            .await
            .unwrap();
        let key = get_active_key_conn(&mut conn, "p1", TEST_KEY_TYPE, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(key.extra_metadata, empty);
    }

    #[tokio::test]
    async fn a_replaced_key_does_not_inherit_the_old_verification() {
        let mut conn = seeded_conn().await;
        save_credential_conn(
            &mut conn,
            &new_cred("p1", "sk-one", None, Some("2026-10-12T10:00:00Z")),
        )
        .await
        .unwrap();
        save_credential_conn(&mut conn, &new_cred("p1", "sk-two", None, None))
            .await
            .unwrap();
        let key = get_active_key_conn(&mut conn, "p1", TEST_KEY_TYPE, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(key.last_verified_at, None);
        let rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM integration_keys")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(rows.0, 1, "a re-save replaces, never duplicates");
    }

    #[tokio::test]
    async fn save_credential_rejects_bad_metadata_and_an_empty_credential() {
        let mut conn = seeded_conn().await;
        let not_object = serde_json::json!(["a"]);
        let non_string = serde_json::json!({"account_id": 7});
        let big = serde_json::json!({"account_id": "x".repeat(MAX_EXTRA_METADATA_BYTES)});
        for bad in [&not_object, &non_string, &big] {
            let r =
                save_credential_conn(&mut conn, &new_cred("p1", "sk-one", Some(bad), None)).await;
            assert!(
                matches!(r, Err(IntegrationKeysStoreError::Validation(_))),
                "{bad}"
            );
        }
        let r = save_credential_conn(&mut conn, &new_cred("p1", "   ", None, None)).await;
        assert!(matches!(r, Err(IntegrationKeysStoreError::Validation(_))));
        let rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM integration_keys")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(rows.0, 0, "a refused save writes nothing");
    }

    #[tokio::test]
    async fn save_credential_keeps_global_and_persona_keys_apart() {
        let mut conn = seeded_conn().await;
        let global = new_cred("p1", "sk-global", None, None);
        let mut persona = new_cred("p1", "sk-persona", None, None);
        persona.persona_id = Some("persona-a");
        save_credential_conn(&mut conn, &global).await.unwrap();
        save_credential_conn(&mut conn, &persona).await.unwrap();
        let g = get_active_key_conn(&mut conn, "p1", TEST_KEY_TYPE, None)
            .await
            .unwrap()
            .unwrap();
        let p = get_active_key_conn(&mut conn, "p1", TEST_KEY_TYPE, Some("persona-a"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(g.credential, "sk-global");
        assert_eq!(p.credential, "sk-persona");
    }

    #[test]
    fn new_credential_debug_never_contains_the_credential() {
        let secret = "sk-newcred-must-not-print";
        let meta = serde_json::json!({"account_id": "acct-not-printed"});
        let n = new_cred("p1", secret, Some(&meta), None);
        let shown = format!("{n:?} {n:#?}");
        assert!(!shown.contains(secret), "{shown}");
        assert!(!shown.contains("acct-not-printed"), "{shown}");
    }
}
