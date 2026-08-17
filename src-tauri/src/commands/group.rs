// src-tauri/src/commands/group.rs
//
// Group 15 — Group folder sync (items.id=287, group.db 266e).
// Commands: set_group_sync_folder, get_group_sync_folder.
//
// Backend-only -- no frontend UI exists yet to call these (there is no
// group.db IPC layer anywhere yet at all; see group_store.rs's and
// document_fork_store.rs's own #[allow(dead_code)] history). Matches the
// codebase's established pattern of shipping commands ahead of frontend.
//
// No key_hex / State<...Registry> needed: group_sync_settings lives in
// shared.db, unencrypted, keyed only by (persona_id, group_id) -- the
// folder path itself is not group content and reading/setting it isn't
// gated on the group key being unlocked (see schema/shared_005.sql's own
// header for the full placement reasoning).

use serde::Serialize;
use specta::Type;

use crate::group_sync::settings_store;

#[derive(Debug, Serialize, Type)]
pub struct GroupSyncSettingsInfo {
    pub folder_path: String,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: String,
}

/// Configure (or reconfigure) this install's folder-sync destination for a
/// (persona_id, group_id) pair. Upsert -- see
/// group_sync::settings_store::set_group_sync_folder's own doc comment.
#[tauri::command]
#[specta::specta]
pub async fn set_group_sync_folder(
    persona_id: String,
    group_id: String,
    folder_path: String,
) -> Result<(), String> {
    settings_store::set_group_sync_folder(&persona_id, &group_id, &folder_path)
        .await
        .map_err(|e| e.to_string())
}

/// Fetch this install's folder-sync settings for a (persona_id, group_id)
/// pair. Returns Ok(None) if sync has never been configured for this pair
/// -- not an error, matching get_focus_settings-style "None is a valid
/// state" shape for a settings row that may genuinely not exist yet.
#[tauri::command]
#[specta::specta]
pub async fn get_group_sync_folder(
    persona_id: String,
    group_id: String,
) -> Result<Option<GroupSyncSettingsInfo>, String> {
    let settings = settings_store::get_group_sync_settings(&persona_id, &group_id)
        .await
        .map_err(|e| e.to_string())?;

    Ok(settings.map(|s| GroupSyncSettingsInfo {
        folder_path: s.folder_path,
        last_synced_at: s.last_synced_at,
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
    async fn get_group_sync_folder_is_none_before_any_configuration() {
        let _env = setup().await;
        let result = get_group_sync_folder("persona-1".to_owned(), "group-1".to_owned())
            .await
            .expect("get_group_sync_folder must succeed");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn set_then_get_round_trips_through_the_command_layer() {
        let _env = setup().await;
        set_group_sync_folder(
            "persona-1".to_owned(),
            "group-1".to_owned(),
            "/mnt/nas/family".to_owned(),
        )
        .await
        .expect("set_group_sync_folder must succeed");

        let info = get_group_sync_folder("persona-1".to_owned(), "group-1".to_owned())
            .await
            .expect("get_group_sync_folder must succeed")
            .expect("settings must exist after set_group_sync_folder");
        assert_eq!(info.folder_path, "/mnt/nas/family");
        assert!(info.last_synced_at.is_none());
        assert!(info.last_error.is_none());
    }

    #[tokio::test]
    async fn set_group_sync_folder_rejects_empty_path_with_a_string_error() {
        let _env = setup().await;
        let result =
            set_group_sync_folder("persona-1".to_owned(), "group-1".to_owned(), "   ".to_owned())
                .await;
        assert!(result.is_err());
    }
}
