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
//
// Group 16 — Group membership (items.id=288, group.db 266f).
// Commands: remove_group_member.
//
// Ships ahead of frontend too, same precedent as Group 15 above -- but for
// a sharper reason than "that's what we did last time": document CRUD
// (283-286) stayed library-only because it had an internal hook (a future
// save event) to eventually attach to; folder-path config (287) broke that
// because it's inherently user-initiated, no internal hook exists. Member
// removal is the same shape as 287's case, not 283-286's -- nothing else in
// this system would ever call remove_member on its own; someone must always
// explicitly trigger it. A library-only version here would be exactly as
// dead as pre-287 group_store CRUD was.
//
// Group 17 — Group creation (items.id=291).
// Commands: create_group.
//
// Same user-initiated shape as Group 16 -- someone must always explicitly
// create a group, so this ships as a real command ahead of frontend rather
// than staying library-only like send_invitation.

use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::auth::group_creation;
use crate::auth::group_membership::{self, DepartureReason};
use crate::auth::registry::{GroupKeyRegistry, KeyRegistry};
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

/// Remove a member from a group (or record their own departure): rotates
/// the group's symmetric key and queues redistribution to remaining
/// members via the same asymmetric-keypair envelope mechanism item 284's
/// invitation flow uses (items.id=288, group.db 266f). Does not itself
/// rekey any local group.db file -- see auth::group_membership's own module
/// header (TWO HALVES) for why that happens separately, per remaining
/// member, via the periodic poll loop in main.rs.
///
/// `reason` is "left" or "removed" -- any other value is rejected with a
/// string error before reaching group_membership::remove_member.
///
/// No group-admin/permission model gates who may call this -- there is no
/// group-admin concept anywhere in this schema yet; building one is a
/// separate scope decision, not made here (known limitation, not silently
/// assumed away).
#[tauri::command]
#[specta::specta]
pub async fn remove_group_member(
    group_id: String,
    departing_persona_id: String,
    reason: String,
    sender_label: String,
    group_key_registry: State<'_, GroupKeyRegistry>,
    key_registry: State<'_, KeyRegistry>,
) -> Result<(), String> {
    let reason: DepartureReason = reason.parse().map_err(|e: group_membership::GroupMembershipError| e.to_string())?;
    group_membership::remove_member(
        &group_id,
        &departing_persona_id,
        reason,
        &sender_label,
        &group_key_registry,
        &key_registry,
    )
    .await
    .map_err(|e| e.to_string())
}

/// Create a new group: generates its symmetric key and id, materializes the
/// creator's own local group.db, and establishes the creator's own
/// membership state (items.id=291). See auth::group_creation's own module
/// header for the full design, including how the creator's own membership
/// is made visible to group_membership::remaining_members.
///
/// The caller must currently be logged in as `creator_persona_id`'s owning
/// account -- returns an error otherwise, rather than silently failing
/// later when personal.db can't be opened.
#[tauri::command]
#[specta::specta]
pub async fn create_group(
    creator_persona_id: String,
    group_display_name: String,
    creator_label: String,
    group_key_registry: State<'_, GroupKeyRegistry>,
    key_registry: State<'_, KeyRegistry>,
) -> Result<String, String> {
    group_creation::create_group(
        &creator_persona_id,
        &group_display_name,
        &creator_label,
        &group_key_registry,
        &key_registry,
    )
    .await
    .map_err(|e| e.to_string())
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
