// src-tauri/src/auth/user_store.rs
//
// User account CRUD against shared.db's users/user_salts tables (items.id=
// 205). Mirrors persistence/persona_store.rs's connection pattern
// (open_shared_db, SAVEPOINT-atomic multi-table writes, sqlx::query()
// runtime style).
//
// open_shared_db() IS DUPLICATED here rather than reused from
// persona_store.rs -- considered reuse first (raised in external review
// this session) and rejected on a concrete basis, not convenience:
// persona_store::open_shared_db() returns Result<_, PersonaStoreError>,
// coupling its signature to that module's own error type. Reusing it here
// would mean either (a) this module's functions awkwardly convert through
// PersonaStoreError, or (b) changing persona_store's existing, working
// signature to something more generic -- a real change to already-shipped
// code for a ~12-line, zero-divergence-risk helper (path+pragma+connect,
// no per-caller variation). Judged not worth it; duplication accepted here
// deliberately, not by default.
//
// SCOPE BOUNDARY, deliberate: this module creates a user + salt row ONLY.
// It does not create a persona, does not run any per-persona DB migration,
// and is never called with persona-creation logic inside it. Onboarding
// design (a separate, later, larger piece of work -- Jason's direction,
// 2026-08-01) may decide the first-run flow should also create a default
// persona; if so, that's an ADDITIVE call from login()'s bootstrap branch
// to persona_store::create_persona() (which already exists), not a change
// to this module. Keeping create_user() single-purpose is what makes that
// addition clean later rather than requiring this module to be reworked.
//
// NO PASSWORD VERIFIER STORED HERE, deliberately: this module creates a
// user + salt row only -- the derived Argon2id master key IS the
// credential (Section 4.1/4.2). There is no separate password_hash
// verification step; a wrong password simply derives a different (wrong)
// key, which then fails to open that user's SQLCipher-encrypted stores.
// login() (a later step in this same file tree) is responsible for
// detecting that failure and writing it to auth_failures.
//
// LOGIN IDENTIFIER: display_name is used as the lookup key for a login
// attempt. This is a SCHEMA-DERIVED inference, not a documented
// architectural decision -- Architecture/AUTH_MULTIUSER_ARCHITECTURE.md
// never discusses login-identifier mechanics at all (verified via direct
// search this session, zero hits for display_name/username/login
// identifier in that document). display_name is simply the only UNIQUE,
// human-facing field on the users table, making it the only viable
// candidate today. If onboarding design later introduces a separate
// login-identifier concept, this is the function to revisit.

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UserStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("User '{0}' already exists")]
    AlreadyExists(String),
    #[error("Stored salt for user '{0}' is not valid hex and cannot be decoded")]
    CorruptSalt(String),
    #[error("Stored KDF parameter for user '{0}' is out of range: {1}")]
    CorruptKdfParams(String, String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct UserRecord {
    pub id: String,
    pub display_name: String,
    pub role: String,
    pub is_primary: bool,
    pub idle_timeout_minutes: i64,
}

async fn open_shared_db() -> Result<SqliteConnection, UserStoreError> {
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

/// True if any user row exists at all -- the "fresh install" check
/// login()'s bootstrap branch uses to decide whether to create the primary
/// admin (Section 2.3: "First run: create the primary admin user...").
pub async fn has_any_users() -> Result<bool, UserStoreError> {
    let mut conn = open_shared_db().await?;
    let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM users LIMIT 1")
        .fetch_optional(&mut conn)
        .await?;
    Ok(row.is_some())
}

/// Fetch a user by display_name. See module header on why display_name is
/// the login identifier.
pub async fn find_user_by_display_name(
    display_name: &str,
) -> Result<Option<UserRecord>, UserStoreError> {
    let mut conn = open_shared_db().await?;
    let row = sqlx::query(
        "SELECT id, display_name, role, is_primary, idle_timeout_minutes
         FROM users WHERE display_name = ?",
    )
    .bind(display_name)
    .fetch_optional(&mut conn)
    .await?;
    Ok(row.map(|r| UserRecord {
        id: r.get("id"),
        display_name: r.get("display_name"),
        role: r.get("role"),
        is_primary: r.get::<i64, _>("is_primary") != 0,
        idle_timeout_minutes: r.get("idle_timeout_minutes"),
    }))
}

/// Decode a hex string into bytes, failing on any malformed input rather
/// than silently dropping unparseable pairs. Authentication material must
/// fail loudly on corruption, not partially decode (corrected this
/// session, per external review -- an earlier draft used filter_map,
/// which would silently truncate malformed hex instead of erroring).
fn hex_decode(user_id: &str, s: &str) -> Result<Vec<u8>, UserStoreError> {
    if !s.len().is_multiple_of(2) {
        return Err(UserStoreError::CorruptSalt(user_id.to_owned()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| UserStoreError::CorruptSalt(user_id.to_owned()))
        })
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Read a user's stored salt + Argon2id parameters, for deriving the
/// master key at login. Returns None if the user has no salt row (should
/// not happen for any user created via create_user(), but checked
/// explicitly rather than assumed).
pub async fn get_salt_params(
    user_id: &str,
) -> Result<Option<(Vec<u8>, u32, u32, u32)>, UserStoreError> {
    let mut conn = open_shared_db().await?;
    let row = sqlx::query(
        "SELECT salt_hex, kdf_memory_kib, kdf_iterations, kdf_parallelism
         FROM user_salts WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut conn)
    .await?;

    let Some(row) = row else { return Ok(None) };

    let salt_hex: String = row.get("salt_hex");
    let salt = hex_decode(user_id, &salt_hex)?;

    let to_u32 = |field: &str, value: i64| -> Result<u32, UserStoreError> {
        u32::try_from(value)
            .map_err(|_| UserStoreError::CorruptKdfParams(user_id.to_owned(), field.to_owned()))
    };
    let memory_kib = to_u32("kdf_memory_kib", row.get("kdf_memory_kib"))?;
    let iterations = to_u32("kdf_iterations", row.get("kdf_iterations"))?;
    let parallelism = to_u32("kdf_parallelism", row.get("kdf_parallelism"))?;

    Ok(Some((salt, memory_kib, iterations, parallelism)))
}

/// Read a user's stored Tier 2 provider preference (items.id=251).
/// `"mistral"` / `"groq"` / `NULL` per schema/shared_001.sql's CHECK
/// constraint. No user row and a `NULL` preference both collapse to
/// `Ok(None)` -- callers (conductor/lifecycle.rs) treat "unset" and
/// "unknown user" identically, so this function doesn't distinguish them.
pub async fn get_tier2_provider_preference(
    user_id: &str,
) -> Result<Option<String>, UserStoreError> {
    let mut conn = open_shared_db().await?;
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT tier2_provider_preference FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut conn)
            .await?;
    Ok(row.and_then(|(pref,)| pref))
}

/// Create a new user + their salt row, atomically (SAVEPOINT, mirrors
/// persona_store::create_persona's pattern -- on failure, ROLLBACK TO
/// without a subsequent RELEASE, since the connection is dropped
/// immediately after returning; verified this session against
/// persona_store.rs's existing, working implementation of the same
/// pattern rather than assumed).
///
/// auth_enabled is hardcoded to 1 here, not exposed as a parameter --
/// Section 2.1 reframes it as an onboarding-time invariant evaluated once
/// at account creation (not a standing runtime toggle), so every account
/// this function creates satisfies that invariant by construction.
///
/// #[allow(clippy::too_many_arguments)] justification (items.id=207): 8
/// params, all distinct account-creation fields with no natural grouping;
/// bundling into a struct would be a real API change touching
/// commands/auth.rs's call site, out of scope for a lint-silencing pass.
/// Revisit if a struct-shaped caller emerges naturally (e.g. an onboarding
/// request type).
#[allow(clippy::too_many_arguments)]
pub async fn create_user(
    user_id: &str,
    display_name: &str,
    role: &str,
    is_primary: bool,
    salt: &[u8],
    kdf_memory_kib: u32,
    kdf_iterations: u32,
    kdf_parallelism: u32,
) -> Result<(), UserStoreError> {
    let created_at = crate::providers::utils::now();
    let mut conn = open_shared_db().await?;

    sqlx::query("SAVEPOINT create_user")
        .execute(&mut conn)
        .await?;

    let step: Result<(), sqlx::Error> = async {
        sqlx::query(
            "INSERT INTO users
             (id, display_name, role, is_primary, auth_enabled, created_at)
             VALUES (?, ?, ?, ?, 1, ?)",
        )
        .bind(user_id)
        .bind(display_name)
        .bind(role)
        .bind(is_primary as i64)
        .bind(&created_at)
        .execute(&mut conn)
        .await?;

        sqlx::query(
            "INSERT INTO user_salts
             (user_id, salt_hex, kdf_algorithm, kdf_memory_kib,
              kdf_iterations, kdf_parallelism, created_at)
             VALUES (?, ?, 'argon2id', ?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(hex_encode(salt))
        .bind(kdf_memory_kib)
        .bind(kdf_iterations)
        .bind(kdf_parallelism)
        .bind(&created_at)
        .execute(&mut conn)
        .await?;

        Ok(())
    }
    .await;

    match step {
        Ok(()) => {
            sqlx::query("RELEASE create_user")
                .execute(&mut conn)
                .await?;
            Ok(())
        }
        Err(e) => {
            let _ = sqlx::query("ROLLBACK TO create_user")
                .execute(&mut conn)
                .await;
            // Structured constraint detection (DatabaseError::is_unique_violation),
            // not string-matching -- corrected this session per external
            // review; verified sqlx exposes this method directly rather
            // than assumed.
            if let sqlx::Error::Database(ref db_err) = e {
                if db_err.is_unique_violation() {
                    return Err(UserStoreError::AlreadyExists(display_name.to_owned()));
                }
            }
            Err(UserStoreError::Database(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serialize all env-mutating tests -- Rust test runner is multi-threaded,
    // and QR_DATA_ROOT is process-global state. Shared across every test
    // module that mutates QR_DATA_ROOT (items.id=205, 2026-08-01) -- see
    // test_support.rs for why this must be one true mutex, not a per-module
    // copy (this file previously had its own local static, which raced
    // against providers::utils's and commands::auth's own local copies
    // under parallel test execution; fixed this session).
    use crate::test_support::ENV_MUTEX;

    /// Sets QR_DATA_ROOT to a fresh temp directory, migrates the shared
    /// schema into it (via the real migrations::migrate_shared_db() path,
    /// same one production startup uses), and returns the TempDir --
    /// caller must keep it alive for the duration of the test (dropping it
    /// deletes the directory). Restores the prior QR_DATA_ROOT on scope
    /// exit via the returned guard.
    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn setup() -> TestEnv {
        let lock = ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();

        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration must succeed in test setup");

        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
        }
    }

    #[tokio::test]
    async fn fresh_db_has_no_users() {
        let _env = setup().await;
        assert!(!has_any_users().await.unwrap());
    }

    #[tokio::test]
    async fn create_user_then_has_any_users_is_true() {
        let _env = setup().await;
        create_user("u1", "Alice", "admin", true, b"salt1234", 1024, 1, 1)
            .await
            .expect("create_user should succeed");
        assert!(has_any_users().await.unwrap());
    }

    #[tokio::test]
    async fn find_user_by_display_name_round_trips() {
        let _env = setup().await;
        create_user("u1", "Alice", "admin", true, b"salt1234", 1024, 1, 1)
            .await
            .unwrap();

        let found = find_user_by_display_name("Alice").await.unwrap();
        assert!(found.is_some());
        let record = found.unwrap();
        assert_eq!(record.id, "u1");
        assert_eq!(record.role, "admin");
        assert!(record.is_primary);
        assert_eq!(record.idle_timeout_minutes, 15, "schema default must apply");
    }

    #[tokio::test]
    async fn find_user_by_display_name_returns_none_when_absent() {
        let _env = setup().await;
        let found = find_user_by_display_name("Nobody").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn create_user_duplicate_display_name_is_already_exists() {
        let _env = setup().await;
        create_user("u1", "Alice", "admin", true, b"salt1234", 1024, 1, 1)
            .await
            .unwrap();

        let result = create_user("u2", "Alice", "user", false, b"salt5678", 1024, 1, 1).await;
        assert!(matches!(result, Err(UserStoreError::AlreadyExists(_))));
    }

    #[tokio::test]
    async fn create_user_second_primary_fails() {
        // shared_001.sql's idx_users_single_primary partial unique index
        // must reject a second is_primary=true row -- confirms create_user
        // doesn't accidentally bypass that constraint.
        let _env = setup().await;
        create_user("u1", "Alice", "admin", true, b"salt1234", 1024, 1, 1)
            .await
            .unwrap();

        let result = create_user("u2", "Bob", "admin", true, b"salt5678", 1024, 1, 1).await;
        assert!(
            result.is_err(),
            "a second is_primary=true user must be rejected"
        );
    }

    #[tokio::test]
    async fn get_salt_params_round_trips() {
        let _env = setup().await;
        create_user(
            "u1",
            "Alice",
            "admin",
            true,
            &[0xDE, 0xAD, 0xBE, 0xEF],
            65536,
            3,
            4,
        )
        .await
        .unwrap();

        let params = get_salt_params("u1").await.unwrap();
        assert!(params.is_some());
        let (salt, mem, iter, par) = params.unwrap();
        assert_eq!(salt, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(mem, 65536);
        assert_eq!(iter, 3);
        assert_eq!(par, 4);
    }

    #[tokio::test]
    async fn get_salt_params_returns_none_for_unknown_user() {
        let _env = setup().await;
        let params = get_salt_params("nonexistent").await.unwrap();
        assert!(params.is_none());
    }

    #[tokio::test]
    async fn get_tier2_provider_preference_round_trips() {
        // No setter exists yet (items.id=251 only wires the read path) --
        // written directly via UPDATE, same as tier2.rs's own tests reach
        // past the command layer to seed store state directly.
        let _env = setup().await;
        create_user("u1", "Alice", "admin", true, b"salt1234", 1024, 1, 1)
            .await
            .unwrap();

        let mut conn = open_shared_db().await.unwrap();
        sqlx::query("UPDATE users SET tier2_provider_preference = ? WHERE id = ?")
            .bind("mistral")
            .bind("u1")
            .execute(&mut conn)
            .await
            .unwrap();

        let pref = get_tier2_provider_preference("u1").await.unwrap();
        assert_eq!(pref, Some("mistral".to_owned()));
    }

    #[tokio::test]
    async fn get_tier2_provider_preference_is_none_when_unset() {
        let _env = setup().await;
        create_user("u1", "Alice", "admin", true, b"salt1234", 1024, 1, 1)
            .await
            .unwrap();

        let pref = get_tier2_provider_preference("u1").await.unwrap();
        assert_eq!(pref, None, "schema default for tier2_provider_preference is NULL");
    }

    #[tokio::test]
    async fn get_tier2_provider_preference_is_none_for_unknown_user() {
        let _env = setup().await;
        let pref = get_tier2_provider_preference("nonexistent").await.unwrap();
        assert_eq!(pref, None);
    }

    #[test]
    fn hex_decode_round_trips_with_hex_encode() {
        let original = vec![0x00, 0xFF, 0x42, 0xAB, 0xCD];
        let encoded = hex_encode(&original);
        let decoded = hex_decode("test-user", &encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn hex_decode_rejects_odd_length() {
        let result = hex_decode("test-user", "abc");
        assert!(matches!(result, Err(UserStoreError::CorruptSalt(_))));
    }

    #[test]
    fn hex_decode_rejects_non_hex_characters() {
        // Regression test for the silent-truncation bug found this session
        // (external review) -- must error, not silently drop the bad pair.
        let result = hex_decode("test-user", "abxxcd");
        assert!(matches!(result, Err(UserStoreError::CorruptSalt(_))));
    }

    #[test]
    fn hex_decode_empty_string_is_empty_vec_not_error() {
        let result = hex_decode("test-user", "").unwrap();
        assert_eq!(result, Vec::<u8>::new());
    }
}
