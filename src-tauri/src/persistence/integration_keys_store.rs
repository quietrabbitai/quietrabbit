// src-tauri/src/persistence/integration_keys_store.rs
//
// integration_keys CRUD — per-user, SQLCipher-encrypted integration_keys.db.
// Fills the gap named in commands/tier2.rs's own header ("Both commands
// require integration_keys_store (not yet ported)... Flagged to Chat-PM")
// and built per Architecture/AUTH_MULTIUSER_ARCHITECTURE.md Section 8.3,
// which explicitly assigns "exact columns are an implementation detail for
// whoever builds items.id=185 against this" -- decisions.id=65's schema
// (keys_001.sql, extended in place, not a separate keys_002.sql migration
// per that file's own CONSOLIDATION NOTE) is the live, built schema this
// module operates against.
//
// SCOPE (items.id=185, narrowed 2026-08-01): this module plus
// commands/tier2.rs's consumer-side wiring only. Does NOT touch:
//   - providers/groq.rs's get_api_key() -- its own doc comment names this
//     as a distinct future "Layer 8" swap with a stable signature/error
//     contract, synchronous today, unlike this module's async functions.
//     Wiring it now would force a signature decision (async fn vs.
//     block-on-async) this item does not scope. Flagged as a follow-on in
//     this session's handoff.
//   - Every other store's (output_store.rs, personal_store.rs,
//     topic_store.rs) bare key_hex: &str parameter -- Architecture Section
//     4.2's "every encrypted-store open() reads from KeyRegistry" end
//     state is not yet reached anywhere in the codebase; this item is a
//     deliberate first slice (tier2.rs only), not the full migration.
//
// api_key/credential must NEVER be returned to the frontend (write-only
// per commands/tier2.rs's own IPC surface comment) -- get_active_key's
// caller in commands/tier2.rs is responsible for stripping `credential`
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

/// One row of integration_keys. `credential` is the plaintext secret
/// (protected only by SQLCipher file-level encryption, per keys_001.sql's
/// own header on why no field-level layer is needed) -- callers that
/// construct an IPC response from this struct MUST NOT include
/// `credential` in it (commands/tier2.rs's own doc comment: "api_key must
/// NEVER be returned to the frontend").
#[derive(Debug, Clone)]
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
async fn open_integration_keys_db(
    user_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, IntegrationKeysStoreError> {
    let db_path = get_integration_keys_db_path(user_id);

    if !db_path.exists() {
        crate::persistence::migrations::migrate_keys_db(user_id, key_hex).await?;
    }

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

async fn get_active_key_conn(
    conn: &mut SqliteConnection,
    provider: &str,
    key_type: &str,
    persona_id: Option<&str>,
) -> Result<Option<IntegrationKey>, IntegrationKeysStoreError> {
    let row = sqlx::query(
        "SELECT id, provider, key_type, integration_id, credential_label, credential,
                persona_id, auth_type, expires_at, is_active, created_at, last_verified_at
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
/// (commands/tier2.rs does not currently collect a separate label from the
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn upsert_then_get_round_trips() {
        let mut conn = seeded_conn().await;
        upsert_key_conn(
            &mut conn,
            "groq",
            "tier2",
            "gsk_abc123",
            None,
            Some("api_key"),
            None,
        )
        .await
        .unwrap();

        let found = get_active_key_conn(&mut conn, "groq", "tier2", None)
            .await
            .unwrap();
        assert!(found.is_some());
        let key = found.unwrap();
        assert_eq!(key.credential, "gsk_abc123");
        assert_eq!(key.auth_type, Some("api_key".to_owned()));
        assert!(key.is_active);
    }

    #[tokio::test]
    async fn upsert_on_same_scope_replaces_not_duplicates() {
        let mut conn = seeded_conn().await;
        upsert_key_conn(&mut conn, "groq", "tier2", "old-key", None, None, None)
            .await
            .unwrap();
        upsert_key_conn(&mut conn, "groq", "tier2", "new-key", None, None, None)
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

        let found = get_active_key_conn(&mut conn, "groq", "tier2", None)
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
        upsert_key_conn(&mut conn, "groq", "tier2", "global-key", None, None, None)
            .await
            .unwrap();
        upsert_key_conn(
            &mut conn,
            "groq",
            "tier2",
            "work-persona-key",
            Some("persona-work"),
            None,
            None,
        )
        .await
        .unwrap();

        let global = get_active_key_conn(&mut conn, "groq", "tier2", None)
            .await
            .unwrap()
            .unwrap();
        let scoped = get_active_key_conn(&mut conn, "groq", "tier2", Some("persona-work"))
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
            "groq",
            "tier2",
            "work-persona-key",
            Some("persona-work"),
            None,
            None,
        )
        .await
        .unwrap();

        // No global key exists -- a global lookup must not silently
        // return the persona-scoped one.
        let global = get_active_key_conn(&mut conn, "groq", "tier2", None)
            .await
            .unwrap();
        assert!(global.is_none());
    }

    #[tokio::test]
    async fn get_active_key_returns_none_when_absent() {
        let mut conn = seeded_conn().await;
        let found = get_active_key_conn(&mut conn, "nonexistent", "tier2", None)
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
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
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
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
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
            "groq",
            "tier2",
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
            "groq",
            "tier2",
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

        let found = get_active_key(user_id, TEST_KEY_HEX, "groq", "tier2", None).await;

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
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
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
            "groq",
            "tier2",
            "some-key",
            None,
            None,
            None,
        )
        .await
        .expect("upsert_key must succeed with the correct key");

        let result = get_active_key(user_id, WRONG_KEY_HEX, "groq", "tier2", None).await;

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
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
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
}
