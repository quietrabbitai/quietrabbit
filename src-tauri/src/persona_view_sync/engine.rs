// src-tauri/src/persona_view_sync/engine.rs
//
// VIEW-ONLY cross-account persona sharing (items.id=304, decisions.id=723):
// grant accept, revoke, and the ongoing push/pull transport. Sibling to
// persona_sync/engine.rs (SYNCED, items.id=303), not a branch inside it.
//
// WHY A SIBLING MODULE, NOT A BRANCH: pull/apply are fundamentally different
// algorithms, not a parameter apart. SYNCED reconciles per-entity through
// source_registry_store::refresh_verdict_conn against modification_state --
// real local-edit protection for a real independent copy. VIEW-ONLY has no
// local copy to protect: every pull deletes the read-only cache wholesale and
// re-inserts the new snapshot (decisions.id=723: "no merge, nothing
// recipient-editable"). Branching persona_sync::engine::apply_update would
// bolt an unrelated algorithm onto an already-dense function. Provisioning
// (source_registry linkage) doesn't exist here at all -- nothing to link a
// read-only cache into. This matches decisions.id=723's own framing of
// VIEW-ONLY as "simpler than SYNCED by construction".
//
// WHAT IS REUSED, DIRECTLY (not duplicated):
//   - auth::persona_sharing::{PersonaSharePayload, PendingPersonaShareRow,
//     fetch_pending_persona_share, load_shared_entity_facts,
//     load_shared_voice_profile_entries} -- the grant envelope and its
//     content-scope filtering are identical for both grant types
//     (decisions.id=723: "the owner-side content being shared is the same
//     shape"). Only what accept does with the decrypted payload differs.
//   - persona_sync::settings_store (persona_share_sync_settings, role=owner)
//     for the OWNER side of a VIEW-ONLY share -- structurally identical to
//     SYNCED's owner side (a real persona_id that pushes the same content
//     shape), so reused unchanged, no schema change. Only the RECIPIENT side
//     needed a new table (this module's own settings_store.rs) -- a
//     VIEW-ONLY recipient has no persona_id to key a settings row on.
//
// WHAT IS DUPLICATED, DELIBERATELY: the shared.db opener, content_hash, and
// write_envelope_atomic below are small, close copies of persona_sync::
// engine's own versions. Same reasoning every DB opener in this codebase
// already gives for duplicating rather than sharing: a different, module-
// specific error type on each side, zero real divergence risk in a ~10-line
// pure/IO helper, not worth the cross-module error-type coupling that
// importing them would otherwise force.
//
// TOMBSTONE REPRESENTATION: PersonaViewUpdatePayload below is its own wire
// type (own schema_version track), not a retrofit onto persona_sync::engine's
// PersonaSyncUpdatePayload -- that type is SYNCED-only and has no revoke
// concept today (decisions.id=617's asymmetric SYNCED-revocation framing
// leaves that explicitly unresolved, items.id=299 point 4). Internally
// tagged (`kind`) so Content vs. Revoked is an explicit, self-describing
// field in the decrypted JSON, not inferred from which fields are present.
//
// PUSH CADENCE: same periodic timer as persona_sync's own sweep (main.rs),
// same content_hash-gating mechanism -- but as a separate sibling call
// (run_periodic_sweep below), not folded into persona_sync::engine's own
// function, for the same module-separation reasoning above. Once an owner
// revokes (pending_persona_shares.revoked_at set, see
// revoke_persona_view_share), the push path emits a Revoked payload instead
// of Content, gated through the exact same content-hash mechanism against a
// fixed marker hash -- written once, then left alone by later sweeps, but
// staying on disk for whenever the recipient's device next checks in. No
// remote wipe exists in this architecture (decisions.id=723's own accepted
// limitation, mirroring group.db's trust-based framing, items.id=210).

use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;
use x25519_dalek::StaticSecret;

use crate::auth::persona_sharing::{
    self, PersonaSharePayload, SharedEntityFact, SharedVoiceProfileEntry,
};
use crate::auth::registry::KeyRegistry;
use crate::auth::sharing_keypair::{self, SharingKeypairError};
use crate::persistence::entity_store::{self, Entity, EntityFilter, ParentFilter};
use crate::persistence::view_cache_store::{self, ViewCacheStatus, ViewCacheStoreError};
use crate::persona_sync::settings_store as owner_settings_store;
use crate::persona_view_sync::settings_store::{self, PersonaViewShareSyncSettingsError};

/// Versions PersonaViewUpdatePayload's wire shape. A distinct version track
/// from both PersonaSharePayload's (grant) and persona_sync::engine::
/// PersonaSyncUpdatePayload's (SYNCED ongoing) own schema_version -- three
/// separate channels, free to evolve independently.
pub const PERSONA_VIEW_UPDATE_PAYLOAD_SCHEMA_VERSION: &str = "1.0";

/// Fixed content_hash surrogate for a Revoked payload -- unlike a Content
/// payload's hash (real digest over real content), a tombstone's "content"
/// never changes, so this constant is what gates repeat pushes from
/// re-writing the envelope every sweep once it's been written the first
/// time. Never a value content_hash() could itself produce (that fn always
/// returns a 64-character hex SHA-256 digest; this is deliberately not
/// hex-shaped).
const REVOKED_CONTENT_HASH_MARKER: &str = "revoked-tombstone";

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum PersonaViewSyncError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("View cache error: {0}")]
    ViewCache(#[from] ViewCacheStoreError),
    #[error("Sharing keypair error: {0}")]
    Sharing(#[from] SharingKeypairError),
    #[error("Owner-side sync settings error: {0}")]
    OwnerSettings(#[from] owner_settings_store::PersonaShareSyncSettingsError),
    #[error("Recipient-side sync settings error: {0}")]
    RecipientSettings(#[from] PersonaViewShareSyncSettingsError),
    #[error("Persona sharing error: {0}")]
    PersonaSharing(#[from] persona_sharing::PersonaSharingError),
    #[error("Personal store error: {0}")]
    PersonalStore(#[from] crate::persistence::personal_store::PersonalStoreError),
    #[error("Validation error: {0}")]
    Validation(String),
}

// ---------------------------------------------------------------------------
// Payload shape
// ---------------------------------------------------------------------------

/// The ongoing-update wire payload. Internally tagged so Content vs. Revoked
/// is explicit in the decrypted JSON -- see this module's own header
/// (TOMBSTONE REPRESENTATION).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PersonaViewUpdatePayload {
    Content {
        schema_version: String,
        /// RFC3339, stamped fresh on every push -- compared lexicographically
        /// against the recipient's own last-applied emitted_at (view_cache_
        /// meta.last_synced_at) to decide "is this newer". Same reasoning
        /// persona_sync::engine's own emitted_at field gives: filesystem
        /// mtimes are not trustworthy across heterogeneous NAS/cloud clients.
        emitted_at: String,
        entities: Vec<Entity>,
        entity_facts: Vec<SharedEntityFact>,
        voice_profile_entries: Vec<SharedVoiceProfileEntry>,
    },
    Revoked {
        schema_version: String,
        emitted_at: String,
    },
}

impl PersonaViewUpdatePayload {
    fn emitted_at(&self) -> &str {
        match self {
            PersonaViewUpdatePayload::Content { emitted_at, .. }
            | PersonaViewUpdatePayload::Revoked { emitted_at, .. } => emitted_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------
// Same shape/location as persona_sync::engine's own sync_dir/sync_file_path
// -- share_id is unique per row regardless of share_type (one
// pending_persona_shares table for both grant types), so no collision risk
// from sharing the path convention.

fn sync_dir(folder_path: &str, share_id: &str) -> std::path::PathBuf {
    std::path::Path::new(folder_path)
        .join("quietrabbit")
        .join("persona-shares")
        .join(share_id)
}

fn sync_file_path(folder_path: &str, share_id: &str) -> std::path::PathBuf {
    sync_dir(folder_path, share_id).join("update.qrshare")
}

// ---------------------------------------------------------------------------
// DB opener (shared.db -- unencrypted)
// ---------------------------------------------------------------------------

async fn open_shared_db() -> Result<SqliteConnection, PersonaViewSyncError> {
    let db_path = crate::persistence::migrations::get_data_root()
        .join("instance")
        .join("shared.db");
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

fn hex_decode(context: &str, s: &str) -> Result<Vec<u8>, PersonaViewSyncError> {
    if !s.len().is_multiple_of(2) {
        return Err(PersonaViewSyncError::Validation(format!(
            "pending_persona_shares.encrypted_payload for share '{context}' is not valid hex"
        )));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| {
                PersonaViewSyncError::Validation(format!(
                    "pending_persona_shares.encrypted_payload for share '{context}' is not valid hex"
                ))
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Envelope file I/O
// ---------------------------------------------------------------------------

async fn write_envelope_atomic(
    path: &std::path::Path,
    bytes: &[u8],
) -> Result<(), PersonaViewSyncError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp_path = std::path::PathBuf::from(tmp);
    tokio::fs::write(&tmp_path, bytes).await?;
    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
}

/// Deterministic hash of a Content payload's real fields -- same shape and
/// same reasoning as persona_sync::engine::content_hash (excludes
/// emitted_at, which changes on every push regardless of content).
fn content_hash(
    entities: &[Entity],
    entity_facts: &[SharedEntityFact],
    voice_profile_entries: &[SharedVoiceProfileEntry],
) -> Result<String, PersonaViewSyncError> {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(&(entities, entity_facts, voice_profile_entries))?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

// ---------------------------------------------------------------------------
// Accept
// ---------------------------------------------------------------------------

/// Accept a pending VIEW-ONLY persona share: decrypt its envelope (same
/// PersonaSharePayload shape send_persona_share/accept_persona_share already
/// use) and populate the recipient's read-only cache -- no Persona created,
/// no entities/entity_facts written on the recipient's account
/// (decisions.id=723). Returns nothing meaningful to a caller beyond success
/// -- contrast with accept_persona_share, which must return a new persona_id
/// because it minted one; there is nothing analogous here.
///
/// ORDERING mirrors accept_persona_share's own reasoning: decrypt happens
/// before any row mutation (tamper/wrong-key leaves the share row untouched,
/// still 'pending'). The cache content is written first (its own SAVEPOINT,
/// real single-file atomicity -- unlike SYNCED's cross-file split); only
/// once that succeeds does shared.db get touched (status -> 'accepted').
pub async fn accept_persona_view_share(
    share_id: &str,
    recipient_user_id: &str,
    recipient_personal_key_hex: &str,
    sharing_private_key: &StaticSecret,
) -> Result<(), PersonaViewSyncError> {
    let mut shared_conn = open_shared_db().await?;
    let share =
        persona_sharing::fetch_pending_persona_share(share_id, recipient_user_id, &mut shared_conn)
            .await?;

    if share.share_type != "view_only" {
        return Err(PersonaViewSyncError::Validation(format!(
            "Persona share '{share_id}' is share_type='{}', not 'view_only' -- \
             accept_persona_view_share cannot accept a SYNCED share.",
            share.share_type
        )));
    }

    let envelope = hex_decode(share_id, &share.encrypted_payload)?;
    let plaintext = sharing_keypair::decrypt_own_envelope(sharing_private_key, &envelope)?;
    let payload: PersonaSharePayload = serde_json::from_slice(&plaintext).map_err(|e| {
        PersonaViewSyncError::Validation(format!(
            "Persona share '{share_id}' decrypted to a payload that failed to deserialize: {e}"
        ))
    })?;

    let mut cache_conn = view_cache_store::open_view_cache_db(
        recipient_user_id,
        share_id,
        recipient_personal_key_hex,
    )
    .await?;

    sqlx::query("SAVEPOINT accept_persona_view_share")
        .execute(&mut cache_conn)
        .await?;

    let now = crate::providers::utils::now();
    let step: Result<(), PersonaViewSyncError> = async {
        view_cache_store::init_meta_conn(
            &mut cache_conn,
            share_id,
            &share.source_persona_display_name,
            &share.source_persona_type,
        )
        .await?;
        view_cache_store::replace_content_conn(
            &mut cache_conn,
            &payload.entities,
            &payload.entity_facts,
            &payload.voice_profile_entries,
            &now,
        )
        .await?;
        Ok(())
    }
    .await;

    match step {
        Ok(()) => {
            sqlx::query("RELEASE accept_persona_view_share")
                .execute(&mut cache_conn)
                .await?;
        }
        Err(e) => {
            if let Err(rollback_err) = sqlx::query("ROLLBACK TO accept_persona_view_share")
                .execute(&mut cache_conn)
                .await
            {
                log::error!(
                    "Savepoint rollback failed in accept_persona_view_share: {rollback_err}"
                );
            }
            let _ = sqlx::query("RELEASE accept_persona_view_share")
                .execute(&mut cache_conn)
                .await;
            return Err(e);
        }
    }
    drop(cache_conn);

    let responded_at = crate::providers::utils::now();
    sqlx::query(
        "UPDATE pending_persona_shares SET status = 'accepted', responded_at = ?
         WHERE id = ? AND recipient_user_id = ?",
    )
    .bind(&responded_at)
    .bind(share_id)
    .bind(recipient_user_id)
    .execute(&mut shared_conn)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Revoke
// ---------------------------------------------------------------------------

/// Owner-initiated revocation (decisions.id=723): stamps this share's own
/// revoked_at, this account's local bookkeeping/UI signal. Does NOT push
/// immediately -- the next periodic sweep's push path (push_if_changed_view)
/// checks revoked_at and emits a Revoked payload instead of Content from
/// then on. Only a VIEW-ONLY share owned by `owner_user_id` can be revoked
/// this way -- rows_affected() == 0 (wrong owner, wrong share_type, or
/// unknown share_id) is a Validation error, not a silent no-op, so a caller
/// can't mistake "nothing happened" for "revoked".
pub async fn revoke_persona_view_share(
    owner_user_id: &str,
    share_id: &str,
) -> Result<(), PersonaViewSyncError> {
    let mut conn = open_shared_db().await?;
    let now = crate::providers::utils::now();

    let result = sqlx::query(
        "UPDATE pending_persona_shares SET revoked_at = ?
         WHERE id = ? AND share_type = 'view_only' AND status = 'accepted'
         AND source_persona_id IN (SELECT persona_id FROM user_personas WHERE user_id = ?)",
    )
    .bind(&now)
    .bind(share_id)
    .bind(owner_user_id)
    .execute(&mut conn)
    .await?;

    if result.rows_affected() == 0 {
        return Err(PersonaViewSyncError::Validation(format!(
            "Persona share '{share_id}' is not an accepted VIEW-ONLY share owned by user '{owner_user_id}'"
        )));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Share discovery (shared.db)
// ---------------------------------------------------------------------------

/// (share_id, source_persona_id, recipient_user_id, revoked_at) for every
/// accepted VIEW-ONLY share owned by a persona `user_id` owns.
async fn list_outbound_view_shares(
    user_id: &str,
) -> Result<Vec<(String, String, String, Option<String>)>, PersonaViewSyncError> {
    let mut conn = open_shared_db().await?;
    let rows = sqlx::query(
        "SELECT pps.id, pps.source_persona_id, pps.recipient_user_id, pps.revoked_at
         FROM pending_persona_shares pps
         JOIN user_personas up ON up.persona_id = pps.source_persona_id
         WHERE up.user_id = ? AND pps.status = 'accepted' AND pps.share_type = 'view_only'",
    )
    .bind(user_id)
    .fetch_all(&mut conn)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push((
            r.try_get("id")?,
            r.try_get("source_persona_id")?,
            r.try_get("recipient_user_id")?,
            r.try_get("revoked_at")?,
        ));
    }
    Ok(out)
}

/// share_id for every accepted VIEW-ONLY share `recipient_user_id` has
/// accepted. No materialized_persona_id equivalent -- there is nothing
/// analogous to look up (decisions.id=723).
async fn list_inbound_view_shares(
    recipient_user_id: &str,
) -> Result<Vec<String>, PersonaViewSyncError> {
    let mut conn = open_shared_db().await?;
    let rows = sqlx::query(
        "SELECT id FROM pending_persona_shares
         WHERE recipient_user_id = ? AND status = 'accepted' AND share_type = 'view_only'",
    )
    .bind(recipient_user_id)
    .fetch_all(&mut conn)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(r.try_get("id")?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

async fn push_all_owned_view_shares(user_id: &str, personal_key_hex: &str) {
    let shares = match list_outbound_view_shares(user_id).await {
        Ok(s) => s,
        Err(e) => {
            log::warn!("persona_view_sync: could not list outbound shares for user={user_id}: {e}");
            return;
        }
    };

    for (share_id, owner_persona_id, recipient_user_id, revoked_at) in shares {
        push_one_view_share(
            user_id,
            &owner_persona_id,
            personal_key_hex,
            &share_id,
            &recipient_user_id,
            revoked_at.is_some(),
        )
        .await;
    }
}

async fn push_one_view_share(
    owner_user_id: &str,
    owner_persona_id: &str,
    owner_personal_key_hex: &str,
    share_id: &str,
    recipient_user_id: &str,
    revoked: bool,
) {
    let result = push_if_changed_view(
        owner_user_id,
        owner_persona_id,
        owner_personal_key_hex,
        share_id,
        recipient_user_id,
        revoked,
    )
    .await;

    match result {
        Ok(_) => {}
        Err(e) => {
            log::warn!(
                "persona_view_sync: push failed for share={share_id} \
                 owner_persona={owner_persona_id}: {e}"
            );
            if let Err(e2) = owner_settings_store::record_push_result(
                owner_persona_id,
                share_id,
                Err(&e.to_string()),
            )
            .await
            {
                log::warn!("persona_view_sync: could not record push failure: {e2}");
            }
        }
    }
}

/// Push `share_id`'s current content (or, once revoked, its tombstone) if it
/// differs from the last push. Reuses persona_sync's own owner-side settings
/// row (role='owner') unchanged -- see this module's own header.
async fn push_if_changed_view(
    owner_user_id: &str,
    owner_persona_id: &str,
    owner_personal_key_hex: &str,
    share_id: &str,
    recipient_user_id: &str,
    revoked: bool,
) -> Result<bool, PersonaViewSyncError> {
    let Some(settings) =
        owner_settings_store::get_persona_share_sync_settings(owner_persona_id, share_id).await?
    else {
        return Ok(false);
    };

    let (payload, hash) = if revoked {
        let payload = PersonaViewUpdatePayload::Revoked {
            schema_version: PERSONA_VIEW_UPDATE_PAYLOAD_SCHEMA_VERSION.to_owned(),
            emitted_at: crate::providers::utils::now(),
        };
        (payload, REVOKED_CONTENT_HASH_MARKER.to_owned())
    } else {
        let active_entities = entity_store::list_entities(
            owner_user_id,
            owner_persona_id,
            owner_personal_key_hex,
            &EntityFilter {
                entity_type: None,
                status: Some("active".to_owned()),
                parent: ParentFilter::Any,
            },
        )
        .await?;
        let active_entity_ids: Vec<String> = active_entities.iter().map(|e| e.id.clone()).collect();

        let mut conn = crate::persistence::personal_store::open_personal_db(
            owner_user_id,
            owner_persona_id,
            owner_personal_key_hex,
        )
        .await?;
        let entity_facts =
            persona_sharing::load_shared_entity_facts(&mut conn, &active_entity_ids).await?;
        let voice_profile_entries =
            persona_sharing::load_shared_voice_profile_entries(&mut conn, owner_persona_id).await?;
        drop(conn);

        let hash = content_hash(&active_entities, &entity_facts, &voice_profile_entries)?;
        let payload = PersonaViewUpdatePayload::Content {
            schema_version: PERSONA_VIEW_UPDATE_PAYLOAD_SCHEMA_VERSION.to_owned(),
            emitted_at: crate::providers::utils::now(),
            entities: active_entities,
            entity_facts,
            voice_profile_entries,
        };
        (payload, hash)
    };

    if settings.last_content_hash.as_deref() == Some(hash.as_str()) {
        owner_settings_store::record_push_result(owner_persona_id, share_id, Ok(None)).await?;
        return Ok(false);
    }

    let recipient_public_key = sharing_keypair::get_public_key(recipient_user_id)
        .await?
        .ok_or_else(|| {
            PersonaViewSyncError::Validation(format!(
                "Recipient user '{recipient_user_id}' has no registered sharing public key"
            ))
        })?;

    let plaintext = serde_json::to_vec(&payload)?;
    let envelope = sharing_keypair::encrypt_to_public_key(&recipient_public_key, &plaintext)?;

    let path = sync_file_path(&settings.folder_path, share_id);
    write_envelope_atomic(&path, &envelope).await?;

    owner_settings_store::record_push_result(owner_persona_id, share_id, Ok(Some(&hash))).await?;

    Ok(true)
}

// ---------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------

async fn pull_all_accepted_view_shares(
    recipient_user_id: &str,
    recipient_personal_key_hex: &str,
    sharing_private_key: &StaticSecret,
) {
    let shares = match list_inbound_view_shares(recipient_user_id).await {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "persona_view_sync: could not list inbound shares for user={recipient_user_id}: {e}"
            );
            return;
        }
    };

    for share_id in shares {
        let result = pull_if_newer_view(
            recipient_user_id,
            recipient_personal_key_hex,
            &share_id,
            sharing_private_key,
        )
        .await;

        if let Err(e) = result {
            log::warn!("persona_view_sync: pull failed for share={share_id}: {e}");
            if let Err(e2) = settings_store::record_pull_result(
                recipient_user_id,
                &share_id,
                Err(&e.to_string()),
            )
            .await
            {
                log::warn!("persona_view_sync: could not record pull failure: {e2}");
            }
        }
    }
}

/// Pull and apply `share_id`'s update if newer than what's already cached.
/// Returns Ok(true) if applied, Ok(false) if sync isn't configured, the
/// share has already ended (terminal -- no reason to keep reading a file
/// that will never change again), no envelope is waiting yet, or it isn't
/// newer than the last one applied.
async fn pull_if_newer_view(
    recipient_user_id: &str,
    recipient_personal_key_hex: &str,
    share_id: &str,
    sharing_private_key: &StaticSecret,
) -> Result<bool, PersonaViewSyncError> {
    let Some(settings) =
        settings_store::get_persona_view_share_sync_settings(recipient_user_id, share_id).await?
    else {
        return Ok(false);
    };

    let mut cache_conn = view_cache_store::open_view_cache_db(
        recipient_user_id,
        share_id,
        recipient_personal_key_hex,
    )
    .await?;
    let meta = view_cache_store::get_meta_conn(&mut cache_conn).await?;

    if let Some(m) = &meta {
        if m.status == ViewCacheStatus::Ended {
            // Terminal state (decisions.id=723) -- nothing will ever arrive
            // that changes this share again.
            return Ok(false);
        }
    }

    let path = sync_file_path(&settings.folder_path, share_id);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(e) => return Err(e.into()),
    };

    let plaintext = sharing_keypair::decrypt_own_envelope(sharing_private_key, &bytes)?;
    let payload: PersonaViewUpdatePayload = serde_json::from_slice(&plaintext)?;

    let is_newer = match meta.as_ref().and_then(|m| m.last_synced_at.as_deref()) {
        None => true,
        Some(last) => payload.emitted_at() > last,
    };
    if !is_newer {
        return Ok(false);
    }

    sqlx::query("SAVEPOINT persona_view_sync_apply_update")
        .execute(&mut cache_conn)
        .await?;

    let step: Result<(), PersonaViewSyncError> = async {
        match &payload {
            PersonaViewUpdatePayload::Content {
                entities,
                entity_facts,
                voice_profile_entries,
                emitted_at,
                ..
            } => {
                view_cache_store::replace_content_conn(
                    &mut cache_conn,
                    entities,
                    entity_facts,
                    voice_profile_entries,
                    emitted_at,
                )
                .await?;
            }
            PersonaViewUpdatePayload::Revoked { emitted_at, .. } => {
                view_cache_store::mark_ended_conn(&mut cache_conn, emitted_at).await?;
            }
        }
        Ok(())
    }
    .await;

    match step {
        Ok(()) => {
            sqlx::query("RELEASE persona_view_sync_apply_update")
                .execute(&mut cache_conn)
                .await?;
        }
        Err(e) => {
            if let Err(rollback_err) = sqlx::query("ROLLBACK TO persona_view_sync_apply_update")
                .execute(&mut cache_conn)
                .await
            {
                log::error!(
                    "Savepoint rollback failed in persona_view_sync apply_update: {rollback_err}"
                );
            }
            let _ = sqlx::query("RELEASE persona_view_sync_apply_update")
                .execute(&mut cache_conn)
                .await;
            return Err(e);
        }
    }

    settings_store::record_pull_result(recipient_user_id, share_id, Ok(())).await?;

    Ok(true)
}

// ---------------------------------------------------------------------------
// Session-level entry points
// ---------------------------------------------------------------------------

/// Push every owned VIEW-ONLY share, then pull every accepted one, for
/// whichever account is currently resident in `key_registry`. A no-op if
/// nobody is logged in. Called from main.rs's existing periodic timer,
/// alongside persona_sync's own sweep -- same timer, no second interval.
pub async fn run_periodic_sweep(key_registry: &KeyRegistry) {
    let Some((user_id, personal_key_hex, sharing_private_key_bytes)) = key_registry
        .with_key(|k| {
            (
                k.user_id.clone(),
                crate::auth::registry::key_hex(&k.master_key),
                k.sharing_private_key,
            )
        })
        .await
    else {
        return;
    };
    let sharing_private_key = StaticSecret::from(sharing_private_key_bytes);

    push_all_owned_view_shares(&user_id, &personal_key_hex).await;
    pull_all_accepted_view_shares(&user_id, &personal_key_hex, &sharing_private_key).await;
}

/// Pull every accepted inbound VIEW-ONLY share once, immediately. Called
/// right after a sharing private key becomes resident (commands/auth.rs::
/// finish_login) -- same reasoning persona_sync::engine::
/// pull_all_accepted_shares_on_login's own doc comment gives.
pub async fn pull_all_accepted_view_shares_on_login(
    user_id: &str,
    personal_key_hex: &str,
    sharing_private_key: &StaticSecret,
) {
    pull_all_accepted_view_shares(user_id, personal_key_hex, sharing_private_key).await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::kdf;
    use crate::auth::persona_sharing::{send_persona_share, ShareType};
    use crate::auth::sharing_keypair as sk;
    use crate::persistence::{entity_store, persona_store, personal_store};
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

    /// Mirrors persona_sync::engine's own test fixture convention.
    async fn make_user_with_persona(
        display_name: &str,
        master_key_fill: u8,
    ) -> (String, String, String, StaticSecret) {
        let user_id = uuid::Uuid::new_v4().to_string();
        let master_key = [master_key_fill; kdf::MASTER_KEY_LEN];
        let (sharing_private_key, sharing_public_key) =
            sk::derive_sharing_keypair(&master_key, &user_id);
        let key_hex: String = master_key.iter().map(|b| format!("{b:02x}")).collect();

        crate::auth::user_store::create_user(
            &user_id,
            display_name,
            "user",
            false,
            b"test-salt",
            1024,
            1,
            1,
            sharing_public_key.as_bytes(),
        )
        .await
        .expect("create_user must succeed");

        let persona_id = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(&persona_id, "Shared Persona", "personal", &user_id, None)
            .await
            .expect("create_persona must succeed");

        (user_id, persona_id, key_hex, sharing_private_key)
    }

    /// Recipient side of VIEW-ONLY needs only an account -- no Persona
    /// (decisions.id=723).
    async fn make_recipient_user(
        display_name: &str,
        master_key_fill: u8,
    ) -> (String, String, StaticSecret) {
        let user_id = uuid::Uuid::new_v4().to_string();
        let master_key = [master_key_fill; kdf::MASTER_KEY_LEN];
        let (sharing_private_key, sharing_public_key) =
            sk::derive_sharing_keypair(&master_key, &user_id);
        let key_hex: String = master_key.iter().map(|b| format!("{b:02x}")).collect();

        crate::auth::user_store::create_user(
            &user_id,
            display_name,
            "user",
            false,
            b"test-salt",
            1024,
            1,
            1,
            sharing_public_key.as_bytes(),
        )
        .await
        .expect("create_user must succeed");

        (user_id, key_hex, sharing_private_key)
    }

    #[tokio::test]
    async fn accept_populates_cache_and_flips_status_without_creating_a_persona() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x30).await;
        let (recipient_id, recipient_key, recipient_priv) = make_recipient_user("Bob", 0x40).await;

        entity_store::create_entity(
            &owner_id,
            &owner_persona,
            &owner_key,
            "person",
            "Contact",
            &[],
            None,
            None,
        )
        .await
        .unwrap();

        let share_id = send_persona_share(
            &owner_id,
            &owner_persona,
            &owner_key,
            &recipient_id,
            ShareType::ViewOnly,
        )
        .await
        .unwrap();

        accept_persona_view_share(&share_id, &recipient_id, &recipient_key, &recipient_priv)
            .await
            .expect("accept_persona_view_share must succeed");

        let mut cache_conn =
            view_cache_store::open_view_cache_db(&recipient_id, &share_id, &recipient_key)
                .await
                .unwrap();
        let meta = view_cache_store::get_meta_conn(&mut cache_conn)
            .await
            .unwrap()
            .expect("meta must exist after accept");
        assert_eq!(meta.status, ViewCacheStatus::Active);
        assert_eq!(meta.source_persona_display_name, "Shared Persona");

        let mut shared_conn = open_shared_db().await.unwrap();
        let status: String =
            sqlx::query_scalar("SELECT status FROM pending_persona_shares WHERE id = ?")
                .bind(&share_id)
                .fetch_one(&mut shared_conn)
                .await
                .unwrap();
        assert_eq!(status, "accepted");

        // No Persona/user_personas row was ever created for the recipient.
        let persona_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM user_personas WHERE user_id = ?")
                .bind(&recipient_id)
                .fetch_one(&mut shared_conn)
                .await
                .unwrap();
        assert_eq!(persona_count, 0);
    }

    #[tokio::test]
    async fn accept_rejects_a_synced_share() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x31).await;
        let (recipient_id, recipient_key, recipient_priv) = make_recipient_user("Bob", 0x41).await;

        let share_id = send_persona_share(
            &owner_id,
            &owner_persona,
            &owner_key,
            &recipient_id,
            ShareType::Synced,
        )
        .await
        .unwrap();

        let result =
            accept_persona_view_share(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await;
        assert!(matches!(result, Err(PersonaViewSyncError::Validation(_))));
    }

    #[tokio::test]
    async fn push_then_pull_round_trips_content_into_the_read_only_cache() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x32).await;
        let (recipient_id, recipient_key, recipient_priv) = make_recipient_user("Bob", 0x42).await;

        let entity_id = entity_store::create_entity(
            &owner_id,
            &owner_persona,
            &owner_key,
            "person",
            "Contact",
            &[],
            None,
            None,
        )
        .await
        .unwrap();
        personal_store::create_entity_fact_with_provenance(
            &owner_id,
            &owner_persona,
            &owner_key,
            Some(entity_id.as_str()),
            "phone",
            "555-1234",
            "personal",
            "interview",
            &owner_persona,
            false,
            None,
            None,
        )
        .await
        .unwrap();

        let share_id = send_persona_share(
            &owner_id,
            &owner_persona,
            &owner_key,
            &recipient_id,
            ShareType::ViewOnly,
        )
        .await
        .unwrap();

        accept_persona_view_share(&share_id, &recipient_id, &recipient_key, &recipient_priv)
            .await
            .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        owner_settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            owner_settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_view_share_sync_folder(&recipient_id, &share_id, folder_path)
            .await
            .unwrap();

        // Change the fact after the grant was sent -- exercises the ongoing
        // channel, not a replay of the original grant payload.
        personal_store::create_entity_fact_with_provenance(
            &owner_id,
            &owner_persona,
            &owner_key,
            Some(entity_id.as_str()),
            "phone",
            "555-9999",
            "personal",
            "user_edit",
            &owner_persona,
            false,
            None,
            None,
        )
        .await
        .unwrap();

        let pushed = push_if_changed_view(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
            false,
        )
        .await
        .expect("push_if_changed_view must succeed");
        assert!(pushed);

        let applied = pull_if_newer_view(&recipient_id, &recipient_key, &share_id, &recipient_priv)
            .await
            .expect("pull_if_newer_view must succeed");
        assert!(applied);

        let mut cache_conn =
            view_cache_store::open_view_cache_db(&recipient_id, &share_id, &recipient_key)
                .await
                .unwrap();
        let value: String = sqlx::query_scalar(
            "SELECT field_value FROM view_cache_entity_facts WHERE field_name = 'phone'",
        )
        .fetch_one(&mut cache_conn)
        .await
        .unwrap();
        assert_eq!(
            value, "555-9999",
            "the post-grant fact update must have applied"
        );
    }

    #[tokio::test]
    async fn a_repeat_push_with_unchanged_content_is_skipped() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x33).await;
        let (recipient_id, _, _) = make_recipient_user("Bob", 0x43).await;

        let share_id = send_persona_share(
            &owner_id,
            &owner_persona,
            &owner_key,
            &recipient_id,
            ShareType::ViewOnly,
        )
        .await
        .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        owner_settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            owner_settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();

        let first = push_if_changed_view(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
            false,
        )
        .await
        .unwrap();
        assert!(first);

        let second = push_if_changed_view(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
            false,
        )
        .await
        .unwrap();
        assert!(!second, "unchanged content must not be re-pushed");
    }

    #[tokio::test]
    async fn revoke_then_push_then_pull_ends_the_share_and_clears_the_cache() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x34).await;
        let (recipient_id, recipient_key, recipient_priv) = make_recipient_user("Bob", 0x44).await;

        entity_store::create_entity(
            &owner_id,
            &owner_persona,
            &owner_key,
            "person",
            "Contact",
            &[],
            None,
            None,
        )
        .await
        .unwrap();

        let share_id = send_persona_share(
            &owner_id,
            &owner_persona,
            &owner_key,
            &recipient_id,
            ShareType::ViewOnly,
        )
        .await
        .unwrap();
        accept_persona_view_share(&share_id, &recipient_id, &recipient_key, &recipient_priv)
            .await
            .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        owner_settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            owner_settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_view_share_sync_folder(&recipient_id, &share_id, folder_path)
            .await
            .unwrap();

        // A real content push+pull first, so there's something to clear.
        push_if_changed_view(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
            false,
        )
        .await
        .unwrap();
        pull_if_newer_view(&recipient_id, &recipient_key, &share_id, &recipient_priv)
            .await
            .unwrap();

        revoke_persona_view_share(&owner_id, &share_id)
            .await
            .expect("revoke_persona_view_share must succeed");

        let tombstone_pushed = push_if_changed_view(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
            true,
        )
        .await
        .expect("tombstone push must succeed");
        assert!(tombstone_pushed);

        let applied = pull_if_newer_view(&recipient_id, &recipient_key, &share_id, &recipient_priv)
            .await
            .expect("tombstone pull must succeed");
        assert!(applied);

        let mut cache_conn =
            view_cache_store::open_view_cache_db(&recipient_id, &share_id, &recipient_key)
                .await
                .unwrap();
        let meta = view_cache_store::get_meta_conn(&mut cache_conn)
            .await
            .unwrap()
            .expect("meta must still exist");
        assert_eq!(meta.status, ViewCacheStatus::Ended);
        assert!(meta.ended_at.is_some());
        assert_eq!(
            view_cache_store::count_entities_conn(&mut cache_conn)
                .await
                .unwrap(),
            0
        );

        // A terminal share must not be re-pulled even if somehow the folder
        // still holds a file -- pull_if_newer_view short-circuits on status.
        let repeat = pull_if_newer_view(&recipient_id, &recipient_key, &share_id, &recipient_priv)
            .await
            .unwrap();
        assert!(!repeat, "an already-ended share must not be pulled again");
    }

    #[tokio::test]
    async fn revoke_rejects_a_share_not_owned_by_the_caller() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x35).await;
        let (recipient_id, _, _) = make_recipient_user("Bob", 0x45).await;
        let (other_id, _, _, _) = make_user_with_persona("Eve", 0x36).await;

        let share_id = send_persona_share(
            &owner_id,
            &owner_persona,
            &owner_key,
            &recipient_id,
            ShareType::ViewOnly,
        )
        .await
        .unwrap();

        let result = revoke_persona_view_share(&other_id, &share_id).await;
        assert!(matches!(result, Err(PersonaViewSyncError::Validation(_))));
    }

    #[tokio::test]
    async fn push_is_a_silent_noop_when_folder_is_unset() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x37).await;
        let (recipient_id, _, _) = make_recipient_user("Bob", 0x47).await;

        let share_id = send_persona_share(
            &owner_id,
            &owner_persona,
            &owner_key,
            &recipient_id,
            ShareType::ViewOnly,
        )
        .await
        .unwrap();

        let pushed = push_if_changed_view(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
            false,
        )
        .await
        .expect("push_if_changed_view must succeed even when unconfigured");
        assert!(!pushed);
    }
}
