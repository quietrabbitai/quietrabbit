// src-tauri/src/commands/auth.rs
//
// Group 11 — Auth. Commands: login, logout, get_recovery_key_display.
//
// login/logout wired items.id=205 (2026-08-01) against the auth foundation
// (auth::kdf, auth::registry, auth::user_store) built earlier this session.
// get_recovery_key_display remains stubbed -- Section 5 (recovery mnemonic)
// is explicitly out of items.id=205's scope per the Chat-PM ruling.
//
// SIGNATURE CHANGE, flagged: login() originally took only `password:
// String`. It now also takes `display_name: String` -- user_store's
// login-lookup path is keyed on display_name (see auth::user_store's own
// module header on why), so a single-parameter login() cannot identify
// which account to authenticate against. No frontend exists yet to be
// broken by this (confirmed this session, Architecture Section 14), but
// this is a real IPC contract change from the original stub, not a
// transparent one.
//
// SCOPE SEAM for future onboarding (Jason's direction, 2026-08-01): the
// bootstrap branch below creates a user account ONLY -- no persona is
// created. A logged-in fresh account has no usable persona/Focus/data
// store until onboarding (a separate, later, larger piece of work) runs.
// The exact point a future persona-creation call would go is marked
// explicitly below, so that addition is a small, additive change to this
// branch, not a rewrite.
//
// LOCKOUT MECHANISM, NOT POLICY (scoped this session): auth_failures/
// auth_lockouts are written to on every failed/successful attempt below,
// but auth_lockout_enabled (instance_config) stays 'disabled' -- no
// threshold, no duration, no actual lockout enforcement is decided or
// built here. is_locked_out() always returns false while the flag is
// disabled; the bookkeeping exists so a future session can flip
// enforcement on without a schema or data-flow change.
//
// KEY VERIFICATION: there is no password_hash column and no separate
// password-verification step (Section 4.1/4.2: the derived Argon2id key
// IS the credential). A wrong password derives a different key, which
// then fails to decrypt integration_keys.db -- SQLCipher's key validation
// happens at connection-open time (verified against
// persistence::personal_store.rs::open_personal_db's identical pattern
// this session), which is what verify_integration_keys_db below
// detects. integration_keys.db is used for this (not personal.db/
// outputs.db) because it is USER-scoped, not persona-scoped
// (providers::utils::db_path_integration_keys(user_id) takes no
// persona_id) -- it is the only encrypted store guaranteed to exist for
// an account regardless of whether onboarding/persona-creation has
// happened yet.
//
// FAILURE CLASSIFICATION: a wrong key and an unrelated I/O error (disk
// full, permissions, genuine file corruption) are deliberately NOT treated
// the same (Jason's direction, 2026-08-01) -- only a confirmed wrong-key
// failure counts toward auth_failures/lockout bookkeeping. See
// KeyVerification below.
//
// SESSION LIFECYCLE ON LOGOUT: auth_sessions rows are soft-expired
// (expires_at set to now), never deleted -- Jason's direction, 2026-08-01,
// to preserve an audit trail of when sessions existed and ended, matching
// how auth_failures is treated as data worth keeping rather than
// discarding.

use tauri::State;

use crate::auth::kdf;
use crate::auth::registry::{KeyRegistry, UnlockedKey};
use crate::auth::user_store;

// ---------------------------------------------------------------------------
// shared.db opener (for auth_sessions/auth_failures/auth_lockouts)
// ---------------------------------------------------------------------------
// Duplicated from persona_store.rs/user_store.rs rather than reused, same
// reasoning as user_store.rs's own header: coupling to a foreign error
// type isn't worth it for ~12 lines with no per-caller variation.

async fn open_shared_db() -> Result<sqlx::SqliteConnection, String> {
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;

    let db_path = crate::persistence::migrations::get_data_root()
        .join("instance")
        .join("shared.db");
    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let journal_mode = if network_storage { "DELETE" } else { "WAL" };
    SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .pragma("journal_mode", journal_mode)
        .connect()
        .await
        .map_err(|e| format!("couldn't open shared.db: {e}"))
}

// ---------------------------------------------------------------------------
// Key verification against integration_keys.db
// ---------------------------------------------------------------------------

enum KeyVerification {
    /// Confirmed wrong key -- SQLCipher rejected an EXISTING file at
    /// connection-open time. Counts toward auth_failures/lockout.
    WrongKey,
    /// Any other failure (I/O, permissions, genuine corruption unrelated
    /// to the key). Must NOT count toward auth_failures/lockout.
    Other(String),
}

/// Open a user's EXISTING integration_keys.db under the given key, to
/// verify the key is correct. Does NOT create the file if missing --
/// unlike migrations::migrate_keys_db (which is create-or-migrate and is
/// used separately, only on the bootstrap path, to establish the file
/// under the real key for the first time). A missing file here is
/// classified as Other, not WrongKey -- an account past bootstrap should
/// always have this file; its absence is a different, more serious
/// problem than a wrong password and should not be silently folded into
/// the failed-attempt count.
async fn verify_integration_keys_db(user_id: &str, key_hex: &str) -> Result<(), KeyVerification> {
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;

    let db_path = crate::providers::utils::db_path_integration_keys(user_id);
    if !db_path.exists() {
        return Err(KeyVerification::Other(
            "integration_keys.db does not exist for this account".to_owned(),
        ));
    }

    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let journal_mode = if network_storage { "DELETE" } else { "WAL" };

    SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        // SQLCipher's PRAGMA key blob-literal syntax requires the x'...'
        // form itself wrapped in an outer pair of double quotes (verified
        // this session against SQLCipher's own documentation and by
        // isolating the exact failure with a standalone diagnostic test --
        // omitting the outer quotes produces "syntax error near x'...'" on
        // every SQLite/SQLCipher build tested here, confirmed reproducibly).
        // persistence::personal_store.rs::open_personal_db has this exact
        // same bug (format!("x'{key_hex}'") without outer quotes) -- it
        // was never caught because that module has zero tests (confirmed
        // this session); flagged in this session's handoff rather than
        // fixed here, since personal_store.rs is outside items.id=205's
        // scope.
        .pragma("key", format!("\"x'{key_hex}'\""))
        .pragma("journal_mode", journal_mode)
        .connect()
        .await
        .map(|_conn| ())
        .map_err(|e| {
            let msg = e.to_string().to_lowercase();
            if msg.contains("not a database") || msg.contains("file is not a database") {
                KeyVerification::WrongKey
            } else {
                KeyVerification::Other(e.to_string())
            }
        })
}

fn key_hex(key: &[u8; kdf::MASTER_KEY_LEN]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Lockout bookkeeping (mechanism only -- see module header)
// ---------------------------------------------------------------------------

/// Always false while instance_config.auth_lockout_enabled = 'disabled'
/// (its current, unchanged default -- this session builds bookkeeping,
/// not enforcement). Queries the flag rather than hardcoding false, so
/// flipping enforcement on later is a data change, not a code change.
async fn is_locked_out(display_name: &str) -> Result<bool, String> {
    let mut conn = open_shared_db().await?;
    let enabled: Option<(String,)> =
        sqlx::query_as("SELECT value FROM instance_config WHERE key = 'auth_lockout_enabled'")
            .fetch_optional(&mut conn)
            .await
            .map_err(|e| e.to_string())?;

    if enabled.map(|(v,)| v) != Some("enabled".to_owned()) {
        return Ok(false);
    }

    let row: Option<(String,)> =
        sqlx::query_as("SELECT locked_until FROM auth_lockouts WHERE display_name = ?")
            .bind(display_name)
            .fetch_optional(&mut conn)
            .await
            .map_err(|e| e.to_string())?;

    match row {
        None => Ok(false),
        Some((locked_until,)) => {
            Ok(locked_until.as_str() > crate::providers::utils::now().as_str())
        }
    }
}

/// Record a failed attempt in auth_failures. Lockout THRESHOLD/DURATION
/// logic is deliberately not built here (policy, not mechanism -- see
/// module header) -- this only appends the bookkeeping row.
async fn record_failed_attempt(display_name: &str) -> Result<(), String> {
    let mut conn = open_shared_db().await?;
    sqlx::query(
        "INSERT INTO auth_failures (id, display_name, attempted_at)
         VALUES (?, ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(display_name)
    .bind(crate::providers::utils::now())
    .execute(&mut conn)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn login(
    display_name: String,
    password: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<(), String> {
    let has_users = user_store::has_any_users()
        .await
        .map_err(|e| e.to_string())?;

    if !has_users {
        // BOOTSTRAP (Architecture Section 2.3: "First run: create the
        // primary admin user... require a password at that point, no
        // skip"). The supplied password becomes the new account's
        // password directly -- there is no separate account-creation step
        // for login() to defer to (Jason's direction, 2026-08-01).
        let salt = kdf::generate_salt().map_err(|e| e.to_string())?;
        let master_key = kdf::derive_master_key(
            password.as_bytes(),
            &salt,
            kdf::DEFAULT_ARGON2_MEMORY_KIB,
            kdf::DEFAULT_ARGON2_ITERATIONS,
            kdf::DEFAULT_ARGON2_PARALLELISM,
        )
        .map_err(|e| e.to_string())?;

        let user_id = uuid::Uuid::new_v4().to_string();
        user_store::create_user(
            &user_id,
            &display_name,
            "admin",
            true,
            &salt,
            kdf::DEFAULT_ARGON2_MEMORY_KIB,
            kdf::DEFAULT_ARGON2_ITERATIONS,
            kdf::DEFAULT_ARGON2_PARALLELISM,
        )
        .await
        .map_err(|e| e.to_string())?;

        // Establishes integration_keys.db fresh, under the real key, for
        // the first time -- this IS creation (migrate_keys_db is
        // create-or-migrate), not verification, so the generic migrations
        // helper is correct here, not verify_integration_keys_db above
        // (which deliberately refuses to create).
        crate::persistence::migrations::migrate_keys_db(&user_id, &key_hex(&master_key))
            .await
            // MigrationError's Display impl (via #[error(...)] on each
            // variant) already produces a message -- only the Failed
            // variant carries a plain_language field specifically,
            // matching it alone would miss Locked/Sqlx/Io. e.to_string()
            // via Display covers all four correctly.
            .map_err(|e| e.to_string())?;

        // Establishes tier3_cookies.db fresh, same create-or-migrate call
        // shape as integration_keys.db just above (items.id=224 resolution,
        // decisions.id=711) -- see tier3_cookies_001.sql's own header.
        crate::persistence::migrations::migrate_tier3_cookies_db(&user_id, &key_hex(&master_key))
            .await
            .map_err(|e| e.to_string())?;

        // <<< SEAM: if future onboarding design decides the first-run
        // flow should also create a default persona, that call goes here
        // -- e.g. persona_store::create_persona(&user_id, ...). Not built
        // now: no onboarding design exists yet to specify what persona
        // type/name/Focus set that would be (Jason's direction,
        // 2026-08-01). A fresh account past this point can log in
        // successfully but has no usable persona/data store until
        // onboarding runs -- a real, named, deliberate intermediate
        // state, not an oversight. >>>

        finish_login(&user_id, master_key, &key_registry).await
    } else {
        let user = user_store::find_user_by_display_name(&display_name)
            .await
            .map_err(|e| e.to_string())?
            // Same error message as a wrong password below -- do not
            // reveal whether the display_name itself exists.
            .ok_or_else(|| "invalid credentials".to_owned())?;

        if is_locked_out(&display_name).await? {
            return Err("account locked, try again later".to_owned());
        }

        let (salt, mem, iter, par) = user_store::get_salt_params(&user.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                // Should be impossible: create_user() always writes both
                // rows atomically. Surfaced distinctly rather than folded
                // into "invalid credentials" -- this is a real internal
                // inconsistency, not a wrong password, and hiding it
                // behind the generic message would make it undebuggable.
                "internal error: account exists with no salt record".to_owned()
            })?;

        let candidate_key = kdf::derive_master_key(password.as_bytes(), &salt, mem, iter, par)
            .map_err(|e| e.to_string())?;

        match verify_integration_keys_db(&user.id, &key_hex(&candidate_key)).await {
            Err(KeyVerification::WrongKey) => {
                record_failed_attempt(&display_name).await?;
                Err("invalid credentials".to_owned())
            }
            Err(KeyVerification::Other(msg)) => {
                // Deliberately NOT recorded as a failed attempt (Jason's
                // direction, 2026-08-01) -- an I/O problem is not evidence
                // the password was wrong.
                Err(format!("couldn't verify credentials: {msg}"))
            }
            Ok(()) => finish_login(&user.id, candidate_key, &key_registry).await,
        }
    }
}

async fn finish_login(
    user_id: &str,
    master_key: [u8; kdf::MASTER_KEY_LEN],
    key_registry: &State<'_, KeyRegistry>,
) -> Result<(), String> {
    let now = crate::providers::utils::now();
    key_registry
        .replace(UnlockedKey {
            user_id: user_id.to_owned(),
            master_key,
            unlocked_at: now.clone(),
        })
        .await;

    let mut conn = open_shared_db().await?;
    // Section 3.1: absolute session lifetime, "hard ceiling... currently
    // 30 days... stays out of the settings surface entirely" -- a
    // constant, not a per-row/per-user configurable field (unlike
    // idle_timeout_minutes on users, which is per-user and adjustable).
    let expires_at = (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339();
    sqlx::query(
        "INSERT INTO auth_sessions
         (session_id, user_id, created_at, last_active_at, expires_at, is_remember_me)
         VALUES (?, ?, ?, ?, ?, 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(&now)
    .bind(&now)
    .bind(&expires_at)
    .execute(&mut conn)
    .await
    .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn logout(key_registry: State<'_, KeyRegistry>) -> Result<(), String> {
    let user_id = key_registry.with_key(|k| k.user_id.clone()).await;
    key_registry.clear().await;

    if let Some(user_id) = user_id {
        let mut conn = open_shared_db().await?;
        let now = crate::providers::utils::now();
        // Soft-expire (Jason's direction, 2026-08-01): update expires_at
        // rather than DELETE, to preserve an audit trail of when sessions
        // existed and ended.
        sqlx::query(
            "UPDATE auth_sessions SET expires_at = ?
             WHERE user_id = ? AND expires_at > ?",
        )
        .bind(&now)
        .bind(&user_id)
        .bind(&now)
        .execute(&mut conn)
        .await
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn get_recovery_key_display() -> Result<crate::commands::NotImplementedPlaceholder, String>
{
    Err("not_implemented".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::Manager;

    // Shared across every test module that mutates QR_DATA_ROOT (items.id=
    // 205, 2026-08-01) -- see test_support.rs for why this must be one
    // true mutex, not a per-module copy. This file previously declared its
    // own local static ENV_MUTEX (reasoning at the time: "not shared...
    // small, stable, no real duplication cost") -- that reasoning was
    // wrong: three textually-identical but structurally independent
    // ENV_MUTEX statics (this file, auth::user_store, providers::utils)
    // raced against each other under parallel test execution, causing 15
    // spurious failures when the full suite ran (confirmed this session --
    // passed 438/438 serial via --test-threads=1, failed 15 in parallel
    // before this fix). Fixed by importing the one shared mutex instead.
    use crate::test_support::ENV_MUTEX;

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

    /// A mock Tauri app with a fresh, empty KeyRegistry managed as state --
    /// tauri::test::mock_app() (feature "test", dev-dependencies only)
    /// builds a real App<MockRuntime> without needing a webview or config
    /// file (verified against tauri 2.11.x docs this session). Commands
    /// are called directly as functions, not through the IPC layer -- this
    /// tests login()/logout()'s own logic, not Tauri's invoke plumbing.
    fn mock_app_with_registry() -> tauri::App<tauri::test::MockRuntime> {
        let app = tauri::test::mock_app();
        app.manage(KeyRegistry::default());
        app
    }

    #[tokio::test]
    async fn bootstrap_login_creates_primary_admin_and_populates_registry() {
        let _env = setup().await;
        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();

        let result = login(
            "Alice".to_owned(),
            "correct horse battery staple".to_owned(),
            registry.clone(),
        )
        .await;
        assert!(result.is_ok(), "bootstrap login should succeed: {result:?}");
        assert!(
            registry.is_occupied().await,
            "registry should hold a key after bootstrap login"
        );

        let user = user_store::find_user_by_display_name("Alice")
            .await
            .unwrap();
        assert!(user.is_some());
        let user = user.unwrap();
        assert_eq!(user.role, "admin");
        assert!(user.is_primary);
    }

    #[tokio::test]
    async fn bootstrap_login_writes_a_session_row() {
        let _env = setup().await;
        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
        )
        .await
        .unwrap();

        let mut conn = open_shared_db().await.unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM auth_sessions")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(
            count.0, 1,
            "exactly one session row should exist after bootstrap login"
        );
    }

    #[tokio::test]
    async fn second_bootstrap_attempt_is_a_real_login_not_a_second_bootstrap() {
        // Once has_any_users() is true, a subsequent login() call must take
        // the existing-account branch, not try to bootstrap a second
        // primary admin (which the DB would reject anyway via the partial
        // unique index -- but login() should never attempt it).
        let _env = setup().await;
        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
        )
        .await
        .unwrap();
        registry.clear().await; // simulate a fresh process, key not resident

        let result = login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
        )
        .await;
        assert!(
            result.is_ok(),
            "logging in again with the correct password should succeed: {result:?}"
        );
    }

    #[tokio::test]
    async fn wrong_password_on_existing_account_fails_and_does_not_populate_registry() {
        let _env = setup().await;
        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();

        login(
            "Alice".to_owned(),
            "correct-password".to_owned(),
            registry.clone(),
        )
        .await
        .unwrap();
        registry.clear().await;

        let result = login(
            "Alice".to_owned(),
            "wrong-password".to_owned(),
            registry.clone(),
        )
        .await;
        assert!(result.is_err(), "wrong password must fail");
        assert!(
            !registry.is_occupied().await,
            "registry must stay empty after a failed login"
        );
    }

    #[tokio::test]
    async fn wrong_password_records_a_failed_attempt() {
        let _env = setup().await;
        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();

        login(
            "Alice".to_owned(),
            "correct-password".to_owned(),
            registry.clone(),
        )
        .await
        .unwrap();
        registry.clear().await;

        let _ = login(
            "Alice".to_owned(),
            "wrong-password".to_owned(),
            registry.clone(),
        )
        .await;

        let mut conn = open_shared_db().await.unwrap();
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM auth_failures WHERE display_name = 'Alice'")
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(
            count.0, 1,
            "one auth_failures row should be recorded for the wrong password"
        );
    }

    #[tokio::test]
    async fn unknown_display_name_fails_without_revealing_nonexistence() {
        let _env = setup().await;
        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();

        // Bootstrap someone first so has_any_users() is true and this
        // exercises the existing-account branch, not bootstrap.
        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
        )
        .await
        .unwrap();
        registry.clear().await;

        let result = login("Nobody".to_owned(), "whatever".to_owned(), registry.clone()).await;
        assert_eq!(result, Err("invalid credentials".to_owned()));
    }

    #[tokio::test]
    async fn logout_clears_the_registry() {
        let _env = setup().await;
        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
        )
        .await
        .unwrap();
        assert!(registry.is_occupied().await);

        logout(registry.clone()).await.unwrap();
        assert!(
            !registry.is_occupied().await,
            "registry must be empty after logout"
        );
    }

    #[tokio::test]
    async fn logout_soft_expires_the_session_row_rather_than_deleting_it() {
        let _env = setup().await;
        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
        )
        .await
        .unwrap();
        logout(registry.clone()).await.unwrap();

        let mut conn = open_shared_db().await.unwrap();
        // Row must still exist (not deleted) -- audit trail, Jason's
        // direction, 2026-08-01.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM auth_sessions")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(
            count.0, 1,
            "session row must persist after logout, not be deleted"
        );

        let row: (String,) = sqlx::query_as("SELECT expires_at FROM auth_sessions LIMIT 1")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        let now = crate::providers::utils::now();
        assert!(
            row.0 <= now,
            "expires_at must be updated to at or before now after logout"
        );
    }

    #[tokio::test]
    async fn logout_on_empty_registry_is_a_no_op_not_an_error() {
        let _env = setup().await;
        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();

        let result = logout(registry.clone()).await;
        assert!(result.is_ok(), "logout with no resident key must not error");
    }
}
