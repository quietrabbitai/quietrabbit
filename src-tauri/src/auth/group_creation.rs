// src-tauri/src/auth/group_creation.rs
//
// items.id=291: the missing first step of household/business group
// sharing. Every other piece of this feature (group_store.rs's document
// CRUD, group_invitations.rs's send/accept, group_membership.rs's
// departure/rotation) presupposes a group's symmetric key and the
// creator's own group.db already exist -- nothing anywhere generated
// either. send_invitation's own doc comment and group_membership.rs's
// module header both flagged this explicitly as a gap; this module closes
// it.
//
// ROSTER RECONCILIATION: group_membership::remaining_members derives a
// group's roster purely from pending_group_invitations rows with
// status='accepted' -- a creator never receives an invitation, so that
// derivation had a documented false negative for creators. Resolved here
// WITHOUT touching remaining_members, remove_member, or the schema: this
// module writes the creator their own pending_group_invitations row,
// already status='accepted', with encrypted_group_key populated by
// encrypting the freshly-generated group key to the creator's OWN sharing
// public key (the same primitives send_invitation already uses on others,
// applied reflexively). Every user already has a registered public key
// from account creation (user_store::create_user's SAVEPOINT populates
// user_sharing_keys), so this needs no new precondition. Every existing
// membership consumer reads through this exact table/status, so this
// single row is sufficient: remaining_members picks the creator up for
// free, and remove_member_inner's rotation-envelope loop will now
// correctly queue the creator a rotation envelope on a later departure --
// previously silently skipped.
//
// A real groups/membership table with an owner/role column was considered
// and rejected: shared_006.sql's own header is explicit that a durable
// membership table is deliberately out of this item's scope, per Jason's
// brief -- a separately-decided, larger piece of work, not a prerequisite
// for creation to behave correctly.
//
// ORDERING (WHY): steps below run in an order chosen so a failure never
// leaves a *publicly visible* membership signal (the shared.db row) without
// the local state to back it up -- same philosophy accept_invitation
// already follows (volatile registry -> durable personal.db write -> only
// then the shared.db signal). No cross-file atomicity is possible here
// (SQLCipher files are independent, same limitation remove_member's own
// doc comment documents for registry-vs-shared.db) -- ordering is the only
// available safety net.
//
// CALLER MUST BE LOGGED IN AS THE CREATOR (hard requirement, not
// best-effort like remove_member's own-departure check): durably persisting
// the group key requires opening personal.db under a resident master key,
// so "not logged in" / "persona not owned by the resident account" are
// real errors here, not silently-skipped branches.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros. shared.db
// is unencrypted -- no PRAGMA key required (same as every sibling module in
// this feature).

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::SqliteConnection;
use thiserror::Error;

use crate::auth::group_invitations::{self, GroupInvitationError};
use crate::auth::kdf;
use crate::auth::registry::{key_hex, GroupKeyRegistry, KeyRegistry, UnlockedGroupKey};
use crate::auth::sharing_keypair::{self, SharingKeypairError};
use crate::persistence::group_key_store;
use crate::persistence::group_store::{self, GroupStoreError};
use crate::persistence::personal_store::PersonalStoreError;

#[derive(Debug, Error)]
pub enum GroupCreationError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Group store error: {0}")]
    Store(#[from] GroupStoreError),
    #[error("Group invitation error: {0}")]
    Invitation(#[from] GroupInvitationError),
    #[error("Sharing keypair error: {0}")]
    Sharing(#[from] SharingKeypairError),
    #[error("Failed to durably persist group key: {0}")]
    PersonalStore(#[from] PersonalStoreError),
    #[error("Could not generate random bytes: {0}")]
    RandomSource(String),
    #[error("No account is currently logged in")]
    NotLoggedIn,
    #[error("Persona '{0}' is not owned by the currently logged-in account")]
    PersonaNotOwnedByCurrentAccount(String),
    #[error("Account '{0}' has no registered sharing public key")]
    CreatorHasNoSharingKey(String),
}

// ---------------------------------------------------------------------------
// DB opener (shared.db -- unencrypted)
// ---------------------------------------------------------------------------
// Duplicated rather than reused -- same reasoning every sibling module in
// this feature already gives: different error type per module, ~12-line
// zero-divergence-risk helper, not worth coupling.

async fn open_shared_db() -> Result<SqliteConnection, GroupCreationError> {
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
// Create
// ---------------------------------------------------------------------------

/// Create a new group: generate its symmetric key and id, materialize the
/// creator's own local group.db, make the key resident and durable, and
/// give the creator a self-accepted pending_group_invitations row (see this
/// module's own header, ROSTER RECONCILIATION). Returns the new group_id.
pub async fn create_group(
    creator_persona_id: &str,
    group_display_name: &str,
    creator_label: &str,
    group_key_registry: &GroupKeyRegistry,
    key_registry: &KeyRegistry,
) -> Result<String, GroupCreationError> {
    // 1. Resolve the caller -- hard requirement, see module header.
    let (current_user_id, personal_key_hex) = key_registry
        .with_key(|k| (k.user_id.clone(), key_hex(&k.master_key)))
        .await
        .ok_or(GroupCreationError::NotLoggedIn)?;

    // 2. Verify the creator persona actually belongs to the resident
    // account -- structural precondition for steps 5/6 below, not a new
    // authorization layer (see module header).
    let mut shared_conn = open_shared_db().await?;
    let owner_user_id =
        group_invitations::resolve_persona_owner(creator_persona_id, &mut shared_conn).await?;
    if owner_user_id != current_user_id {
        return Err(GroupCreationError::PersonaNotOwnedByCurrentAccount(
            creator_persona_id.to_owned(),
        ));
    }

    // 3. Generate.
    let group_id = uuid::Uuid::new_v4().to_string();
    let mut group_key = [0u8; kdf::MASTER_KEY_LEN];
    getrandom::fill(&mut group_key)
        .map_err(|e| GroupCreationError::RandomSource(e.to_string()))?;
    let group_key_hex = key_hex(&group_key);

    // 4. Materialize the creator's own local group.db -- open_group_db
    // self-heals (migrates) a file that has never existed. Nothing further
    // to write into group.db itself: group_001.sql's own header is explicit
    // that a group's identity is carried by file path + shared.db, "not by
    // a row in here."
    let _conn = group_store::open_group_db(creator_persona_id, &group_id, &group_key_hex).await?;
    drop(_conn);

    // 5. Make the key resident, then durable -- same ordering
    // accept_invitation uses and for the same reason: a failure here must
    // not leave a registry entry with nothing backing it up across restart.
    let unlocked_at = crate::providers::utils::now();
    group_key_registry
        .replace(
            creator_persona_id,
            &group_id,
            UnlockedGroupKey {
                group_id: group_id.clone(),
                group_key,
                unlocked_at: unlocked_at.clone(),
            },
        )
        .await;

    group_key_store::save_group_key(
        &current_user_id,
        creator_persona_id,
        &personal_key_hex,
        &group_id,
        &group_key_hex,
        &unlocked_at,
    )
    .await?;

    // 6. Self-accepted invitation row -- written last, as the "this
    // membership is now real" signal (see module header, ROSTER
    // RECONCILIATION and ORDERING).
    let creator_public_key = sharing_keypair::get_public_key(&current_user_id)
        .await?
        .ok_or_else(|| GroupCreationError::CreatorHasNoSharingKey(current_user_id.clone()))?;
    let envelope = sharing_keypair::encrypt_to_public_key(&creator_public_key, &group_key)?;

    let invitation_id = uuid::Uuid::new_v4().to_string();
    let now = crate::providers::utils::now();
    sqlx::query(
        "INSERT INTO pending_group_invitations
         (id, recipient_persona_id, group_id, group_display_name,
          encrypted_group_key, sender_label, status, created_at, responded_at)
         VALUES (?, ?, ?, ?, ?, ?, 'accepted', ?, ?)",
    )
    .bind(&invitation_id)
    .bind(creator_persona_id)
    .bind(&group_id)
    .bind(group_display_name)
    .bind(hex_encode(&envelope))
    .bind(creator_label)
    .bind(&now)
    .bind(&now)
    .execute(&mut shared_conn)
    .await?;

    Ok(group_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::registry::UnlockedKey;
    use crate::auth::sharing_keypair;
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

    /// Creates a user + one persona owned by that user, and logs that user
    /// in (populates key_registry) -- returns (user_id, persona_id).
    /// Mirrors group_invitations.rs's own make_user_with_persona fixture,
    /// plus the login step create_group requires.
    async fn make_logged_in_user_with_persona(
        display_name: &str,
        master_key_fill: u8,
        key_registry: &KeyRegistry,
    ) -> (String, String) {
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

        key_registry
            .replace(UnlockedKey {
                user_id: user_id.clone(),
                master_key,
                sharing_private_key: sharing_private_key.to_bytes(),
                unlocked_at: crate::providers::utils::now(),
            })
            .await;

        (user_id, persona_id)
    }

    #[tokio::test]
    async fn create_group_materializes_the_creators_local_group_db() {
        let _env = setup().await;
        let key_registry = KeyRegistry::default();
        let group_key_registry = GroupKeyRegistry::default();
        let (_user_id, persona_id) =
            make_logged_in_user_with_persona("Alice", 0x11, &key_registry).await;

        let group_id = create_group(
            &persona_id,
            "The Household",
            "Alice",
            &group_key_registry,
            &key_registry,
        )
        .await
        .expect("create_group must succeed");

        let group_key_hex = group_key_registry
            .key_hex_for(&persona_id, &group_id)
            .await
            .expect("group key must be resident after creation");

        group_store::open_group_db(&persona_id, &group_id, &group_key_hex)
            .await
            .expect("the creator's own group.db must be openable under the returned key");
    }

    #[tokio::test]
    async fn create_group_durably_persists_the_group_key() {
        let _env = setup().await;
        let key_registry = KeyRegistry::default();
        let group_key_registry = GroupKeyRegistry::default();
        let (user_id, persona_id) =
            make_logged_in_user_with_persona("Bob", 0x22, &key_registry).await;

        let personal_key_hex = key_registry
            .personal_key_hex()
            .await
            .expect("key must be resident");

        let group_id = create_group(
            &persona_id,
            "The Household",
            "Bob",
            &group_key_registry,
            &key_registry,
        )
        .await
        .expect("create_group must succeed");

        let rows = group_key_store::list_group_keys(&user_id, &persona_id, &personal_key_hex)
            .await
            .expect("list_group_keys must succeed");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group_id, group_id);
    }

    #[tokio::test]
    async fn create_group_makes_the_creator_visible_to_remaining_members() {
        let _env = setup().await;
        let key_registry = KeyRegistry::default();
        let group_key_registry = GroupKeyRegistry::default();
        let (_user_id, persona_id) =
            make_logged_in_user_with_persona("Carol", 0x33, &key_registry).await;

        let group_id = create_group(
            &persona_id,
            "The Household",
            "Carol",
            &group_key_registry,
            &key_registry,
        )
        .await
        .expect("create_group must succeed");

        let mut conn = open_shared_db().await.expect("open shared.db");
        let remaining = crate::auth::group_membership::remaining_members(
            &group_id,
            "not-anyones-persona-id",
            &mut conn,
        )
        .await
        .expect("remaining_members must succeed");

        assert!(
            remaining.contains(&persona_id),
            "the creator's own self-accepted invitation row must make them visible to \
             remaining_members -- this is the roster-reconciliation fix this module exists for"
        );
    }

    #[tokio::test]
    async fn create_group_fails_when_not_logged_in() {
        let _env = setup().await;
        let key_registry = KeyRegistry::default();
        let group_key_registry = GroupKeyRegistry::default();

        let result = create_group(
            "some-persona-id",
            "The Household",
            "Nobody",
            &group_key_registry,
            &key_registry,
        )
        .await;

        assert!(matches!(result, Err(GroupCreationError::NotLoggedIn)));
    }

    #[tokio::test]
    async fn create_group_fails_for_a_persona_not_owned_by_the_resident_account() {
        let _env = setup().await;
        let key_registry = KeyRegistry::default();
        let group_key_registry = GroupKeyRegistry::default();

        // Logged in as Dave...
        let (_dave_user_id, _dave_persona_id) =
            make_logged_in_user_with_persona("Dave", 0x44, &key_registry).await;
        // ...but Erin's persona belongs to a different, not-currently-resident account.
        let erin_key_registry = KeyRegistry::default();
        let (_erin_user_id, erin_persona_id) =
            make_logged_in_user_with_persona("Erin", 0x55, &erin_key_registry).await;

        let result = create_group(
            &erin_persona_id,
            "The Household",
            "Dave",
            &group_key_registry,
            &key_registry,
        )
        .await;

        assert!(matches!(
            result,
            Err(GroupCreationError::PersonaNotOwnedByCurrentAccount(_))
        ));
    }

    /// Regression test for the bug create_group's self-accepted invitation
    /// row fixes (see this module's own header, ROSTER RECONCILIATION):
    /// before this item, a departing SECOND member's rotation would never
    /// reach the creator, since remaining_members couldn't see them. Prove
    /// it now does, end to end through remove_member's own real rotation
    /// path -- not just remaining_members in isolation.
    #[tokio::test]
    async fn a_second_members_departure_queues_the_creator_a_rotation_envelope() {
        let _env = setup().await;
        let creator_key_registry = KeyRegistry::default();
        let group_key_registry = GroupKeyRegistry::default();
        let (_creator_user_id, creator_persona_id) =
            make_logged_in_user_with_persona("Frank", 0x66, &creator_key_registry).await;

        let group_id = create_group(
            &creator_persona_id,
            "The Household",
            "Frank",
            &group_key_registry,
            &creator_key_registry,
        )
        .await
        .expect("create_group must succeed");

        // A second member, Grace, is invited and accepts -- reusing the
        // real group key resident in the creator's own registry.
        let group_key: [u8; kdf::MASTER_KEY_LEN] = group_key_registry
            .with_key(&creator_persona_id, &group_id, |k| k.group_key)
            .await
            .expect("group key must be resident after creation");

        let grace_key_registry = KeyRegistry::default();
        let (_grace_user_id, grace_persona_id) =
            make_logged_in_user_with_persona("Grace", 0x77, &grace_key_registry).await;
        let grace_master_key = [0x77u8; kdf::MASTER_KEY_LEN];
        let (grace_sharing_private_key, _) =
            sharing_keypair::derive_sharing_keypair(&grace_master_key, &_grace_user_id);

        let invitation_id = group_invitations::send_invitation(
            &grace_persona_id,
            &group_id,
            "The Household",
            "Frank",
            &group_key,
        )
        .await
        .expect("send_invitation must succeed");
        group_invitations::accept_invitation(
            &invitation_id,
            &grace_persona_id,
            &grace_sharing_private_key,
            &group_key_registry,
            &grace_key_registry.personal_key_hex().await.unwrap(),
        )
        .await
        .expect("accept_invitation must succeed");

        // Grace departs -- remove_member should now queue a rotation
        // envelope for the creator too, not just any other invited member.
        crate::auth::group_membership::remove_member(
            &group_id,
            &grace_persona_id,
            crate::auth::group_membership::DepartureReason::Left,
            "Frank",
            &group_key_registry,
            &KeyRegistry::default(),
        )
        .await
        .expect("remove_member must succeed");

        let mut conn = open_shared_db().await.unwrap();
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT recipient_persona_id FROM pending_group_key_rotations WHERE group_id = ?",
        )
        .bind(&group_id)
        .fetch_all(&mut conn)
        .await
        .unwrap();

        assert!(
            rows.iter().any(|(id,)| id == &creator_persona_id),
            "the creator must be queued a rotation envelope when a fellow member departs -- \
             previously impossible, since remaining_members couldn't see the creator at all"
        );
    }
}
