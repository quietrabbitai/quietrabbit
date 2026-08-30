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
// No key_hex / State<...Registry> needed: persona_view_share_settings lives
// in shared.db, unencrypted, keyed only by (recipient_user_id, share_id) --
// the folder path itself is not share content, same reasoning
// commands/persona_sync.rs's own header gives for its settings pair.

use serde::Serialize;
use specta::Type;

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
) -> Result<(), String> {
    settings_store::set_persona_view_share_sync_folder(&recipient_user_id, &share_id, &folder_path)
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
) -> Result<Option<PersonaViewShareSyncSettingsInfo>, String> {
    let settings =
        settings_store::get_persona_view_share_sync_settings(&recipient_user_id, &share_id)
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
    async fn get_persona_view_share_sync_folder_is_none_before_any_configuration() {
        let _env = setup().await;
        let result = get_persona_view_share_sync_folder("user-1".to_owned(), "share-1".to_owned())
            .await
            .expect("get_persona_view_share_sync_folder must succeed");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn set_then_get_round_trips_through_the_command_layer() {
        let _env = setup().await;
        set_persona_view_share_sync_folder(
            "user-1".to_owned(),
            "share-1".to_owned(),
            "/mnt/nas/family".to_owned(),
        )
        .await
        .expect("set_persona_view_share_sync_folder must succeed");

        let info = get_persona_view_share_sync_folder("user-1".to_owned(), "share-1".to_owned())
            .await
            .expect("get_persona_view_share_sync_folder must succeed")
            .expect("settings must exist after set_persona_view_share_sync_folder");
        assert_eq!(info.folder_path, "/mnt/nas/family");
        assert!(info.last_error.is_none());
    }

    #[tokio::test]
    async fn set_rejects_empty_path_with_a_string_error() {
        let _env = setup().await;
        let result = set_persona_view_share_sync_folder(
            "user-1".to_owned(),
            "share-1".to_owned(),
            "   ".to_owned(),
        )
        .await;
        assert!(result.is_err());
    }
}
