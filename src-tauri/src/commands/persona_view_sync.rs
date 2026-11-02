// src-tauri/src/commands/persona_view_sync.rs
//
// Group 20 -- VIEW-ONLY persona share folder sync (items.id=304,
// decisions.id=723). Commands: set_persona_view_share_sync_folder,
// get_persona_view_share_sync_folder.
//
// Backend-only -- no frontend UI exists yet to call these, same "ships ahead
// of frontend" precedent commands/persona_sync.rs's own header already
// documents for SYNCED's identically-shaped pair. Grant send/accept/revoke
// and any richer "browse the read-only cache" surface stay library
// primitives with no #[tauri::command] yet -- this item's job is the
// mechanism, not the UI, same scoping items.id=302 (materialization) used.
//
// Both commands need the unlocked session (items.id=661, decisions.id=862):
// persona_view_share_settings lives in the recipient's encrypted
// integration_keys.db, not shared.db, because the folder path is a user-chosen
// location that must not be readable without the password.
// recipient_user_id stays an argument but must be the unlocked account.

use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::persona_view_sync::settings_store;

#[derive(Debug, Serialize, Type)]
pub struct PersonaViewShareSyncSettingsInfo {
    pub folder_path: String,
    pub last_error: Option<String>,
    pub updated_at: String,
}

/// Configure (or reconfigure) this install's folder-sync source location for
/// a (recipient_user_id, share_id) pair. Upsert on folder_path.
#[tauri::command]
#[specta::specta]
pub async fn set_persona_view_share_sync_folder(
    recipient_user_id: String,
    share_id: String,
    folder_path: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<(), String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let key_hex_str =
        crate::commands::persona::session_for_user(&key_registry, &recipient_user_id).await?;
    settings_store::set_persona_view_share_sync_folder(
        &recipient_user_id,
        &key_hex_str,
        &share_id,
        &folder_path,
    )
    .await
    .map_err(|e| e.to_string())
}

/// Fetch this install's folder-sync settings for a (recipient_user_id,
/// share_id) pair. Returns Ok(None) if sync has never been configured for
/// this pair -- not an error, matching get_persona_share_sync_folder's
/// "None is a valid state" shape.
#[tauri::command]
#[specta::specta]
pub async fn get_persona_view_share_sync_folder(
    recipient_user_id: String,
    share_id: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<Option<PersonaViewShareSyncSettingsInfo>, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let key_hex_str =
        crate::commands::persona::session_for_user(&key_registry, &recipient_user_id).await?;
    let settings = settings_store::get_persona_view_share_sync_settings(
        &recipient_user_id,
        &key_hex_str,
        &share_id,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(settings.map(|s| PersonaViewShareSyncSettingsInfo {
        folder_path: s.folder_path,
        last_error: s.last_error,
        updated_at: s.updated_at,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{mock_app_with_registry, populate_registry, ENV_MUTEX};
    use tauri::Manager;

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
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
        let lock = ENV_MUTEX.lock().await;
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

    fn mock_app_with_pool(pool: sqlx::SqlitePool) -> tauri::App<tauri::test::MockRuntime> {
        mock_app_with_registry(pool)
    }

    #[tokio::test]
    async fn get_persona_view_share_sync_folder_is_none_before_any_configuration() {
        let _env = setup().await;
        let app = mock_app_with_pool(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(
            &registry,
            "user-1",
            [0x11u8; crate::auth::kdf::MASTER_KEY_LEN],
        )
        .await;
        let result =
            get_persona_view_share_sync_folder("user-1".to_owned(), "share-1".to_owned(), registry)
                .await
                .expect("get_persona_view_share_sync_folder must succeed");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn set_then_get_round_trips_through_the_command_layer() {
        let _env = setup().await;
        let app = mock_app_with_pool(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(
            &registry,
            "user-1",
            [0x11u8; crate::auth::kdf::MASTER_KEY_LEN],
        )
        .await;
        set_persona_view_share_sync_folder(
            "user-1".to_owned(),
            "share-1".to_owned(),
            "/mnt/nas/family".to_owned(),
            registry.clone(),
        )
        .await
        .expect("set_persona_view_share_sync_folder must succeed");

        let info =
            get_persona_view_share_sync_folder("user-1".to_owned(), "share-1".to_owned(), registry)
                .await
                .expect("get_persona_view_share_sync_folder must succeed")
                .expect("settings must exist after set_persona_view_share_sync_folder");
        assert_eq!(info.folder_path, "/mnt/nas/family");
        assert!(info.last_error.is_none());
    }

    #[tokio::test]
    async fn set_rejects_empty_path_with_a_string_error() {
        let _env = setup().await;
        let app = mock_app_with_pool(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(
            &registry,
            "user-1",
            [0x11u8; crate::auth::kdf::MASTER_KEY_LEN],
        )
        .await;
        let result = set_persona_view_share_sync_folder(
            "user-1".to_owned(),
            "share-1".to_owned(),
            "   ".to_owned(),
            registry,
        )
        .await;
        assert!(result.is_err());
    }

    /// items.id=661: nobody logged in, or a recipient_user_id that is not the
    /// unlocked account, is refused by both commands.
    #[tokio::test]
    async fn both_commands_refuse_without_a_session_or_for_another_user() {
        let _env = setup().await;
        let app = mock_app_with_pool(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();

        assert!(get_persona_view_share_sync_folder(
            "user-1".to_owned(),
            "share-1".to_owned(),
            registry.clone()
        )
        .await
        .is_err());

        populate_registry(
            &registry,
            "user-1",
            [0x11u8; crate::auth::kdf::MASTER_KEY_LEN],
        )
        .await;
        assert!(get_persona_view_share_sync_folder(
            "someone-else".to_owned(),
            "share-1".to_owned(),
            registry.clone()
        )
        .await
        .is_err());
        assert!(set_persona_view_share_sync_folder(
            "someone-else".to_owned(),
            "share-1".to_owned(),
            "/mnt/nas".to_owned(),
            registry
        )
        .await
        .is_err());
    }
}
