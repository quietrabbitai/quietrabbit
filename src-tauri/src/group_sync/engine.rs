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
use crate::persistence::group_store::{self, DocumentRecord, GroupStoreError, PermissionGrant};

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

// items.id=292 (decisions.id=720): document_permissions grants sync as
// their own artifact, one directory over from documents/ -- same
// `.qrsync` extension (it already generically means "a per-artifact
// SQLCipher-encrypted sync file"; the subdirectory is what differentiates
// artifact type, same convention sync_documents_dir already establishes),
// NOT folded into the document's own .qrsync file. Folding in was
// considered and rejected: grant_permission/revoke_permission don't go
// through create_and_push_document/update_and_push_document, so the
// content artifact's own push trigger doesn't naturally cover a
// permissions-only change (a grant with no content edit would never get
// pushed), and a combined artifact would need its own independent
// staleness signal since document_permissions has no updated_at the way
// documents does.

fn sync_permissions_dir(folder_path: &str, group_id: &str) -> std::path::PathBuf {
    std::path::Path::new(folder_path)
        .join("quietrabbit")
        .join("groups")
        .join(group_id)
        .join("permissions")
}

fn permission_sync_file_path(
    folder_path: &str,
    group_id: &str,
    document_id: &str,
) -> std::path::PathBuf {
    sync_permissions_dir(folder_path, group_id).join(format!("{document_id}.qrsync"))
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

/// Write the complete current grant manifest for one document. Whole-table
/// clear-and-refill, NOT a per-row upsert like write_sync_file's single-row
/// document upsert -- a shrinking grant list (a revoke) must also remove
/// the dropped grantee's row from the file itself, otherwise a revoke would
/// never be representable in the pushed artifact at all.
async fn write_permissions_sync_file(
    path: &std::path::Path,
    key_hex: &str,
    grants: &[PermissionGrant],
) -> Result<(), GroupSyncError> {
    use sqlx::ConnectOptions;

    let mut conn = crate::providers::utils::connect_options_encrypted(path, key_hex)
        .connect()
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS synced_permissions (
            document_id     TEXT NOT NULL,
            persona_id      TEXT NOT NULL,
            tier            TEXT NOT NULL,
            granted_at      TEXT NOT NULL,
            PRIMARY KEY (document_id, persona_id)
        )",
    )
    .execute(&mut conn)
    .await?;

    sqlx::query("DELETE FROM synced_permissions")
        .execute(&mut conn)
        .await?;

    // document_id isn't passed in explicitly -- callers only ever write one
    // document's manifest per file, keyed by this file's own path/filename
    // (permission_sync_file_path), mirrored back out on read via
    // path.file_stem() rather than trusted from row content (see
    // apply_one_synced_permissions_file's own comment on why: an
    // all-revoked/empty manifest has no rows to read a document_id from at
    // all, so the filename must stay the authoritative source).
    let document_id = path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();

    for g in grants {
        sqlx::query(
            "INSERT INTO synced_permissions (document_id, persona_id, tier, granted_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(document_id)
        .bind(&g.persona_id)
        .bind(&g.tier)
        .bind(&g.granted_at)
        .execute(&mut conn)
        .await?;
    }

    Ok(())
}

async fn read_permissions_sync_file(
    path: &std::path::Path,
    key_hex: &str,
) -> Result<Vec<PermissionGrant>, GroupSyncError> {
    use sqlx::ConnectOptions;
    use sqlx::Row;

    let mut conn = crate::providers::utils::connect_options_encrypted(path, key_hex)
        .create_if_missing(false)
        .connect()
        .await?;

    let rows = sqlx::query("SELECT persona_id, tier, granted_at FROM synced_permissions")
        .fetch_all(&mut conn)
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(PermissionGrant {
            persona_id: r.try_get("persona_id")?,
            tier: r.try_get("tier")?,
            granted_at: r.try_get("granted_at")?,
        });
    }
    Ok(out)
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

/// Re-push every document `persona_id` owns in `group_id`, using the
/// already-rotated new key (items.id=288, key rotation on member
/// departure). Called by auth::group_membership::apply_pending_rotations
/// immediately after a local rekey completes -- overwrites each owned
/// document's stale .qrsync file (still sitting in the shared folder,
/// encrypted under the old, now-evicted key) at its existing path, since
/// sync_file_path is keyed by document_id alone, not by key version.
///
/// Documents owned by OTHER personas, including a departed member's, are
/// not touched here -- push is owner-only (see this module's own header,
/// PUSH SCOPE); nobody but the original owner's own install can ever
/// re-push them. A departed member's previously-owned documents therefore
/// become permanently stale/orphaned in the shared folder after rotation --
/// a known, accepted limitation (items.id=288's own scoping), not solved by
/// this function.
pub async fn republish_owned_documents(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
) -> Result<(), GroupSyncError> {
    let owned_ids: Vec<String> = group_store::list_documents(persona_id, group_id, key_hex)
        .await?
        .into_iter()
        .filter(|d| d.owner_persona_id == persona_id)
        .map(|d| d.id)
        .collect();

    for document_id in &owned_ids {
        push_if_owner(persona_id, group_id, key_hex, document_id).await;
        // A stale permissions .qrsync file (still encrypted under the old,
        // now-evicted key) needs the same re-publish the content file just
        // got above -- same reasoning, items.id=292 extending items.id=288.
        push_permissions(persona_id, group_id, key_hex, document_id).await;
    }

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
    let record_result = result
        .as_ref()
        .map(|_| ())
        .map_err(std::string::ToString::to_string);
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

/// Grant (or promote/demote) `grantee_persona_id`'s tier via
/// group_store::grant_permission (unmodified -- reuse of its owner-only
/// check), then push the document's full resulting grant manifest. Unlike
/// push_if_owner, no separate owner re-check is needed before pushing:
/// grant_permission already requires `persona_id` to be the document's
/// owner to succeed at all, so reaching the push step already confirms it.
pub async fn grant_and_push_permission(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    document_id: &str,
    grantee_persona_id: &str,
    tier: &str,
) -> Result<(), GroupSyncError> {
    group_store::grant_permission(
        persona_id,
        group_id,
        key_hex,
        document_id,
        grantee_persona_id,
        tier,
    )
    .await?;

    push_permissions(persona_id, group_id, key_hex, document_id).await;

    Ok(())
}

/// Revoke `grantee_persona_id`'s permission via group_store::revoke_
/// permission (unmodified), then push the document's full resulting grant
/// manifest -- the revoke itself propagates to other installs as this
/// persona's absence from that manifest (see apply_synced_permissions'
/// own doc comment: full-replace, not a separate revoke artifact).
pub async fn revoke_and_push_permission(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    document_id: &str,
    grantee_persona_id: &str,
) -> Result<(), GroupSyncError> {
    group_store::revoke_permission(
        persona_id,
        group_id,
        key_hex,
        document_id,
        grantee_persona_id,
    )
    .await?;

    push_permissions(persona_id, group_id, key_hex, document_id).await;

    Ok(())
}

/// Push `document_id`'s full current grant manifest. Same silent-no-op-if-
/// unconfigured / log-and-record-via-settings_store::record_sync_result-on-
/// failure contract as push_if_owner/push_document_inner -- a sync failure
/// must not turn an otherwise-successful local grant/revoke into an Err.
async fn push_permissions(persona_id: &str, group_id: &str, key_hex: &str, document_id: &str) {
    let grants = match group_store::list_permissions_for_document(
        persona_id,
        group_id,
        key_hex,
        document_id,
    )
    .await
    {
        Ok(g) => g,
        Err(e) => {
            log::warn!(
                "group_sync: could not re-fetch permissions for document \
                     {document_id} after write (persona={persona_id} group={group_id}), \
                     skipping permissions push: {e}"
            );
            return;
        }
    };

    let result = push_permissions_inner(persona_id, group_id, key_hex, document_id, &grants).await;
    let record_result = result
        .as_ref()
        .map(|_| ())
        .map_err(std::string::ToString::to_string);
    if let Err(e) = &record_result {
        log::warn!(
            "group_sync: permissions push failed for persona={persona_id} group={group_id} \
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
        log::warn!("group_sync: could not record permissions push result: {e}");
    }
}

async fn push_permissions_inner(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    document_id: &str,
    grants: &[PermissionGrant],
) -> Result<(), GroupSyncError> {
    let Some(settings) = settings_store::get_group_sync_settings(persona_id, group_id).await?
    else {
        // Sync not configured for this group on this install yet -- silent
        // no-op, matches push_document_inner's own contract.
        return Ok(());
    };

    let path = permission_sync_file_path(&settings.folder_path, group_id, document_id);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    write_permissions_sync_file(&path, key_hex, grants).await
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
    let local =
        group_store::get_document_unchecked(persona_id, group_id, key_hex, &remote.id).await?;

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
        &group_store::SyncedDocumentFields {
            document_id: &remote.id,
            title: &remote.title,
            content: &remote.content,
            owner_persona_id: &remote.owner_persona_id,
            created_at: &remote.created_at,
            updated_at: &remote.updated_at,
            extra_metadata: &remote.extra_metadata,
        },
    )
    .await?;

    Ok(true)
}

/// Sweep the configured shared folder's permissions/ subdirectory for
/// grant manifests differing from what's already stored locally, applying
/// any found. Structurally identical to pull_if_newer -- same settings/
/// NotFound/per-file-error handling, same record_sync_result bookkeeping,
/// same PullSummary shape.
pub async fn pull_permissions_if_newer(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
) -> Result<PullSummary, GroupSyncError> {
    let Some(settings) = settings_store::get_group_sync_settings(persona_id, group_id).await?
    else {
        return Ok(PullSummary::default());
    };

    let perms_dir = sync_permissions_dir(&settings.folder_path, group_id);
    let mut summary = PullSummary::default();

    let mut entries = match tokio::fs::read_dir(&perms_dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Err(e2) = settings_store::record_sync_result(persona_id, group_id, Ok(())).await
            {
                log::warn!("group_sync: could not record permissions pull result: {e2}");
            }
            return Ok(summary);
        }
        Err(e) => {
            let msg = e.to_string();
            log::warn!(
                "group_sync: permissions pull failed to read sync folder for \
                 persona={persona_id} group={group_id}: {e}"
            );
            if let Err(e2) =
                settings_store::record_sync_result(persona_id, group_id, Err(msg.as_str())).await
            {
                log::warn!("group_sync: could not record permissions pull failure: {e2}");
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
                    "group_sync: error walking permissions sync folder entries for \
                     persona={persona_id} group={group_id}, stopping this sweep: {e}"
                );
                break;
            }
        };

        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("qrsync") {
            continue;
        }

        match apply_one_synced_permissions_file(persona_id, group_id, key_hex, &path).await {
            Ok(true) => summary.applied += 1,
            Ok(false) => summary.skipped += 1,
            Err(e) => {
                log::warn!(
                    "group_sync: skipping unreadable permissions sync file {path:?} for \
                     persona={persona_id} group={group_id}: {e}"
                );
                summary.skipped += 1;
            }
        }
    }

    if let Err(e) = settings_store::record_sync_result(persona_id, group_id, Ok(())).await {
        log::warn!("group_sync: could not record permissions pull result: {e}");
    }

    Ok(summary)
}

/// Returns Ok(true) if the pulled manifest was applied, Ok(false) if it was
/// read successfully but skipped (own document, document not synced
/// locally yet, or manifest unchanged from what's already here).
async fn apply_one_synced_permissions_file(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    path: &std::path::Path,
) -> Result<bool, GroupSyncError> {
    // document_id comes from the filename, not from manifest row content --
    // an all-revoked manifest has zero rows, so row content alone can't
    // always recover which document it belongs to (see write_permissions_
    // sync_file's own comment).
    let Some(document_id) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
        return Ok(false);
    };

    // A permissions manifest presupposes the document itself already
    // exists locally -- it can race ahead of the content pull that creates
    // it (separate artifact, separate sweep). Treat "not here yet" as a
    // normal, self-healing case: skip, the next sweep (after a content
    // pull has landed the document) will pick it up. This also doubles as
    // the source for the owner check below, avoiding a second query.
    let Some(local_doc) =
        group_store::get_document_unchecked(persona_id, group_id, key_hex, document_id).await?
    else {
        return Ok(false);
    };

    // Never apply our own document's permissions manifest back over
    // ourselves -- mirrors apply_one_synced_file's identical guard for
    // content.
    if local_doc.owner_persona_id == persona_id {
        return Ok(false);
    }

    let remote_grants = read_permissions_sync_file(path, key_hex).await?;
    let local_grants =
        group_store::list_permissions_for_document(persona_id, group_id, key_hex, document_id)
            .await?;

    if same_grant_set(&remote_grants, &local_grants) {
        return Ok(false);
    }

    group_store::apply_synced_permissions(
        persona_id,
        group_id,
        key_hex,
        document_id,
        &remote_grants,
    )
    .await?;

    Ok(true)
}

/// (persona_id, tier) set equality, order-independent -- document_id isn't
/// compared since both sides are already scoped to the same document_id by
/// the caller, and granted_at isn't compared since it's not access-control-
/// relevant (require_read_access_conn/require_write_access_conn only ever
/// check tier, never granted_at) -- an owner re-granting the same tier
/// bumps granted_at without changing what access anyone actually has, and
/// treating that as "changed" would cause a needless reapply every sweep
/// until the next real tier change.
fn same_grant_set(a: &[PermissionGrant], b: &[PermissionGrant]) -> bool {
    use std::collections::BTreeSet;

    let to_set = |grants: &[PermissionGrant]| -> BTreeSet<(String, String)> {
        grants
            .iter()
            .map(|g| (g.persona_id.clone(), g.tier.clone()))
            .collect()
    };

    to_set(a) == to_set(b)
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
        let doc_id = create_and_push_document("alice", "group-1", KEY_HEX, "alice", "Doc", "v1")
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
        assert_eq!(
            summary,
            PullSummary {
                applied: 1,
                skipped: 0
            }
        );

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
        assert_eq!(
            repeat,
            PullSummary {
                applied: 0,
                skipped: 1
            }
        );

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
        assert_eq!(
            after_second_pull,
            PullSummary {
                applied: 1,
                skipped: 0
            }
        );

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
        assert_eq!(
            summary,
            PullSummary {
                applied: 0,
                skipped: 1
            }
        );
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

        update_and_push_document("alice", "group-1", KEY_HEX, &doc_id, "v2")
            .await
            .unwrap();
        let summary = pull_if_newer("bob", "group-1", KEY_HEX).await.unwrap();
        assert_eq!(
            summary,
            PullSummary {
                applied: 1,
                skipped: 0
            }
        );

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

    // -- items.id=292: document_permissions grant sync ----------------------

    #[tokio::test]
    async fn permissions_push_is_a_silent_noop_when_folder_is_unset() {
        let _env = setup().await;
        let doc_id = create_and_push_document("alice", "group-1", KEY_HEX, "alice", "Doc", "v1")
            .await
            .unwrap();

        // No set_group_sync_folder call -- sync not configured.
        grant_and_push_permission("alice", "group-1", KEY_HEX, &doc_id, "bob", "write")
            .await
            .expect("grant_and_push_permission must succeed even with no sync folder configured");
    }

    #[tokio::test]
    async fn grant_then_pull_round_trips_a_new_grant_to_a_second_install() {
        let _env = setup().await;
        let shared_folder = tempfile::tempdir().expect("failed to create shared folder tempdir");
        let shared_path = shared_folder.path().to_str().unwrap();
        settings_store::set_group_sync_folder("alice", "group-1", shared_path)
            .await
            .unwrap();
        settings_store::set_group_sync_folder("carol", "group-1", shared_path)
            .await
            .unwrap();

        let doc_id = create_and_push_document("alice", "group-1", KEY_HEX, "alice", "Doc", "v1")
            .await
            .unwrap();
        // Carol must have the document locally before her own permissions
        // pull can attach a grant to it (documents/ and permissions/ are
        // independent sweeps).
        pull_if_newer("carol", "group-1", KEY_HEX).await.unwrap();

        grant_and_push_permission("alice", "group-1", KEY_HEX, &doc_id, "carol", "write")
            .await
            .unwrap();

        let summary = pull_permissions_if_newer("carol", "group-1", KEY_HEX)
            .await
            .expect("pull_permissions_if_newer must succeed");
        assert_eq!(
            summary,
            PullSummary {
                applied: 1,
                skipped: 0
            }
        );

        let carol_grants =
            group_store::list_permissions_for_document("carol", "group-1", KEY_HEX, &doc_id)
                .await
                .unwrap();
        assert_eq!(carol_grants.len(), 1);
        assert_eq!(carol_grants[0].persona_id, "carol");
        assert_eq!(carol_grants[0].tier, "write");
    }

    #[tokio::test]
    async fn revoke_then_pull_removes_the_grantees_local_row_on_another_install() {
        let _env = setup().await;
        let shared_folder = tempfile::tempdir().expect("failed to create shared folder tempdir");
        let shared_path = shared_folder.path().to_str().unwrap();
        settings_store::set_group_sync_folder("alice", "group-1", shared_path)
            .await
            .unwrap();
        settings_store::set_group_sync_folder("carol", "group-1", shared_path)
            .await
            .unwrap();

        let doc_id = create_and_push_document("alice", "group-1", KEY_HEX, "alice", "Doc", "v1")
            .await
            .unwrap();
        pull_if_newer("carol", "group-1", KEY_HEX).await.unwrap();

        grant_and_push_permission("alice", "group-1", KEY_HEX, &doc_id, "carol", "write")
            .await
            .unwrap();
        pull_permissions_if_newer("carol", "group-1", KEY_HEX)
            .await
            .unwrap();
        assert_eq!(
            group_store::list_permissions_for_document("carol", "group-1", KEY_HEX, &doc_id)
                .await
                .unwrap()
                .len(),
            1,
            "carol must have the grant locally before it can be revoked away"
        );

        revoke_and_push_permission("alice", "group-1", KEY_HEX, &doc_id, "carol")
            .await
            .unwrap();
        let summary = pull_permissions_if_newer("carol", "group-1", KEY_HEX)
            .await
            .expect("pull_permissions_if_newer must succeed");
        assert_eq!(
            summary,
            PullSummary {
                applied: 1,
                skipped: 0
            }
        );

        let carol_grants =
            group_store::list_permissions_for_document("carol", "group-1", KEY_HEX, &doc_id)
                .await
                .unwrap();
        assert!(
            carol_grants.is_empty(),
            "carol's local grant row must be gone after the revoke propagates"
        );
    }

    #[tokio::test]
    async fn an_unchanged_permissions_manifest_is_not_reapplied_on_a_repeat_pull() {
        let _env = setup().await;
        let shared_folder = tempfile::tempdir().expect("failed to create shared folder tempdir");
        let shared_path = shared_folder.path().to_str().unwrap();
        settings_store::set_group_sync_folder("alice", "group-1", shared_path)
            .await
            .unwrap();
        settings_store::set_group_sync_folder("carol", "group-1", shared_path)
            .await
            .unwrap();

        let doc_id = create_and_push_document("alice", "group-1", KEY_HEX, "alice", "Doc", "v1")
            .await
            .unwrap();
        pull_if_newer("carol", "group-1", KEY_HEX).await.unwrap();
        grant_and_push_permission("alice", "group-1", KEY_HEX, &doc_id, "carol", "write")
            .await
            .unwrap();

        let first = pull_permissions_if_newer("carol", "group-1", KEY_HEX)
            .await
            .unwrap();
        assert_eq!(
            first,
            PullSummary {
                applied: 1,
                skipped: 0
            }
        );

        let second = pull_permissions_if_newer("carol", "group-1", KEY_HEX)
            .await
            .unwrap();
        assert_eq!(
            second,
            PullSummary {
                applied: 0,
                skipped: 1
            },
            "nothing changed since the last pull -- must not reapply"
        );
    }

    #[tokio::test]
    async fn permissions_pull_never_applies_a_manifest_for_a_document_the_puller_owns() {
        let _env = setup().await;
        let shared_folder = tempfile::tempdir().expect("failed to create shared folder tempdir");
        let shared_path = shared_folder.path().to_str().unwrap();
        settings_store::set_group_sync_folder("alice", "group-1", shared_path)
            .await
            .unwrap();

        let doc_id = create_and_push_document("alice", "group-1", KEY_HEX, "alice", "Doc", "v1")
            .await
            .unwrap();
        grant_and_push_permission("alice", "group-1", KEY_HEX, &doc_id, "bob", "write")
            .await
            .unwrap();

        // Alice pulling her own group must see the file (skipped: 1) but
        // never apply her own document's manifest back over herself.
        let summary = pull_permissions_if_newer("alice", "group-1", KEY_HEX)
            .await
            .unwrap();
        assert_eq!(
            summary,
            PullSummary {
                applied: 0,
                skipped: 1
            }
        );
    }

    #[tokio::test]
    async fn a_permissions_manifest_ahead_of_its_document_is_skipped_then_applied_once_the_document_exists(
    ) {
        let _env = setup().await;
        let shared_folder = tempfile::tempdir().expect("failed to create shared folder tempdir");
        let shared_path = shared_folder.path().to_str().unwrap();
        settings_store::set_group_sync_folder("alice", "group-1", shared_path)
            .await
            .unwrap();
        settings_store::set_group_sync_folder("carol", "group-1", shared_path)
            .await
            .unwrap();

        let doc_id = create_and_push_document("alice", "group-1", KEY_HEX, "alice", "Doc", "v1")
            .await
            .unwrap();
        grant_and_push_permission("alice", "group-1", KEY_HEX, &doc_id, "carol", "write")
            .await
            .unwrap();

        // Carol pulls permissions before ever having pulled the document
        // itself -- the manifest must be skipped, not applied, and must not
        // error the sweep.
        let before = pull_permissions_if_newer("carol", "group-1", KEY_HEX)
            .await
            .expect("pull_permissions_if_newer must not error on an unsynced document");
        assert_eq!(
            before,
            PullSummary {
                applied: 0,
                skipped: 1
            }
        );
        assert!(
            group_store::list_permissions_for_document("carol", "group-1", KEY_HEX, &doc_id)
                .await
                .unwrap()
                .is_empty()
        );

        // Once the document itself has synced, a later sweep picks the
        // already-waiting manifest up.
        pull_if_newer("carol", "group-1", KEY_HEX).await.unwrap();
        let after = pull_permissions_if_newer("carol", "group-1", KEY_HEX)
            .await
            .unwrap();
        assert_eq!(
            after,
            PullSummary {
                applied: 1,
                skipped: 0
            }
        );
        assert_eq!(
            group_store::list_permissions_for_document("carol", "group-1", KEY_HEX, &doc_id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn same_grant_set_ignores_order_and_granted_at() {
        let a = vec![
            PermissionGrant {
                persona_id: "bob".to_owned(),
                tier: "write".to_owned(),
                granted_at: "2026-01-01T00:00:00Z".to_owned(),
            },
            PermissionGrant {
                persona_id: "carol".to_owned(),
                tier: "read_only".to_owned(),
                granted_at: "2026-01-02T00:00:00Z".to_owned(),
            },
        ];
        let b = vec![
            PermissionGrant {
                persona_id: "carol".to_owned(),
                tier: "read_only".to_owned(),
                granted_at: "2099-12-31T00:00:00Z".to_owned(),
            },
            PermissionGrant {
                persona_id: "bob".to_owned(),
                tier: "write".to_owned(),
                granted_at: "2026-01-01T00:00:00Z".to_owned(),
            },
        ];
        assert!(same_grant_set(&a, &b));

        let c = vec![PermissionGrant {
            persona_id: "bob".to_owned(),
            tier: "read_only".to_owned(),
            granted_at: "2026-01-01T00:00:00Z".to_owned(),
        }];
        assert!(!same_grant_set(&a, &c));
    }
}
