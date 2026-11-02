// src-tauri/src/group_sync/settings_store.rs
//
// group_sync_settings CRUD -- items.id=287. Where this install's folder-sync
// destination for a (persona_id, group_id) pair is, and the outcome of the
// most recent push/pull attempt.
//
// The table lives in the user's encrypted integration_keys.db (keys_006.sql),
// not in shared.db (items.id=661, decisions.id=862): the folder path is a
// user-chosen location and last_error can embed one, and shared.db is
// readable without the password. Every function therefore takes the owning
// user's id and data key (GroupSyncCtx for the engine) and opens that file.
// The group engine's own `key_hex` is the GROUP's symmetric key and is never
// used here.
//
// PK is (persona_id, group_id) -- full PK required for all reads, same
// shape as persistence::focus_settings_store's (persona_id, focus_id).
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros.
//
// CONNECTION MODEL: one encrypted connection per call
// (integration_keys_store::open_integration_keys_db).

use crate::persistence::integration_keys_store::{
    open_integration_keys_db, IntegrationKeysStoreError,
};
use sqlx::Row;
use thiserror::Error;

/// Whose encrypted settings a group-sync call reads and writes: the unlocked
/// account's id and its DATA key as hex. Passed down the group engine in place
/// of the shared.db pool it used to take, because the engine's functions have
/// the group key but not the account's key (items.id=661).
#[derive(Debug, Clone, Copy)]
pub struct GroupSyncCtx<'a> {
    pub user_id: &'a str,
    pub data_key_hex: &'a str,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum GroupSyncSettingsError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Validation error: {0}")]
    Validation(String),
    /// The encrypted file could not be opened (no key, epoch changed).
    #[error("{0}")]
    Keys(#[from] IntegrationKeysStoreError),
}

// ---------------------------------------------------------------------------
// Data type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSyncSettings {
    pub persona_id: String,
    pub group_id: String,
    pub folder_path: String,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: String,
}

fn row_to_settings(r: &sqlx::sqlite::SqliteRow) -> Result<GroupSyncSettings, sqlx::Error> {
    Ok(GroupSyncSettings {
        persona_id: r.try_get("persona_id")?,
        group_id: r.try_get("group_id")?,
        folder_path: r.try_get("folder_path")?,
        last_synced_at: r.try_get("last_synced_at")?,
        last_error: r.try_get("last_error")?,
        updated_at: r.try_get("updated_at")?,
    })
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// Fetch this install's folder-sync settings for (persona_id, group_id).
/// Returns None if sync has never been configured for this pair -- callers
/// (engine.rs's push/pull) treat that as "sync not set up yet", a silent
/// no-op, not an error.
pub async fn get_group_sync_settings(
    ctx: &GroupSyncCtx<'_>,
    persona_id: &str,
    group_id: &str,
) -> Result<Option<GroupSyncSettings>, GroupSyncSettingsError> {
    let mut conn = open_integration_keys_db(ctx.user_id, ctx.data_key_hex).await?;

    let row = sqlx::query(
        "SELECT persona_id, group_id, folder_path, last_synced_at, last_error, updated_at
         FROM group_sync_settings WHERE persona_id = ? AND group_id = ?",
    )
    .bind(persona_id)
    .bind(group_id)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(
            row_to_settings(&r).map_err(GroupSyncSettingsError::Database)?,
        )),
    }
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Set (or replace) the folder-sync destination for (persona_id, group_id).
/// Upsert -- reconfiguring an already-set group just points it somewhere
/// else, not an error. Does not touch last_synced_at/last_error: pointing
/// at a new folder doesn't retroactively change the outcome of the last
/// attempt against the old one.
pub async fn set_group_sync_folder(
    ctx: &GroupSyncCtx<'_>,
    persona_id: &str,
    group_id: &str,
    folder_path: &str,
) -> Result<(), GroupSyncSettingsError> {
    if folder_path.trim().is_empty() {
        return Err(GroupSyncSettingsError::Validation(
            "folder_path must not be empty".to_owned(),
        ));
    }

    let now = crate::providers::utils::now();
    let mut conn = open_integration_keys_db(ctx.user_id, ctx.data_key_hex).await?;

    sqlx::query(
        "INSERT INTO group_sync_settings (persona_id, group_id, folder_path, updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(persona_id, group_id)
         DO UPDATE SET folder_path = excluded.folder_path, updated_at = excluded.updated_at",
    )
    .bind(persona_id)
    .bind(group_id)
    .bind(folder_path)
    .bind(&now)
    .execute(&mut conn)
    .await?;

    Ok(())
}

/// Record the outcome of a push or pull attempt for (persona_id, group_id).
/// Ok(()) sets last_error back to NULL and refreshes last_synced_at (a
/// later success clears an earlier failure). Err(msg) leaves
/// last_synced_at untouched -- it must keep reflecting the last time sync
/// actually succeeded, not the last time it was attempted -- and sets
/// last_error to the given message. No-op (not an error) if no settings
/// row exists yet for this pair -- e.g. a push attempted before the folder
/// was ever configured has nothing to record against.
pub async fn record_sync_result(
    ctx: &GroupSyncCtx<'_>,
    persona_id: &str,
    group_id: &str,
    result: Result<(), &str>,
) -> Result<(), GroupSyncSettingsError> {
    let now = crate::providers::utils::now();
    let mut conn = open_integration_keys_db(ctx.user_id, ctx.data_key_hex).await?;

    match result {
        Ok(()) => {
            sqlx::query(
                "UPDATE group_sync_settings
                 SET last_synced_at = ?, last_error = NULL
                 WHERE persona_id = ? AND group_id = ?",
            )
            .bind(&now)
            .bind(persona_id)
            .bind(group_id)
            .execute(&mut conn)
            .await?;
        }
        Err(msg) => {
            sqlx::query(
                "UPDATE group_sync_settings
                 SET last_error = ?
                 WHERE persona_id = ? AND group_id = ?",
            )
            .bind(msg)
            .bind(persona_id)
            .bind(group_id)
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

    const CTX: GroupSyncCtx<'static> = GroupSyncCtx {
        user_id: "user-1",
        data_key_hex: "0101010101010101010101010101010101010101010101010101010101010101",
    };

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
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
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
        }
    }

    #[tokio::test]
    async fn get_group_sync_settings_returns_none_when_unconfigured() {
        let _env = setup().await;
        let result = get_group_sync_settings(&CTX, "persona-1", "group-1")
            .await
            .expect("get_group_sync_settings must succeed");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn set_then_get_round_trips() {
        let _env = setup().await;
        set_group_sync_folder(&CTX, "persona-1", "group-1", "/mnt/nas/family")
            .await
            .expect("set_group_sync_folder must succeed");

        let settings = get_group_sync_settings(&CTX, "persona-1", "group-1")
            .await
            .expect("get_group_sync_settings must succeed")
            .expect("settings must exist after set_group_sync_folder");
        assert_eq!(settings.folder_path, "/mnt/nas/family");
        assert!(settings.last_synced_at.is_none());
        assert!(settings.last_error.is_none());
    }

    #[tokio::test]
    async fn set_group_sync_folder_rejects_empty_path() {
        let _env = setup().await;
        let result = set_group_sync_folder(&CTX, "persona-1", "group-1", "   ").await;
        assert!(matches!(result, Err(GroupSyncSettingsError::Validation(_))));
    }

    #[tokio::test]
    async fn set_group_sync_folder_reconfigures_rather_than_erroring() {
        let _env = setup().await;
        set_group_sync_folder(&CTX, "persona-1", "group-1", "/mnt/nas/old")
            .await
            .unwrap();
        set_group_sync_folder(&CTX, "persona-1", "group-1", "/mnt/nas/new")
            .await
            .expect("reconfiguring an already-set group must not error");

        let settings = get_group_sync_settings(&CTX, "persona-1", "group-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(settings.folder_path, "/mnt/nas/new");
    }

    #[tokio::test]
    async fn record_sync_result_ok_sets_last_synced_at_and_clears_last_error() {
        let _env = setup().await;
        set_group_sync_folder(&CTX, "persona-1", "group-1", "/mnt/nas/family")
            .await
            .unwrap();
        record_sync_result(&CTX, "persona-1", "group-1", Err("folder unreachable"))
            .await
            .unwrap();
        record_sync_result(&CTX, "persona-1", "group-1", Ok(()))
            .await
            .expect("record_sync_result must succeed");

        let settings = get_group_sync_settings(&CTX, "persona-1", "group-1")
            .await
            .unwrap()
            .unwrap();
        assert!(settings.last_synced_at.is_some());
        assert!(
            settings.last_error.is_none(),
            "a later success must clear an earlier failure"
        );
    }

    #[tokio::test]
    async fn record_sync_result_err_sets_last_error_without_touching_last_synced_at() {
        let _env = setup().await;
        set_group_sync_folder(&CTX, "persona-1", "group-1", "/mnt/nas/family")
            .await
            .unwrap();
        record_sync_result(&CTX, "persona-1", "group-1", Ok(()))
            .await
            .unwrap();
        let after_success = get_group_sync_settings(&CTX, "persona-1", "group-1")
            .await
            .unwrap()
            .unwrap();
        let synced_at = after_success.last_synced_at.clone();

        record_sync_result(&CTX, "persona-1", "group-1", Err("folder unreachable"))
            .await
            .expect("record_sync_result must succeed");

        let after_failure = get_group_sync_settings(&CTX, "persona-1", "group-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after_failure.last_synced_at, synced_at,
            "last_synced_at must keep reflecting the last real success, not the last attempt"
        );
        assert_eq!(
            after_failure.last_error,
            Some("folder unreachable".to_owned())
        );
    }

    #[tokio::test]
    async fn record_sync_result_on_unconfigured_pair_is_a_noop_not_an_error() {
        let _env = setup().await;
        let result = record_sync_result(&CTX, "persona-1", "group-1", Ok(())).await;
        assert!(
            result.is_ok(),
            "recording against a never-configured pair must not error"
        );
        assert!(get_group_sync_settings(&CTX, "persona-1", "group-1")
            .await
            .unwrap()
            .is_none());
    }

    /// items.id=661: the folder path and an error that embeds it are stored
    /// in the encrypted file, so neither is readable from disk without the
    /// key, and shared.db has no such table at all.
    #[tokio::test]
    async fn the_path_and_the_error_are_not_readable_from_disk_without_the_key() {
        let _env = setup().await;
        set_group_sync_folder(&CTX, "persona-1", "group-1", "/mnt/SENTINEL-GROUP-PATH")
            .await
            .unwrap();
        record_sync_result(
            &CTX,
            "persona-1",
            "group-1",
            Err("cannot open /mnt/SENTINEL-GROUP-PATH"),
        )
        .await
        .unwrap();

        let got = get_group_sync_settings(&CTX, "persona-1", "group-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.folder_path, "/mnt/SENTINEL-GROUP-PATH");

        let db = crate::providers::utils::db_path_integration_keys(CTX.user_id);
        let mut raw = std::fs::read(&db).unwrap();
        for sidecar in ["-wal", "-shm"] {
            let mut name = db.clone().into_os_string();
            name.push(sidecar);
            if let Ok(more) = std::fs::read(std::path::PathBuf::from(name)) {
                raw.extend(more);
            }
        }
        assert!(
            !raw.windows(b"SENTINEL-GROUP-PATH".len())
                .any(|w| w == b"SENTINEL-GROUP-PATH"),
            "the folder path must not appear in the file's bytes"
        );
    }

    /// A different account's key cannot open this account's settings file.
    #[tokio::test]
    async fn another_data_key_cannot_read_the_settings() {
        let _env = setup().await;
        set_group_sync_folder(&CTX, "persona-1", "group-1", "/mnt/nas")
            .await
            .unwrap();
        let wrong = GroupSyncCtx {
            user_id: CTX.user_id,
            data_key_hex: "0202020202020202020202020202020202020202020202020202020202020202",
        };
        assert!(get_group_sync_settings(&wrong, "persona-1", "group-1")
            .await
            .is_err());
    }
}
