// src-tauri/src/auth/persona_sharing.rs
//
// SYNCED persona-sharing grant flow (items.id=299, narrowed 2026-08-17
// Chat-PM/Jason): serialize an owner's Persona identity+facts content,
// encrypt it to a recipient account's public key (Architecture Section 4.3,
// items.id=289), and write a pending envelope row. items.id=302 adds the
// recipient-side counterpart: decrypt that envelope and materialize it into
// a real, independent Persona (new personas row + its own personal.db).
// Recipient-side listing UI, ongoing sync wiring, and share-stopping remain
// out of scope -- the still-open remainder of items.id=301 (mirroring
// items.id=210's own precedent before items.id=266 was decomposed into
// 283-292). This module ships send/accept as library primitives only,
// ahead of any real caller -- same convention every group.db primitive
// already established (group_store.rs::open_group_db, everything in
// group_invitations.rs): #[allow(dead_code)], real tests, no
// #[tauri::command].
//
// MODULE PLACEMENT: not persistence/ (CRUD-only against a single db) and
// not a #[tauri::command] layer (nothing calls this yet) -- same reasoning
// group_invitations.rs's own header gives: this composes crypto
// (sharing_keypair) + persona_store (shared.db) + personal_store/
// entity_store (personal.db) + a new shared.db table, the same shape
// commands/auth.rs::login() composes user_store + sharing_keypair +
// KeyRegistry.
//
// CONTENT SCOPE (Jason's decision, this session): "identity + facts only" --
// entities, entity_facts, voice profiles scoped to this Persona, and
// floor_consent_preference. Explicitly excludes outputs.db (focus_run
// history) and disclosure_log (the owner's own audit trail stays with the
// owner; a recipient's materialized instance starts its own). This governs
// PersonaSharePayload below -- built to be full-fidelity content (unlike
// the existing personal_store::export_personal_fields, which was checked
// and is the wrong fit: metadata-only, no field_value at all, sensitivity-
// ceiling-filtered to general/personal, singleton facts only).
//
// FILTERING, and why: entities limited to status='active' (skip archived/
// retired clutter a fresh independent copy shouldn't inherit). entity_facts
// limited to valid_until IS NULL (current facts only, matching
// load_entity_facts_for_context's own convention) AND cross_persona_export
// = 0 (native facts only -- a fact this Persona already imported
// cross-Persona from a sibling Persona under the SAME account is a
// different provenance boundary than an account-to-account SYNCED share;
// re-exporting it here would layer decisions.id=546's provenance model
// incorrectly) AND (entity_id IS NULL OR entity_id references one of the
// included active entities) -- keeps the payload internally consistent, no
// entity_facts row pointing at an entity that didn't make the cut.
// voice_profiles limited to persona_id = source_persona_id (not the global
// persona_id IS NULL rows, which belong to the sending account as a whole,
// not to this specific Persona's shareable identity).
//
// entity_facts is NOT built by reusing personal_store::load_entity_facts_
// for_context's EntityFact struct directly: EntityFact.field_value carries
// #[serde(skip)] ("decrypted -- never serialized"), a deliberate guard
// against exactly this kind of accidental leak into some other serialized
// context. Serializing an EntityFact vec directly here would silently drop
// every fact's actual value, producing a payload that looks fine and
// contains nothing. SharedEntityFact below is a distinct, explicit
// representation built via this module's own query, not that reused
// struct.
//
// QUERY STYLE: runtime sqlx::query() / sqlx::QueryBuilder only -- no
// query!() macros, matching the rest of this codebase.

use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;
use x25519_dalek::StaticSecret;

use crate::auth::sharing_keypair::{self, SharingKeypairError};
use crate::persistence::entity_store::{Entity, EntityFilter, ParentFilter};
use crate::persistence::persona_store::{self, PersonaStoreError};
use crate::persistence::personal_store::{self, PersonalStoreError};

/// Versions PersonaSharePayload's wire shape. Materialization
/// (items.id=301+) is future work against a payload shape that doesn't
/// exist yet elsewhere in this codebase -- this must be right the first
/// time a consumer is built, not discovered as an afterthought.
pub const PERSONA_SHARE_PAYLOAD_SCHEMA_VERSION: &str = "1.0";

/// Which of decisions.id=617's two grant types a pending_persona_shares row
/// is (items.id=304, decisions.id=723). Added to an already-shipped table
/// via a plain ADD COLUMN with no SQL-level CHECK (schema/shared_010.sql's
/// own header: SQLite can't cleanly add a CHECK to an existing table without
/// a full rebuild), so the allowed-value set is enforced here instead --
/// same pattern persona_sync::settings_store::SyncRole::parse already
/// establishes for exactly this situation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareType {
    Synced,
    ViewOnly,
}

impl ShareType {
    pub fn as_str(self) -> &'static str {
        match self {
            ShareType::Synced => "synced",
            ShareType::ViewOnly => "view_only",
        }
    }

    pub fn parse(s: &str) -> Result<Self, PersonaSharingError> {
        match s {
            "synced" => Ok(ShareType::Synced),
            "view_only" => Ok(ShareType::ViewOnly),
            other => Err(PersonaSharingError::UnknownShareType(other.to_owned())),
        }
    }
}

#[derive(Debug, Error)]
pub enum PersonaSharingError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Sharing keypair error: {0}")]
    Sharing(#[from] SharingKeypairError),
    #[error("Persona store error: {0}")]
    PersonaStore(#[from] PersonaStoreError),
    #[error("Personal store error: {0}")]
    PersonalStore(#[from] PersonalStoreError),
    #[error("Failed to serialize persona share payload: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Persona '{0}' is not owned by user '{1}'")]
    PersonaNotOwnedByUser(String, String),
    #[error("Recipient user '{0}' has no registered sharing public key")]
    RecipientHasNoSharingKey(String),
    #[error("Persona share '{0}' not found")]
    NotFound(String),
    #[error("Persona share '{0}' is not pending (status: {1})")]
    NotPending(String, String),
    #[error("Persona share '{0}' has a stored encrypted_payload that is not valid hex")]
    CorruptStoredEnvelope(String),
    #[error("Persona share '{0}' decrypted to a payload that failed to deserialize: {1}")]
    CorruptPayload(String, serde_json::Error),
    #[error("Unknown pending_persona_shares.share_type '{0}'. Must be synced or view_only.")]
    UnknownShareType(String),
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Duplicated from group_invitations.rs::hex_decode rather than reused --
/// same reasoning this file's own hex_encode already gives: different
/// error type per module, not worth coupling.
fn hex_decode(context: &str, s: &str) -> Result<Vec<u8>, PersonaSharingError> {
    if !s.len().is_multiple_of(2) {
        return Err(PersonaSharingError::CorruptStoredEnvelope(
            context.to_owned(),
        ));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| PersonaSharingError::CorruptStoredEnvelope(context.to_owned()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Payload shape
// ---------------------------------------------------------------------------

// pub(crate) (not private): items.id=303's persona_sync engine reuses these
// two shapes and the loaders below directly for its own ongoing-update
// payload, rather than a second, divergence-risking copy of this content-
// scope filtering logic. PersonaSharePayload itself stays private — the
// ongoing-update payload is its own distinct struct (persona_sync::engine::
// PersonaSyncUpdatePayload), not this one; see that struct's own doc
// comment for why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SharedEntityFact {
    pub(crate) id: String,
    pub(crate) entity_id: Option<String>,
    pub(crate) field_name: String,
    pub(crate) field_value: String,
    pub(crate) sensitivity: String,
    pub(crate) abstraction_tier2: String,
    pub(crate) abstraction_tier3: String,
    pub(crate) source: String,
    pub(crate) valid_from: Option<String>,
    pub(crate) created_at: String,
    pub(crate) extra_metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SharedVoiceProfileEntry {
    pub(crate) id: String,
    pub(crate) source_id: Option<String>,
    pub(crate) precedence: i64,
    pub(crate) attribute: String,
    pub(crate) value: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) extra_metadata: serde_json::Value,
}

// pub(crate) (not private): items.id=304's accept_persona_view_share decrypts
// and deserializes this exact same envelope shape -- decisions.id=723
// confirms the owner-side grant content is identical for both grant types,
// only what accept does with it afterward differs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PersonaSharePayload {
    pub(crate) schema_version: String,
    pub(crate) floor_consent_preference: Option<serde_json::Value>,
    pub(crate) entities: Vec<Entity>,
    pub(crate) entity_facts: Vec<SharedEntityFact>,
    pub(crate) voice_profile_entries: Vec<SharedVoiceProfileEntry>,
}

// ---------------------------------------------------------------------------
// Payload construction
// ---------------------------------------------------------------------------

pub(crate) async fn load_shared_entity_facts(
    conn: &mut SqliteConnection,
    active_entity_ids: &[String],
) -> Result<Vec<SharedEntityFact>, PersonaSharingError> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, entity_id, field_name, field_value, sensitivity,
         abstraction_tier2, abstraction_tier3, source, valid_from,
         created_at, extra_metadata
         FROM entity_facts
         WHERE valid_until IS NULL AND cross_persona_export = 0
         AND (entity_id IS NULL",
    );
    if !active_entity_ids.is_empty() {
        qb.push(" OR entity_id IN (");
        let mut sep = qb.separated(", ");
        for id in active_entity_ids {
            sep.push_bind(id);
        }
        sep.push_unseparated(")");
    }
    qb.push(") ORDER BY field_name");

    let rows = qb.build().fetch_all(&mut *conn).await?;

    let mut facts = Vec::with_capacity(rows.len());
    for r in rows {
        let metadata_json: String = r.try_get("extra_metadata")?;
        let extra_metadata: serde_json::Value =
            serde_json::from_str(&metadata_json).unwrap_or(serde_json::json!({}));
        facts.push(SharedEntityFact {
            id: r.try_get("id")?,
            entity_id: r.try_get("entity_id")?,
            field_name: r.try_get("field_name")?,
            field_value: r.try_get("field_value")?,
            sensitivity: r.try_get("sensitivity")?,
            abstraction_tier2: r.try_get("abstraction_tier2")?,
            abstraction_tier3: r.try_get("abstraction_tier3")?,
            source: r.try_get("source")?,
            valid_from: r.try_get("valid_from")?,
            created_at: r.try_get("created_at")?,
            extra_metadata,
        });
    }
    Ok(facts)
}

pub(crate) async fn load_shared_voice_profile_entries(
    conn: &mut SqliteConnection,
    source_persona_id: &str,
) -> Result<Vec<SharedVoiceProfileEntry>, PersonaSharingError> {
    let rows = sqlx::query(
        "SELECT id, source_id, precedence, attribute, value, created_at,
         updated_at, extra_metadata
         FROM voice_profiles WHERE persona_id = ?
         ORDER BY precedence, attribute",
    )
    .bind(source_persona_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut entries = Vec::with_capacity(rows.len());
    for r in rows {
        let metadata_json: String = r.try_get("extra_metadata")?;
        let extra_metadata: serde_json::Value =
            serde_json::from_str(&metadata_json).unwrap_or(serde_json::json!({}));
        entries.push(SharedVoiceProfileEntry {
            id: r.try_get("id")?,
            source_id: r.try_get("source_id")?,
            precedence: r.try_get("precedence")?,
            attribute: r.try_get("attribute")?,
            value: r.try_get("value")?,
            created_at: r.try_get("created_at")?,
            updated_at: r.try_get("updated_at")?,
            extra_metadata,
        });
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// Serialize `source_persona_id`'s identity+facts content (see module
/// header for exact scope), encrypt it to `recipient_user_id`'s registered
/// sharing public key, and write a new pending_persona_shares row. Returns
/// the new share's id.
///
/// `owner_user_id`/`owner_key_hex`: the resident account's own id and
/// personal_key_hex (KeyRegistry::personal_key_hex) -- the single-slot
/// KeyRegistry means only the currently-unlocked account can be the sender,
/// consistent with decisions.id=617's "owner initiates" framing. Verified
/// against `source_persona_id`'s actual ownership below, not assumed from
/// the caller's say-so.
#[allow(dead_code)] // items.id=299 (narrowed): ahead of its first real caller (items.id=301+)
pub async fn send_persona_share(
    pool: &sqlx::SqlitePool,
    owner_user_id: &str,
    source_persona_id: &str,
    owner_key_hex: &str,
    recipient_user_id: &str,
    share_type: ShareType,
) -> Result<String, PersonaSharingError> {
    let persona = persona_store::get_persona_for_user(pool, owner_user_id, source_persona_id)
        .await?
        .ok_or_else(|| {
            PersonaSharingError::PersonaNotOwnedByUser(
                source_persona_id.to_owned(),
                owner_user_id.to_owned(),
            )
        })?;

    let floor_consent_preference = persona
        .extra_metadata
        .get("floor_consent_preference")
        .cloned();

    let active_entities = crate::persistence::entity_store::list_entities(
        owner_user_id,
        source_persona_id,
        owner_key_hex,
        &EntityFilter {
            entity_type: None,
            status: Some("active".to_owned()),
            parent: ParentFilter::Any,
        },
    )
    .await?;
    let active_entity_ids: Vec<String> = active_entities.iter().map(|e| e.id.clone()).collect();

    let mut personal_conn =
        personal_store::open_personal_db(owner_user_id, source_persona_id, owner_key_hex).await?;
    let entity_facts = load_shared_entity_facts(&mut personal_conn, &active_entity_ids).await?;
    let voice_profile_entries =
        load_shared_voice_profile_entries(&mut personal_conn, source_persona_id).await?;

    let payload = PersonaSharePayload {
        schema_version: PERSONA_SHARE_PAYLOAD_SCHEMA_VERSION.to_owned(),
        floor_consent_preference,
        entities: active_entities,
        entity_facts,
        voice_profile_entries,
    };
    let plaintext = serde_json::to_vec(&payload)?;

    let recipient_public_key = sharing_keypair::get_public_key(pool, recipient_user_id)
        .await?
        .ok_or_else(|| {
            PersonaSharingError::RecipientHasNoSharingKey(recipient_user_id.to_owned())
        })?;
    let envelope = sharing_keypair::encrypt_to_public_key(&recipient_public_key, &plaintext)?;

    let id = uuid::Uuid::new_v4().to_string();
    let created_at = crate::providers::utils::now();

    let mut shared_conn = pool.acquire().await?;
    sqlx::query(
        "INSERT INTO pending_persona_shares
         (id, recipient_user_id, source_persona_id, source_persona_display_name,
          source_persona_type, payload_schema_version, encrypted_payload, status,
          created_at, share_type)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?)",
    )
    .bind(&id)
    .bind(recipient_user_id)
    .bind(source_persona_id)
    .bind(&persona.display_name)
    .bind(&persona.persona_type)
    .bind(PERSONA_SHARE_PAYLOAD_SCHEMA_VERSION)
    .bind(hex_encode(&envelope))
    .bind(&created_at)
    .bind(share_type.as_str())
    .execute(&mut *shared_conn)
    .await?;

    Ok(id)
}

// ---------------------------------------------------------------------------
// Accept / materialize (items.id=302)
// ---------------------------------------------------------------------------

// pub(crate) (not private): items.id=304's persona_view_sync engine reuses
// this row shape and the fetch below directly for accept_persona_view_share
// -- the fetch-by-(id, recipient)/pending-status logic is identical for both
// grant types, only what happens to the decrypted payload afterward differs.
pub(crate) struct PendingPersonaShareRow {
    pub(crate) source_persona_display_name: String,
    pub(crate) source_persona_type: String,
    pub(crate) encrypted_payload: String,
    /// "synced" | "view_only" (ShareType::as_str). accept_persona_share
    /// (this file) never reads this field -- it's SYNCED-only and doesn't
    /// need to check its own share_type. accept_persona_view_share
    /// (persona_view_sync::engine) does, to reject accepting a SYNCED share
    /// through the VIEW-ONLY path.
    pub(crate) share_type: String,
}

/// Mirrors group_invitations.rs::fetch_pending_invitation: id + recipient
/// both filter the same WHERE clause, so "exists but belongs to a different
/// recipient" collapses into the same NotFound as "doesn't exist" -- no
/// separate error, no existence leak.
pub(crate) async fn fetch_pending_persona_share(
    share_id: &str,
    recipient_user_id: &str,
    conn: &mut SqliteConnection,
) -> Result<PendingPersonaShareRow, PersonaSharingError> {
    let row = sqlx::query(
        "SELECT source_persona_display_name, source_persona_type, encrypted_payload, status,
                share_type
         FROM pending_persona_shares
         WHERE id = ? AND recipient_user_id = ?",
    )
    .bind(share_id)
    .bind(recipient_user_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| PersonaSharingError::NotFound(share_id.to_owned()))?;

    let status: String = row.try_get("status")?;
    if status != "pending" {
        return Err(PersonaSharingError::NotPending(share_id.to_owned(), status));
    }

    Ok(PendingPersonaShareRow {
        source_persona_display_name: row.try_get("source_persona_display_name")?,
        source_persona_type: row.try_get("source_persona_type")?,
        encrypted_payload: row.try_get("encrypted_payload")?,
        share_type: row.try_get("share_type")?,
    })
}

/// Accept a pending SYNCED persona share: decrypt its envelope with the
/// recipient's own resident sharing private key (KeyRegistry::with_key
/// exposes it as UnlockedKey.sharing_private_key -- reconstruct via
/// StaticSecret::from), and materialize the decrypted payload into a new,
/// independent Persona -- a fresh personal.db (entities, entity_facts,
/// voice profiles) plus a new personas/user_personas row in shared.db.
/// Returns the new persona's id.
///
/// ORDERING, deliberate, mirroring accept_invitation's own reasoning
/// (group_invitations.rs:318-336): decrypt happens before ANY row mutation
/// -- on tamper/wrong-key this returns Err(Sharing(DecryptionFailed))
/// unchanged and the share row is left exactly as it was (still 'pending').
/// The new persona's personal.db content is written FIRST, in its own
/// SAVEPOINT; only once that succeeds do we touch shared.db (INSERT
/// personas, INSERT user_personas, UPDATE the share to 'accepted'), in a
/// second SAVEPOINT. No single SAVEPOINT can span both files -- same
/// constraint accept_invitation lives with (memory / personal.db / shared.db
/// are different storage systems there; personal.db / shared.db are here).
/// This ordering means a failure in the personal.db write leaves the share
/// 'pending' and shared.db untouched (safe retry, new persona_id next
/// attempt); a failure in the shared.db write after personal.db succeeded
/// leaves an orphaned personal.db file that no persona ever points at --
/// inert, not user-visible, acceptable at this pre-release stage.
// items.id=303: persona_sync::engine::accept_and_provision_sync is this
// function's first real caller (wraps it unmodified, then provisions the
// ongoing-sync relationship) -- no longer ahead of a caller, so the
// #[allow(dead_code)] items.id=302 originally carried here is gone.
pub async fn accept_persona_share(
    pool: &sqlx::SqlitePool,
    share_id: &str,
    recipient_user_id: &str,
    recipient_personal_key_hex: &str,
    sharing_private_key: &StaticSecret,
) -> Result<String, PersonaSharingError> {
    let mut shared_conn = pool.acquire().await?;
    let share = fetch_pending_persona_share(share_id, recipient_user_id, &mut shared_conn).await?;

    let envelope = hex_decode(share_id, &share.encrypted_payload)?;
    let plaintext = sharing_keypair::decrypt_own_envelope(sharing_private_key, &envelope)?;
    let payload: PersonaSharePayload = serde_json::from_slice(&plaintext)
        .map_err(|e| PersonaSharingError::CorruptPayload(share_id.to_owned(), e))?;

    let persona_id = uuid::Uuid::new_v4().to_string();
    let now = crate::providers::utils::now();

    // --- personal.db: entities, entity_facts, voice_profiles -----------
    let mut personal_conn = personal_store::open_personal_db(
        recipient_user_id,
        &persona_id,
        recipient_personal_key_hex,
    )
    .await?;

    sqlx::query("SAVEPOINT accept_persona_share")
        .execute(&mut personal_conn)
        .await?;

    let content_step: Result<(), sqlx::Error> = async {
        // entities.parent_entity_id is a real, enforced FK (sqlx enables
        // PRAGMA foreign_keys by default) -- a single insert pass in
        // payload order could violate it if a child sorts before its
        // parent, and a parent excluded from the payload (e.g. archived,
        // filtered out by send_persona_share) would violate it permanently.
        // Two passes avoids both: pass 1 inserts every entity with
        // parent_entity_id forced NULL (never violates the FK); pass 2
        // fills in parent_entity_id only where that parent actually made
        // it into this payload -- a parent that didn't is correctly left
        // NULL, since it doesn't exist anywhere in the recipient's new
        // personal.db either.
        let included_entity_ids: std::collections::HashSet<&str> =
            payload.entities.iter().map(|e| e.id.as_str()).collect();

        for entity in &payload.entities {
            let aliases_json =
                serde_json::to_string(&entity.aliases).unwrap_or_else(|_| "[]".to_owned());
            let extra_metadata_json = entity.extra_metadata.to_string();
            sqlx::query(
                "INSERT INTO entities
                 (id, entity_type, display_name, aliases, parent_entity_id, status,
                  modification_state, source_registry_id, source_url, created_at,
                  extra_metadata, redact_identification, hide_from_shared_surfaces)
                 VALUES (?, ?, ?, ?, NULL, ?, 'user_created', NULL, ?, ?, ?, ?, ?)",
            )
            .bind(&entity.id)
            .bind(&entity.entity_type)
            .bind(&entity.display_name)
            .bind(&aliases_json)
            .bind(&entity.status)
            .bind(&entity.source_url)
            .bind(&entity.created_at)
            .bind(&extra_metadata_json)
            .bind(entity.redact_identification)
            .bind(entity.hide_from_shared_surfaces)
            .execute(&mut personal_conn)
            .await?;
        }

        for entity in &payload.entities {
            let Some(parent_id) = entity.parent_entity_id.as_deref() else {
                continue;
            };
            if !included_entity_ids.contains(parent_id) {
                continue;
            }
            sqlx::query("UPDATE entities SET parent_entity_id = ? WHERE id = ?")
                .bind(parent_id)
                .bind(&entity.id)
                .execute(&mut personal_conn)
                .await?;
        }

        for fact in &payload.entity_facts {
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
            .bind(&fact.created_at)
            .bind(&extra_metadata_json)
            .bind(&persona_id)
            .execute(&mut personal_conn)
            .await?;
        }

        for entry in &payload.voice_profile_entries {
            let extra_metadata_json = entry.extra_metadata.to_string();
            sqlx::query(
                "INSERT INTO voice_profiles
                 (id, persona_id, source_id, precedence, attribute, value,
                  created_at, updated_at, extra_metadata, modification_state)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'user_created')",
            )
            .bind(&entry.id)
            .bind(&persona_id)
            .bind(&entry.source_id)
            .bind(entry.precedence)
            .bind(&entry.attribute)
            .bind(&entry.value)
            .bind(&entry.created_at)
            .bind(&entry.updated_at)
            .bind(&extra_metadata_json)
            .execute(&mut personal_conn)
            .await?;
        }

        Ok(())
    }
    .await;

    match content_step {
        Ok(()) => {
            sqlx::query("RELEASE accept_persona_share")
                .execute(&mut personal_conn)
                .await?;
        }
        Err(e) => {
            if let Err(rollback_err) = sqlx::query("ROLLBACK TO accept_persona_share")
                .execute(&mut personal_conn)
                .await
            {
                log::error!(
                    "Savepoint rollback failed in accept_persona_share content write \
                     (persona_id='{persona_id}'): {rollback_err} -- original error still \
                     being propagated: {e}"
                );
            }
            return Err(PersonaSharingError::Database(e));
        }
    }
    drop(personal_conn);

    // --- shared.db: personas, user_personas, share status ---------------
    let extra_metadata_json = match &payload.floor_consent_preference {
        Some(v) => serde_json::json!({ "floor_consent_preference": v }).to_string(),
        None => "{}".to_owned(),
    };

    sqlx::query("SAVEPOINT accept_persona_share")
        .execute(&mut *shared_conn)
        .await?;

    let registration_step: Result<(), sqlx::Error> = async {
        sqlx::query(
            "INSERT INTO personas (id, display_name, persona_type, created_at, extra_metadata)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&persona_id)
        .bind(&share.source_persona_display_name)
        .bind(&share.source_persona_type)
        .bind(&now)
        .bind(&extra_metadata_json)
        .execute(&mut *shared_conn)
        .await?;

        sqlx::query("INSERT INTO user_personas (user_id, persona_id, joined_at) VALUES (?, ?, ?)")
            .bind(recipient_user_id)
            .bind(&persona_id)
            .bind(&now)
            .execute(&mut *shared_conn)
            .await?;

        // materialized_persona_id (items.id=303, shared_009.sql): durable
        // record of which persona this share became, so the sync engine can
        // find it across restarts without depending on whatever called
        // accept_persona_share to have recorded it somewhere else.
        sqlx::query(
            "UPDATE pending_persona_shares
             SET status = 'accepted', responded_at = ?, materialized_persona_id = ?
             WHERE id = ?",
        )
        .bind(&now)
        .bind(&persona_id)
        .bind(share_id)
        .execute(&mut *shared_conn)
        .await?;

        Ok(())
    }
    .await;

    match registration_step {
        Ok(()) => {
            sqlx::query("RELEASE accept_persona_share")
                .execute(&mut *shared_conn)
                .await?;
        }
        Err(e) => {
            if let Err(rollback_err) = sqlx::query("ROLLBACK TO accept_persona_share")
                .execute(&mut *shared_conn)
                .await
            {
                log::error!(
                    "Savepoint rollback failed in accept_persona_share registration \
                     (persona_id='{persona_id}'): {rollback_err} -- original error still \
                     being propagated: {e}"
                );
            }
            return Err(PersonaSharingError::Database(e));
        }
    }

    Ok(persona_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::kdf;
    use crate::auth::sharing_keypair as sk;
    use crate::persistence::entity_store::{self, EntityUpdate};
    use crate::test_support::ENV_MUTEX;
    use x25519_dalek::StaticSecret;

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

    /// Creates a user + one persona owned by that user, returning
    /// (user_id, persona_id, key_hex, sharing_private_key). Mirrors
    /// group_invitations.rs's own test fixture convention.
    async fn make_user_with_persona(
        pool: &sqlx::SqlitePool,
        display_name: &str,
        master_key_fill: u8,
    ) -> (String, String, String, StaticSecret) {
        let user_id = uuid::Uuid::new_v4().to_string();
        let master_key = [master_key_fill; kdf::MASTER_KEY_LEN];
        let (sharing_private_key, sharing_public_key) =
            sk::derive_sharing_keypair(&master_key, &user_id);
        let key_hex: String = master_key.iter().map(|b| format!("{b:02x}")).collect();

        crate::auth::user_store::create_user(
            pool,
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
        persona_store::create_persona(
            pool,
            &persona_id,
            "Shared Persona",
            "personal",
            &user_id,
            None,
        )
        .await
        .expect("create_persona must succeed");

        (user_id, persona_id, key_hex, sharing_private_key)
    }

    async fn decrypt_payload(
        recipient_private_key: &StaticSecret,
        envelope_hex: &str,
    ) -> PersonaSharePayload {
        let envelope: Vec<u8> = (0..envelope_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&envelope_hex[i..i + 2], 16).unwrap())
            .collect();
        let plaintext = sk::decrypt_own_envelope(recipient_private_key, &envelope).unwrap();
        serde_json::from_slice(&plaintext).unwrap()
    }

    /// Sends a share from owner -> recipient carrying one entity, one
    /// entity_fact on it, and one persona-scoped voice_profile entry -- the
    /// fixture every accept-side test below builds on. Returns
    /// (share_id, entity_id).
    async fn send_share_with_content(
        pool: &sqlx::SqlitePool,
        owner_id: &str,
        owner_persona: &str,
        owner_key_hex: &str,
        recipient_id: &str,
    ) -> (String, String) {
        let entity_id = entity_store::create_entity(
            owner_id,
            owner_persona,
            owner_key_hex,
            "person",
            "Sam",
            &[],
            None,
            None,
        )
        .await
        .unwrap();

        let mut conn = personal_store::open_personal_db(owner_id, owner_persona, owner_key_hex)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO entity_facts
             (id, entity_id, field_name, field_value, sensitivity, source_persona_id,
              cross_persona_export, created_at)
             VALUES ('accept-fact-1', ?, 'favorite_color', 'green', 'general', ?, 0, ?)",
        )
        .bind(&entity_id)
        .bind(owner_persona)
        .bind(crate::providers::utils::now())
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO voice_profiles
             (id, persona_id, precedence, attribute, value, created_at, updated_at)
             VALUES ('accept-vp-1', ?, 4, 'tone', 'direct', ?, ?)",
        )
        .bind(owner_persona)
        .bind(crate::providers::utils::now())
        .bind(crate::providers::utils::now())
        .execute(&mut conn)
        .await
        .unwrap();
        drop(conn);

        let share_id = send_persona_share(
            pool,
            owner_id,
            owner_persona,
            owner_key_hex,
            recipient_id,
            ShareType::Synced,
        )
        .await
        .expect("send_persona_share must succeed");

        (share_id, entity_id)
    }

    #[tokio::test]
    async fn round_trip_send_produces_a_decryptable_matching_payload() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0x11).await;
        let (recipient_id, _recipient_persona, _, recipient_private_key) =
            make_user_with_persona(pool, "Bob", 0x22).await;

        let entity_id = entity_store::create_entity(
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            "person",
            "Sam",
            &[],
            None,
            None,
        )
        .await
        .unwrap();

        let mut conn = personal_store::open_personal_db(&owner_id, &owner_persona, &owner_key_hex)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO entity_facts
             (id, entity_id, field_name, field_value, sensitivity, source_persona_id,
              cross_persona_export, created_at)
             VALUES ('fact-1', ?, 'favorite_color', 'green', 'general', ?, 0, ?)",
        )
        .bind(&entity_id)
        .bind(&owner_persona)
        .bind(crate::providers::utils::now())
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO voice_profiles
             (id, persona_id, precedence, attribute, value, created_at, updated_at)
             VALUES ('vp-1', ?, 4, 'tone', 'direct', ?, ?)",
        )
        .bind(&owner_persona)
        .bind(crate::providers::utils::now())
        .bind(crate::providers::utils::now())
        .execute(&mut conn)
        .await
        .unwrap();
        drop(conn);

        let share_id = send_persona_share(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &recipient_id,
            ShareType::Synced,
        )
        .await
        .expect("send_persona_share must succeed");

        let mut shared_conn = pool.acquire().await.unwrap();
        let row = sqlx::query(
            "SELECT recipient_user_id, source_persona_id, status, encrypted_payload
             FROM pending_persona_shares WHERE id = ?",
        )
        .bind(&share_id)
        .fetch_one(&mut *shared_conn)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("recipient_user_id"), recipient_id);
        assert_eq!(row.get::<String, _>("source_persona_id"), owner_persona);
        assert_eq!(row.get::<String, _>("status"), "pending");

        let payload = decrypt_payload(
            &recipient_private_key,
            &row.get::<String, _>("encrypted_payload"),
        )
        .await;
        assert_eq!(payload.schema_version, PERSONA_SHARE_PAYLOAD_SCHEMA_VERSION);
        assert_eq!(payload.entities.len(), 1);
        assert_eq!(payload.entities[0].id, entity_id);
        assert_eq!(payload.entity_facts.len(), 1);
        assert_eq!(payload.entity_facts[0].field_value, "green");
        assert_eq!(payload.voice_profile_entries.len(), 1);
        assert_eq!(payload.voice_profile_entries[0].value, "direct");
    }

    #[tokio::test]
    async fn archived_entity_and_its_facts_are_excluded() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0x33).await;
        let (recipient_id, _, _, recipient_private_key) =
            make_user_with_persona(pool, "Bob", 0x44).await;

        let entity_id = entity_store::create_entity(
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            "person",
            "Old Contact",
            &[],
            None,
            None,
        )
        .await
        .unwrap();
        entity_store::update_entity(
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &entity_id,
            &EntityUpdate {
                status: Some("archived".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let mut conn = personal_store::open_personal_db(&owner_id, &owner_persona, &owner_key_hex)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO entity_facts
             (id, entity_id, field_name, field_value, sensitivity, source_persona_id,
              cross_persona_export, created_at)
             VALUES ('fact-archived', ?, 'note', 'stale', 'general', ?, 0, ?)",
        )
        .bind(&entity_id)
        .bind(&owner_persona)
        .bind(crate::providers::utils::now())
        .execute(&mut conn)
        .await
        .unwrap();
        drop(conn);

        let share_id = send_persona_share(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &recipient_id,
            ShareType::Synced,
        )
        .await
        .unwrap();

        let mut shared_conn = pool.acquire().await.unwrap();
        let encrypted_payload: String =
            sqlx::query_scalar("SELECT encrypted_payload FROM pending_persona_shares WHERE id = ?")
                .bind(&share_id)
                .fetch_one(&mut *shared_conn)
                .await
                .unwrap();
        let payload = decrypt_payload(&recipient_private_key, &encrypted_payload).await;

        assert!(payload.entities.is_empty());
        assert!(
            payload.entity_facts.is_empty(),
            "a fact belonging to an excluded (archived) entity must not appear \
             even though the fact row itself is still active"
        );
    }

    #[tokio::test]
    async fn cross_persona_exported_fact_is_excluded() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0x55).await;
        let (recipient_id, _, _, recipient_private_key) =
            make_user_with_persona(pool, "Bob", 0x66).await;

        let mut conn = personal_store::open_personal_db(&owner_id, &owner_persona, &owner_key_hex)
            .await
            .unwrap();
        // Singleton fact (entity_id NULL), imported cross-Persona from a
        // sibling Persona under the same account -- must not cross an
        // account boundary via a SYNCED share.
        sqlx::query(
            "INSERT INTO entity_facts
             (id, entity_id, field_name, field_value, sensitivity, source_persona_id,
              cross_persona_export, origin_persona_id, created_at)
             VALUES ('fact-xpe', NULL, 'allergy', 'peanuts', 'medical', ?, 1, 'sibling-persona', ?)",
        )
        .bind(&owner_persona)
        .bind(crate::providers::utils::now())
        .execute(&mut conn)
        .await
        .unwrap();
        drop(conn);

        let share_id = send_persona_share(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &recipient_id,
            ShareType::Synced,
        )
        .await
        .unwrap();

        let mut shared_conn = pool.acquire().await.unwrap();
        let encrypted_payload: String =
            sqlx::query_scalar("SELECT encrypted_payload FROM pending_persona_shares WHERE id = ?")
                .bind(&share_id)
                .fetch_one(&mut *shared_conn)
                .await
                .unwrap();
        let payload = decrypt_payload(&recipient_private_key, &encrypted_payload).await;

        assert!(payload.entity_facts.is_empty());
    }

    #[tokio::test]
    async fn global_voice_profile_entry_is_excluded() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0x77).await;
        let (recipient_id, _, _, recipient_private_key) =
            make_user_with_persona(pool, "Bob", 0x88).await;

        let mut conn = personal_store::open_personal_db(&owner_id, &owner_persona, &owner_key_hex)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO voice_profiles
             (id, persona_id, precedence, attribute, value, created_at, updated_at)
             VALUES ('vp-global', NULL, 3, 'formality', 'casual', ?, ?)",
        )
        .bind(crate::providers::utils::now())
        .bind(crate::providers::utils::now())
        .execute(&mut conn)
        .await
        .unwrap();
        drop(conn);

        let share_id = send_persona_share(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &recipient_id,
            ShareType::Synced,
        )
        .await
        .unwrap();

        let mut shared_conn = pool.acquire().await.unwrap();
        let encrypted_payload: String =
            sqlx::query_scalar("SELECT encrypted_payload FROM pending_persona_shares WHERE id = ?")
                .bind(&share_id)
                .fetch_one(&mut *shared_conn)
                .await
                .unwrap();
        let payload = decrypt_payload(&recipient_private_key, &encrypted_payload).await;

        assert!(payload.voice_profile_entries.is_empty());
    }

    #[tokio::test]
    async fn persona_not_owned_by_caller_is_rejected() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, _, owner_key_hex, _) = make_user_with_persona(pool, "Alice", 0x99).await;
        let (_other_owner, other_persona, _, _) = make_user_with_persona(pool, "Carol", 0xAA).await;
        let (recipient_id, _, _, _) = make_user_with_persona(pool, "Bob", 0xBB).await;

        let result = send_persona_share(
            pool,
            &owner_id,
            &other_persona,
            &owner_key_hex,
            &recipient_id,
            ShareType::Synced,
        )
        .await;

        assert!(matches!(
            result,
            Err(PersonaSharingError::PersonaNotOwnedByUser(_, _))
        ));
    }

    #[tokio::test]
    async fn recipient_with_no_sharing_key_is_rejected() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0xCC).await;

        let result = send_persona_share(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            "nonexistent-user-id",
            ShareType::Synced,
        )
        .await;

        assert!(matches!(
            result,
            Err(PersonaSharingError::RecipientHasNoSharingKey(_))
        ));
    }

    #[tokio::test]
    async fn round_trip_accept_materializes_persona_with_content() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0x31).await;
        let (recipient_id, _, recipient_key_hex, recipient_private_key) =
            make_user_with_persona(pool, "Bob", 0x32).await;

        let (share_id, entity_id) = send_share_with_content(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &recipient_id,
        )
        .await;

        let new_persona_id = accept_persona_share(
            pool,
            &share_id,
            &recipient_id,
            &recipient_key_hex,
            &recipient_private_key,
        )
        .await
        .expect("accept_persona_share must succeed");

        let new_persona = persona_store::get_persona(pool, &new_persona_id)
            .await
            .unwrap()
            .expect("materialized persona must exist");
        assert_eq!(new_persona.display_name, "Shared Persona");
        assert_eq!(new_persona.persona_type, "personal");
        assert!(
            persona_store::is_user_in_persona(pool, &recipient_id, &new_persona_id)
                .await
                .unwrap()
        );

        let mut conn =
            personal_store::open_personal_db(&recipient_id, &new_persona_id, &recipient_key_hex)
                .await
                .unwrap();
        let entity_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities WHERE id = ?")
            .bind(&entity_id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(entity_count, 1);
        let fact_value: String =
            sqlx::query_scalar("SELECT field_value FROM entity_facts WHERE id = 'accept-fact-1'")
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(fact_value, "green");
        let vp_value: String =
            sqlx::query_scalar("SELECT value FROM voice_profiles WHERE id = 'accept-vp-1'")
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(vp_value, "direct");
        drop(conn);

        let mut shared_conn = pool.acquire().await.unwrap();
        let share_status: String =
            sqlx::query_scalar("SELECT status FROM pending_persona_shares WHERE id = ?")
                .bind(&share_id)
                .fetch_one(&mut *shared_conn)
                .await
                .unwrap();
        assert_eq!(share_status, "accepted");
    }

    #[tokio::test]
    async fn accept_persona_share_materializes_parent_child_hierarchy_regardless_of_payload_order()
    {
        // Regression test for items.id=302's two-pass insert
        // (accept_persona_share, entities.parent_entity_id is a real,
        // enforced FK). list_entities orders payload.entities by
        // display_name, so naming the child before the parent
        // alphabetically forces the child to appear first in the payload --
        // exactly the ordering a naive single-pass insert would violate the
        // FK on.
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0x61).await;
        let (recipient_id, _, recipient_key_hex, recipient_private_key) =
            make_user_with_persona(pool, "Bob", 0x62).await;

        let parent_id = entity_store::create_entity(
            &owner_id,
            &owner_persona,
            &owner_key_hex,
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
            &owner_key_hex,
            "person",
            "Aaron Child",
            &[],
            Some(&parent_id),
            None,
        )
        .await
        .unwrap();

        let share_id = send_persona_share(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &recipient_id,
            ShareType::Synced,
        )
        .await
        .expect("send_persona_share must succeed");

        let new_persona_id = accept_persona_share(
            pool,
            &share_id,
            &recipient_id,
            &recipient_key_hex,
            &recipient_private_key,
        )
        .await
        .expect("accept_persona_share must succeed despite child-before-parent payload order");

        let mut conn =
            personal_store::open_personal_db(&recipient_id, &new_persona_id, &recipient_key_hex)
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
    async fn materialized_entity_facts_get_synced_share_provenance_tag() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0x41).await;
        let (recipient_id, _, recipient_key_hex, recipient_private_key) =
            make_user_with_persona(pool, "Bob", 0x42).await;

        let (share_id, _) = send_share_with_content(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &recipient_id,
        )
        .await;

        let new_persona_id = accept_persona_share(
            pool,
            &share_id,
            &recipient_id,
            &recipient_key_hex,
            &recipient_private_key,
        )
        .await
        .unwrap();

        let mut conn =
            personal_store::open_personal_db(&recipient_id, &new_persona_id, &recipient_key_hex)
                .await
                .unwrap();
        let row = sqlx::query(
            "SELECT source, source_persona_id, cross_persona_export, origin_persona_id
             FROM entity_facts WHERE id = 'accept-fact-1'",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("source"), "synced_share");
        assert_eq!(row.get::<String, _>("source_persona_id"), new_persona_id);
        assert_eq!(row.get::<i64, _>("cross_persona_export"), 0);
        assert!(row.get::<Option<String>, _>("origin_persona_id").is_none());
    }

    #[tokio::test]
    async fn floor_consent_preference_is_embedded_in_new_persona() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0x51).await;
        let (recipient_id, _, recipient_key_hex, recipient_private_key) =
            make_user_with_persona(pool, "Bob", 0x52).await;

        let mut conn = pool.acquire().await.unwrap();
        sqlx::query("UPDATE personas SET extra_metadata = ? WHERE id = ?")
            .bind(r#"{"floor_consent_preference":{"mode":"modified","abstraction_tier":2}}"#)
            .bind(&owner_persona)
            .execute(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        let (share_id, _) = send_share_with_content(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &recipient_id,
        )
        .await;

        let new_persona_id = accept_persona_share(
            pool,
            &share_id,
            &recipient_id,
            &recipient_key_hex,
            &recipient_private_key,
        )
        .await
        .unwrap();

        let new_persona = persona_store::get_persona(pool, &new_persona_id)
            .await
            .unwrap()
            .unwrap();
        let pref = new_persona
            .extra_metadata
            .get("floor_consent_preference")
            .expect("floor_consent_preference must be present");
        assert_eq!(pref["abstraction_tier"], 2);
    }

    #[tokio::test]
    async fn materialized_entity_drops_source_registry_id_and_resets_modification_state() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0x61).await;
        let (recipient_id, _, recipient_key_hex, recipient_private_key) =
            make_user_with_persona(pool, "Bob", 0x62).await;

        let entity_id = entity_store::create_entity(
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            "person",
            "Imported Contact",
            &[],
            None,
            None,
        )
        .await
        .unwrap();

        let mut conn = personal_store::open_personal_db(&owner_id, &owner_persona, &owner_key_hex)
            .await
            .unwrap();
        // source_registry_id is a real, enforced FK -- a genuine row is
        // required, a bogus id would fail the UPDATE below, not just be
        // silently tolerated (sqlx enables PRAGMA foreign_keys by default).
        sqlx::query(
            "INSERT INTO source_registry (id, persona_id, focus_slug, source_type, created_at)
             VALUES ('real-source', ?, 'contacts', 'url_ingestion', ?)",
        )
        .bind(&owner_persona)
        .bind(crate::providers::utils::now())
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE entities SET modification_state = 'pristine', source_registry_id = 'real-source'
             WHERE id = ?",
        )
        .bind(&entity_id)
        .execute(&mut conn)
        .await
        .unwrap();
        drop(conn);

        let share_id = send_persona_share(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &recipient_id,
            ShareType::Synced,
        )
        .await
        .unwrap();

        let new_persona_id = accept_persona_share(
            pool,
            &share_id,
            &recipient_id,
            &recipient_key_hex,
            &recipient_private_key,
        )
        .await
        .unwrap();

        let mut conn =
            personal_store::open_personal_db(&recipient_id, &new_persona_id, &recipient_key_hex)
                .await
                .unwrap();
        let row =
            sqlx::query("SELECT modification_state, source_registry_id FROM entities WHERE id = ?")
                .bind(&entity_id)
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(row.get::<String, _>("modification_state"), "user_created");
        assert!(row.get::<Option<String>, _>("source_registry_id").is_none());
    }

    #[tokio::test]
    async fn accept_unknown_share_id_is_not_found() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (recipient_id, _, recipient_key_hex, recipient_private_key) =
            make_user_with_persona(pool, "Bob", 0x71).await;

        let result = accept_persona_share(
            pool,
            "nonexistent-share-id",
            &recipient_id,
            &recipient_key_hex,
            &recipient_private_key,
        )
        .await;

        assert!(matches!(result, Err(PersonaSharingError::NotFound(_))));
    }

    #[tokio::test]
    async fn accept_already_accepted_share_is_not_pending() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0x81).await;
        let (recipient_id, _, recipient_key_hex, recipient_private_key) =
            make_user_with_persona(pool, "Bob", 0x82).await;

        let (share_id, _) = send_share_with_content(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &recipient_id,
        )
        .await;

        accept_persona_share(
            pool,
            &share_id,
            &recipient_id,
            &recipient_key_hex,
            &recipient_private_key,
        )
        .await
        .expect("first accept must succeed");

        let second = accept_persona_share(
            pool,
            &share_id,
            &recipient_id,
            &recipient_key_hex,
            &recipient_private_key,
        )
        .await;

        assert!(matches!(second, Err(PersonaSharingError::NotPending(_, _))));
    }

    #[tokio::test]
    async fn accept_wrong_recipient_is_not_found() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0x91).await;
        let (recipient_id, _, _, _) = make_user_with_persona(pool, "Bob", 0x92).await;
        let (other_id, _, other_key_hex, other_private_key) =
            make_user_with_persona(pool, "Carol", 0x93).await;

        let (share_id, _) = send_share_with_content(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &recipient_id,
        )
        .await;

        let result = accept_persona_share(
            pool,
            &share_id,
            &other_id,
            &other_key_hex,
            &other_private_key,
        )
        .await;

        assert!(matches!(result, Err(PersonaSharingError::NotFound(_))));
    }

    #[tokio::test]
    async fn tampered_envelope_fails_with_decryption_failed_and_leaves_share_pending() {
        let _env = setup().await;
        let pool = &_env.pool;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona(pool, "Alice", 0xA1).await;
        let (recipient_id, _, recipient_key_hex, recipient_private_key) =
            make_user_with_persona(pool, "Bob", 0xA2).await;

        let (share_id, _) = send_share_with_content(
            pool,
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            &recipient_id,
        )
        .await;

        let mut conn = pool.acquire().await.unwrap();
        let encrypted_hex: String =
            sqlx::query_scalar("SELECT encrypted_payload FROM pending_persona_shares WHERE id = ?")
                .bind(&share_id)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        let mut bytes = hex_decode("test", &encrypted_hex).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        sqlx::query("UPDATE pending_persona_shares SET encrypted_payload = ? WHERE id = ?")
            .bind(hex_encode(&bytes))
            .bind(&share_id)
            .execute(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        let result = accept_persona_share(
            pool,
            &share_id,
            &recipient_id,
            &recipient_key_hex,
            &recipient_private_key,
        )
        .await;

        assert!(
            matches!(
                result,
                Err(PersonaSharingError::Sharing(
                    SharingKeypairError::DecryptionFailed
                ))
            ),
            "expected DecryptionFailed, got {result:?}"
        );

        let mut conn = pool.acquire().await.unwrap();
        let status: String =
            sqlx::query_scalar("SELECT status FROM pending_persona_shares WHERE id = ?")
                .bind(&share_id)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(
            status, "pending",
            "tampered accept must leave share pending"
        );
    }
}
