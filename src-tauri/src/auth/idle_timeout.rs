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

use sqlx::{Row, SqliteConnection};

use crate::auth::registry::KeyRegistry;
use crate::auth::user_store;

// ---------------------------------------------------------------------------
// shared.db opener
// ---------------------------------------------------------------------------
// Duplicated from commands/auth.rs/user_store.rs rather than reused, same
// reasoning as those modules' own headers: coupling to a foreign error type
// isn't worth it for ~12 lines with no per-caller variation.

async fn open_shared_db() -> Result<SqliteConnection, String> {
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
pub async fn run_periodic_check(key_registry: &KeyRegistry) {
    let Some(user_id) = key_registry.with_key(|k| k.user_id.clone()).await else {
        return;
    };

    let user = match user_store::find_user_by_id(&user_id).await {
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

    let mut conn = match open_shared_db().await {
        Ok(c) => c,
        Err(e) => {
            log::warn!("idle_timeout: {e}");
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
    .fetch_one(&mut conn)
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
    .execute(&mut conn)
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

    fn mock_app_with_registry() -> tauri::App<tauri::test::MockRuntime> {
        let app = tauri::test::mock_app();
        app.manage(KeyRegistry::default());
        app.manage(GroupKeyRegistry::default());
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
        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();

        login(
            "Alice".to_owned(),
            "correct horse battery staple".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
        )
        .await
        .unwrap();
        assert!(registry.is_occupied().await);

        // Back-date last_active_at well past the default 15-minute
        // idle_timeout_minutes -- no dedicated "advance the clock" API
        // exists, so this simulates elapsed idle time the same way
        // persona_sync's own absence tests simulate a cleared field:
        // direct SQL against the row a real command call already produced.
        let mut conn = open_shared_db().await.unwrap();
        let stale = (chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
        sqlx::query("UPDATE auth_sessions SET last_active_at = ?")
            .bind(&stale)
            .execute(&mut conn)
            .await
            .unwrap();

        run_periodic_check(&registry).await;

        assert!(
            !registry.is_occupied().await,
            "an idle session past the boundary must clear the resident key"
        );

        let now = crate::providers::utils::now();
        let still_live: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM auth_sessions WHERE expires_at > ?")
                .bind(&now)
                .fetch_one(&mut conn)
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
        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();

        login(
            "Alice".to_owned(),
            "correct horse battery staple".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
        )
        .await
        .unwrap();

        // One minute short of the default 15-minute threshold.
        let mut conn = open_shared_db().await.unwrap();
        let almost_stale = (chrono::Utc::now() - chrono::Duration::minutes(14)).to_rfc3339();
        sqlx::query("UPDATE auth_sessions SET last_active_at = ?")
            .bind(&almost_stale)
            .execute(&mut conn)
            .await
            .unwrap();

        run_periodic_check(&registry).await;

        assert!(
            registry.is_occupied().await,
            "a session inside the idle window must not be cleared"
        );

        let now = crate::providers::utils::now();
        let still_live: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM auth_sessions WHERE expires_at > ?")
                .bind(&now)
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(still_live.0, 1, "the session must remain live too");
    }

    #[tokio::test]
    async fn run_periodic_check_is_a_noop_when_nobody_is_logged_in() {
        let _env = setup().await;
        let app = mock_app_with_registry();
        let registry = app.state::<KeyRegistry>();

        // No login() call at all -- KeyRegistry starts empty.
        run_periodic_check(&registry).await;

        assert!(!registry.is_occupied().await);
    }
}
