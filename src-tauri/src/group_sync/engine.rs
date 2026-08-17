// src-tauri/src/group_sync/engine.rs
//
// group.db folder-sync push/pull core (items.id=287, group.db 266e --
// Working/GROUP_DB_DESIGN_20260802.md Section 2.4). Pull cadence and
// failure handling confirmed with Jason during planning:
//   - pull cadence: app-start (per-group, the moment a group's key becomes
//     resident -- see auth::group_invitations::accept_invitation) plus a
//     periodic timer while the app is open (main.rs's setup() spawn).
//   - failure handling: push/pull I/O failures are logged and recorded via
//     settings_store::record_sync_result, retried next cycle -- they never
//     turn a local group.db write into an Err. Local edits are never
//     blocked on the sync folder's reachability.
//
// SAVE HOOK: group_store.rs's create_document/update_document have no
// #[tauri::command] wrapper yet (nothing calls them outside tests except
// document_fork_store.rs's read-only get_document use) -- there is no
// existing "on save" event to attach a push to. create_and_push_document /
// update_and_push_document below ARE that hook: thin wrappers that call the
// corresponding group_store CRUD fn unmodified (reuse, not duplication of
// its permission-check logic) and push only after that write succeeds.
// Any future IPC command should call these instead of calling group_store
// directly for owned/canon writes, and gets sync for free.
//
// PUSH SCOPE: owner-only, per items.id=287's own dispatch text ("push-on-
// save for OWNED documents"). Section 2.3 also lets a write-tier grantee
// "Update canon" directly -- that edit succeeds locally (group_store
// already permits it) but is NOT pushed by this item; it stays local-only
// until the actual owner's own install next saves the document. Real gap,
// flagged rather than solved here -- matches the item's literal scope and
// Section 2.5's R1+ deferral of true multi-writer-per-document.
//
// PER-DOCUMENT SYNC FILE FORMAT: Section 2.4 requires the shared folder to
// never see plaintext -- same guarantee personal.db's SQLCipher encryption
// already provides. Each pushed document becomes its own small SQLCipher-
// encrypted SQLite file (one `synced_document` table, one row), written via
// the exact same providers::utils::connect_options_encrypted(path, key_hex)
// helper group.db/personal.db already use -- no new crypto primitive. Per-
// document (not whole-group.db-file) granularity is required: each
// member's local group.db holds copies of ALL documents in the group,
// including ones they don't own, so pushing the whole local file would
// overwrite the shared folder with stale/foreign documents and break the
// single-writer-per-document invariant Section 2.4 depends on.
//
// Path: <folder_path>/quietrabbit/groups/<group_id>/documents/<document_id>.qrsync
// "quietrabbit" namespace segment: the configured folder may be a general-
// purpose shared drive used for other things too -- avoid collisions.
//
// "NEWER" COMPARISON: documents has no version column (decisions.id=715) --
// updated_at (ISO-8601, RFC3339 via providers::utils::now()) is the only
// thing that changes on a canon edit, and sorts correctly lexicographically.

use thiserror::Error;

use crate::group_sync::settings_store::{self, GroupSyncSettingsError};
use crate::persistence::group_store::{self, DocumentRecord, GroupStoreError};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum GroupSyncError {
    #[error("group_store error: {0}")]
    Store(#[from] GroupStoreError),
    #[error("group_sync_settings error: {0}")]
    Settings(#[from] GroupSyncSettingsError),
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

fn sync_documents_dir(folder_path: &str, group_id: &str) -> std::path::PathBuf {
    std::path::Path::new(folder_path)
        .join("quietrabbit")
        .join("groups")
        .join(group_id)
        .join("documents")
}

fn sync_file_path(folder_path: &str, group_id: &str, document_id: &str) -> std::path::PathBuf {
    sync_documents_dir(folder_path, group_id).join(format!("{document_id}.qrsync"))
}

// ---------------------------------------------------------------------------
// Per-document sync file I/O
// ---------------------------------------------------------------------------

async fn write_sync_file(
    path: &std::path::Path,
    key_hex: &str,
    doc: &DocumentRecord,
) -> Result<(), GroupSyncError> {
    use sqlx::ConnectOptions;

    let mut conn = crate::providers::utils::connect_options_encrypted(path, key_hex)
        .connect()
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS synced_document (
            id                  TEXT PRIMARY KEY,
            title               TEXT NOT NULL,
            content             TEXT NOT NULL,
            owner_persona_id    TEXT NOT NULL,
            created_at          TEXT NOT NULL,
            updated_at          TEXT NOT NULL,
            extra_metadata      TEXT NOT NULL
        )",
    )
    .execute(&mut conn)
    .await?;

    sqlx::query(
        "INSERT INTO synced_document
            (id, title, content, owner_persona_id, created_at, updated_at, extra_metadata)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            title = excluded.title,
            content = excluded.content,
            owner_persona_id = excluded.owner_persona_id,
            updated_at = excluded.updated_at,
            extra_metadata = excluded.extra_metadata",
    )
    .bind(&doc.id)
    .bind(&doc.title)
    .bind(&doc.content)
    .bind(&doc.owner_persona_id)
    .bind(&doc.created_at)
    .bind(&doc.updated_at)
    .bind(&doc.extra_metadata)
    .execute(&mut conn)
    .await?;

    Ok(())
}

struct SyncedDocument {
    id: String,
    title: String,
    content: String,
    owner_persona_id: String,
    created_at: String,
    updated_at: String,
    extra_metadata: String,
}

async fn read_sync_file(
    path: &std::path::Path,
    key_hex: &str,
) -> Result<SyncedDocument, GroupSyncError> {
    use sqlx::ConnectOptions;
    use sqlx::Row;

    let mut conn = crate::providers::utils::connect_options_encrypted(path, key_hex)
        .create_if_missing(false)
        .connect()
        .await?;

    let row = sqlx::query(
        "SELECT id, title, content, owner_persona_id, created_at, updated_at, extra_metadata
         FROM synced_document LIMIT 1",
    )
    .fetch_one(&mut conn)
    .await?;

    Ok(SyncedDocument {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        content: row.try_get("content")?,
        owner_persona_id: row.try_get("owner_persona_id")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        extra_metadata: row.try_get("extra_metadata")?,
    })
}

// ---------------------------------------------------------------------------
// Push (save hook)
// ---------------------------------------------------------------------------

/// Create a document via group_store::create_document (unmodified -- same
/// permission behavior, same signature shape), then push it if `persona_id`
/// is the document's owner. Push failure is logged and recorded, never
/// turns this into an Err -- the local create already succeeded.
pub async fn create_and_push_document(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    owner_persona_id: &str,
    title: &str,
    content: &str,
) -> Result<String, GroupSyncError> {
    let document_id = group_store::create_document(
        persona_id,
        group_id,
        key_hex,
        owner_persona_id,
        title,
        content,
    )
    .await?;

    push_if_owner(persona_id, group_id, key_hex, &document_id).await;

    Ok(document_id)
}

/// Update a document via group_store::update_document (unmodified), then
/// push it if `persona_id` is the document's owner. Same failure-handling
/// contract as create_and_push_document.
pub async fn update_and_push_document(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    document_id: &str,
    content: &str,
) -> Result<(), GroupSyncError> {
    group_store::update_document(persona_id, group_id, key_hex, document_id, content).await?;

    push_if_owner(persona_id, group_id, key_hex, document_id).await;

    Ok(())
}

async fn push_if_owner(persona_id: &str, group_id: &str, key_hex: &str, document_id: &str) {
    let doc = match group_store::get_document(persona_id, group_id, key_hex, document_id).await {
        Ok(d) => d,
        Err(e) => {
            log::warn!(
                "group_sync: could not re-fetch document {document_id} after write \
                 (persona={persona_id} group={group_id}), skipping push: {e}"
            );
            return;
        }
    };

    // Owner-only push (see this module's own header, PUSH SCOPE).
    if doc.owner_persona_id != persona_id {
        return;
    }

    let result = push_document_inner(persona_id, group_id, key_hex, &doc).await;
    let record_result = result.as_ref().map(|_| ()).map_err(std::string::ToString::to_string);
    if let Err(e) = &record_result {
        log::warn!(
            "group_sync: push failed for persona={persona_id} group={group_id} \
             document={document_id}: {e}"
        );
    }
    if let Err(e) = settings_store::record_sync_result(
        persona_id,
        group_id,
        record_result.as_ref().map(|_| ()).map_err(|s| s.as_str()),
    )
    .await
    {
        log::warn!("group_sync: could not record push result: {e}");
    }
}

async fn push_document_inner(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    doc: &DocumentRecord,
) -> Result<(), GroupSyncError> {
    let Some(settings) = settings_store::get_group_sync_settings(persona_id, group_id).await?
    else {
        // Sync not configured for this group on this install yet -- silent
        // no-op, not an error (design doc: "configurable folder-location
        // setting", not a mandatory one).
        return Ok(());
    };

    let path = sync_file_path(&settings.folder_path, group_id, &doc.id);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    write_sync_file(&path, key_hex, doc).await
}

// ---------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PullSummary {
    pub applied: usize,
    pub skipped: usize,
}

/// Sweep the configured shared folder for documents newer than what's
/// already stored locally, applying any found. Never returns Err on a
/// folder-reachability problem -- logs and records via
/// settings_store::record_sync_result instead, matching the failure-
/// handling decision (retry next cycle, never block the caller). Returns
/// Ok(PullSummary::default()) if sync isn't configured for this group on
/// this install, or the sync folder/documents dir doesn't exist yet
/// (nobody has pushed anything).
pub async fn pull_if_newer(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
) -> Result<PullSummary, GroupSyncError> {
    let Some(settings) = settings_store::get_group_sync_settings(persona_id, group_id).await?
    else {
        return Ok(PullSummary::default());
    };

    let docs_dir = sync_documents_dir(&settings.folder_path, group_id);
    let mut summary = PullSummary::default();

    let mut entries = match tokio::fs::read_dir(&docs_dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Sync folder (or its documents subdir) doesn't exist yet --
            // nobody has pushed anything to this group. Not a failure.
            if let Err(e2) = settings_store::record_sync_result(persona_id, group_id, Ok(())).await
            {
                log::warn!("group_sync: could not record pull result: {e2}");
            }
            return Ok(summary);
        }
        Err(e) => {
            let msg = e.to_string();
            log::warn!(
                "group_sync: pull failed to read sync folder for \
                 persona={persona_id} group={group_id}: {e}"
            );
            if let Err(e2) =
                settings_store::record_sync_result(persona_id, group_id, Err(msg.as_str())).await
            {
                log::warn!("group_sync: could not record pull failure: {e2}");
            }
            return Ok(summary);
        }
    };

    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                log::warn!(
                    "group_sync: error walking sync folder entries for \
                     persona={persona_id} group={group_id}, stopping this sweep: {e}"
                );
                break;
            }
        };

        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("qrsync") {
            continue;
        }

        match apply_one_synced_file(persona_id, group_id, key_hex, &path).await {
            Ok(true) => summary.applied += 1,
            Ok(false) => summary.skipped += 1,
            Err(e) => {
                // A bad/corrupt individual file is logged and skipped, not
                // fatal to the sweep -- one damaged sync file must not
                // block pulling every other document.
                log::warn!(
                    "group_sync: skipping unreadable sync file {path:?} for \
                     persona={persona_id} group={group_id}: {e}"
                );
                summary.skipped += 1;
            }
        }
    }

    if let Err(e) = settings_store::record_sync_result(persona_id, group_id, Ok(())).await {
        log::warn!("group_sync: could not record pull result: {e}");
    }

    Ok(summary)
}

/// Returns Ok(true) if the remote document was applied, Ok(false) if it was
/// read successfully but skipped (own document, or not actually newer).
async fn apply_one_synced_file(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    path: &std::path::Path,
) -> Result<bool, GroupSyncError> {
    let remote = read_sync_file(path, key_hex).await?;

    // Never pull our own document back over ourselves -- we're the
    // authoritative source for anything we own.
    if remote.owner_persona_id == persona_id {
        return Ok(false);
    }

    // get_document_unchecked, not get_document: a document received via a
    // previous pull has no document_permissions row for this persona (see
    // that function's own doc comment) -- the checked read path would make
    // this persona unable to ever compare against, and therefore ever
    // re-pull an update to, a document they don't own.
    let local = group_store::get_document_unchecked(persona_id, group_id, key_hex, &remote.id)
        .await?;

    let should_apply = match &local {
        None => true,
        Some(existing) => remote.updated_at.as_str() > existing.updated_at.as_str(),
    };

    if !should_apply {
        return Ok(false);
    }

    group_store::apply_synced_document(
        persona_id,
        group_id,
        key_hex,
        &remote.id,
        &remote.title,
        &remote.content,
        &remote.owner_persona_id,
        &remote.created_at,
        &remote.updated_at,
        &remote.extra_metadata,
    )
    .await?;

    Ok(true)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";

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
        let lock = crate::test_support::ENV_MUTEX.lock().unwrap();
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
    async fn push_is_a_silent_noop_when_folder_is_unset() {
        let _env = setup().await;
        // No set_group_sync_folder call -- sync not configured.
        let doc_id = create_and_push_document(
            "alice",
            "group-1",
            KEY_HEX,
            "alice",
            "Doc",
            "v1",
        )
        .await
        .expect("create_and_push_document must succeed even with no sync folder configured");

        let doc = group_store::get_document("alice", "group-1", KEY_HEX, &doc_id)
            .await
            .expect("the local write must have succeeded regardless of sync config");
        assert_eq!(doc.content, "v1");
    }

    #[tokio::test]
    async fn push_then_pull_round_trips_a_new_document_between_two_installs() {
        let _env = setup().await;
        let shared_folder = tempfile::tempdir().expect("failed to create shared folder tempdir");
        let shared_path = shared_folder.path().to_str().unwrap();

        settings_store::set_group_sync_folder("alice", "group-1", shared_path)
            .await
            .unwrap();
        settings_store::set_group_sync_folder("bob", "group-1", shared_path)
            .await
            .unwrap();

        let doc_id = create_and_push_document(
            "alice",
            "group-1",
            KEY_HEX,
            "alice",
            "Family Budget",
            "original content",
        )
        .await
        .expect("create_and_push_document must succeed");

        // Bob has never seen this document before -- his local group.db has
        // no row for it at all.
        let before = group_store::get_document_unchecked("bob", "group-1", KEY_HEX, &doc_id)
            .await
            .unwrap();
        assert!(before.is_none());

        let summary = pull_if_newer("bob", "group-1", KEY_HEX)
            .await
            .expect("pull_if_newer must succeed");
        assert_eq!(summary, PullSummary { applied: 1, skipped: 0 });

        let after = group_store::get_document_unchecked("bob", "group-1", KEY_HEX, &doc_id)
            .await
            .unwrap()
            .expect("bob must now have a local copy of alice's pushed document");
        assert_eq!(after.content, "original content");
        assert_eq!(after.owner_persona_id, "alice");
    }

    #[tokio::test]
    async fn a_second_push_is_picked_up_by_the_next_pull_an_unchanged_doc_is_not_reapplied() {
        let _env = setup().await;
        let shared_folder = tempfile::tempdir().expect("failed to create shared folder tempdir");
        let shared_path = shared_folder.path().to_str().unwrap();
        settings_store::set_group_sync_folder("alice", "group-1", shared_path)
            .await
            .unwrap();
        settings_store::set_group_sync_folder("bob", "group-1", shared_path)
            .await
            .unwrap();

        let doc_id = create_and_push_document("alice", "group-1", KEY_HEX, "alice", "Doc", "v1")
            .await
            .unwrap();
        pull_if_newer("bob", "group-1", KEY_HEX).await.unwrap();

        // Pulling again with nothing new pushed must not re-apply.
        let repeat = pull_if_newer("bob", "group-1", KEY_HEX).await.unwrap();
        assert_eq!(repeat, PullSummary { applied: 0, skipped: 1 });

        // updated_at is a timestamp with second-ish granularity in some
        // environments -- force a strictly-later value so the "newer" check
        // is unambiguous rather than racing the clock.
        update_and_push_document("alice", "group-1", KEY_HEX, &doc_id, "v2")
            .await
            .unwrap();
        {
            let mut conn = group_store::open_group_db("alice", "group-1", KEY_HEX)
                .await
                .unwrap();
            sqlx::query("UPDATE documents SET updated_at = '2099-01-01T00:00:00Z' WHERE id = ?")
                .bind(&doc_id)
                .execute(&mut conn)
                .await
                .unwrap();
        }
        // Re-push the forced-later timestamp (update_and_push_document
        // above already pushed the pre-forced version; push again so the
        // shared-folder copy reflects the forced updated_at too).
        push_if_owner("alice", "group-1", KEY_HEX, &doc_id).await;

        let after_second_pull = pull_if_newer("bob", "group-1", KEY_HEX).await.unwrap();
        assert_eq!(after_second_pull, PullSummary { applied: 1, skipped: 0 });

        let doc = group_store::get_document_unchecked("bob", "group-1", KEY_HEX, &doc_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(doc.content, "v2");
    }

    #[tokio::test]
    async fn pull_never_applies_a_document_owned_by_the_pulling_persona() {
        let _env = setup().await;
        let shared_folder = tempfile::tempdir().expect("failed to create shared folder tempdir");
        let shared_path = shared_folder.path().to_str().unwrap();
        settings_store::set_group_sync_folder("alice", "group-1", shared_path)
            .await
            .unwrap();

        create_and_push_document("alice", "group-1", KEY_HEX, "alice", "Doc", "v1")
            .await
            .unwrap();

        // Alice pulling her own group must not error or attempt to apply
        // her own pushed document back over herself -- the file is seen
        // (hence skipped: 1, not 0), just never applied (applied: 0).
        let summary = pull_if_newer("alice", "group-1", KEY_HEX).await.unwrap();
        assert_eq!(summary, PullSummary { applied: 0, skipped: 1 });
    }

    #[tokio::test]
    async fn pull_preserves_a_local_checkout_across_an_applied_update() {
        let _env = setup().await;
        let shared_folder = tempfile::tempdir().expect("failed to create shared folder tempdir");
        let shared_path = shared_folder.path().to_str().unwrap();
        settings_store::set_group_sync_folder("alice", "group-1", shared_path)
            .await
            .unwrap();
        settings_store::set_group_sync_folder("bob", "group-1", shared_path)
            .await
            .unwrap();

        let doc_id = create_and_push_document("alice", "group-1", KEY_HEX, "alice", "Doc", "v1")
            .await
            .unwrap();
        pull_if_newer("bob", "group-1", KEY_HEX).await.unwrap();

        // Bob grants himself no permission (irrelevant to checkout, which
        // is independent of document_permissions) and checks the document
        // out locally -- group_store::checkout_document requires owner-or-
        // write access, which bob doesn't have on alice's document, so
        // exercise the checkout directly against bob's local row via the
        // *_conn helper is not accessible from here; instead force the
        // checkout columns directly to simulate an in-progress local
        // checkout state that pull must not disturb.
        {
            let mut conn = group_store::open_group_db("bob", "group-1", KEY_HEX)
                .await
                .unwrap();
            sqlx::query(
                "UPDATE documents SET checked_out_by_persona_id = ?, checked_out_at = ? \
                 WHERE id = ?",
            )
            .bind("bob")
            .bind("2026-08-01T00:00:00Z")
            .bind(&doc_id)
            .execute(&mut conn)
            .await
            .unwrap();
        }

        update_and_push_document("alice", "group-1", KEY_HEX, &doc_id, "v2").await.unwrap();
        let summary = pull_if_newer("bob", "group-1", KEY_HEX).await.unwrap();
        assert_eq!(summary, PullSummary { applied: 1, skipped: 0 });

        let doc = group_store::get_document_unchecked("bob", "group-1", KEY_HEX, &doc_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(doc.content, "v2", "the content update must still apply");
        assert_eq!(
            doc.checked_out_by_persona_id,
            Some("bob".to_owned()),
            "pull must not silently release bob's local checkout"
        );
    }

    #[tokio::test]
    async fn pull_against_a_missing_shared_folder_does_not_error_or_panic() {
        let _env = setup().await;
        settings_store::set_group_sync_folder("bob", "group-1", "/nonexistent/path/for/testing")
            .await
            .unwrap();

        let summary = pull_if_newer("bob", "group-1", KEY_HEX)
            .await
            .expect("pull_if_newer must return Ok even when the folder doesn't exist");
        assert_eq!(summary, PullSummary::default());

        let settings = settings_store::get_group_sync_settings("bob", "group-1")
            .await
            .unwrap()
            .unwrap();
        assert!(
            settings.last_error.is_none(),
            "a simply-not-yet-created folder (nobody has pushed anything) is not \
             a failure -- only a real I/O error other than NotFound should record one"
        );
    }

    #[tokio::test]
    async fn pull_is_a_noop_when_sync_is_not_configured() {
        let _env = setup().await;
        // No set_group_sync_folder call at all for this pair.
        let summary = pull_if_newer("nobody", "group-1", KEY_HEX)
            .await
            .expect("pull_if_newer must succeed with no configured folder");
        assert_eq!(summary, PullSummary::default());
    }
}
