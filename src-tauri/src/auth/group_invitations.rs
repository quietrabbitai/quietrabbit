// src-tauri/src/auth/group_invitations.rs
//
// group.db invitation flow: encrypt, transport, accept (items.id=284, third
// of six items.id=266 sub-items). Wires together three pieces items.id=283
// and items.id=289 built as foundation only: auth::sharing_keypair's
// encrypt/decrypt primitives, shared.db's pending_group_invitations table
// (schema/shared_003.sql), and auth::registry::GroupKeyRegistry.
//
// MODULE PLACEMENT: not persistence/ (that tier is CRUD-only against a
// single db, e.g. persona_store.rs) and not auth/user_store.rs (also
// CRUD-only) -- this module composes crypto (sharing_keypair) + a resident
// registry (GroupKeyRegistry) + shared.db CRUD, the same shape
// commands/auth.rs::login() composes user_store + sharing_keypair +
// KeyRegistry. login()'s independently-opening-connections-per-call style
// (no shared transaction across composed store calls) is the real
// precedent this module follows -- but there is no #[tauri::command] layer
// here to hold that composition (see SCOPE below), so it lives as a plain
// library module instead, alongside sharing_keypair.rs.
//
// SCOPE, decided this session: library functions only, no #[tauri::command].
// GROUP_DB_DESIGN_20260802.md Section 4 item 1 explicitly defers "concrete
// invitation-flow mechanics -- how a person is addressed/found to invite
// into a group at all... UX-level, not architecture-blocking." No invite-UI
// item exists yet to specify what an IPC command here should even look like.
// group_store.rs::open_group_db (items.id=283) set the exact precedent:
// build the primitive, mark it #[allow(dead_code)], flag it for "its first
// real caller" later. Every pub fn below follows that same convention.
//
// PERSONA -> USER_ID RESOLUTION (user_personas, shared_001.sql): the table
// is schema-level many-to-many (PRIMARY KEY(user_id, persona_id)), and
// persistence::persona_store::add_user_to_persona() already exists to add a
// second user to a persona -- but no #[tauri::command] anywhere in this
// codebase calls it (checked this session). So today, in practice, every
// persona has exactly one owning row -- the one create_persona() inserts
// atomically alongside the persona itself. That single-row case is real but
// not schema-guaranteed, so resolve_persona_owner() below queries and
// checks explicitly rather than assuming it: 0 rows is a real error
// (persona exists with no owner -- should be impossible given
// create_persona's atomicity, but not assumed), and >1 DISTINCT user_id
// rows fails loudly (PersonaOwnershipAmbiguous) rather than silently taking
// the first one. That second case is unreachable today (nothing calls
// add_user_to_persona), but the schema and the store API both genuinely
// allow it, so this function does not pretend otherwise.
//
// FORMER GAP, closed by items.id=290 (decisions.id=718): GroupKeyRegistry is
// explicitly volatile Tauri-managed state (see its own header in
// auth/registry.rs) -- after accept_invitation populates it, that in-memory
// slot used to be the ONLY place the decrypted group key existed. On app
// restart or logout, the key was gone, pending_group_invitations.status
// still read 'accepted', and nothing re-derived it -- the invitation
// envelope itself isn't a durable secret store, it's only decryptable via
// the exact flow that already ran once. accept_invitation now also writes
// the key to personal.db's group_keys table (group_key_store.rs) right
// after populating the registry; commands::auth::finish_login reads it back
// at login to rehydrate the registry.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros. shared.db
// is unencrypted -- no PRAGMA key required (same as persona_store.rs).

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;
use x25519_dalek::StaticSecret;

use crate::auth::kdf;
use crate::auth::registry::{GroupKeyRegistry, UnlockedGroupKey};
use crate::auth::sharing_keypair::{self, SharingKeypairError};
use crate::persistence::group_key_store;
use crate::persistence::personal_store::PersonalStoreError;

#[derive(Debug, Error)]
pub enum GroupInvitationError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Sharing keypair error: {0}")]
    Sharing(#[from] SharingKeypairError),
    #[error("Persona '{0}' has no owning user in user_personas")]
    PersonaHasNoOwningUser(String),
    #[error("Persona '{0}' resolves to more than one owning user -- ambiguous")]
    PersonaOwnershipAmbiguous(String),
    #[error("User '{0}' has no registered sharing public key")]
    RecipientHasNoSharingKey(String),
    #[error("Invitation '{0}' has a stored encrypted_group_key that is not valid hex")]
    CorruptStoredEnvelope(String),
    #[error("Invitation '{0}' decrypted to a group key of the wrong length")]
    CorruptGroupKey(String),
    #[error("Invitation '{0}' not found")]
    NotFound(String),
    #[error("Invitation '{0}' is not pending (status: {1})")]
    NotPending(String, String),
    #[error("Failed to durably persist group key: {0}")]
    PersonalStore(#[from] PersonalStoreError),
}

// ---------------------------------------------------------------------------
// DB opener (shared.db -- unencrypted)
// ---------------------------------------------------------------------------
// Duplicated rather than reused -- same reasoning user_store.rs's own header
// gives: different error type per module, ~12-line zero-divergence-risk
// helper, not worth coupling.

async fn open_shared_db() -> Result<SqliteConnection, GroupInvitationError> {
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

fn hex_decode(context: &str, s: &str) -> Result<Vec<u8>, GroupInvitationError> {
    if !s.len().is_multiple_of(2) {
        return Err(GroupInvitationError::CorruptStoredEnvelope(
            context.to_owned(),
        ));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| GroupInvitationError::CorruptStoredEnvelope(context.to_owned()))
        })
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// persona -> owning user_id resolution
// ---------------------------------------------------------------------------

/// pub(crate), not private: items.id=288's group_membership module also
/// needs to resolve a remaining member's owning account before it can look
/// up their public key for a rotation envelope -- same lookup this module's
/// own send_invitation already does, no reason to duplicate the query.
pub(crate) async fn resolve_persona_owner(
    persona_id: &str,
    conn: &mut SqliteConnection,
) -> Result<String, GroupInvitationError> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT DISTINCT user_id FROM user_personas WHERE persona_id = ?")
            .bind(persona_id)
            .fetch_all(&mut *conn)
            .await?;

    match rows.len() {
        0 => Err(GroupInvitationError::PersonaHasNoOwningUser(
            persona_id.to_owned(),
        )),
        1 => Ok(rows.into_iter().next().expect("length checked above").0),
        _ => Err(GroupInvitationError::PersonaOwnershipAmbiguous(
            persona_id.to_owned(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Data type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingInvitation {
    pub id: String,
    pub group_id: String,
    pub group_display_name: String,
    pub sender_label: String,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// Send a group invitation: resolve `recipient_persona_id`'s owning
/// account's public key (via user_personas), encrypt `group_symmetric_key`
/// to it, and write a new pending_group_invitations row. Returns the new
/// invitation's id.
///
/// Presupposes the group's symmetric key already exists (generated by
/// whoever created the group's group.db, group_store.rs::open_group_db --
/// items.id=291's auth::group_creation::create_group, once it landed;
/// flagged in this item's own handoff as a gap at the time this was
/// written, same shape as items.id=289 turning out to be a hidden
/// prerequisite for this item).
#[allow(dead_code)] // items.id=284: ahead of its first real caller (invite-UI item, not yet scoped)
pub async fn send_invitation(
    recipient_persona_id: &str,
    group_id: &str,
    group_display_name: &str,
    sender_label: &str,
    group_symmetric_key: &[u8; kdf::MASTER_KEY_LEN],
) -> Result<String, GroupInvitationError> {
    let mut conn = open_shared_db().await?;
    let owner_user_id = resolve_persona_owner(recipient_persona_id, &mut conn).await?;

    // Own connection, matching login()'s independently-opening-calls style
    // -- no shared transaction with the INSERT below, none needed (a public
    // key lookup and an invitation write are not one logical operation).
    let recipient_public_key = sharing_keypair::get_public_key(&owner_user_id)
        .await?
        .ok_or_else(|| GroupInvitationError::RecipientHasNoSharingKey(owner_user_id.clone()))?;

    let envelope =
        sharing_keypair::encrypt_to_public_key(&recipient_public_key, group_symmetric_key)?;

    let id = uuid::Uuid::new_v4().to_string();
    let created_at = crate::providers::utils::now();

    sqlx::query(
        "INSERT INTO pending_group_invitations
         (id, recipient_persona_id, group_id, group_display_name,
          encrypted_group_key, sender_label, status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, 'pending', ?)",
    )
    .bind(&id)
    .bind(recipient_persona_id)
    .bind(group_id)
    .bind(group_display_name)
    .bind(hex_encode(&envelope))
    .bind(sender_label)
    .bind(&created_at)
    .execute(&mut conn)
    .await?;

    Ok(id)
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[allow(dead_code)] // items.id=284: ahead of its first real caller (invite-UI item, not yet scoped)
pub async fn list_pending_invitations(
    recipient_persona_id: &str,
) -> Result<Vec<PendingInvitation>, GroupInvitationError> {
    let mut conn = open_shared_db().await?;

    let rows = sqlx::query(
        "SELECT id, group_id, group_display_name, sender_label, created_at
         FROM pending_group_invitations
         WHERE recipient_persona_id = ? AND status = 'pending'
         ORDER BY created_at",
    )
    .bind(recipient_persona_id)
    .fetch_all(&mut conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| PendingInvitation {
            id: r.get("id"),
            group_id: r.get("group_id"),
            group_display_name: r.get("group_display_name"),
            sender_label: r.get("sender_label"),
            created_at: r.get("created_at"),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Accept / decline
// ---------------------------------------------------------------------------

struct InvitationRow {
    group_id: String,
    encrypted_group_key: String,
}

async fn fetch_pending_invitation(
    invitation_id: &str,
    recipient_persona_id: &str,
    conn: &mut SqliteConnection,
) -> Result<InvitationRow, GroupInvitationError> {
    let row = sqlx::query(
        "SELECT group_id, encrypted_group_key, status
         FROM pending_group_invitations
         WHERE id = ? AND recipient_persona_id = ?",
    )
    .bind(invitation_id)
    .bind(recipient_persona_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| GroupInvitationError::NotFound(invitation_id.to_owned()))?;

    let status: String = row.get("status");
    if status != "pending" {
        return Err(GroupInvitationError::NotPending(
            invitation_id.to_owned(),
            status,
        ));
    }

    Ok(InvitationRow {
        group_id: row.get("group_id"),
        encrypted_group_key: row.get("encrypted_group_key"),
    })
}

/// Accept a pending invitation: decrypt its envelope with the recipient's
/// own resident sharing private key (KeyRegistry::with_key exposes it as
/// UnlockedKey.sharing_private_key -- reconstruct via
/// StaticSecret::from(unlocked_key.sharing_private_key)), populate
/// GroupKeyRegistry, durably persist the key to personal.db (items.id=290,
/// decisions.id=718), and mark the row accepted.
///
/// ORDERING, deliberate: decrypt (step 2) happens before ANY row mutation --
/// on tamper/wrong-key it returns Err(GroupInvitationError::Sharing(
/// SharingKeypairError::DecryptionFailed)) unchanged and the row is left
/// exactly as it was (still 'pending'), not silently marked accepted or
/// declined. The personal.db write (items.id=290) happens after the
/// registry populate but BEFORE the status UPDATE below, and its failure
/// propagates as Err -- durable storage is this function's actual point,
/// not a nice-to-have, so a write failure must leave the invitation
/// 'pending' for retry rather than silently completing without it. No
/// SAVEPOINT wraps registry populate / personal.db write / status UPDATE:
/// three different storage systems (memory, personal.db, shared.db), so no
/// SAVEPOINT could make them atomic together regardless. What actually
/// makes this safe is idempotency, not atomicity -- decrypt_own_envelope is
/// a pure function of the stored ciphertext and the caller's private key,
/// and group_key_store::save_group_key upserts on group_id, so a crash at
/// any point before the final status UPDATE just leaves the row 'pending';
/// retrying accept_invitation on that still-pending row reproduces the
/// identical group key and re-overwrites the same registry slot / personal.db
/// row, rather than producing a different or corrupted result.
#[allow(dead_code)] // items.id=284: ahead of its first real caller (invite-UI item, not yet scoped)
pub async fn accept_invitation(
    invitation_id: &str,
    recipient_persona_id: &str,
    sharing_private_key: &StaticSecret,
    group_key_registry: &GroupKeyRegistry,
    personal_key_hex: &str,
) -> Result<(), GroupInvitationError> {
    let mut conn = open_shared_db().await?;
    let invitation =
        fetch_pending_invitation(invitation_id, recipient_persona_id, &mut conn).await?;

    let envelope = hex_decode(invitation_id, &invitation.encrypted_group_key)?;
    let plaintext = sharing_keypair::decrypt_own_envelope(sharing_private_key, &envelope)?;

    let group_key: [u8; kdf::MASTER_KEY_LEN] = plaintext
        .try_into()
        .map_err(|_| GroupInvitationError::CorruptGroupKey(invitation_id.to_owned()))?;

    let unlocked_at = crate::providers::utils::now();
    group_key_registry
        .replace(
            recipient_persona_id,
            &invitation.group_id,
            UnlockedGroupKey {
                group_id: invitation.group_id.clone(),
                group_key,
                unlocked_at: unlocked_at.clone(),
            },
        )
        .await;

    // items.id=290, decisions.id=718: durable backing for the registry
    // entry just populated above -- without this, the key above is gone the
    // instant this process restarts, with pending_group_invitations still
    // reading 'accepted' and no way to re-derive it. Hard requirement (see
    // this fn's own doc comment on ORDERING) -- a failure here must abort
    // before the row is marked accepted, not complete silently without it.
    let owner_user_id = resolve_persona_owner(recipient_persona_id, &mut conn).await?;
    group_key_store::save_group_key(
        &owner_user_id,
        recipient_persona_id,
        personal_key_hex,
        &invitation.group_id,
        &crate::auth::registry::key_hex(&group_key),
        &unlocked_at,
    )
    .await?;

    sqlx::query(
        "UPDATE pending_group_invitations
         SET status = 'accepted', responded_at = ?
         WHERE id = ?",
    )
    .bind(crate::providers::utils::now())
    .bind(invitation_id)
    .execute(&mut conn)
    .await?;

    // items.id=287 "app-start" pull cadence, per-group: the registry starts
    // empty at process boot (nothing is unlocked before login), so a
    // literal process-boot pull would find nothing to poll. The moment that
    // actually matters is right here -- a group's key just became resident
    // for the first time this session. main.rs's periodic timer covers the
    // steady-state case; this covers the immediate one. Best-effort, same
    // as every other push/pull call site -- a sync failure must not turn
    // an otherwise-successful accept_invitation into an Err.
    let group_key_hex = crate::auth::registry::key_hex(&group_key);
    if let Err(e) = crate::group_sync::engine::pull_if_newer(
        recipient_persona_id,
        &invitation.group_id,
        &group_key_hex,
    )
    .await
    {
        log::warn!(
            "accept_invitation: post-accept pull failed for persona={recipient_persona_id} \
             group={}: {e}",
            invitation.group_id
        );
    }
    if let Err(e) = crate::group_sync::engine::pull_permissions_if_newer(
        recipient_persona_id,
        &invitation.group_id,
        &group_key_hex,
    )
    .await
    {
        log::warn!(
            "accept_invitation: post-accept permissions pull failed for \
             persona={recipient_persona_id} group={}: {e}",
            invitation.group_id
        );
    }

    Ok(())
}

/// Decline a pending invitation. No decryption -- just a status update.
#[allow(dead_code)] // items.id=284: ahead of its first real caller (invite-UI item, not yet scoped)
pub async fn decline_invitation(
    invitation_id: &str,
    recipient_persona_id: &str,
) -> Result<(), GroupInvitationError> {
    let mut conn = open_shared_db().await?;
    // fetch_pending_invitation's NotFound/NotPending distinction applies
    // identically here -- reused rather than duplicating the same
    // SELECT-and-classify logic for a second time.
    fetch_pending_invitation(invitation_id, recipient_persona_id, &mut conn).await?;

    sqlx::query(
        "UPDATE pending_group_invitations
         SET status = 'declined', responded_at = ?
         WHERE id = ?",
    )
    .bind(crate::providers::utils::now())
    .bind(invitation_id)
    .execute(&mut conn)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::persona_store;
    use crate::test_support::ENV_MUTEX;

    const PERSONAL_KEY_HEX: &str =
        "aabbccddeeff00112233445566778899aabbccddeeff0011223344556677aa";

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
    /// (user_id, persona_id, sharing_private_key). Mirrors user_store.rs /
    /// persona_store.rs test fixture conventions.
    async fn make_user_with_persona(
        display_name: &str,
        master_key_fill: u8,
    ) -> (String, String, StaticSecret) {
        let user_id = uuid::Uuid::new_v4().to_string();
        let master_key = [master_key_fill; kdf::MASTER_KEY_LEN];
        let (sharing_private_key, sharing_public_key) =
            sharing_keypair::derive_sharing_keypair(&master_key, &user_id);

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
        persona_store::create_persona(&persona_id, "Test Persona", "personal", &user_id, None)
            .await
            .expect("create_persona must succeed");

        (user_id, persona_id, sharing_private_key)
    }

    #[tokio::test]
    async fn round_trip_accept_populates_group_key_registry() {
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Alice", 0x11).await;
        let (_recipient_user, recipient_persona, recipient_private_key) =
            make_user_with_persona("Bob", 0x22).await;

        let group_id = "group-1";
        let group_key = [0x99u8; kdf::MASTER_KEY_LEN];

        let invitation_id = send_invitation(
            &recipient_persona,
            group_id,
            "Family Documents",
            &sender_persona,
            &group_key,
        )
        .await
        .expect("send_invitation must succeed");

        let pending = list_pending_invitations(&recipient_persona)
            .await
            .expect("list_pending_invitations must succeed");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, invitation_id);
        assert_eq!(pending[0].group_display_name, "Family Documents");

        let registry = GroupKeyRegistry::default();
        accept_invitation(
            &invitation_id,
            &recipient_persona,
            &recipient_private_key,
            &registry,
            PERSONAL_KEY_HEX,
        )
        .await
        .expect("accept_invitation must succeed");

        let stored_key = registry
            .with_key(&recipient_persona, group_id, |k| k.group_key)
            .await;
        assert_eq!(stored_key, Some(group_key));

        // Accepted invitation must no longer show up as pending.
        let pending_after = list_pending_invitations(&recipient_persona)
            .await
            .expect("list_pending_invitations must succeed");
        assert!(pending_after.is_empty());
    }

    #[tokio::test]
    async fn accept_invitation_durably_persists_the_group_key_to_personal_db() {
        // items.id=290, decisions.id=718: the whole point of the durable
        // write -- prove it actually lands in personal.db's group_keys
        // table, not just the in-memory registry.
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Alice", 0xA1).await;
        let (recipient_user, recipient_persona, recipient_private_key) =
            make_user_with_persona("Bob", 0xA2).await;

        let group_id = "group-durable-1";
        let group_key = [0xE1u8; kdf::MASTER_KEY_LEN];

        let invitation_id = send_invitation(
            &recipient_persona,
            group_id,
            "Durable Test Group",
            &sender_persona,
            &group_key,
        )
        .await
        .expect("send_invitation must succeed");

        let registry = GroupKeyRegistry::default();
        accept_invitation(
            &invitation_id,
            &recipient_persona,
            &recipient_private_key,
            &registry,
            PERSONAL_KEY_HEX,
        )
        .await
        .expect("accept_invitation must succeed");

        let rows = crate::persistence::group_key_store::list_group_keys(
            &recipient_user,
            &recipient_persona,
            PERSONAL_KEY_HEX,
        )
        .await
        .expect("list_group_keys must succeed");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group_id, group_id);
        assert_eq!(
            rows[0].group_key_hex,
            crate::auth::registry::key_hex(&group_key)
        );
    }

    #[tokio::test]
    async fn decline_updates_status_without_touching_registry() {
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Alice", 0x33).await;
        let (_recipient_user, recipient_persona, _recipient_private_key) =
            make_user_with_persona("Bob", 0x44).await;

        let group_key = [0x77u8; kdf::MASTER_KEY_LEN];
        let invitation_id = send_invitation(
            &recipient_persona,
            "group-2",
            "Business Docs",
            &sender_persona,
            &group_key,
        )
        .await
        .expect("send_invitation must succeed");

        decline_invitation(&invitation_id, &recipient_persona)
            .await
            .expect("decline_invitation must succeed");

        let registry = GroupKeyRegistry::default();
        assert!(!registry.is_occupied(&recipient_persona, "group-2").await);

        let pending = list_pending_invitations(&recipient_persona)
            .await
            .expect("list_pending_invitations must succeed");
        assert!(
            pending.is_empty(),
            "declined invitation must not be pending"
        );
    }

    #[tokio::test]
    async fn tampered_envelope_fails_with_decryption_failed_and_leaves_row_pending() {
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Alice", 0x55).await;
        let (_recipient_user, recipient_persona, recipient_private_key) =
            make_user_with_persona("Bob", 0x66).await;

        let group_key = [0x88u8; kdf::MASTER_KEY_LEN];
        let invitation_id = send_invitation(
            &recipient_persona,
            "group-3",
            "Tampered Group",
            &sender_persona,
            &group_key,
        )
        .await
        .expect("send_invitation must succeed");

        // Flip a bit inside the stored hex-encoded envelope.
        let mut conn = open_shared_db().await.unwrap();
        let (encrypted_hex,): (String,) = sqlx::query_as(
            "SELECT encrypted_group_key FROM pending_group_invitations WHERE id = ?",
        )
        .bind(&invitation_id)
        .fetch_one(&mut conn)
        .await
        .unwrap();
        let mut bytes = hex_decode("test", &encrypted_hex).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        sqlx::query("UPDATE pending_group_invitations SET encrypted_group_key = ? WHERE id = ?")
            .bind(hex_encode(&bytes))
            .bind(&invitation_id)
            .execute(&mut conn)
            .await
            .unwrap();

        let registry = GroupKeyRegistry::default();
        let result = accept_invitation(
            &invitation_id,
            &recipient_persona,
            &recipient_private_key,
            &registry,
            PERSONAL_KEY_HEX,
        )
        .await;

        assert!(
            matches!(
                result,
                Err(GroupInvitationError::Sharing(
                    SharingKeypairError::DecryptionFailed
                ))
            ),
            "expected DecryptionFailed, got {result:?}"
        );

        // Row must still be pending -- accept must not have mutated it.
        let pending = list_pending_invitations(&recipient_persona)
            .await
            .expect("list_pending_invitations must succeed");
        assert_eq!(pending.len(), 1, "tampered accept must leave row pending");
        assert!(!registry.is_occupied(&recipient_persona, "group-3").await);
    }

    #[tokio::test]
    async fn ambiguous_persona_ownership_is_rejected_not_silently_resolved() {
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Alice", 0x77).await;
        let (_recipient_user, recipient_persona, _) = make_user_with_persona("Bob", 0x88).await;

        // Prove the schema-reachable multi-owner case is actually exercised,
        // not just asserted in a comment: add a second user to the
        // recipient persona via the existing (currently uncalled-by-any-
        // command) persona_store::add_user_to_persona.
        let second_user_id = uuid::Uuid::new_v4().to_string();
        let master_key = [0x99u8; kdf::MASTER_KEY_LEN];
        let (_priv, pub_key) =
            sharing_keypair::derive_sharing_keypair(&master_key, &second_user_id);
        crate::auth::user_store::create_user(
            &second_user_id,
            "Carol",
            "user",
            false,
            b"another-salt",
            1024,
            1,
            1,
            pub_key.as_bytes(),
        )
        .await
        .expect("create_user must succeed");
        persona_store::add_user_to_persona(&second_user_id, &recipient_persona)
            .await
            .expect("add_user_to_persona must succeed");

        let group_key = [0xAAu8; kdf::MASTER_KEY_LEN];
        let result = send_invitation(
            &recipient_persona,
            "group-4",
            "Ambiguous Group",
            &sender_persona,
            &group_key,
        )
        .await;

        assert!(
            matches!(
                result,
                Err(GroupInvitationError::PersonaOwnershipAmbiguous(_))
            ),
            "expected PersonaOwnershipAmbiguous, got {result:?}"
        );
    }

    #[tokio::test]
    async fn accept_unknown_invitation_id_is_not_found() {
        let _env = setup().await;
        let (_user, persona_id, private_key) = make_user_with_persona("Solo", 0xBB).await;
        let registry = GroupKeyRegistry::default();

        let result = accept_invitation(
            "nonexistent-id",
            &persona_id,
            &private_key,
            &registry,
            PERSONAL_KEY_HEX,
        )
        .await;
        assert!(matches!(result, Err(GroupInvitationError::NotFound(_))));
    }

    #[tokio::test]
    async fn accept_already_accepted_invitation_is_not_pending() {
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Alice", 0xCC).await;
        let (_recipient_user, recipient_persona, recipient_private_key) =
            make_user_with_persona("Bob", 0xDD).await;

        let group_key = [0x11u8; kdf::MASTER_KEY_LEN];
        let invitation_id = send_invitation(
            &recipient_persona,
            "group-5",
            "Twice",
            &sender_persona,
            &group_key,
        )
        .await
        .expect("send_invitation must succeed");

        let registry = GroupKeyRegistry::default();
        accept_invitation(
            &invitation_id,
            &recipient_persona,
            &recipient_private_key,
            &registry,
            PERSONAL_KEY_HEX,
        )
        .await
        .expect("first accept must succeed");

        let second = accept_invitation(
            &invitation_id,
            &recipient_persona,
            &recipient_private_key,
            &registry,
            PERSONAL_KEY_HEX,
        )
        .await;
        assert!(matches!(
            second,
            Err(GroupInvitationError::NotPending(_, _))
        ));
    }

    #[test]
    fn hex_decode_rejects_odd_length() {
        let result = hex_decode("inv-1", "abc");
        assert!(matches!(
            result,
            Err(GroupInvitationError::CorruptStoredEnvelope(_))
        ));
    }

    #[test]
    fn hex_decode_rejects_non_hex_characters() {
        let result = hex_decode("inv-1", "zzzz");
        assert!(matches!(
            result,
            Err(GroupInvitationError::CorruptStoredEnvelope(_))
        ));
    }

    #[test]
    fn hex_decode_round_trips_with_hex_encode() {
        let original = vec![0x00, 0xFF, 0x42, 0xAB, 0xCD];
        let encoded = hex_encode(&original);
        let decoded = hex_decode("inv-1", &encoded).unwrap();
        assert_eq!(decoded, original);
    }
}
