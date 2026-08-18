// src-tauri/src/auth/persona_sharing.rs
//
// SYNCED persona-sharing grant flow (items.id=299, narrowed 2026-08-17
// Chat-PM/Jason): serialize an owner's Persona identity+facts content,
// encrypt it to a recipient account's public key (Architecture Section 4.3,
// items.id=289), and write a pending envelope row. This is the ONE piece of
// items.id=299's original five-piece scope that is fully resolved
// (items.id=189, 2026-08-16) and does not depend on the still-open design
// question. Recipient-side listing/accept/decline, materialization (turning
// the decrypted payload into a real new Persona + personal.db), ongoing
// sync wiring, and share-stopping are ALL out of scope here -- they require
// a dedicated design pass (items.id=301, mirroring items.id=210's own
// precedent before items.id=266 was decomposed into 283-292) that has not
// happened yet. This module ships the send-side primitive only, ahead of
// its first real caller -- same convention every group.db primitive already
// established (group_store.rs::open_group_db, everything in
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
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

use crate::auth::sharing_keypair::{self, SharingKeypairError};
use crate::persistence::entity_store::{Entity, EntityFilter, ParentFilter};
use crate::persistence::persona_store::{self, PersonaStoreError};
use crate::persistence::personal_store::{self, PersonalStoreError};

/// Versions PersonaSharePayload's wire shape. Materialization
/// (items.id=301+) is future work against a payload shape that doesn't
/// exist yet elsewhere in this codebase -- this must be right the first
/// time a consumer is built, not discovered as an afterthought.
pub const PERSONA_SHARE_PAYLOAD_SCHEMA_VERSION: &str = "1.0";

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
}

// ---------------------------------------------------------------------------
// DB opener (shared.db -- unencrypted)
// ---------------------------------------------------------------------------
// Duplicated rather than reused -- same reasoning group_invitations.rs's own
// header gives: different error type per module, ~12-line
// zero-divergence-risk helper, not worth coupling.

async fn open_shared_db() -> Result<SqliteConnection, PersonaSharingError> {
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

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Payload shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SharedEntityFact {
    id: String,
    entity_id: Option<String>,
    field_name: String,
    field_value: String,
    sensitivity: String,
    abstraction_tier2: String,
    abstraction_tier3: String,
    source: String,
    valid_from: Option<String>,
    created_at: String,
    extra_metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SharedVoiceProfileEntry {
    id: String,
    source_id: Option<String>,
    precedence: i64,
    attribute: String,
    value: String,
    created_at: String,
    updated_at: String,
    extra_metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PersonaSharePayload {
    schema_version: String,
    floor_consent_preference: Option<serde_json::Value>,
    entities: Vec<Entity>,
    entity_facts: Vec<SharedEntityFact>,
    voice_profile_entries: Vec<SharedVoiceProfileEntry>,
}

// ---------------------------------------------------------------------------
// Payload construction
// ---------------------------------------------------------------------------

async fn load_shared_entity_facts(
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

async fn load_shared_voice_profile_entries(
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
    owner_user_id: &str,
    source_persona_id: &str,
    owner_key_hex: &str,
    recipient_user_id: &str,
) -> Result<String, PersonaSharingError> {
    let persona = persona_store::get_persona_for_user(owner_user_id, source_persona_id)
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

    let recipient_public_key = sharing_keypair::get_public_key(recipient_user_id)
        .await?
        .ok_or_else(|| {
            PersonaSharingError::RecipientHasNoSharingKey(recipient_user_id.to_owned())
        })?;
    let envelope = sharing_keypair::encrypt_to_public_key(&recipient_public_key, &plaintext)?;

    let id = uuid::Uuid::new_v4().to_string();
    let created_at = crate::providers::utils::now();

    let mut shared_conn = open_shared_db().await?;
    sqlx::query(
        "INSERT INTO pending_persona_shares
         (id, recipient_user_id, source_persona_id, source_persona_display_name,
          payload_schema_version, encrypted_payload, status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, 'pending', ?)",
    )
    .bind(&id)
    .bind(recipient_user_id)
    .bind(source_persona_id)
    .bind(&persona.display_name)
    .bind(PERSONA_SHARE_PAYLOAD_SCHEMA_VERSION)
    .bind(hex_encode(&envelope))
    .bind(&created_at)
    .execute(&mut shared_conn)
    .await?;

    Ok(id)
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

    /// Creates a user + one persona owned by that user, returning
    /// (user_id, persona_id, key_hex, sharing_private_key). Mirrors
    /// group_invitations.rs's own test fixture convention.
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

    #[tokio::test]
    async fn round_trip_send_produces_a_decryptable_matching_payload() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona("Alice", 0x11).await;
        let (recipient_id, _recipient_persona, _, recipient_private_key) =
            make_user_with_persona("Bob", 0x22).await;

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

        let share_id = send_persona_share(&owner_id, &owner_persona, &owner_key_hex, &recipient_id)
            .await
            .expect("send_persona_share must succeed");

        let mut shared_conn = open_shared_db().await.unwrap();
        let row = sqlx::query(
            "SELECT recipient_user_id, source_persona_id, status, encrypted_payload
             FROM pending_persona_shares WHERE id = ?",
        )
        .bind(&share_id)
        .fetch_one(&mut shared_conn)
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
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona("Alice", 0x33).await;
        let (recipient_id, _, _, recipient_private_key) = make_user_with_persona("Bob", 0x44).await;

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

        let share_id = send_persona_share(&owner_id, &owner_persona, &owner_key_hex, &recipient_id)
            .await
            .unwrap();

        let mut shared_conn = open_shared_db().await.unwrap();
        let encrypted_payload: String =
            sqlx::query_scalar("SELECT encrypted_payload FROM pending_persona_shares WHERE id = ?")
                .bind(&share_id)
                .fetch_one(&mut shared_conn)
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
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona("Alice", 0x55).await;
        let (recipient_id, _, _, recipient_private_key) = make_user_with_persona("Bob", 0x66).await;

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

        let share_id = send_persona_share(&owner_id, &owner_persona, &owner_key_hex, &recipient_id)
            .await
            .unwrap();

        let mut shared_conn = open_shared_db().await.unwrap();
        let encrypted_payload: String =
            sqlx::query_scalar("SELECT encrypted_payload FROM pending_persona_shares WHERE id = ?")
                .bind(&share_id)
                .fetch_one(&mut shared_conn)
                .await
                .unwrap();
        let payload = decrypt_payload(&recipient_private_key, &encrypted_payload).await;

        assert!(payload.entity_facts.is_empty());
    }

    #[tokio::test]
    async fn global_voice_profile_entry_is_excluded() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona("Alice", 0x77).await;
        let (recipient_id, _, _, recipient_private_key) = make_user_with_persona("Bob", 0x88).await;

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

        let share_id = send_persona_share(&owner_id, &owner_persona, &owner_key_hex, &recipient_id)
            .await
            .unwrap();

        let mut shared_conn = open_shared_db().await.unwrap();
        let encrypted_payload: String =
            sqlx::query_scalar("SELECT encrypted_payload FROM pending_persona_shares WHERE id = ?")
                .bind(&share_id)
                .fetch_one(&mut shared_conn)
                .await
                .unwrap();
        let payload = decrypt_payload(&recipient_private_key, &encrypted_payload).await;

        assert!(payload.voice_profile_entries.is_empty());
    }

    #[tokio::test]
    async fn persona_not_owned_by_caller_is_rejected() {
        let _env = setup().await;
        let (owner_id, _, owner_key_hex, _) = make_user_with_persona("Alice", 0x99).await;
        let (_other_owner, other_persona, _, _) = make_user_with_persona("Carol", 0xAA).await;
        let (recipient_id, _, _, _) = make_user_with_persona("Bob", 0xBB).await;

        let result =
            send_persona_share(&owner_id, &other_persona, &owner_key_hex, &recipient_id).await;

        assert!(matches!(
            result,
            Err(PersonaSharingError::PersonaNotOwnedByUser(_, _))
        ));
    }

    #[tokio::test]
    async fn recipient_with_no_sharing_key_is_rejected() {
        let _env = setup().await;
        let (owner_id, owner_persona, owner_key_hex, _) =
            make_user_with_persona("Alice", 0xCC).await;

        let result = send_persona_share(
            &owner_id,
            &owner_persona,
            &owner_key_hex,
            "nonexistent-user-id",
        )
        .await;

        assert!(matches!(
            result,
            Err(PersonaSharingError::RecipientHasNoSharingKey(_))
        ));
    }
}
