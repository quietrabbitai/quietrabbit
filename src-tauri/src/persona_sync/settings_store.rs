// src-tauri/src/persona_sync/settings_store.rs
//
// persona_share_sync_settings CRUD for shared.db (unencrypted) --
// items.id=303, decisions.id=722. Where this install's folder-sync
// destination for a (persona_id, share_id) pair is, which role this
// persona plays for that share (owner pushes / recipient pulls -- never
// both for the same share_id), and the outcome of the most recent
// push/pull attempt. See schema/shared_009.sql's own header for the full
// placement reasoning -- directly modeled on group_sync_settings
// (shared_005.sql) / group_sync::settings_store.rs, with the explicit role
// column that group's symmetric membership never needed.
//
// PK is (persona_id, share_id) -- full PK required for all reads, same
// shape group_sync_settings/focus_settings_store already establish.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros.
// shared.db is unencrypted -- no PRAGMA key required.
//
// CONNECTION MODEL: one connection per call, same as every other shared.db
// store in this codebase.

use std::path::PathBuf;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum PersonaShareSyncSettingsError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Validation error: {0}")]
    Validation(String),
}

// ---------------------------------------------------------------------------
// Data type
// ---------------------------------------------------------------------------

/// Which side of a directed SYNCED persona share this settings row
/// represents. Never both for the same share_id -- a given persona is
/// either the share's owner (pushes) or its recipient (pulls), fixed for
/// the share's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncRole {
    Owner,
    Recipient,
}

impl SyncRole {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncRole::Owner => "owner",
            SyncRole::Recipient => "recipient",
        }
    }

    pub fn parse(s: &str) -> Result<Self, PersonaShareSyncSettingsError> {
        match s {
            "owner" => Ok(SyncRole::Owner),
            "recipient" => Ok(SyncRole::Recipient),
            other => Err(PersonaShareSyncSettingsError::Validation(format!(
                "Unknown persona_share_sync_settings.role '{other}'. Must be owner or recipient."
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaShareSyncSettings {
    pub persona_id: String,
    pub share_id: String,
    pub role: SyncRole,
    pub folder_path: String,
    pub last_synced_at: Option<String>,
    pub last_pushed_at: Option<String>,
    pub last_content_hash: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: String,
}

fn row_to_settings(
    r: &sqlx::sqlite::SqliteRow,
) -> Result<PersonaShareSyncSettings, PersonaShareSyncSettingsError> {
    let role_raw: String = r.try_get("role")?;
    Ok(PersonaShareSyncSettings {
        persona_id: r.try_get("persona_id")?,
        share_id: r.try_get("share_id")?,
        role: SyncRole::parse(&role_raw)?,
        folder_path: r.try_get("folder_path")?,
        last_synced_at: r.try_get("last_synced_at")?,
        last_pushed_at: r.try_get("last_pushed_at")?,
        last_content_hash: r.try_get("last_content_hash")?,
        last_error: r.try_get("last_error")?,
        updated_at: r.try_get("updated_at")?,
    })
}

// ---------------------------------------------------------------------------
// DB opener
// ---------------------------------------------------------------------------
// Duplicated rather than reused -- same reasoning group_sync::settings_
// store.rs's own header gives: different error type per module, ~12-line
// zero-divergence-risk helper, not worth coupling.

fn get_shared_db_path() -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("instance")
        .join("shared.db")
}

async fn open_shared_db() -> Result<SqliteConnection, PersonaShareSyncSettingsError> {
    let db_path = get_shared_db_path();
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
// Read
// ---------------------------------------------------------------------------

/// Fetch this install's folder-sync settings for (persona_id, share_id).
/// Returns None if sync has never been configured for this pair -- callers
/// (engine.rs's push/pull) treat that as "sync not set up yet", a silent
/// no-op, not an error.
pub async fn get_persona_share_sync_settings(
    persona_id: &str,
    share_id: &str,
) -> Result<Option<PersonaShareSyncSettings>, PersonaShareSyncSettingsError> {
    let mut conn = open_shared_db().await?;

    let row = sqlx::query(
        "SELECT persona_id, share_id, role, folder_path, last_synced_at,
                last_pushed_at, last_content_hash, last_error, updated_at
         FROM persona_share_sync_settings WHERE persona_id = ? AND share_id = ?",
    )
    .bind(persona_id)
    .bind(share_id)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_settings(&r)?)),
    }
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Set (or replace) the folder-sync destination and role for
/// (persona_id, share_id). Upsert -- reconfiguring an already-set pair just
/// points it somewhere else, not an error. A reconfigure may not change
/// role -- a share's direction is fixed for its lifetime -- so `role` is
/// bound but excluded from the DO UPDATE clause on purpose: the first
/// set_persona_share_sync_folder call for a pair establishes its role, and
/// every later call for the same pair must therefore pass the same role or
/// this becomes a silent, confusing no-op on that column. Does not touch
/// last_synced_at/last_pushed_at/last_content_hash/last_error: pointing at
/// a new folder doesn't retroactively change the outcome of the last
/// attempt against the old one.
pub async fn set_persona_share_sync_folder(
    persona_id: &str,
    share_id: &str,
    role: SyncRole,
    folder_path: &str,
) -> Result<(), PersonaShareSyncSettingsError> {
    if folder_path.trim().is_empty() {
        return Err(PersonaShareSyncSettingsError::Validation(
            "folder_path must not be empty".to_owned(),
        ));
    }

    let now = crate::providers::utils::now();
    let mut conn = open_shared_db().await?;

    sqlx::query(
        "INSERT INTO persona_share_sync_settings
            (persona_id, share_id, role, folder_path, updated_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(persona_id, share_id)
         DO UPDATE SET folder_path = excluded.folder_path, updated_at = excluded.updated_at",
    )
    .bind(persona_id)
    .bind(share_id)
    .bind(role.as_str())
    .bind(folder_path)
    .bind(&now)
    .execute(&mut conn)
    .await?;

    Ok(())
}

/// Record the outcome of a pull attempt for (persona_id, share_id). Ok(())
/// sets last_error back to NULL and refreshes last_synced_at to the
/// pulled update's own emitted_at (not "now" -- so a later, out-of-order
/// pull attempt can still compare against the actual content timestamp
/// last applied, not merely the last time a sweep ran). Err(msg) leaves
/// last_synced_at untouched and sets last_error. No-op if no settings row
/// exists yet for this pair.
pub async fn record_pull_result(
    persona_id: &str,
    share_id: &str,
    result: Result<&str, &str>,
) -> Result<(), PersonaShareSyncSettingsError> {
    let mut conn = open_shared_db().await?;

    match result {
        Ok(emitted_at) => {
            sqlx::query(
                "UPDATE persona_share_sync_settings
                 SET last_synced_at = ?, last_error = NULL
                 WHERE persona_id = ? AND share_id = ?",
            )
            .bind(emitted_at)
            .bind(persona_id)
            .bind(share_id)
            .execute(&mut conn)
            .await?;
        }
        Err(msg) => {
            sqlx::query(
                "UPDATE persona_share_sync_settings
                 SET last_error = ?
                 WHERE persona_id = ? AND share_id = ?",
            )
            .bind(msg)
            .bind(persona_id)
            .bind(share_id)
            .execute(&mut conn)
            .await?;
        }
    }

    Ok(())
}

/// Record the outcome of a push attempt for (persona_id, share_id).
/// Ok(Some(hash)) means a write actually happened -- stamps last_pushed_at
/// and last_content_hash. Ok(None) means the sweep found nothing changed
/// (content hash unchanged) and skipped the write entirely -- clears
/// last_error (a prior failure is no longer relevant once content is
/// confirmed unchanged and reachable) but leaves last_pushed_at/
/// last_content_hash exactly as they were, since nothing was actually
/// (re)written. Err(msg) leaves all three untouched and sets last_error.
pub async fn record_push_result(
    persona_id: &str,
    share_id: &str,
    result: Result<Option<&str>, &str>,
) -> Result<(), PersonaShareSyncSettingsError> {
    let now = crate::providers::utils::now();
    let mut conn = open_shared_db().await?;

    match result {
        Ok(Some(content_hash)) => {
            sqlx::query(
                "UPDATE persona_share_sync_settings
                 SET last_pushed_at = ?, last_content_hash = ?, last_error = NULL
                 WHERE persona_id = ? AND share_id = ?",
            )
            .bind(&now)
            .bind(content_hash)
            .bind(persona_id)
            .bind(share_id)
            .execute(&mut conn)
            .await?;
        }
        Ok(None) => {
            sqlx::query(
                "UPDATE persona_share_sync_settings
                 SET last_error = NULL
                 WHERE persona_id = ? AND share_id = ?",
            )
            .bind(persona_id)
            .bind(share_id)
            .execute(&mut conn)
            .await?;
        }
        Err(msg) => {
            sqlx::query(
                "UPDATE persona_share_sync_settings
                 SET last_error = ?
                 WHERE persona_id = ? AND share_id = ?",
            )
            .bind(msg)
            .bind(persona_id)
            .bind(share_id)
            .execute(&mut conn)
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

    #[tokio::test]
    async fn get_settings_returns_none_when_unconfigured() {
        let _env = setup().await;
        let result = get_persona_share_sync_settings("persona-1", "share-1")
            .await
            .expect("get_persona_share_sync_settings must succeed");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn set_then_get_round_trips() {
        let _env = setup().await;
        set_persona_share_sync_folder("persona-1", "share-1", SyncRole::Owner, "/mnt/nas/family")
            .await
            .expect("set_persona_share_sync_folder must succeed");

        let settings = get_persona_share_sync_settings("persona-1", "share-1")
            .await
            .expect("get must succeed")
            .expect("settings must exist after set");
        assert_eq!(settings.folder_path, "/mnt/nas/family");
        assert_eq!(settings.role, SyncRole::Owner);
        assert!(settings.last_synced_at.is_none());
        assert!(settings.last_pushed_at.is_none());
        assert!(settings.last_content_hash.is_none());
        assert!(settings.last_error.is_none());
    }

    #[tokio::test]
    async fn set_rejects_empty_path() {
        let _env = setup().await;
        let result =
            set_persona_share_sync_folder("persona-1", "share-1", SyncRole::Owner, "   ").await;
        assert!(matches!(
            result,
            Err(PersonaShareSyncSettingsError::Validation(_))
        ));
    }

    #[tokio::test]
    async fn set_reconfigures_folder_path_rather_than_erroring() {
        let _env = setup().await;
        set_persona_share_sync_folder("persona-1", "share-1", SyncRole::Recipient, "/mnt/old")
            .await
            .unwrap();
        set_persona_share_sync_folder("persona-1", "share-1", SyncRole::Recipient, "/mnt/new")
            .await
            .expect("reconfiguring an already-set pair must not error");

        let settings = get_persona_share_sync_settings("persona-1", "share-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(settings.folder_path, "/mnt/new");
    }

    #[tokio::test]
    async fn record_pull_result_ok_sets_last_synced_at_to_the_given_timestamp() {
        let _env = setup().await;
        set_persona_share_sync_folder("persona-1", "share-1", SyncRole::Recipient, "/mnt/nas")
            .await
            .unwrap();
        record_pull_result("persona-1", "share-1", Err("unreachable"))
            .await
            .unwrap();
        record_pull_result("persona-1", "share-1", Ok("2026-08-20T00:00:00Z"))
            .await
            .expect("record_pull_result must succeed");

        let settings = get_persona_share_sync_settings("persona-1", "share-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            settings.last_synced_at,
            Some("2026-08-20T00:00:00Z".to_owned())
        );
        assert!(
            settings.last_error.is_none(),
            "a later success must clear an earlier failure"
        );
    }

    #[tokio::test]
    async fn record_push_result_some_stamps_pushed_at_and_hash() {
        let _env = setup().await;
        set_persona_share_sync_folder("persona-1", "share-1", SyncRole::Owner, "/mnt/nas")
            .await
            .unwrap();
        record_push_result("persona-1", "share-1", Ok(Some("abc123")))
            .await
            .expect("record_push_result must succeed");

        let settings = get_persona_share_sync_settings("persona-1", "share-1")
            .await
            .unwrap()
            .unwrap();
        assert!(settings.last_pushed_at.is_some());
        assert_eq!(settings.last_content_hash, Some("abc123".to_owned()));
    }

    #[tokio::test]
    async fn record_push_result_none_leaves_hash_and_pushed_at_untouched() {
        let _env = setup().await;
        set_persona_share_sync_folder("persona-1", "share-1", SyncRole::Owner, "/mnt/nas")
            .await
            .unwrap();
        record_push_result("persona-1", "share-1", Ok(Some("abc123")))
            .await
            .unwrap();
        let after_write = get_persona_share_sync_settings("persona-1", "share-1")
            .await
            .unwrap()
            .unwrap();

        record_push_result("persona-1", "share-1", Ok(None))
            .await
            .expect("a skipped push must still succeed");
        let after_skip = get_persona_share_sync_settings("persona-1", "share-1")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(after_skip.last_pushed_at, after_write.last_pushed_at);
        assert_eq!(after_skip.last_content_hash, after_write.last_content_hash);
    }

    #[tokio::test]
    async fn record_result_on_unconfigured_pair_is_a_noop_not_an_error() {
        let _env = setup().await;
        assert!(
            record_pull_result("persona-1", "share-1", Ok("2026-01-01T00:00:00Z"))
                .await
                .is_ok()
        );
        assert!(record_push_result("persona-1", "share-1", Ok(Some("x")))
            .await
            .is_ok());
        assert!(get_persona_share_sync_settings("persona-1", "share-1")
            .await
            .unwrap()
            .is_none());
    }
}
