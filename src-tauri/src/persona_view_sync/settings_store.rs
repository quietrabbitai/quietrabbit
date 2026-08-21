// src-tauri/src/persona_view_sync/settings_store.rs
//
// persona_view_share_settings CRUD for shared.db (unencrypted) -- items.id=304
// (decisions.id=723). This is the VIEW-ONLY *recipient's* own folder-sync
// bookkeeping only: where this install looks for the owner's push, and
// whether the last pull attempt errored. See schema/shared_010.sql's own
// header for why this is a new table rather than a reuse of
// persona_sync::settings_store's persona_share_sync_settings (that table's
// PK is (persona_id, share_id); a VIEW-ONLY recipient has no persona_id for
// this share at all) -- and why it carries no last_synced_at (that value's
// single source of truth is view_cache_meta.last_synced_at, in the encrypted
// cache db this same pull already opens every sweep).
//
// The VIEW-ONLY *owner* side reuses persona_sync::settings_store directly
// (role='owner') -- unchanged, no new table needed there.
//
// PK is (recipient_user_id, share_id) -- full PK required for all reads.
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

#[derive(Debug, Error)]
pub enum PersonaViewShareSyncSettingsError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Validation error: {0}")]
    Validation(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaViewShareSyncSettings {
    pub recipient_user_id: String,
    pub share_id: String,
    pub folder_path: String,
    pub last_error: Option<String>,
    pub updated_at: String,
}

fn row_to_settings(
    r: &sqlx::sqlite::SqliteRow,
) -> Result<PersonaViewShareSyncSettings, PersonaViewShareSyncSettingsError> {
    Ok(PersonaViewShareSyncSettings {
        recipient_user_id: r.try_get("recipient_user_id")?,
        share_id: r.try_get("share_id")?,
        folder_path: r.try_get("folder_path")?,
        last_error: r.try_get("last_error")?,
        updated_at: r.try_get("updated_at")?,
    })
}

// ---------------------------------------------------------------------------
// DB opener
// ---------------------------------------------------------------------------
// Duplicated rather than reused -- same reasoning every other module in this
// codebase gives for this exact ~12-line helper: different error type per
// module, zero divergence risk, not worth coupling.

fn get_shared_db_path() -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("instance")
        .join("shared.db")
}

async fn open_shared_db() -> Result<SqliteConnection, PersonaViewShareSyncSettingsError> {
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

/// Returns None if sync has never been configured for this pair -- callers
/// (engine.rs's pull sweep) treat that as "sync not set up yet", a silent
/// no-op, matching persona_sync::settings_store's own contract.
pub async fn get_persona_view_share_sync_settings(
    recipient_user_id: &str,
    share_id: &str,
) -> Result<Option<PersonaViewShareSyncSettings>, PersonaViewShareSyncSettingsError> {
    let mut conn = open_shared_db().await?;

    let row = sqlx::query(
        "SELECT recipient_user_id, share_id, folder_path, last_error, updated_at
         FROM persona_view_share_settings WHERE recipient_user_id = ? AND share_id = ?",
    )
    .bind(recipient_user_id)
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

/// Set (or replace) the folder-sync source location for (recipient_user_id,
/// share_id). Upsert -- same reconfigure-in-place semantics
/// persona_sync::settings_store::set_persona_share_sync_folder already
/// establishes. Does not touch last_error: pointing at a new folder doesn't
/// retroactively change the outcome of the last attempt against the old one.
pub async fn set_persona_view_share_sync_folder(
    recipient_user_id: &str,
    share_id: &str,
    folder_path: &str,
) -> Result<(), PersonaViewShareSyncSettingsError> {
    if folder_path.trim().is_empty() {
        return Err(PersonaViewShareSyncSettingsError::Validation(
            "folder_path must not be empty".to_owned(),
        ));
    }

    let now = crate::providers::utils::now();
    let mut conn = open_shared_db().await?;

    sqlx::query(
        "INSERT INTO persona_view_share_settings
            (recipient_user_id, share_id, folder_path, updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(recipient_user_id, share_id)
         DO UPDATE SET folder_path = excluded.folder_path, updated_at = excluded.updated_at",
    )
    .bind(recipient_user_id)
    .bind(share_id)
    .bind(folder_path)
    .bind(&now)
    .execute(&mut conn)
    .await?;

    Ok(())
}

/// Record the outcome of a pull attempt. Ok(()) clears last_error. Err(msg)
/// sets it. No-op if no settings row exists yet for this pair -- matching
/// persona_sync::settings_store's own "no row = nothing to record" contract.
pub async fn record_pull_result(
    recipient_user_id: &str,
    share_id: &str,
    result: Result<(), &str>,
) -> Result<(), PersonaViewShareSyncSettingsError> {
    let mut conn = open_shared_db().await?;

    match result {
        Ok(()) => {
            sqlx::query(
                "UPDATE persona_view_share_settings SET last_error = NULL
                 WHERE recipient_user_id = ? AND share_id = ?",
            )
            .bind(recipient_user_id)
            .bind(share_id)
            .execute(&mut conn)
            .await?;
        }
        Err(msg) => {
            sqlx::query(
                "UPDATE persona_view_share_settings SET last_error = ?
                 WHERE recipient_user_id = ? AND share_id = ?",
            )
            .bind(msg)
            .bind(recipient_user_id)
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
        let result = get_persona_view_share_sync_settings("user-1", "share-1")
            .await
            .expect("get must succeed");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn set_then_get_round_trips() {
        let _env = setup().await;
        set_persona_view_share_sync_folder("user-1", "share-1", "/mnt/nas/family")
            .await
            .expect("set must succeed");

        let settings = get_persona_view_share_sync_settings("user-1", "share-1")
            .await
            .expect("get must succeed")
            .expect("settings must exist after set");
        assert_eq!(settings.folder_path, "/mnt/nas/family");
        assert!(settings.last_error.is_none());
    }

    #[tokio::test]
    async fn set_rejects_empty_path() {
        let _env = setup().await;
        let result = set_persona_view_share_sync_folder("user-1", "share-1", "   ").await;
        assert!(matches!(
            result,
            Err(PersonaViewShareSyncSettingsError::Validation(_))
        ));
    }

    #[tokio::test]
    async fn set_reconfigures_folder_path_rather_than_erroring() {
        let _env = setup().await;
        set_persona_view_share_sync_folder("user-1", "share-1", "/mnt/old")
            .await
            .unwrap();
        set_persona_view_share_sync_folder("user-1", "share-1", "/mnt/new")
            .await
            .expect("reconfiguring an already-set pair must not error");

        let settings = get_persona_view_share_sync_settings("user-1", "share-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(settings.folder_path, "/mnt/new");
    }

    #[tokio::test]
    async fn record_pull_result_ok_clears_a_prior_error() {
        let _env = setup().await;
        set_persona_view_share_sync_folder("user-1", "share-1", "/mnt/nas")
            .await
            .unwrap();
        record_pull_result("user-1", "share-1", Err("unreachable"))
            .await
            .unwrap();
        record_pull_result("user-1", "share-1", Ok(()))
            .await
            .expect("record_pull_result must succeed");

        let settings = get_persona_view_share_sync_settings("user-1", "share-1")
            .await
            .unwrap()
            .unwrap();
        assert!(settings.last_error.is_none());
    }

    #[tokio::test]
    async fn record_result_on_unconfigured_pair_is_a_noop_not_an_error() {
        let _env = setup().await;
        assert!(record_pull_result("user-1", "share-1", Ok(())).await.is_ok());
        assert!(get_persona_view_share_sync_settings("user-1", "share-1")
            .await
            .unwrap()
            .is_none());
    }
}
