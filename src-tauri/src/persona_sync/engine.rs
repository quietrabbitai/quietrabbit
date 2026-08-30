// src-tauri/src/persona_sync/engine.rs
//
// Ongoing sync transport for SYNCED persona sharing (items.id=303,
// decisions.id=722). Builds on items.id=302's recipient-side materialization
// and items.id=299/289's X25519 per-recipient envelope mechanism -- no new
// crypto primitive, no server component (decisions.id=722). Delivery mirrors
// group.db's folder-sync (items.id=287, group_sync::engine) as closely as
// the two problems' real shape allows; see PUSH CADENCE and DELIVERY FORMAT
// below for where it deliberately does not.
//
// PUSH CADENCE (Jason's direction, narrowing decisions.id=722's literal
// "push-on-save"): periodic re-push on the same timer main.rs already runs
// group.db's pull on, not a hook into every entity/entity_fact write path.
// group.db's push-on-save is cheap to do literally because document CRUD
// has exactly one save function; persona content is written from ~6 separate
// functions across entity_store.rs/personal_store.rs/dedup_store.rs, core
// storage code used by every feature, not just sharing -- hooking all of
// them would mean every future contributor touching those files has to know
// a push side-effect lives there. A content-hash check (content_hash below)
// keeps a periodic sweep from writing anything when nothing has actually
// changed since the last push.
//
// DELIVERY FORMAT: unlike group.db's per-document .qrsync files (each its
// own small SQLCipher-encrypted SQLite database, symmetric-key encrypted),
// this module's shared-folder artifact is the raw X25519 envelope itself
// (sharing_keypair::encrypt_to_public_key's own wire format) written
// directly as bytes -- extension .qrshare, not .qrsync, because the format
// genuinely differs and conflating them in the shared folder would be
// misleading to anyone poking at it. No SQLCipher wrapper: the asymmetric
// envelope already provides the confidentiality guarantee a shared,
// potentially third-party-synced folder needs; wrapping it in a second,
// symmetric-key encryption layer would add nothing. Single canonical
// filename per share, overwritten each push (same "newer" contract as
// group.db, via an emitted_at field inside the decrypted payload rather
// than filesystem mtimes -- not reliable across heterogeneous NAS/cloud
// clients). Written via a temp-file-then-rename, a small, deliberate
// improvement over group_sync::engine's own accepted no-atomicity gap: this
// format has no page-level integrity mechanism of its own the way a
// SQLCipher file does, so a concurrent reader mid-write matters more here.
//
// RECONCILIATION MODEL: reuses decisions.id=502's existing provenance
// framework (source_registry_store::refresh_verdict_conn /
// apply_source_update_conn) rather than inventing a second one --
// source_registry_store.rs's own module header already names this exact
// scenario ("decisions.id=617's synced household grants reconcile a
// recipient instance against the owner's through this same framework") as
// its intended second adopter. Concretely: provision_sync_relationship
// creates one source_registry row per materialized share and retroactively
// stamps that share's entities 'pristine' (302's own materialization
// stamps them 'user_created' -- correct at the time, since no sync
// relationship existed yet to link into; see that function's own doc
// comment). From there, an incoming update's entities/entity_facts are
// gated per-entity by refresh_verdict_conn exactly like any other source
// refresh: AutoAccept applies, Conflict is skipped and flags
// source_registry.status = 'pending_refresh' (full per-field conflict UI is
// out of scope for this transport item), Ignore is skipped.
//
// LOCAL-EDIT GUARD DEPENDENCY: this reconciliation model only protects a
// recipient's local edits because entity_store::update_entity and
// personal_store::create_entity_fact_with_provenance now call
// source_registry_store::mark_record_user_modified_conn on a genuine local
// edit (items.id=303 wiring that previously-dead function in for the first
// time). This engine's own writes go through the *_conn layer directly
// (entity_store::update_entity_conn, raw entity_facts SQL below) specifically
// to bypass that guard -- a sync-applied update must never mark itself as a
// conflicting local edit. voice_profiles (items.id=306, personal_007.sql)
// now carries the same guard: personal_store::save_voice_profile_entry calls
// source_registry_store::mark_voice_profile_user_modified_conn on a genuine
// local edit, and this engine's voice_profile_entries loop below writes
// through voice_profile_refresh_verdict_conn / raw SQL directly, same
// bypass-the-guard reasoning as the entities/entity_facts paths above.
//
// SCOPE, deliberately narrow (matches this item's own transport framing):
//   - entity_facts deletion-by-absence (owner cleared a field entirely, not
//     just changed its value) is handled (items.id=307) -- see apply_update's
//     two absence-retirement passes after the entity_facts value-change loop.
//     voice_profiles deletion-by-absence is handled too (items.id=309) --
//     see apply_update's absence-retirement pass after the voice_profile_
//     entries value-change loop. Unlike entity_facts it needs no
//     source='synced_share'-style provenance tag: accept_persona_share
//     always mints a fresh persona_id per share, so persona_id alone
//     already scopes to one sync relationship (persona_id +
//     modification_state != 'user_created'), and retirement is a genuine
//     DELETE, not a supersede -- voice_profiles has no FK pointing at its
//     id and no immutable-provenance contract the way entity_facts/
//     disclosure_log do.
//   - entity-identity conflicts (dedup) are out of scope -- matching is
//     purely by preserved entity id (stable end to end since
//     accept_persona_share binds the owner's own entity.id, never
//     regenerating it), no dedup_store fuzzy matching.
//   - application-layer share-stopping (items.id=299 point 4) is untouched.

use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;
use x25519_dalek::StaticSecret;

use crate::auth::persona_sharing::{SharedEntityFact, SharedVoiceProfileEntry};
use crate::auth::registry::KeyRegistry;
use crate::auth::sharing_keypair::{self, SharingKeypairError};
use crate::persistence::entity_store::{self, Entity, EntityFilter, EntityUpdate, ParentFilter};
use crate::persistence::personal_store::{self, PersonalStoreError};
use crate::persistence::source_registry_store::{self, RefreshVerdict};
use crate::persona_sync::settings_store::{self, PersonaShareSyncSettingsError};

/// Versions PersonaSyncUpdatePayload's wire shape. Deliberately a distinct
/// version track from PersonaSharePayload's own schema_version (items.id=299)
/// -- the one-shot grant payload and the ongoing-update payload are separate
/// channels that should be free to evolve independently, matching
/// PERSONA_SHARE_PAYLOAD_SCHEMA_VERSION's own stated intent.
pub const PERSONA_SYNC_UPDATE_PAYLOAD_SCHEMA_VERSION: &str = "1.0";

/// source_registry.source_type value this module registers at provisioning
/// time. New, not one of decisions.id=502's enumerated KNOWN_SOURCE_TYPES --
/// source_registry_store.rs's own module header anticipates exactly this
/// ("the source_type value naming another QR instance is deliberately NOT
/// invented here" -- until now).
pub const PERSONA_SYNC_SOURCE_TYPE: &str = "persona_share_sync";

/// source_registry.focus_slug value this module registers at provisioning
/// time. focus_slug is NOT NULL and every other source_registry row is
/// scoped to a real Focus slug (e.g. "cooking") -- a whole-Persona sync
/// relationship isn't Focus-scoped at all, so this is a reserved sentinel,
/// not a real Focus. The leading underscore marks it as such; no real Focus
/// slug in this codebase uses that convention.
pub const PERSONA_SYNC_FOCUS_SLUG: &str = "_persona_sync";

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum PersonaSyncError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Personal store error: {0}")]
    PersonalStore(#[from] PersonalStoreError),
    #[error("Sharing keypair error: {0}")]
    Sharing(#[from] SharingKeypairError),
    #[error("persona_share_sync_settings error: {0}")]
    Settings(#[from] PersonaShareSyncSettingsError),
    #[error("Persona sharing error: {0}")]
    PersonaSharing(#[from] crate::auth::persona_sharing::PersonaSharingError),
    #[error("Validation error: {0}")]
    Validation(String),
}

// ---------------------------------------------------------------------------
// Payload shape
// ---------------------------------------------------------------------------

/// The ongoing-update wire payload -- a full, filtered snapshot of the
/// owner's current shareable content, re-sent whole on every push rather
/// than diffed (matches group.db's own per-document whole-content resend;
/// avoids the real complexity of incremental diffing for what is, at
/// personal-data scale, a small payload). Field shapes are the exact
/// content-scope-filtered types accept_persona_share/send_persona_share
/// already established (Entity, SharedEntityFact, SharedVoiceProfileEntry)
/// -- reused directly, not re-derived, so a filtering-rule change made to
/// one channel doesn't silently diverge from the other.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PersonaSyncUpdatePayload {
    schema_version: String,
    /// RFC3339 (providers::utils::now()), stamped fresh by the pusher on
    /// every push. Compared lexicographically against the recipient's own
    /// last_synced_at to decide "is this newer" -- filesystem mtimes are not
    /// trustworthy across heterogeneous NAS/cloud-sync clients, same
    /// reasoning group_sync::engine's own header gives for using
    /// documents.updated_at instead.
    emitted_at: String,
    entities: Vec<Entity>,
    entity_facts: Vec<SharedEntityFact>,
    voice_profile_entries: Vec<SharedVoiceProfileEntry>,
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

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
// Duplicated rather than reused -- same reasoning every other module in this
// codebase gives for this exact ~12-line helper: different error type per
// module, zero divergence risk, not worth coupling.

async fn open_shared_db() -> Result<SqliteConnection, PersonaSyncError> {
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

// ---------------------------------------------------------------------------
// Envelope file I/O
// ---------------------------------------------------------------------------

/// Write `bytes` to `path` via a temp-file-then-rename, creating parent
/// directories as needed. See this module's own header (DELIVERY FORMAT) on
/// why this format gets atomicity group_sync::engine's own artifacts don't.
async fn write_envelope_atomic(
    path: &std::path::Path,
    bytes: &[u8],
) -> Result<(), PersonaSyncError> {
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

/// Deterministic hash of the content that would be pushed, over the same
/// three fields the payload itself carries but excluding emitted_at (which
/// changes on every push regardless of whether content changed) -- lets a
/// periodic sweep skip writing anything when nothing has actually changed.
/// serde_json's default (non-preserve_order) Map is a BTreeMap, so key
/// order -- and therefore this hash -- is stable across calls regardless of
/// query result ordering upstream.
fn content_hash(
    entities: &[Entity],
    entity_facts: &[SharedEntityFact],
    voice_profile_entries: &[SharedVoiceProfileEntry],
) -> Result<String, PersonaSyncError> {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(&(entities, entity_facts, voice_profile_entries))?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

/// Establish the ongoing-sync relationship for a persona items.id=302's
/// accept_persona_share just materialized. Runs once, immediately after
/// accept_persona_share succeeds (see accept_and_provision_sync below) --
/// not folded into accept_persona_share itself, which stays untouched
/// (aside from the additive materialized_persona_id column bind,
/// items.id=303's only change to that function).
///
/// Two things happen, both scoped to this persona's own personal.db:
///   1. A new source_registry row is created (PERSONA_SYNC_SOURCE_TYPE /
///      PERSONA_SYNC_FOCUS_SLUG), with connection_config recording share_id
///      for debugging/traceability.
///   2. Every entity this share just materialized is retroactively flipped
///      from 'user_created' (302's own stamp, correct at the time, since no
///      sync relationship existed to link into yet) to 'pristine', linked
///      to the new source_registry row. The WHERE clause
///      (source_registry_id IS NULL AND modification_state = 'user_created')
///      scopes this to exactly the entities 302 just wrote, and it will not
///      touch anything the recipient creates themselves afterward, since a
///      manually-created entity is also 'user_created'/NULL at creation, but
///      only briefly, before any sync provisioning could plausibly race it
///      (provisioning runs immediately after materialization, before the
///      recipient has had any chance to act). This does NOT contradict
///      items.id=302's own tested behavior (materialized_entity_drops_
///      source_registry_id_and_resets_modification_state, persona_sharing.rs)
///      -- that test asserts materialization doesn't inherit the *owner's*
///      unrelated source_registry_id, which stays true; this is an entirely
///      new row, created fresh here, never the owner's dangling FK target.
///
/// IDEMPOTENCY, precisely scoped: a repeat call is safe for entity state --
/// step 2's WHERE clause means an already-provisioned entity is never
/// re-touched or re-linked, so a retry after a partial failure elsewhere
/// can't corrupt it. It is NOT idempotent for the source_registry row
/// itself: register_source_conn has no dedup (by design -- decisions.id=502
/// explicitly supports several same-type sources), so a repeat call mints a
/// second, entity-less row every time. Calling this more than once per
/// share is therefore harmless but wasteful, not something a caller should
/// rely on doing routinely.
///
/// Returns the new source_registry row's id.
pub async fn provision_sync_relationship(
    recipient_user_id: &str,
    persona_id: &str,
    recipient_personal_key_hex: &str,
    share_id: &str,
) -> Result<String, PersonaSyncError> {
    let mut conn =
        personal_store::open_personal_db(recipient_user_id, persona_id, recipient_personal_key_hex)
            .await?;

    sqlx::query("SAVEPOINT provision_sync_relationship")
        .execute(&mut conn)
        .await?;

    let step: Result<String, PersonaSyncError> = async {
        let source_id = source_registry_store::register_source_conn(
            &mut conn,
            persona_id,
            PERSONA_SYNC_FOCUS_SLUG,
            PERSONA_SYNC_SOURCE_TYPE,
            None,
            Some(serde_json::json!({ "share_id": share_id })),
        )
        .await?;

        sqlx::query(
            "UPDATE entities SET modification_state = 'pristine', source_registry_id = ?
             WHERE source_registry_id IS NULL AND modification_state = 'user_created'",
        )
        .bind(&source_id)
        .execute(&mut conn)
        .await?;

        // voice_profiles (items.id=306): same retroactive user_created ->
        // pristine flip as entities above, minus the source_registry_id link
        // -- voice_profiles has no such column (personal_007.sql; not
        // needed, see that migration's own header). Scoped by persona_id +
        // modification_state alone rather than an explicit id list from the
        // materialized share: accept_persona_share's voice_profiles INSERT
        // (persona_sharing.rs) always stamps 'user_created' on a fresh
        // persona.db that has no other write path today (personal_store::
        // save_voice_profile_entry has no caller yet), so this is exactly
        // the same "provisioning runs immediately after materialization,
        // before the recipient could plausibly race it" accepted window the
        // entities UPDATE above already documents and relies on.
        sqlx::query(
            "UPDATE voice_profiles SET modification_state = 'pristine'
             WHERE persona_id = ? AND modification_state = 'user_created'",
        )
        .bind(persona_id)
        .execute(&mut conn)
        .await?;

        Ok(source_id)
    }
    .await;

    match step {
        Ok(id) => {
            sqlx::query("RELEASE provision_sync_relationship")
                .execute(&mut conn)
                .await?;
            Ok(id)
        }
        Err(e) => {
            if let Err(rollback_err) = sqlx::query("ROLLBACK TO provision_sync_relationship")
                .execute(&mut conn)
                .await
            {
                log::error!(
                    "Savepoint rollback failed in provision_sync_relationship: {rollback_err}"
                );
            }
            let _ = sqlx::query("RELEASE provision_sync_relationship")
                .execute(&mut conn)
                .await;
            Err(e)
        }
    }
}

/// Accept a pending SYNCED persona share and provision its ongoing-sync
/// relationship in one call -- wraps persona_sharing::accept_persona_share
/// unmodified (same "thin wrapper that calls the underlying primitive
/// unmodified and IS the eventual save/accept hook" shape
/// group_sync::engine::create_and_push_document already established for
/// group.db) plus provision_sync_relationship above. Returns the new
/// persona's id.
///
/// ORDERING RISK, accepted rather than solved (same posture
/// accept_persona_share's own doc comment already takes toward its
/// personal.db/shared.db split): if provisioning fails after acceptance
/// already succeeded, the caller is left with a materialized-but-
/// unprovisioned persona -- inert until provision_sync_relationship is
/// retried directly against the now-known persona_id, not user-visible,
/// acceptable at this pre-release stage.
pub async fn accept_and_provision_sync(
    share_id: &str,
    recipient_user_id: &str,
    recipient_personal_key_hex: &str,
    sharing_private_key: &StaticSecret,
) -> Result<String, PersonaSyncError> {
    let persona_id = crate::auth::persona_sharing::accept_persona_share(
        share_id,
        recipient_user_id,
        recipient_personal_key_hex,
        sharing_private_key,
    )
    .await?;

    provision_sync_relationship(
        recipient_user_id,
        &persona_id,
        recipient_personal_key_hex,
        share_id,
    )
    .await?;

    Ok(persona_id)
}

/// This persona's provisioned source_registry row, if any. Ok(None) means
/// provisioning hasn't run for this persona -- a normal state for a persona
/// that was never a SYNCED-share recipient, or one materialized before
/// provisioning could run.
async fn find_source_registry_id(
    conn: &mut SqliteConnection,
) -> Result<Option<String>, PersonaSyncError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT id FROM source_registry WHERE source_type = ? LIMIT 1")
            .bind(PERSONA_SYNC_SOURCE_TYPE)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(row.map(|(id,)| id))
}

// ---------------------------------------------------------------------------
// Share discovery (shared.db)
// ---------------------------------------------------------------------------

/// (share_id, source_persona_id, recipient_user_id) for every accepted
/// SYNCED share owned by a persona `user_id` owns.
async fn list_outbound_shares(
    user_id: &str,
) -> Result<Vec<(String, String, String)>, PersonaSyncError> {
    let mut conn = open_shared_db().await?;
    let rows = sqlx::query(
        "SELECT pps.id, pps.source_persona_id, pps.recipient_user_id
         FROM pending_persona_shares pps
         JOIN user_personas up ON up.persona_id = pps.source_persona_id
         WHERE up.user_id = ? AND pps.status = 'accepted'",
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
        ));
    }
    Ok(out)
}

/// (share_id, materialized_persona_id) for every accepted SYNCED share
/// `recipient_user_id` has materialized. Excludes any row where
/// materialized_persona_id is still NULL -- accepted but not yet
/// provisioned is not expected to persist (accept_and_provision_sync does
/// both together), but a row in that transient state has nothing this sweep
/// could act on yet.
async fn list_inbound_shares(
    recipient_user_id: &str,
) -> Result<Vec<(String, String)>, PersonaSyncError> {
    let mut conn = open_shared_db().await?;
    let rows = sqlx::query(
        "SELECT id, materialized_persona_id FROM pending_persona_shares
         WHERE recipient_user_id = ? AND status = 'accepted'
         AND materialized_persona_id IS NOT NULL",
    )
    .bind(recipient_user_id)
    .fetch_all(&mut conn)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let persona_id: Option<String> = r.try_get("materialized_persona_id")?;
        if let Some(persona_id) = persona_id {
            out.push((r.try_get("id")?, persona_id));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

/// Sweep every accepted outbound share owned by `user_id`, pushing any whose
/// content has changed since the last push. Never propagates a per-share
/// failure -- logged, matching group_sync::engine's own fail-silent-retry-
/// next-cycle posture throughout.
async fn push_all_owned_shares(user_id: &str, personal_key_hex: &str) {
    let shares = match list_outbound_shares(user_id).await {
        Ok(s) => s,
        Err(e) => {
            log::warn!("persona_sync: could not list outbound shares for user={user_id}: {e}");
            return;
        }
    };

    for (share_id, owner_persona_id, recipient_user_id) in shares {
        push_one_share(
            user_id,
            &owner_persona_id,
            personal_key_hex,
            &share_id,
            &recipient_user_id,
        )
        .await;
    }
}

/// Push one share if its content has changed, recording the outcome either
/// way. Never returns Err to its caller -- see push_all_owned_shares.
async fn push_one_share(
    owner_user_id: &str,
    owner_persona_id: &str,
    owner_personal_key_hex: &str,
    share_id: &str,
    recipient_user_id: &str,
) {
    let result = push_if_changed(
        owner_user_id,
        owner_persona_id,
        owner_personal_key_hex,
        share_id,
        recipient_user_id,
    )
    .await;

    match result {
        Ok(_) => {}
        Err(e) => {
            log::warn!(
                "persona_sync: push failed for share={share_id} \
                 owner_persona={owner_persona_id}: {e}"
            );
            if let Err(e2) =
                settings_store::record_push_result(owner_persona_id, share_id, Err(&e.to_string()))
                    .await
            {
                log::warn!("persona_sync: could not record push failure: {e2}");
            }
        }
    }
}

/// Push `share_id`'s current content if it differs from the last push.
/// Returns Ok(true) if a write happened, Ok(false) if sync isn't configured
/// for this pair yet, or content is unchanged. Records the outcome on every
/// path that reaches a settings row (an unconfigured pair has none to
/// record against, matching group_sync's own "no row = nothing to record"
/// contract).
async fn push_if_changed(
    owner_user_id: &str,
    owner_persona_id: &str,
    owner_personal_key_hex: &str,
    share_id: &str,
    recipient_user_id: &str,
) -> Result<bool, PersonaSyncError> {
    let Some(settings) =
        settings_store::get_persona_share_sync_settings(owner_persona_id, share_id).await?
    else {
        // Sync not configured for this share on this install yet -- silent
        // no-op, matching group_sync::engine's own contract.
        return Ok(false);
    };

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

    let mut conn =
        personal_store::open_personal_db(owner_user_id, owner_persona_id, owner_personal_key_hex)
            .await?;
    let entity_facts =
        crate::auth::persona_sharing::load_shared_entity_facts(&mut conn, &active_entity_ids)
            .await?;
    let voice_profile_entries = crate::auth::persona_sharing::load_shared_voice_profile_entries(
        &mut conn,
        owner_persona_id,
    )
    .await?;
    drop(conn);

    let hash = content_hash(&active_entities, &entity_facts, &voice_profile_entries)?;

    if settings.last_content_hash.as_deref() == Some(hash.as_str()) {
        settings_store::record_push_result(owner_persona_id, share_id, Ok(None)).await?;
        return Ok(false);
    }

    let recipient_public_key = sharing_keypair::get_public_key(recipient_user_id)
        .await?
        .ok_or_else(|| {
            PersonaSyncError::Validation(format!(
                "Recipient user '{recipient_user_id}' has no registered sharing public key"
            ))
        })?;

    let payload = PersonaSyncUpdatePayload {
        schema_version: PERSONA_SYNC_UPDATE_PAYLOAD_SCHEMA_VERSION.to_owned(),
        emitted_at: crate::providers::utils::now(),
        entities: active_entities,
        entity_facts,
        voice_profile_entries,
    };
    let plaintext = serde_json::to_vec(&payload)?;
    let envelope = sharing_keypair::encrypt_to_public_key(&recipient_public_key, &plaintext)?;

    let path = sync_file_path(&settings.folder_path, share_id);
    write_envelope_atomic(&path, &envelope).await?;

    settings_store::record_push_result(owner_persona_id, share_id, Ok(Some(&hash))).await?;

    Ok(true)
}

// ---------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------

/// Sweep every accepted inbound share materialized for `recipient_user_id`,
/// pulling and applying any update newer than what's already been synced.
/// Never propagates a per-share failure -- see push_all_owned_shares.
async fn pull_all_accepted_shares(
    recipient_user_id: &str,
    recipient_personal_key_hex: &str,
    sharing_private_key: &StaticSecret,
) {
    let shares = match list_inbound_shares(recipient_user_id).await {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "persona_sync: could not list inbound shares for user={recipient_user_id}: {e}"
            );
            return;
        }
    };

    for (share_id, persona_id) in shares {
        let result = pull_if_newer(
            recipient_user_id,
            &persona_id,
            recipient_personal_key_hex,
            &share_id,
            sharing_private_key,
        )
        .await;

        if let Err(e) = result {
            log::warn!("persona_sync: pull failed for share={share_id} persona={persona_id}: {e}");
            if let Err(e2) =
                settings_store::record_pull_result(&persona_id, &share_id, Err(&e.to_string()))
                    .await
            {
                log::warn!("persona_sync: could not record pull failure: {e2}");
            }
        }
    }
}

/// Pull and apply `share_id`'s update if newer than what's already synced.
/// Returns Ok(true) if applied, Ok(false) if sync isn't configured, no
/// envelope is waiting yet, or it isn't newer than the last one applied.
async fn pull_if_newer(
    recipient_user_id: &str,
    persona_id: &str,
    recipient_personal_key_hex: &str,
    share_id: &str,
    sharing_private_key: &StaticSecret,
) -> Result<bool, PersonaSyncError> {
    let Some(settings) =
        settings_store::get_persona_share_sync_settings(persona_id, share_id).await?
    else {
        return Ok(false);
    };

    let path = sync_file_path(&settings.folder_path, share_id);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Nobody has pushed anything yet -- not a failure.
            return Ok(false);
        }
        Err(e) => return Err(e.into()),
    };

    let plaintext = sharing_keypair::decrypt_own_envelope(sharing_private_key, &bytes)?;
    let payload: PersonaSyncUpdatePayload = serde_json::from_slice(&plaintext)?;

    let is_newer = match &settings.last_synced_at {
        None => true,
        Some(last) => payload.emitted_at.as_str() > last.as_str(),
    };
    if !is_newer {
        return Ok(false);
    }

    apply_update(
        recipient_user_id,
        persona_id,
        recipient_personal_key_hex,
        &payload,
    )
    .await?;

    settings_store::record_pull_result(persona_id, share_id, Ok(&payload.emitted_at)).await?;

    Ok(true)
}

/// Apply one pulled snapshot to the recipient's personal.db, gated
/// per-entity through decisions.id=502's refresh_verdict framework. See this
/// module's own header (RECONCILIATION MODEL) for the overall approach.
async fn apply_update(
    recipient_user_id: &str,
    persona_id: &str,
    recipient_personal_key_hex: &str,
    payload: &PersonaSyncUpdatePayload,
) -> Result<(), PersonaSyncError> {
    let mut conn =
        personal_store::open_personal_db(recipient_user_id, persona_id, recipient_personal_key_hex)
            .await?;

    let source_id = find_source_registry_id(&mut conn).await?.ok_or_else(|| {
        PersonaSyncError::Validation(format!(
            "No provisioned source_registry row on persona '{persona_id}' -- \
             provision_sync_relationship must run before a pull can apply anything."
        ))
    })?;

    sqlx::query("SAVEPOINT persona_sync_apply_update")
        .execute(&mut conn)
        .await?;

    let step: Result<(), PersonaSyncError> = async {
        let incoming_ids: std::collections::HashSet<&str> =
            payload.entities.iter().map(|e| e.id.as_str()).collect();

        // Pass 1: insert entities new since the last sync, or apply an
        // AutoAccept content update to ones that already exist. parent_
        // entity_id is deliberately excluded here (forced NULL on insert,
        // untouched on update) -- see pass 2, same two-pass FK-ordering
        // reasoning accept_persona_share's own materialization already uses.
        for entity in &payload.entities {
            let exists = entity_store::get_entity_conn(&mut conn, &entity.id)
                .await?
                .is_some();
            if !exists {
                insert_synced_entity(&mut conn, entity, &source_id).await?;
                continue;
            }

            match source_registry_store::refresh_verdict_conn(&mut conn, &entity.id).await? {
                Some(RefreshVerdict::AutoAccept) => {
                    let update = EntityUpdate {
                        display_name: Some(entity.display_name.clone()),
                        aliases: Some(entity.aliases.clone()),
                        status: Some(entity.status.clone()),
                        extra_metadata: Some(entity.extra_metadata.clone()),
                        ..Default::default()
                    };
                    entity_store::update_entity_conn(&mut conn, &entity.id, &update).await?;
                }
                Some(RefreshVerdict::Conflict) => {
                    source_registry_store::set_source_status_conn(
                        &mut conn,
                        &source_id,
                        "pending_refresh",
                    )
                    .await?;
                }
                Some(RefreshVerdict::Ignore) | None => {}
            }
        }

        // Pass 2: parent_entity_id, uniformly for new and just-updated
        // entities alike -- re-checking the verdict here (cheap; this sweep
        // runs at most every few minutes on small, personal-data-scale
        // payloads) means a Conflict/Ignore entity's hierarchy is left
        // alone exactly like its other fields were in pass 1, and a
        // freshly-inserted entity (always pristine, always AutoAccept)
        // always gets linked.
        for entity in &payload.entities {
            let Some(parent_id) = entity.parent_entity_id.as_deref() else {
                continue;
            };
            if !incoming_ids.contains(parent_id) {
                continue;
            }
            if source_registry_store::refresh_verdict_conn(&mut conn, &entity.id).await?
                != Some(RefreshVerdict::AutoAccept)
            {
                continue;
            }
            sqlx::query("UPDATE entities SET parent_entity_id = ? WHERE id = ?")
                .bind(parent_id)
                .bind(&entity.id)
                .execute(&mut conn)
                .await?;
        }

        // Entities that trace to this share's source but fell out of the
        // incoming snapshot (owner archived/deleted them, or they no
        // longer pass send_persona_share's own active-only filter) --
        // reuse mark_records_deleted_in_source_conn as-is, already built
        // and already source-scoped so this source can never tombstone
        // another source's records.
        let currently_synced: Vec<String> = {
            let rows = sqlx::query(
                "SELECT id FROM entities WHERE source_registry_id = ? AND status = 'active'",
            )
            .bind(&source_id)
            .fetch_all(&mut conn)
            .await?;
            let mut ids = Vec::with_capacity(rows.len());
            for r in &rows {
                ids.push(r.try_get::<String, _>("id")?);
            }
            ids
        };
        let missing: Vec<String> = currently_synced
            .iter()
            .filter(|id| !incoming_ids.contains(id.as_str()))
            .cloned()
            .collect();
        source_registry_store::mark_records_deleted_in_source_conn(&mut conn, &source_id, &missing)
            .await?;

        // entity_facts: matched per (entity_id, field_name) by comparing the
        // incoming currently-active fact's own id against the recipient's
        // own active fact id for that field -- same id means already
        // synced, nothing to do. Gated by the *owning entity's* verdict
        // (facts carry no modification_state of their own); a singleton
        // fact (entity_id None) has no owning entity to gate on and always
        // applies, matching materialization's own unconditional handling of
        // singleton facts.
        for fact in &payload.entity_facts {
            let local_active_id: Option<String> = sqlx::query_scalar(
                "SELECT id FROM entity_facts WHERE field_name = ? AND valid_until IS NULL
                 AND (entity_id = ? OR (entity_id IS NULL AND ? IS NULL))",
            )
            .bind(&fact.field_name)
            .bind(&fact.entity_id)
            .bind(&fact.entity_id)
            .fetch_optional(&mut conn)
            .await?;

            if local_active_id.as_deref() == Some(fact.id.as_str()) {
                continue;
            }

            let should_apply = match &fact.entity_id {
                None => true,
                Some(eid) => {
                    match source_registry_store::refresh_verdict_conn(&mut conn, eid).await? {
                        Some(RefreshVerdict::AutoAccept) => true,
                        Some(RefreshVerdict::Conflict) => {
                            source_registry_store::set_source_status_conn(
                                &mut conn,
                                &source_id,
                                "pending_refresh",
                            )
                            .await?;
                            false
                        }
                        Some(RefreshVerdict::Ignore) | None => false,
                    }
                }
            };
            if !should_apply {
                continue;
            }

            apply_synced_fact(&mut conn, persona_id, fact).await?;
        }

        // entity_facts deletion-by-absence (items.id=307): the loop above is
        // push-driven (iterates payload.entity_facts), so it can insert or
        // supersede a fact but has no way to notice a field_name that used
        // to have an active fact and no longer appears in the incoming
        // snapshot at all -- the owner clearing a field entirely produces no
        // entry to react to. This is a second, comparison-in-the-other-
        // direction pass, symmetric to the entity-level missing/
        // mark_records_deleted_in_source_conn pass above. Every push is a
        // full snapshot, not a diff (push_if_changed sends every currently-
        // active entity/fact, every time), so absence here is unambiguous --
        // there's no partial-push explanation to rule out first.
        //
        // Entity-scoped fields: reuse currently_synced, narrowed to the
        // entities still active after the entity-deletion pass above (an
        // entity that itself just went deleted_in_source doesn't also need
        // its individual fields retired -- out of scope here, matching this
        // item's own framing of "field cleared, entity still present").
        // Gated by the same per-entity refresh_verdict_conn the value-change
        // loop above uses, for the same reason: mark_record_user_modified_conn
        // is entity-granular, not field-granular, so a Conflict entity must
        // hold back a missing field exactly like it holds back a changed
        // value -- one entity, one verdict, one hold-for-review state, not a
        // second conflict model.
        let incoming_entity_fields: std::collections::HashSet<(&str, &str)> = payload
            .entity_facts
            .iter()
            .filter_map(|f| {
                f.entity_id
                    .as_deref()
                    .map(|eid| (eid, f.field_name.as_str()))
            })
            .collect();

        let now = crate::providers::utils::now();

        for entity_id in currently_synced
            .iter()
            .filter(|id| incoming_ids.contains(id.as_str()))
        {
            let local_active_fields: Vec<String> = {
                let rows = sqlx::query(
                    "SELECT field_name FROM entity_facts
                     WHERE entity_id = ? AND valid_until IS NULL",
                )
                .bind(entity_id)
                .fetch_all(&mut conn)
                .await?;
                let mut names = Vec::with_capacity(rows.len());
                for r in &rows {
                    names.push(r.try_get::<String, _>("field_name")?);
                }
                names
            };

            let missing_fields: Vec<String> = local_active_fields
                .into_iter()
                .filter(|fname| {
                    !incoming_entity_fields.contains(&(entity_id.as_str(), fname.as_str()))
                })
                .collect();

            if missing_fields.is_empty() {
                continue;
            }

            match source_registry_store::refresh_verdict_conn(&mut conn, entity_id).await? {
                Some(RefreshVerdict::AutoAccept) => {
                    for field_name in &missing_fields {
                        sqlx::query(
                            "UPDATE entity_facts SET valid_until = ?
                             WHERE entity_id = ? AND field_name = ? AND valid_until IS NULL",
                        )
                        .bind(&now)
                        .bind(entity_id)
                        .bind(field_name)
                        .execute(&mut conn)
                        .await?;
                    }
                }
                Some(RefreshVerdict::Conflict) => {
                    source_registry_store::set_source_status_conn(
                        &mut conn,
                        &source_id,
                        "pending_refresh",
                    )
                    .await?;
                }
                Some(RefreshVerdict::Ignore) | None => {}
            }
        }

        // Singleton fields (entity_id NULL): no owning entity to gate on,
        // so scope by provenance instead -- only a singleton fact this sync
        // relationship itself previously wrote (source = 'synced_share', the
        // tag both accept_persona_share's materialization and
        // apply_synced_fact above use) is eligible for absence-retirement.
        // Unscoped, this would be wrong: the recipient may have their own,
        // unrelated local singleton fact that happens to share a field_name
        // the owner's persona never had -- absence from the owner's payload
        // says nothing about a field the owner never sent. Applied
        // unconditionally (no verdict check), matching apply_synced_fact's
        // own existing unconditional handling of singleton value-changes --
        // there is no local-edit guard for singletons anywhere in this
        // framework today (create_entity_fact_with_provenance skips it when
        // entity_id is None), so this doesn't invent one just for deletion.
        let incoming_singleton_fields: std::collections::HashSet<&str> = payload
            .entity_facts
            .iter()
            .filter(|f| f.entity_id.is_none())
            .map(|f| f.field_name.as_str())
            .collect();

        let local_synced_singletons: Vec<String> = {
            let rows = sqlx::query(
                "SELECT field_name FROM entity_facts
                 WHERE entity_id IS NULL AND valid_until IS NULL AND source = 'synced_share'",
            )
            .fetch_all(&mut conn)
            .await?;
            let mut names = Vec::with_capacity(rows.len());
            for r in &rows {
                names.push(r.try_get::<String, _>("field_name")?);
            }
            names
        };

        for field_name in local_synced_singletons {
            if incoming_singleton_fields.contains(field_name.as_str()) {
                continue;
            }
            sqlx::query(
                "UPDATE entity_facts SET valid_until = ?
                 WHERE entity_id IS NULL AND field_name = ? AND valid_until IS NULL",
            )
            .bind(&now)
            .bind(&field_name)
            .execute(&mut conn)
            .await?;
        }

        // voice_profile_entries (items.id=306): gated per-row through
        // voice_profile_refresh_verdict_conn, same shape as entities pass 1
        // above -- a row new since the last sync inserts directly as
        // 'pristine' (mirrors insert_synced_entity's own entities INSERT,
        // skipping 'user_created' entirely since this write genuinely is a
        // sync product, not a QR-native one); an existing row is gated by
        // its own verdict. This closes the gap this module's own header
        // (SCOPE) previously flagged: "voice_profiles has no
        // modification_state column at all".
        for entry in &payload.voice_profile_entries {
            let extra_metadata_json = entry.extra_metadata.to_string();
            let verdict =
                source_registry_store::voice_profile_refresh_verdict_conn(&mut conn, &entry.id)
                    .await?;

            match verdict {
                None => {
                    sqlx::query(
                        "INSERT INTO voice_profiles
                         (id, persona_id, source_id, precedence, attribute, value,
                          created_at, updated_at, extra_metadata, modification_state)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pristine')",
                    )
                    .bind(&entry.id)
                    .bind(persona_id)
                    .bind(&entry.source_id)
                    .bind(entry.precedence)
                    .bind(&entry.attribute)
                    .bind(&entry.value)
                    .bind(&entry.created_at)
                    .bind(&entry.updated_at)
                    .bind(&extra_metadata_json)
                    .execute(&mut conn)
                    .await?;
                }
                Some(RefreshVerdict::AutoAccept) => {
                    sqlx::query(
                        "UPDATE voice_profiles SET
                            precedence = ?, attribute = ?, value = ?,
                            updated_at = ?, extra_metadata = ?,
                            modification_state = 'pristine'
                         WHERE id = ?",
                    )
                    .bind(entry.precedence)
                    .bind(&entry.attribute)
                    .bind(&entry.value)
                    .bind(&entry.updated_at)
                    .bind(&extra_metadata_json)
                    .bind(&entry.id)
                    .execute(&mut conn)
                    .await?;
                }
                Some(RefreshVerdict::Conflict) => {
                    source_registry_store::set_source_status_conn(
                        &mut conn,
                        &source_id,
                        "pending_refresh",
                    )
                    .await?;
                }
                Some(RefreshVerdict::Ignore) => {}
            }
        }

        // voice_profiles deletion-by-absence (items.id=309): mirrors the
        // entity_facts absence passes above, but voice_profiles has no
        // owning entity to delegate a verdict through (see
        // voice_profile_refresh_verdict_conn's own header) and no
        // source_registry_id column to scope by -- persona_id is
        // sufficient because accept_persona_share always mints a fresh
        // persona per share (persona_sharing.rs), so persona_id is already
        // 1:1 with this sync relationship. modification_state !=
        // 'user_created' at the SQL level keeps the recipient's own
        // local-only rows out of the candidate set entirely, on top of the
        // per-row verdict gate below.
        let incoming_voice_profile_ids: std::collections::HashSet<&str> = payload
            .voice_profile_entries
            .iter()
            .map(|e| e.id.as_str())
            .collect();

        let local_synced_voice_profile_ids: Vec<String> = {
            let rows = sqlx::query(
                "SELECT id FROM voice_profiles
                 WHERE persona_id = ? AND modification_state != 'user_created'",
            )
            .bind(persona_id)
            .fetch_all(&mut conn)
            .await?;
            let mut ids = Vec::with_capacity(rows.len());
            for r in &rows {
                ids.push(r.try_get::<String, _>("id")?);
            }
            ids
        };

        for vp_id in local_synced_voice_profile_ids {
            if incoming_voice_profile_ids.contains(vp_id.as_str()) {
                continue;
            }

            match source_registry_store::voice_profile_refresh_verdict_conn(&mut conn, &vp_id)
                .await?
            {
                Some(RefreshVerdict::AutoAccept) => {
                    sqlx::query("DELETE FROM voice_profiles WHERE id = ?")
                        .bind(&vp_id)
                        .execute(&mut conn)
                        .await?;
                }
                Some(RefreshVerdict::Conflict) => {
                    source_registry_store::set_source_status_conn(
                        &mut conn,
                        &source_id,
                        "pending_refresh",
                    )
                    .await?;
                }
                Some(RefreshVerdict::Ignore) | None => {}
            }
        }

        Ok(())
    }
    .await;

    match step {
        Ok(()) => {
            sqlx::query("RELEASE persona_sync_apply_update")
                .execute(&mut conn)
                .await?;
            Ok(())
        }
        Err(e) => {
            if let Err(rollback_err) = sqlx::query("ROLLBACK TO persona_sync_apply_update")
                .execute(&mut conn)
                .await
            {
                log::error!(
                    "Savepoint rollback failed in persona_sync apply_update: {rollback_err}"
                );
            }
            let _ = sqlx::query("RELEASE persona_sync_apply_update")
                .execute(&mut conn)
                .await;
            Err(e)
        }
    }
}

/// Insert an entity new since the last sync -- pristine and linked to this
/// share's source_registry row from day one (so its own next update is
/// eligible for AutoAccept, not stuck user_created the way a raw manual
/// create would leave it). parent_entity_id forced NULL -- see apply_
/// update's pass 2 for why.
async fn insert_synced_entity(
    conn: &mut SqliteConnection,
    entity: &Entity,
    source_id: &str,
) -> Result<(), PersonaSyncError> {
    let aliases_json = serde_json::to_string(&entity.aliases)?;
    let extra_metadata_json = entity.extra_metadata.to_string();
    sqlx::query(
        "INSERT INTO entities
         (id, entity_type, display_name, aliases, parent_entity_id, status,
          modification_state, source_registry_id, source_url, created_at,
          extra_metadata, redact_identification, hide_from_shared_surfaces)
         VALUES (?, ?, ?, ?, NULL, ?, 'pristine', ?, ?, ?, ?, ?, ?)",
    )
    .bind(&entity.id)
    .bind(&entity.entity_type)
    .bind(&entity.display_name)
    .bind(&aliases_json)
    .bind(&entity.status)
    .bind(source_id)
    .bind(&entity.source_url)
    .bind(&entity.created_at)
    .bind(&extra_metadata_json)
    .bind(entity.redact_identification)
    .bind(entity.hide_from_shared_surfaces)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Supersede-then-insert one incoming fact, carrying through
/// abstraction_tier2/abstraction_tier3/valid_from exactly as the owner set
/// them. Deliberately raw SQL, not personal_store::create_entity_fact_with_
/// provenance_conn: that helper always resets the abstraction tiers and
/// valid_from to their schema defaults (it has no parameters for them, by
/// design, for its own fresh-write callers) -- silently dropping whatever
/// privacy-relevant tier the owner actually set would be a real regression.
/// Mirrors accept_persona_share's own raw INSERT column set exactly, for
/// the same reason.
async fn apply_synced_fact(
    conn: &mut SqliteConnection,
    recipient_persona_id: &str,
    fact: &SharedEntityFact,
) -> Result<(), PersonaSyncError> {
    let now = crate::providers::utils::now();
    sqlx::query(
        "UPDATE entity_facts SET valid_until = ?
         WHERE field_name = ? AND valid_until IS NULL
         AND (entity_id = ? OR (entity_id IS NULL AND ? IS NULL))",
    )
    .bind(&now)
    .bind(&fact.field_name)
    .bind(&fact.entity_id)
    .bind(&fact.entity_id)
    .execute(&mut *conn)
    .await?;

    let extra_metadata_json = fact.extra_metadata.to_string();
    sqlx::query(
        "INSERT INTO entity_facts
         (id, entity_id, field_name, field_value, sensitivity,
          abstraction_tier2, abstraction_tier3, source, valid_from,
          created_at, extra_metadata, source_persona_id,
          cross_persona_export, origin_persona_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'synced_share', ?, ?, ?, ?, 0, NULL)",
    )
    .bind(&fact.id)
    .bind(&fact.entity_id)
    .bind(&fact.field_name)
    .bind(&fact.field_value)
    .bind(&fact.sensitivity)
    .bind(&fact.abstraction_tier2)
    .bind(&fact.abstraction_tier3)
    .bind(&fact.valid_from)
    .bind(crate::providers::utils::now())
    .bind(&extra_metadata_json)
    .bind(recipient_persona_id)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Session-level entry points
// ---------------------------------------------------------------------------

/// Push every owned share, then pull every accepted share, for whichever
/// account is currently resident in `key_registry`. A no-op if nobody is
/// logged in. Called from main.rs's existing periodic timer, alongside
/// group_sync's own pull sweep -- same interval, no second timer.
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

    push_all_owned_shares(&user_id, &personal_key_hex).await;
    pull_all_accepted_shares(&user_id, &personal_key_hex, &sharing_private_key).await;
}

/// Pull every accepted inbound share once, immediately. Called right after
/// a sharing private key becomes resident (commands/auth.rs::finish_login)
/// -- the "app-start" half of the pull cadence, same reasoning
/// auth::group_invitations::accept_invitation's own immediate pull call
/// gives: the registry starts empty at process boot, so this is the moment
/// that actually matters; main.rs's periodic timer covers the steady state.
pub async fn pull_all_accepted_shares_on_login(
    user_id: &str,
    personal_key_hex: &str,
    sharing_private_key: &StaticSecret,
) {
    pull_all_accepted_shares(user_id, personal_key_hex, sharing_private_key).await;
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
    use crate::persistence::persona_store;
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

    /// Mirrors persona_sharing.rs's own test fixture convention.
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

    async fn entity_row(user_id: &str, persona_id: &str, key_hex: &str, entity_id: &str) -> Entity {
        entity_store::get_entity(user_id, persona_id, key_hex, entity_id)
            .await
            .unwrap()
            .expect("entity must exist")
    }

    #[tokio::test]
    async fn provisioning_flips_materialized_entities_to_pristine_and_links_source() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x10).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x20).await;

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
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .expect("accept_and_provision_sync must succeed");

        let mut conn = personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
            .await
            .unwrap();
        let source_id = find_source_registry_id(&mut conn)
            .await
            .unwrap()
            .expect("provisioning must create a source_registry row");

        let source = source_registry_store::get_source_conn(&mut conn, &source_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source.source_type, PERSONA_SYNC_SOURCE_TYPE);
        assert_eq!(source.focus_slug, PERSONA_SYNC_FOCUS_SLUG);

        let (entity_id,): (String,) = sqlx::query_as("SELECT id FROM entities LIMIT 1")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        drop(conn);

        let entity = entity_row(&recipient_id, &persona_id, &recipient_key, &entity_id).await;
        assert_eq!(entity.modification_state, "pristine");
        assert_eq!(entity.source_registry_id, Some(source_id));
    }

    #[tokio::test]
    async fn provisioning_is_safe_to_call_twice_without_corrupting_entity_state() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x16).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x26).await;

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
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .unwrap();

        let mut conn = personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
            .await
            .unwrap();
        let first_source_id = find_source_registry_id(&mut conn).await.unwrap().unwrap();
        let (entity_id,): (String,) = sqlx::query_as("SELECT id FROM entities LIMIT 1")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        drop(conn);

        // A retried/duplicated provisioning call (e.g. a retried accept
        // flow) must not re-link or otherwise disturb an entity that's
        // already provisioned -- see provision_sync_relationship's own doc
        // comment (IDEMPOTENCY) on why this is scoped to entity-state
        // safety specifically, not to preventing a second source_registry
        // row.
        provision_sync_relationship(&recipient_id, &persona_id, &recipient_key, &share_id)
            .await
            .expect("a repeat provisioning call must not error");

        let entity = entity_row(&recipient_id, &persona_id, &recipient_key, &entity_id).await;
        assert_eq!(
            entity.source_registry_id,
            Some(first_source_id),
            "a repeat call must not re-link an already-provisioned entity to a new source row"
        );
        assert_eq!(
            entity.modification_state, "pristine",
            "a repeat call must not disturb the entity's already-correct state"
        );
    }

    #[tokio::test]
    async fn a_push_that_writes_leaves_the_final_file_with_no_tmp_residue() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x17).await;
        let (recipient_id, _, _, recipient_priv) = make_user_with_persona("Bob", 0x27).await;

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
            ShareType::Synced,
        )
        .await
        .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();

        let pushed = push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .expect("push_if_changed must succeed");
        assert!(
            pushed,
            "content changed since the last push (there was none) -- must write"
        );

        let final_path = sync_file_path(folder_path, &share_id);
        assert!(
            final_path.exists(),
            "the final envelope file must exist after a push that wrote"
        );

        let mut tmp = final_path.as_os_str().to_owned();
        tmp.push(".tmp");
        assert!(
            !std::path::PathBuf::from(tmp).exists(),
            "write_envelope_atomic's temp file must be renamed away, never left behind"
        );

        // Not just "a file exists" -- confirm it's the real payload: decrypts
        // with the recipient's own key and carries the entity just created.
        let bytes = tokio::fs::read(&final_path).await.unwrap();
        let plaintext = sharing_keypair::decrypt_own_envelope(&recipient_priv, &bytes)
            .expect("the written file must decrypt with the recipient's own sharing key");
        let payload: PersonaSyncUpdatePayload = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(payload.entities.len(), 1);
        assert_eq!(payload.entities[0].display_name, "Contact");

        // Settings must reflect the write actually happened, not a skip.
        let settings = settings_store::get_persona_share_sync_settings(&owner_persona, &share_id)
            .await
            .unwrap()
            .unwrap();
        assert!(settings.last_pushed_at.is_some());
        assert!(settings.last_content_hash.is_some());
    }

    #[tokio::test]
    async fn push_is_a_silent_noop_when_folder_is_unset() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x11).await;
        let (recipient_id, _, _, _) = make_user_with_persona("Bob", 0x21).await;

        let applied = push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            "share-not-configured",
            &recipient_id,
        )
        .await
        .expect("push_if_changed must succeed with no folder configured");
        assert!(!applied);
    }

    #[tokio::test]
    async fn push_then_pull_round_trips_a_new_entity_and_fact() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x12).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x22).await;

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
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_share_sync_folder(
            &persona_id,
            &share_id,
            settings_store::SyncRole::Recipient,
            folder_path,
        )
        .await
        .unwrap();

        // Update the fact on the owner's side after the share was sent --
        // exercises that the ongoing channel carries a change the one-shot
        // grant never saw, not just a replay of the original payload.
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

        let pushed = push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .expect("push_if_changed must succeed");
        assert!(pushed);

        let applied = pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .expect("pull_if_newer must succeed");
        assert!(applied);

        let mut conn = personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
            .await
            .unwrap();
        let value: String = sqlx::query_scalar(
            "SELECT field_value FROM entity_facts
             WHERE entity_id = ? AND field_name = 'phone' AND valid_until IS NULL",
        )
        .bind(&entity_id)
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(
            value, "555-9999",
            "the post-share fact update must have applied"
        );
    }

    #[tokio::test]
    async fn push_then_pull_materializes_a_new_parent_child_hierarchy_regardless_of_order() {
        // Regression test for items.id=302's two-pass insert, applied here
        // to the ongoing-sync path (apply_update / insert_synced_entity,
        // items.id=303) rather than the initial accept. list_entities
        // orders payload.entities by display_name, so naming the child
        // before the parent alphabetically forces the child to appear
        // first in the pull payload -- exactly the ordering a naive
        // single-pass insert would violate entities.parent_entity_id's FK
        // on.
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x14).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x24).await;

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
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_share_sync_folder(
            &persona_id,
            &share_id,
            settings_store::SyncRole::Recipient,
            folder_path,
        )
        .await
        .unwrap();

        // New parent/child pair, created after the initial accept, so this
        // hierarchy is only ever seen via apply_update's pull path.
        let parent_id = entity_store::create_entity(
            &owner_id,
            &owner_persona,
            &owner_key,
            "person",
            "Zed Parent",
            &[],
            None,
            None,
        )
        .await
        .unwrap();
        let child_id = entity_store::create_entity(
            &owner_id,
            &owner_persona,
            &owner_key,
            "person",
            "Aaron Child",
            &[],
            Some(&parent_id),
            None,
        )
        .await
        .unwrap();

        let pushed = push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .expect("push_if_changed must succeed");
        assert!(pushed);

        let applied = pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .expect("pull_if_newer must succeed despite child-before-parent payload order");
        assert!(applied);

        let mut conn = personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
            .await
            .unwrap();
        let linked_parent_id: Option<String> =
            sqlx::query_scalar("SELECT parent_entity_id FROM entities WHERE id = ?")
                .bind(&child_id)
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(linked_parent_id.as_deref(), Some(parent_id.as_str()));
        let parent_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities WHERE id = ?")
            .bind(&parent_id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(parent_count, 1);
    }

    #[tokio::test]
    async fn a_repeat_push_with_unchanged_content_is_skipped() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x13).await;
        let (recipient_id, _, _, _) = make_user_with_persona("Bob", 0x23).await;

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
            ShareType::Synced,
        )
        .await
        .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            shared_folder.path().to_str().unwrap(),
        )
        .await
        .unwrap();

        let first = push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .unwrap();
        assert!(first, "the first push must write, nothing pushed yet");

        let second = push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .unwrap();
        assert!(
            !second,
            "nothing changed since the last push -- must be skipped"
        );
    }

    #[tokio::test]
    async fn a_local_edit_after_pull_blocks_a_later_conflicting_update() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x14).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x24).await;

        let entity_id = entity_store::create_entity(
            &owner_id,
            &owner_persona,
            &owner_key,
            "person",
            "Original Name",
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
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_share_sync_folder(
            &persona_id,
            &share_id,
            settings_store::SyncRole::Recipient,
            folder_path,
        )
        .await
        .unwrap();

        // The recipient edits the entity locally -- this is exactly the
        // guard items.id=303 wires into entity_store::update_entity
        // (Q2 in the design plan).
        entity_store::update_entity(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &entity_id,
            &EntityUpdate {
                display_name: Some("Recipient's Own Name".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // The owner changes the same entity and pushes again.
        entity_store::update_entity(
            &owner_id,
            &owner_persona,
            &owner_key,
            &entity_id,
            &EntityUpdate {
                display_name: Some("Owner's New Name".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .unwrap();

        let applied = pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .expect("pull_if_newer must succeed even when every change conflicts");
        assert!(
            applied,
            "the sweep ran and recorded a newer snapshot, even though nothing was applied"
        );

        let entity = entity_row(&recipient_id, &persona_id, &recipient_key, &entity_id).await;
        assert_eq!(
            entity.display_name, "Recipient's Own Name",
            "a local edit must never be silently overwritten by a later sync"
        );
        assert_eq!(entity.modification_state, "user_modified");

        let mut conn = personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
            .await
            .unwrap();
        let source_id = find_source_registry_id(&mut conn).await.unwrap().unwrap();
        let source = source_registry_store::get_source_conn(&mut conn, &source_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            source.status, "pending_refresh",
            "a conflicting update must flag the source for attention"
        );
    }

    #[tokio::test]
    async fn a_local_voice_profile_edit_after_pull_blocks_a_later_conflicting_update() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x16).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x26).await;

        // precedence=4 (persona-scoped) matches load_shared_voice_profile_
        // entries' own filter (persona_id = source_persona_id) -- a global
        // (precedence=3, persona_id NULL) entry is excluded from sharing.
        let vp_id = personal_store::save_voice_profile_entry(
            &owner_id,
            &owner_persona,
            &owner_key,
            "formality",
            "casual",
            4,
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
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_share_sync_folder(
            &persona_id,
            &share_id,
            settings_store::SyncRole::Recipient,
            folder_path,
        )
        .await
        .unwrap();

        // The recipient edits the voice profile locally -- the same guard
        // items.id=306 wires into personal_store::save_voice_profile_entry.
        personal_store::save_voice_profile_entry(
            &recipient_id,
            &persona_id,
            &recipient_key,
            "formality",
            "recipient's own value",
            4,
            None,
            None,
        )
        .await
        .unwrap();

        // The owner changes the same voice profile and pushes again.
        personal_store::save_voice_profile_entry(
            &owner_id,
            &owner_persona,
            &owner_key,
            "formality",
            "owner's new value",
            4,
            None,
            None,
        )
        .await
        .unwrap();
        push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .unwrap();

        let applied = pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .expect("pull_if_newer must succeed even when every change conflicts");
        assert!(
            applied,
            "the sweep ran and recorded a newer snapshot, even though nothing was applied"
        );

        let mut conn = personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
            .await
            .unwrap();
        let value: String = sqlx::query_scalar("SELECT value FROM voice_profiles WHERE id = ?")
            .bind(&vp_id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(
            value, "recipient's own value",
            "a local voice profile edit must never be silently overwritten by a later sync"
        );
        let modification_state: String =
            sqlx::query_scalar("SELECT modification_state FROM voice_profiles WHERE id = ?")
                .bind(&vp_id)
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(modification_state, "user_modified");

        let source_id = find_source_registry_id(&mut conn).await.unwrap().unwrap();
        let source = source_registry_store::get_source_conn(&mut conn, &source_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            source.status, "pending_refresh",
            "a conflicting voice profile update must flag the source for attention"
        );
    }

    #[tokio::test]
    async fn an_entity_missing_from_a_later_push_is_marked_deleted_in_source() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x15).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x25).await;

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

        let share_id = send_persona_share(
            &owner_id,
            &owner_persona,
            &owner_key,
            &recipient_id,
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_share_sync_folder(
            &persona_id,
            &share_id,
            settings_store::SyncRole::Recipient,
            folder_path,
        )
        .await
        .unwrap();

        // Owner archives the entity -- it drops out of send/push's own
        // status='active' filter entirely.
        entity_store::update_entity(
            &owner_id,
            &owner_persona,
            &owner_key,
            &entity_id,
            &EntityUpdate {
                status: Some("archived".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .unwrap();
        pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .unwrap();

        let entity = entity_row(&recipient_id, &persona_id, &recipient_key, &entity_id).await;
        assert_eq!(entity.status, "deleted_in_source");
    }

    #[tokio::test]
    async fn a_field_missing_from_a_later_push_is_retired_on_the_recipient() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x16).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x26).await;

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
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_share_sync_folder(
            &persona_id,
            &share_id,
            settings_store::SyncRole::Recipient,
            folder_path,
        )
        .await
        .unwrap();

        // Owner clears the field entirely -- no dedicated "clear" API exists
        // yet, so simulate it the same way an owner-side entity archive is
        // simulated in the deletion-by-absence test above: supersede with no
        // reinsert, directly on the owner's own connection.
        {
            let mut owner_conn =
                personal_store::open_personal_db(&owner_id, &owner_persona, &owner_key)
                    .await
                    .unwrap();
            sqlx::query(
                "UPDATE entity_facts SET valid_until = ?
                 WHERE entity_id = ? AND field_name = 'phone' AND valid_until IS NULL",
            )
            .bind(crate::providers::utils::now())
            .bind(&entity_id)
            .execute(&mut owner_conn)
            .await
            .unwrap();
        }

        let pushed = push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .expect("push_if_changed must succeed");
        assert!(pushed);

        let applied = pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .expect("pull_if_newer must succeed");
        assert!(applied);

        let mut conn = personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
            .await
            .unwrap();
        let still_active: Option<String> = sqlx::query_scalar(
            "SELECT id FROM entity_facts
             WHERE entity_id = ? AND field_name = 'phone' AND valid_until IS NULL",
        )
        .bind(&entity_id)
        .fetch_optional(&mut conn)
        .await
        .unwrap();
        assert!(
            still_active.is_none(),
            "a field the owner cleared entirely must be retired on the recipient, not linger"
        );

        // The entity itself is untouched -- this is a field-level gap, not
        // an entity-level one.
        let entity = entity_row(&recipient_id, &persona_id, &recipient_key, &entity_id).await;
        assert_eq!(entity.status, "active");
    }

    #[tokio::test]
    async fn a_missing_singleton_field_is_retired_when_source_matches_synced_share() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x17).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x27).await;

        personal_store::save_personal_field(
            &owner_id,
            &owner_persona,
            &owner_key,
            "email",
            "alice@example.com",
            "personal",
            "user",
            "self",
            "pass",
            "pass",
            "interview",
            None,
        )
        .await
        .unwrap();

        let share_id = send_persona_share(
            &owner_id,
            &owner_persona,
            &owner_key,
            &recipient_id,
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_share_sync_folder(
            &persona_id,
            &share_id,
            settings_store::SyncRole::Recipient,
            folder_path,
        )
        .await
        .unwrap();

        // The recipient's own, unrelated local singleton fact -- same
        // field_name as something the owner will later clear, but never
        // synced (source != 'synced_share'). Must survive untouched: proves
        // the source-scoping, not just the happy path.
        personal_store::save_personal_field(
            &recipient_id,
            &persona_id,
            &recipient_key,
            "local_only_marker",
            "recipient's own value",
            "personal",
            "user",
            "self",
            "pass",
            "pass",
            "interview",
            None,
        )
        .await
        .unwrap();

        // First pull: materializes the owner's "email" singleton on the
        // recipient's side as source = 'synced_share'.
        push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .unwrap();
        pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .unwrap();

        {
            let mut conn =
                personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
                    .await
                    .unwrap();
            let synced_value: String = sqlx::query_scalar(
                "SELECT field_value FROM entity_facts
                 WHERE entity_id IS NULL AND field_name = 'email' AND valid_until IS NULL",
            )
            .fetch_one(&mut conn)
            .await
            .unwrap();
            assert_eq!(synced_value, "alice@example.com");
        }

        // Owner clears the singleton field entirely and pushes again.
        {
            let mut owner_conn =
                personal_store::open_personal_db(&owner_id, &owner_persona, &owner_key)
                    .await
                    .unwrap();
            sqlx::query(
                "UPDATE entity_facts SET valid_until = ?
                 WHERE entity_id IS NULL AND field_name = 'email' AND valid_until IS NULL",
            )
            .bind(crate::providers::utils::now())
            .execute(&mut owner_conn)
            .await
            .unwrap();
        }

        let pushed = push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .expect("push_if_changed must succeed");
        assert!(pushed);

        let applied = pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .expect("pull_if_newer must succeed");
        assert!(applied);

        let mut conn = personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
            .await
            .unwrap();
        let still_active: Option<String> = sqlx::query_scalar(
            "SELECT id FROM entity_facts
             WHERE entity_id IS NULL AND field_name = 'email' AND valid_until IS NULL",
        )
        .fetch_optional(&mut conn)
        .await
        .unwrap();
        assert!(
            still_active.is_none(),
            "a synced singleton field the owner cleared entirely must be retired"
        );

        let unrelated_still_active: Option<String> = sqlx::query_scalar(
            "SELECT id FROM entity_facts
             WHERE entity_id IS NULL AND field_name = 'local_only_marker'
             AND valid_until IS NULL",
        )
        .fetch_optional(&mut conn)
        .await
        .unwrap();
        assert!(
            unrelated_still_active.is_some(),
            "a local singleton fact this sync relationship never wrote must never be touched"
        );
    }

    #[tokio::test]
    async fn a_local_edit_blocks_field_absence_retirement_too() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x18).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x28).await;

        let entity_id = entity_store::create_entity(
            &owner_id,
            &owner_persona,
            &owner_key,
            "person",
            "Original Name",
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
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_share_sync_folder(
            &persona_id,
            &share_id,
            settings_store::SyncRole::Recipient,
            folder_path,
        )
        .await
        .unwrap();

        // The recipient edits the entity locally (a different field/
        // attribute than the one the owner is about to clear) -- same
        // entity-granular guard as a_local_edit_after_pull_blocks_a_later_
        // conflicting_update above (Q2 in the design plan): any local edit
        // flips the whole entity to user_modified, not just the field that
        // was touched.
        entity_store::update_entity(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &entity_id,
            &EntityUpdate {
                display_name: Some("Recipient's Own Name".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // The owner clears the (unrelated) phone field and pushes.
        {
            let mut owner_conn =
                personal_store::open_personal_db(&owner_id, &owner_persona, &owner_key)
                    .await
                    .unwrap();
            sqlx::query(
                "UPDATE entity_facts SET valid_until = ?
                 WHERE entity_id = ? AND field_name = 'phone' AND valid_until IS NULL",
            )
            .bind(crate::providers::utils::now())
            .bind(&entity_id)
            .execute(&mut owner_conn)
            .await
            .unwrap();
        }
        push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .unwrap();

        let applied = pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .expect("pull_if_newer must succeed even when retirement is held back");
        assert!(applied);

        let mut conn = personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
            .await
            .unwrap();
        let still_active: Option<String> = sqlx::query_scalar(
            "SELECT id FROM entity_facts
             WHERE entity_id = ? AND field_name = 'phone' AND valid_until IS NULL",
        )
        .bind(&entity_id)
        .fetch_optional(&mut conn)
        .await
        .unwrap();
        assert!(
            still_active.is_some(),
            "absence-retirement must be held back on a Conflict entity, \
             same as a value-change would be"
        );

        let source_id = find_source_registry_id(&mut conn).await.unwrap().unwrap();
        let source = source_registry_store::get_source_conn(&mut conn, &source_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            source.status, "pending_refresh",
            "a conflicting absence must flag the source for attention, same as a value change"
        );
    }

    #[tokio::test]
    async fn a_voice_profile_entry_missing_from_a_later_push_is_deleted_on_the_recipient() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x19).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x29).await;

        let vp_id = personal_store::save_voice_profile_entry(
            &owner_id,
            &owner_persona,
            &owner_key,
            "formality",
            "casual",
            4,
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
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_share_sync_folder(
            &persona_id,
            &share_id,
            settings_store::SyncRole::Recipient,
            folder_path,
        )
        .await
        .unwrap();

        push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .unwrap();
        pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .unwrap();

        // Owner deletes the entry entirely (personal_store::delete_voice_
        // profile_entry -- a real, if previously uncalled, hard delete) and
        // pushes again.
        personal_store::delete_voice_profile_entry(
            &owner_id,
            &owner_persona,
            &owner_key,
            "formality",
            4,
            None,
        )
        .await
        .unwrap();

        let pushed = push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .expect("push_if_changed must succeed");
        assert!(pushed);

        let applied = pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .expect("pull_if_newer must succeed");
        assert!(applied);

        let mut conn = personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
            .await
            .unwrap();
        let still_present: Option<String> =
            sqlx::query_scalar("SELECT id FROM voice_profiles WHERE id = ?")
                .bind(&vp_id)
                .fetch_optional(&mut conn)
                .await
                .unwrap();
        assert!(
            still_present.is_none(),
            "a voice profile entry the owner deleted entirely must be deleted on the recipient too"
        );
    }

    #[tokio::test]
    async fn a_local_voice_profile_edit_blocks_absence_retirement_too() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x1a).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x2a).await;

        let vp_id = personal_store::save_voice_profile_entry(
            &owner_id,
            &owner_persona,
            &owner_key,
            "formality",
            "casual",
            4,
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
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_share_sync_folder(
            &persona_id,
            &share_id,
            settings_store::SyncRole::Recipient,
            folder_path,
        )
        .await
        .unwrap();

        push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .unwrap();
        pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .unwrap();

        // The recipient edits the voice profile locally -- same
        // items.id=306 guard exercised in a_local_voice_profile_edit_after_
        // pull_blocks_a_later_conflicting_update above, this time checked
        // against absence rather than a value change.
        personal_store::save_voice_profile_entry(
            &recipient_id,
            &persona_id,
            &recipient_key,
            "formality",
            "recipient's own value",
            4,
            None,
            None,
        )
        .await
        .unwrap();

        // The owner deletes the entry entirely and pushes.
        personal_store::delete_voice_profile_entry(
            &owner_id,
            &owner_persona,
            &owner_key,
            "formality",
            4,
            None,
        )
        .await
        .unwrap();
        push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .unwrap();

        let applied = pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .expect("pull_if_newer must succeed even when retirement is held back");
        assert!(applied);

        let mut conn = personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
            .await
            .unwrap();
        let still_present: Option<String> =
            sqlx::query_scalar("SELECT id FROM voice_profiles WHERE id = ?")
                .bind(&vp_id)
                .fetch_optional(&mut conn)
                .await
                .unwrap();
        assert!(
            still_present.is_some(),
            "absence-retirement must be held back on a Conflict voice profile entry, \
             same as a value-change would be"
        );

        let source_id = find_source_registry_id(&mut conn).await.unwrap().unwrap();
        let source = source_registry_store::get_source_conn(&mut conn, &source_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            source.status, "pending_refresh",
            "a conflicting absence must flag the source for attention, same as a value change"
        );
    }

    #[tokio::test]
    async fn a_user_created_voice_profile_entry_survives_an_absence_push() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key, _) = make_user_with_persona("Alice", 0x1b).await;
        let (recipient_id, _, recipient_key, recipient_priv) =
            make_user_with_persona("Bob", 0x2b).await;

        let share_id = send_persona_share(
            &owner_id,
            &owner_persona,
            &owner_key,
            &recipient_id,
            ShareType::Synced,
        )
        .await
        .unwrap();
        let persona_id =
            accept_and_provision_sync(&share_id, &recipient_id, &recipient_key, &recipient_priv)
                .await
                .unwrap();

        let shared_folder = tempfile::tempdir().unwrap();
        let folder_path = shared_folder.path().to_str().unwrap();
        settings_store::set_persona_share_sync_folder(
            &owner_persona,
            &share_id,
            settings_store::SyncRole::Owner,
            folder_path,
        )
        .await
        .unwrap();
        settings_store::set_persona_share_sync_folder(
            &persona_id,
            &share_id,
            settings_store::SyncRole::Recipient,
            folder_path,
        )
        .await
        .unwrap();

        // The recipient's own, unrelated voice profile entry -- created
        // directly on the recipient's persona, never synced from the owner
        // (modification_state = 'user_created'). Regression guard for the
        // "modification_state != 'user_created'" SQL filter specifically:
        // without it, this row would wrongly become an absence-retirement
        // candidate on the very next empty-payload push.
        let local_vp_id = personal_store::save_voice_profile_entry(
            &recipient_id,
            &persona_id,
            &recipient_key,
            "local_only_tone",
            "recipient's own value",
            4,
            None,
            None,
        )
        .await
        .unwrap();

        push_if_changed(
            &owner_id,
            &owner_persona,
            &owner_key,
            &share_id,
            &recipient_id,
        )
        .await
        .unwrap();
        let applied = pull_if_newer(
            &recipient_id,
            &persona_id,
            &recipient_key,
            &share_id,
            &recipient_priv,
        )
        .await
        .expect("pull_if_newer must succeed even with an empty owner payload");
        assert!(applied);

        let mut conn = personal_store::open_personal_db(&recipient_id, &persona_id, &recipient_key)
            .await
            .unwrap();
        let still_present: Option<String> =
            sqlx::query_scalar("SELECT id FROM voice_profiles WHERE id = ?")
                .bind(&local_vp_id)
                .fetch_optional(&mut conn)
                .await
                .unwrap();
        assert!(
            still_present.is_some(),
            "a user_created voice profile entry this sync relationship never wrote \
             must never be touched by absence retirement"
        );
    }
}
