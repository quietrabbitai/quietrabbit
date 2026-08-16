// src-tauri/src/persistence/tier3_cookie_store.rs
//
// tier3_provider_cookies CRUD — per-user, SQLCipher-encrypted
// tier3_cookies.db. items.id=224 resolution (decisions.id=711): CEF's one
// working global RequestContext holds the live, working cookie jar; this
// store is the source of truth across app restarts. See
// schema/tier3_cookies_001.sql's own header for the full column rationale
// and the CEF Cookie field-mapping notes (same_site/priority as raw i32,
// creation/last_access/expires as CEF's opaque Basetime.val, round-tripped
// verbatim, never reinterpreted here).
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros, matching
// every other store in this module.
// PRAGMA key applied via providers::utils::connect_options_encrypted (P4 --
// see integration_keys_store.rs's own header for why a third hand-rolled
// SqliteConnectOptions would be a violation of that same rule).
//
// REPLACE-ALL-FOR-PROVIDER WRITE MODEL: upsert_cookies() replaces the
// entire (user_id, provider_id) row set with the caller's snapshot in one
// SAVEPOINT-wrapped delete-then-insert, rather than diffing individual
// cookies. This matches how the caller actually has the data: a full
// visit_url_cookies() read-back of CEF's jar for that provider's domain at
// pane-close (see commands/tier3_pane.rs), not a single changed cookie. A
// cookie deleted from the live jar since the last save (e.g. the provider
// itself expired/cleared it) must not resurrect on the next load -- a
// per-cookie upsert would leave stale rows behind; a full replace can't.

use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum Tier3CookieStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Migration error: {0}")]
    Migration(#[from] crate::persistence::migrations::MigrationError),
}

// ---------------------------------------------------------------------------
// Data type
// ---------------------------------------------------------------------------

/// One stored cookie row. Field shape mirrors cef::Cookie (vendored cef
/// crate v151.1.0+151.3.12) minus `size` (a wire-protocol field, not data)
/// -- see schema/tier3_cookies_001.sql's header for the full field-by-field
/// rationale. Conversions to/from cef::Cookie live in
/// commands/tier3_pane.rs, the only caller that touches the CEF type --
/// this module stays CEF-agnostic, matching entity_store.rs/
/// integration_keys_store.rs's own convention of not importing cef here.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub httponly: bool,
    pub same_site: i32,
    pub priority: i32,
    pub has_expires: bool,
    pub expires: Option<i64>,
    pub creation: i64,
    pub last_access: i64,
}

fn row_to_stored_cookie(r: &sqlx::sqlite::SqliteRow) -> Result<StoredCookie, sqlx::Error> {
    Ok(StoredCookie {
        name: r.try_get("name")?,
        value: r.try_get("value")?,
        domain: r.try_get("domain")?,
        path: r.try_get("path")?,
        secure: r.try_get::<i64, _>("secure")? != 0,
        httponly: r.try_get::<i64, _>("httponly")? != 0,
        same_site: r.try_get::<i64, _>("same_site")? as i32,
        priority: r.try_get::<i64, _>("priority")? as i32,
        has_expires: r.try_get::<i64, _>("has_expires")? != 0,
        expires: r.try_get("expires")?,
        creation: r.try_get("creation")?,
        last_access: r.try_get("last_access")?,
    })
}

// ---------------------------------------------------------------------------
// Path + DB opener
// ---------------------------------------------------------------------------

fn get_tier3_cookies_db_path(user_id: &str) -> std::path::PathBuf {
    crate::providers::utils::db_path_tier3_cookies(user_id)
}

/// Open tier3_cookies.db with SQLCipher key. Mirrors
/// integration_keys_store.rs::open_integration_keys_db's shape exactly.
async fn open_tier3_cookies_db(
    user_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, Tier3CookieStoreError> {
    let db_path = get_tier3_cookies_db_path(user_id);

    if !db_path.exists() {
        crate::persistence::migrations::migrate_tier3_cookies_db(user_id, key_hex).await?;
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

/// All stored cookies for (user_id, provider_id) -- the pane-open restore
/// path (commands/tier3_pane.rs::open_tier3_panes).
pub async fn list_cookies(
    user_id: &str,
    key_hex: &str,
    provider_id: &str,
) -> Result<Vec<StoredCookie>, Tier3CookieStoreError> {
    let mut conn = open_tier3_cookies_db(user_id, key_hex).await?;
    list_cookies_conn(&mut conn, provider_id).await
}

pub(crate) async fn list_cookies_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
) -> Result<Vec<StoredCookie>, Tier3CookieStoreError> {
    let rows = sqlx::query(
        "SELECT name, value, domain, path, secure, httponly, same_site,
                priority, has_expires, expires, creation, last_access
         FROM tier3_provider_cookies
         WHERE provider_id = ?",
    )
    .bind(provider_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut cookies = Vec::with_capacity(rows.len());
    for r in &rows {
        cookies.push(row_to_stored_cookie(r)?);
    }
    Ok(cookies)
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Replace the entire stored cookie set for (user_id, provider_id) with
/// `cookies` -- the pane-close persist path
/// (commands/tier3_pane.rs::close_tier3_pane). See module header on why
/// this is a full replace, not a per-cookie upsert. SAVEPOINT-wrapped
/// delete-then-insert, mirroring personal_store.rs's own
/// supersede-then-insert atomicity pattern (same rationale: a failure
/// partway through must not leave a half-replaced row set).
pub async fn upsert_cookies(
    user_id: &str,
    key_hex: &str,
    provider_id: &str,
    cookies: &[StoredCookie],
) -> Result<(), Tier3CookieStoreError> {
    let mut conn = open_tier3_cookies_db(user_id, key_hex).await?;
    upsert_cookies_conn(&mut conn, provider_id, cookies).await
}

pub(crate) async fn upsert_cookies_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
    cookies: &[StoredCookie],
) -> Result<(), Tier3CookieStoreError> {
    sqlx::query("SAVEPOINT upsert_tier3_cookies")
        .execute(&mut *conn)
        .await?;

    let step: Result<(), sqlx::Error> = async {
        sqlx::query("DELETE FROM tier3_provider_cookies WHERE provider_id = ?")
            .bind(provider_id)
            .execute(&mut *conn)
            .await?;

        let updated_at = crate::providers::utils::now();
        for cookie in cookies {
            let id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO tier3_provider_cookies
                 (id, provider_id, name, value, domain, path, secure,
                  httponly, same_site, priority, has_expires, expires,
                  creation, last_access, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(provider_id)
            .bind(&cookie.name)
            .bind(&cookie.value)
            .bind(&cookie.domain)
            .bind(&cookie.path)
            .bind(cookie.secure as i64)
            .bind(cookie.httponly as i64)
            .bind(cookie.same_site as i64)
            .bind(cookie.priority as i64)
            .bind(cookie.has_expires as i64)
            .bind(cookie.expires)
            .bind(cookie.creation)
            .bind(cookie.last_access)
            .bind(&updated_at)
            .execute(&mut *conn)
            .await?;
        }

        Ok(())
    }
    .await;

    match step {
        Ok(()) => {
            sqlx::query("RELEASE upsert_tier3_cookies")
                .execute(&mut *conn)
                .await?;
            Ok(())
        }
        Err(e) => {
            if let Err(rollback_err) = sqlx::query("ROLLBACK TO upsert_tier3_cookies")
                .execute(&mut *conn)
                .await
            {
                log::error!("Savepoint rollback failed in upsert_cookies: {rollback_err}");
            }
            let _ = sqlx::query("RELEASE upsert_tier3_cookies")
                .execute(&mut *conn)
                .await;
            Err(Tier3CookieStoreError::Database(e))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::migrations::parse_statements;
    use sqlx::sqlite::SqliteConnectOptions;

    const TIER3_COOKIES_SCHEMA_V1: &str = include_str!("../../schema/tier3_cookies_001.sql");

    async fn test_db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");

        for stmt in parse_statements(TIER3_COOKIES_SCHEMA_V1) {
            sqlx::query(&stmt)
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{stmt}"));
        }
        conn
    }

    fn sample_cookie(name: &str, domain: &str) -> StoredCookie {
        StoredCookie {
            name: name.to_owned(),
            value: "v".to_owned(),
            domain: domain.to_owned(),
            path: "/".to_owned(),
            secure: true,
            httponly: true,
            same_site: 1,
            priority: 1,
            has_expires: true,
            expires: Some(1_700_000_000),
            creation: 1_600_000_000,
            last_access: 1_600_000_000,
        }
    }

    #[tokio::test]
    async fn list_is_empty_for_unknown_provider() {
        let mut conn = test_db().await;
        let cookies = list_cookies_conn(&mut conn, "claude").await.unwrap();
        assert!(cookies.is_empty());
    }

    #[tokio::test]
    async fn upsert_then_list_round_trips() {
        let mut conn = test_db().await;
        let cookies = vec![
            sample_cookie("session", "claude.ai"),
            sample_cookie("csrf", "claude.ai"),
        ];
        upsert_cookies_conn(&mut conn, "claude", &cookies)
            .await
            .unwrap();

        let mut fetched = list_cookies_conn(&mut conn, "claude").await.unwrap();
        fetched.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(fetched.len(), 2);
        assert_eq!(fetched[0].name, "csrf");
        assert_eq!(fetched[1].name, "session");
        assert_eq!(fetched[1].value, "v");
        assert_eq!(fetched[1].expires, Some(1_700_000_000));
    }

    #[tokio::test]
    async fn upsert_replaces_not_accumulates() {
        let mut conn = test_db().await;
        upsert_cookies_conn(&mut conn, "claude", &[sample_cookie("old", "claude.ai")])
            .await
            .unwrap();
        upsert_cookies_conn(&mut conn, "claude", &[sample_cookie("new", "claude.ai")])
            .await
            .unwrap();

        let fetched = list_cookies_conn(&mut conn, "claude").await.unwrap();
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].name, "new");
    }

    #[tokio::test]
    async fn different_providers_do_not_collide() {
        let mut conn = test_db().await;
        upsert_cookies_conn(&mut conn, "claude", &[sample_cookie("session", "claude.ai")])
            .await
            .unwrap();
        upsert_cookies_conn(
            &mut conn,
            "chatgpt",
            &[sample_cookie("session", "chatgpt.com")],
        )
        .await
        .unwrap();

        let claude_cookies = list_cookies_conn(&mut conn, "claude").await.unwrap();
        let chatgpt_cookies = list_cookies_conn(&mut conn, "chatgpt").await.unwrap();
        assert_eq!(claude_cookies.len(), 1);
        assert_eq!(claude_cookies[0].domain, "claude.ai");
        assert_eq!(chatgpt_cookies.len(), 1);
        assert_eq!(chatgpt_cookies[0].domain, "chatgpt.com");
    }

    #[tokio::test]
    async fn upsert_empty_snapshot_clears_provider() {
        let mut conn = test_db().await;
        upsert_cookies_conn(&mut conn, "claude", &[sample_cookie("session", "claude.ai")])
            .await
            .unwrap();
        upsert_cookies_conn(&mut conn, "claude", &[]).await.unwrap();

        let fetched = list_cookies_conn(&mut conn, "claude").await.unwrap();
        assert!(fetched.is_empty());
    }
}
