// src-tauri/src/auth/idle_timeout.rs
//
// Idle-timeout enforcement for KeyRegistry (items.id=311). Closes a gap
// items.id=272's security/privacy audit found: users.idle_timeout_minutes
// (shared_001.sql, default 15, bounded 5-240) was stored and returned into
// UserRecord but nothing ever read it, and auth_sessions.last_active_at was
// written once at login and never updated or read again.
//
// ACTIVITY SIGNAL (items.id=311 design question 1): frontend-driven, not
// backend-command-tracked. There is no centralized auth guard/middleware in
// this codebase -- every command module independently reaches into
// KeyRegistry -- so "bump last_active_at on every authenticated command"
// would mean touching every command module now and forever after, or
// building an interceptor layer as a prerequisite. It's also the less
// correct signal: a long-running backend call (an LLM generation, a Focus
// execution) firing while the user has genuinely stepped away would keep
// resetting the idle clock under that design, defeating the point of an
// idle lock. commands::auth::record_activity is the one new IPC command a
// debounced frontend listener calls; this module never receives raw
// activity events, only reads auth_sessions.last_active_at back.
//
// SLEEP/SUSPEND (items.id=311 design question 2): see KeyRegistry::clear()'s
// own doc comment for the full investigation. Summary: no reliable
// cross-platform Tauri hook exists on this app's desktop targets, and the
// elapsed-wall-clock check below already produces the same outcome a sleep/
// suspend hook would, since wall-clock time keeps advancing through a
// suspend regardless of whether this process's own timer was frozen for
// the duration.
//
// TIMER PLACEMENT: driven by its own dedicated tokio::time::interval in
// main.rs, deliberately not folded into the existing 300s persona_sync/
// group_sync sweep loop there -- idle_timeout_minutes can be set as low as
// 5 minutes, and a 300s check granularity could let a 5-minute setting run
// up to 2x over before firing. Keeping this on its own timer also isolates
// it from that loop's fire-and-forget panic exposure (a panic anywhere in
// that loop kills the entire timer permanently, taking every sweep riding
// it down with it -- confirmed against tauri::async_runtime::spawn's
// vendored source, a discarded JoinHandle with no supervisor).

use sqlx::Row;

use crate::auth::registry::{GroupKeyRegistry, KeyRegistry};
use crate::auth::user_store;

/// Pure boundary check, no DB access -- unit-testable in isolation. Fails
/// closed (treats an unparsable timestamp as expired) since this guards a
/// resident master key: the safer default when data is unexpectedly
/// malformed. Both timestamps are always produced by this same codebase's
/// providers::utils::now() (chrono::Utc::now().to_rfc3339()), so the parse
/// failure path is a hardening backstop, not an expected case.
fn is_idle_expired(idle_timeout_minutes: i64, last_active_at: &str, now: &str) -> bool {
    let (Ok(last), Ok(now)) = (
        chrono::DateTime::parse_from_rfc3339(last_active_at),
        chrono::DateTime::parse_from_rfc3339(now),
    ) else {
        log::error!(
            "idle_timeout: unparsable timestamp (last_active_at={last_active_at:?}, \
             now={now:?}) -- failing closed, treating as expired"
        );
        return true;
    };
    now.signed_duration_since(last).num_seconds() >= idle_timeout_minutes * 60
}

/// Check the currently-resident account's idle time and clear KeyRegistry
/// if it has exceeded that account's users.idle_timeout_minutes. A no-op if
/// nobody is logged in, if the account has no live session row, or on any
/// DB error -- fail-silent (log::warn!, return), matching every sibling
/// periodic-sweep function's posture (persona_sync::engine::
/// run_periodic_sweep and persona_view_sync's own). Must never panic: this
/// drives its own always-on timer in main.rs.
pub async fn run_periodic_check(
    pool: &sqlx::SqlitePool,
    key_registry: &KeyRegistry,
    group_key_registry: &GroupKeyRegistry,
) {
    let Some(user_id) = key_registry.with_key(|k| k.user_id.clone()).await else {
        return;
    };

    let user = match user_store::find_user_by_id(pool, &user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            log::warn!("idle_timeout: resident user_id={user_id} has no matching users row");
            return;
        }
        Err(e) => {
            log::warn!("idle_timeout: couldn't look up user={user_id}: {e}");
            return;
        }
    };

    let mut conn = match pool.acquire().await {
        Ok(c) => c,
        Err(e) => {
            log::warn!("idle_timeout: couldn't acquire shared.db connection: {e}");
            return;
        }
    };

    let now = crate::providers::utils::now();

    let last_active_at: Option<String> = match sqlx::query(
        "SELECT MAX(last_active_at) AS last_active_at
         FROM auth_sessions WHERE user_id = ? AND expires_at > ?",
    )
    .bind(&user_id)
    .bind(&now)
    .fetch_one(&mut *conn)
    .await
    {
        Ok(row) => row.get("last_active_at"),
        Err(e) => {
            log::warn!("idle_timeout: couldn't read auth_sessions for user={user_id}: {e}");
            return;
        }
    };

    // No live session row for this account -- nothing to check (e.g. the
    // resident key predates any session row, which shouldn't happen in
    // practice, but there is nothing to compare against either way).
    let Some(last_active_at) = last_active_at else {
        return;
    };

    if !is_idle_expired(user.idle_timeout_minutes, &last_active_at, &now) {
        return;
    }

    key_registry.clear().await;
    // items.id=469: same gap as commands::auth::logout -- see
    // auth::clear_group_keys_for_user for why this is needed alongside
    // key_registry.clear() and not subsumed by it.
    crate::auth::clear_group_keys_for_user(pool, group_key_registry, &user_id).await;

    // Soft-expire, same pattern commands::auth::logout already uses --
    // update expires_at rather than DELETE, to preserve an audit trail of
    // when sessions existed and ended.
    if let Err(e) = sqlx::query(
        "UPDATE auth_sessions SET expires_at = ?
         WHERE user_id = ? AND expires_at > ?",
    )
    .bind(&now)
    .bind(&user_id)
    .bind(&now)
    .execute(&mut *conn)
    .await
    {
        log::warn!("idle_timeout: couldn't soft-expire sessions for user={user_id}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::Manager;

    use crate::auth::registry::GroupKeyRegistry;
    use crate::commands::auth::login;
    use crate::test_support::ENV_MUTEX;

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
        pool: sqlx::SqlitePool,
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

        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool must connect");

        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
            pool,
        }
    }

    fn mock_app_with_registry(pool: sqlx::SqlitePool) -> tauri::App<tauri::test::MockRuntime> {
        let app = tauri::test::mock_app();
        app.manage(KeyRegistry::default());
        app.manage(GroupKeyRegistry::default());
        app.manage(pool);
        app
    }

    // ------------------------------------------------------------------
    // Pure boundary tests -- no DB.
    // ------------------------------------------------------------------

    #[test]
    fn exactly_at_the_boundary_is_expired() {
        let last = "2026-08-21T12:00:00+00:00";
        let now = "2026-08-21T12:15:00+00:00"; // exactly 15 minutes later
        assert!(is_idle_expired(15, last, now));
    }

    #[test]
    fn one_second_before_the_boundary_is_not_expired() {
        let last = "2026-08-21T12:00:00+00:00";
        let now = "2026-08-21T12:14:59+00:00"; // 14m59s later
        assert!(!is_idle_expired(15, last, now));
    }

    #[test]
    fn zero_elapsed_is_not_expired() {
        let t = "2026-08-21T12:00:00+00:00";
        assert!(!is_idle_expired(15, t, t));
    }

    #[test]
    fn well_past_the_boundary_is_expired() {
        let last = "2026-08-21T12:00:00+00:00";
        let now = "2026-08-21T14:00:00+00:00"; // 2 hours later
        assert!(is_idle_expired(15, last, now));
    }

    #[test]
    fn an_unparsable_timestamp_fails_closed() {
        assert!(is_idle_expired(15, "not a timestamp", "also not one"));
    }

    // ------------------------------------------------------------------
    // Integration tests -- real user + session row via login().
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn run_periodic_check_clears_registry_and_soft_expires_session_when_idle_timeout_exceeded(
    ) {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "correct horse battery staple".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        assert!(registry.is_occupied().await);

        // Back-date last_active_at well past the default 15-minute
        // idle_timeout_minutes -- no dedicated "advance the clock" API
        // exists, so this simulates elapsed idle time the same way
        // persona_sync's own absence tests simulate a cleared field:
        // direct SQL against the row a real command call already produced.
        let mut conn = pool.acquire().await.unwrap();
        let stale = (chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
        sqlx::query("UPDATE auth_sessions SET last_active_at = ?")
            .bind(&stale)
            .execute(&mut *conn)
            .await
            .unwrap();

        run_periodic_check(&pool, &registry, &group_key_registry).await;

        assert!(
            !registry.is_occupied().await,
            "an idle session past the boundary must clear the resident key"
        );

        let now = crate::providers::utils::now();
        let still_live: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM auth_sessions WHERE expires_at > ?")
                .bind(&now)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(
            still_live.0, 0,
            "an idle-timeout fire must soft-expire the session too, same as logout()"
        );
    }

    #[tokio::test]
    async fn run_periodic_check_does_not_clear_registry_when_idle_timeout_not_yet_exceeded() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "correct horse battery staple".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();

        // One minute short of the default 15-minute threshold.
        let mut conn = pool.acquire().await.unwrap();
        let almost_stale = (chrono::Utc::now() - chrono::Duration::minutes(14)).to_rfc3339();
        sqlx::query("UPDATE auth_sessions SET last_active_at = ?")
            .bind(&almost_stale)
            .execute(&mut *conn)
            .await
            .unwrap();

        run_periodic_check(&pool, &registry, &group_key_registry).await;

        assert!(
            registry.is_occupied().await,
            "a session inside the idle window must not be cleared"
        );

        let now = crate::providers::utils::now();
        let still_live: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM auth_sessions WHERE expires_at > ?")
                .bind(&now)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(still_live.0, 1, "the session must remain live too");
    }

    /// items.id=483: shared.db access is pooled (max_connections=5) instead
    /// of one-connection-per-call. Before pooling, every tick of this
    /// function opened a brand-new connection via open_shared_db() that was
    /// never explicitly closed -- connection count grew without bound as
    /// main.rs's periodic timer kept firing. Simulates many ticks against a
    /// non-idle-expired session (so run_periodic_check runs its full acquire
    /// + query body every time, not the "nobody logged in" early return) and
    /// asserts the pool's total connection count never exceeds max_connections,
    /// proving connections are being returned to the pool and reused rather
    /// than accumulating.
    #[tokio::test]
    async fn run_periodic_check_does_not_grow_shared_db_connection_count_across_many_ticks() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "correct horse battery staple".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();

        // Comfortably inside the default 15-minute idle window, so every
        // tick below runs the full acquire-a-connection-and-query body
        // instead of short-circuiting on an already-expired session.
        let mut conn = pool.acquire().await.unwrap();
        let fresh = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        sqlx::query("UPDATE auth_sessions SET last_active_at = ?")
            .bind(&fresh)
            .execute(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        for _ in 0..25 {
            run_periodic_check(&pool, &registry, &group_key_registry).await;
        }

        assert!(
            registry.is_occupied().await,
            "session is still inside the idle window and must not have been cleared"
        );
        assert!(
            pool.size() <= 5,
            "shared.db pool must stay within max_connections=5 across repeated ticks, \
             got {} -- connections are leaking instead of being returned to the pool",
            pool.size()
        );
    }

    #[tokio::test]
    async fn run_periodic_check_is_a_noop_when_nobody_is_logged_in() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        // No login() call at all -- KeyRegistry starts empty.
        run_periodic_check(&pool, &registry, &group_key_registry).await;

        assert!(!registry.is_occupied().await);
    }

    // -- group-key eviction (items.id=469) ------------------------------

    #[tokio::test]
    async fn run_periodic_check_clears_group_key_registry_for_the_accounts_personas_when_idle_timeout_exceeded(
    ) {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "correct horse battery staple".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();

        let user = crate::auth::user_store::find_user_by_display_name(&pool, "Alice")
            .await
            .unwrap()
            .unwrap();
        let persona_id = uuid::Uuid::new_v4().to_string();
        crate::persistence::persona_store::create_persona(
            &pool,
            &persona_id,
            "Test Persona",
            "personal",
            &user.id,
            None,
        )
        .await
        .expect("create_persona must succeed");

        group_key_registry
            .replace(
                &persona_id,
                "group-1",
                crate::auth::registry::UnlockedGroupKey {
                    group_id: "group-1".to_owned(),
                    group_key: [0xAAu8; crate::auth::kdf::MASTER_KEY_LEN],
                    unlocked_at: crate::providers::utils::now(),
                },
            )
            .await;
        assert!(group_key_registry.is_occupied(&persona_id, "group-1").await);

        // Back-date last_active_at past the default 15-minute idle_timeout_minutes,
        // same technique the pre-existing KeyRegistry-only test above uses.
        let mut conn = pool.acquire().await.unwrap();
        let stale = (chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
        sqlx::query("UPDATE auth_sessions SET last_active_at = ?")
            .bind(&stale)
            .execute(&mut *conn)
            .await
            .unwrap();

        run_periodic_check(&pool, &registry, &group_key_registry).await;

        assert!(
            !registry.is_occupied().await,
            "sanity check: the master key must also be cleared"
        );
        assert!(
            !group_key_registry.is_occupied(&persona_id, "group-1").await,
            "an idle-timeout fire must evict this persona's group keys too, not just the \
             account master key"
        );
    }
}
