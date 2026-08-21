// src-tauri/src/commands/persona_sync.rs
//
// Group 19 -- Persona share folder sync (items.id=303, decisions.id=722).
// Commands: set_persona_share_sync_folder, get_persona_share_sync_folder.
//
// Backend-only -- no frontend UI exists yet to call these, same "ships
// ahead of frontend" precedent commands/group.rs's own header already
// documents for group.db's identically-shaped pair (items.id=287).
//
// No key_hex / State<...Registry> needed: persona_share_sync_settings lives
// in shared.db, unencrypted, keyed only by (persona_id, share_id) -- the
// folder path itself is not share content, same reasoning
// schema/shared_009.sql's own header gives.

use serde::Serialize;
use specta::Type;

use crate::persona_sync::settings_store::{self, SyncRole};

#[derive(Debug, Serialize, Type)]
pub struct PersonaShareSyncSettingsInfo {
    pub role: String,
    pub folder_path: String,
    pub last_synced_at: Option<String>,
    pub last_pushed_at: Option<String>,
    pub last_content_hash: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: String,
}

/// Configure (or reconfigure) this install's folder-sync destination for a
/// (persona_id, share_id) pair. `role` is "owner" or "recipient" -- see
/// settings_store::set_persona_share_sync_folder's own doc comment on why a
/// reconfigure may not change it. Upsert on folder_path.
#[tauri::command]
#[specta::specta]
pub async fn set_persona_share_sync_folder(
    persona_id: String,
    share_id: String,
    role: String,
    folder_path: String,
) -> Result<(), String> {
    let role = SyncRole::parse(&role).map_err(|e| e.to_string())?;
    settings_store::set_persona_share_sync_folder(&persona_id, &share_id, role, &folder_path)
        .await
        .map_err(|e| e.to_string())
}

/// Fetch this install's folder-sync settings for a (persona_id, share_id)
/// pair. Returns Ok(None) if sync has never been configured for this pair
/// -- not an error, matching get_group_sync_folder's "None is a valid
/// state" shape.
#[tauri::command]
#[specta::specta]
pub async fn get_persona_share_sync_folder(
    persona_id: String,
    share_id: String,
) -> Result<Option<PersonaShareSyncSettingsInfo>, String> {
    let settings = settings_store::get_persona_share_sync_settings(&persona_id, &share_id)
        .await
        .map_err(|e| e.to_string())?;

    Ok(settings.map(|s| PersonaShareSyncSettingsInfo {
        role: s.role.as_str().to_owned(),
        folder_path: s.folder_path,
        last_synced_at: s.last_synced_at,
        last_pushed_at: s.last_pushed_at,
        last_content_hash: s.last_content_hash,
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
    async fn get_persona_share_sync_folder_is_none_before_any_configuration() {
        let _env = setup().await;
        let result =
            get_persona_share_sync_folder("persona-1".to_owned(), "share-1".to_owned())
                .await
                .expect("get_persona_share_sync_folder must succeed");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn set_then_get_round_trips_through_the_command_layer() {
        let _env = setup().await;
        set_persona_share_sync_folder(
            "persona-1".to_owned(),
            "share-1".to_owned(),
            "owner".to_owned(),
            "/mnt/nas/family".to_owned(),
        )
        .await
        .expect("set_persona_share_sync_folder must succeed");

        let info = get_persona_share_sync_folder("persona-1".to_owned(), "share-1".to_owned())
            .await
            .expect("get_persona_share_sync_folder must succeed")
            .expect("settings must exist after set_persona_share_sync_folder");
        assert_eq!(info.role, "owner");
        assert_eq!(info.folder_path, "/mnt/nas/family");
        assert!(info.last_synced_at.is_none());
    }

    #[tokio::test]
    async fn set_rejects_an_unknown_role_with_a_string_error() {
        let _env = setup().await;
        let result = set_persona_share_sync_folder(
            "persona-1".to_owned(),
            "share-1".to_owned(),
            "bystander".to_owned(),
            "/mnt/nas/family".to_owned(),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn set_rejects_empty_path_with_a_string_error() {
        let _env = setup().await;
        let result = set_persona_share_sync_folder(
            "persona-1".to_owned(),
            "share-1".to_owned(),
            "recipient".to_owned(),
            "   ".to_owned(),
        )
        .await;
        assert!(result.is_err());
    }
}
