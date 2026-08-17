// src-tauri/src/auth/group_membership.rs
//
// Key rotation on member departure (items.id=288, group.db 266f -- last of
// six items.id=266 group.db sub-items). GROUP_DB_DESIGN_20260802.md Section
// 2.5: when a member (or their Persona) leaves or is removed from a group,
// generate a new group key, redistribute it to remaining members via the
// same asymmetric-keypair mechanism items.id=284's invitation flow already
// uses, re-encrypt group.db under the new key. Section 2.5 is explicit this
// is NOT fully scoped -- everything below is this item's own design work.
//
// TWO HALVES, deliberately split at the shared.db/local-file boundary:
//   remove_member: runs on the initiator's install. Records the departure,
//     derives the remaining-members roster, generates a fresh group key,
//     and queues one encrypted rotation envelope per remaining member in
//     shared.db (pending_group_key_rotations, schema/shared_006.sql). Does
//     NOT touch any group.db file directly, including the initiator's own,
//     if they happen to also be a remaining member -- see apply_pending_
//     rotations below for why that's a separate step, not folded in here.
//   apply_pending_rotations: runs on EACH remaining member's own install
//     (including the initiator's, if applicable), independently, whenever
//     it's next polled. Decrypts any pending envelope addressed to that
//     persona, rekeys that persona's own local group.db copy, swaps the
//     resident registry entry, and re-pushes that persona's owned documents
//     under the new key.
//
// WHY NOT do the local rekey synchronously inside remove_member for the
// initiator's own membership: every group.db is a separate per-(persona_id,
// group_id) local file (persistence/group_store.rs's own header) -- there
// is no "the" group.db this function could rekey once and be done.
// Redistribution (shared.db writes) and local rekey (per-install file writes)
// are different operations on different machines in the general case (a
// household's members each run their own QR install), so remove_member can
// only ever do the former; apply_pending_rotations is what makes the latter
// actually happen, on whichever install is polling at the time -- same
// split accept_invitation (shared.db write + registry populate) already
// has relative to this module's own group_invitations.rs sibling.
//
// TRIGGER, decided this session (no existing entry point to hook into --
// confirmed by exhaustive search, nothing named remove/leave/kick/departure
// exists anywhere in this codebase before this item): remove_member is
// called directly by commands::group::remove_group_member, which ships
// ahead of frontend, same as items.id=287's set_group_sync_folder --
// member removal has no internal "save"-style hook the way document CRUD
// did for 283-286; someone must always explicitly initiate it. Rotation
// APPLICATION is poll-driven: main.rs's existing periodic timer (items.id=
// 287's folder-sync loop) gained a call to apply_pending_rotations per
// resident persona, run before that tick's pull sweep -- a rotation must
// land before a pull can usefully decrypt anything pushed under the new
// key. If a remaining member's OLD key isn't currently resident in
// GroupKeyRegistry when polled, the rotation for that persona simply stays
// 'pending' and is retried next poll. items.id=290/decisions.id=718 closed
// the common cause of this (group keys now survive restart -- rehydrated at
// login by commands::auth::finish_login from personal.db's group_keys
// table), but the skip-and-retry behavior itself stays as a safety net --
// rehydration is itself best-effort (a corrupt row/persona is skipped and
// logged rather than blocking login) and only runs at login, not
// continuously.
//
// ROSTER DERIVATION, deliberately NOT a durable membership table
// (items.id=291 stays out of scope, per Jason's brief): "remaining
// members" = every persona_id with an ACCEPTED row in
// pending_group_invitations for this group_id, minus anyone recorded in the
// new group_departures table (schema/shared_006.sql), minus the persona
// currently departing. group_departures stores no keys and answers only
// "who has left," never "who is currently a member" in general -- a
// strictly narrower question than either 290 or 291 would need to answer.
// KNOWN FALSE NEGATIVE, inherited not introduced: a group's creator never
// appears in pending_group_invitations (they never received one, and
// items.id=291's group-creation flow doesn't exist yet to define "creator"
// in the first place) -- invisible to this derivation regardless.
//
// .qrsync RE-PUSH: apply_pending_rotations calls group_sync::engine::
// republish_owned_documents (items.id=288 addition to that module) right
// after a successful local rekey -- every .qrsync file already in the
// shared folder is encrypted under the group key with no key-version
// marker, so it goes stale the instant rotation completes elsewhere. Only
// each document's OWNER can re-push it (push is owner-only, items.id=287's
// own scope), so a departed member's previously-owned documents become
// permanently orphaned after rotation -- known, accepted limitation, no
// ownership-reassignment mechanism is built here.
//
// DEPARTURE SCOPE, forward-only (verified, not just cited, against
// Architecture/AUTH_MULTIUSER_ARCHITECTURE.md Section 10: "SYNCED not
// reliably revocable once independent history exists -- 'leaving' is the
// recipient's own act"): this module does nothing to a departed member's
// already-pulled local content or their own copy of the old key beyond
// evicting that key from THIS install's GroupKeyRegistry, if resident. No
// remote-wipe mechanism exists anywhere in this system to build against.
//
// QUERY STYLE: runtime sqlx::query()/query_as() only -- no query!() macros
// (many-small-encrypted-DB topology, no static DATABASE_URL). open_shared_db
// is duplicated here rather than reused from group_invitations.rs -- same
// reasoning that module's own header gives for duplicating it from
// user_store.rs: different error type per module, ~12-line
// zero-divergence-risk helper.

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::SqliteConnection;
use thiserror::Error;
use x25519_dalek::StaticSecret;

use crate::auth::group_invitations::{self, GroupInvitationError};
use crate::auth::kdf;
use crate::auth::registry::{GroupKeyRegistry, KeyRegistry, UnlockedGroupKey};
use crate::auth::sharing_keypair::{self, SharingKeypairError};
use crate::group_sync::engine as group_sync_engine;
use crate::persistence::group_key_store;
use crate::persistence::group_store::{self, GroupStoreError};
use crate::persistence::personal_store::PersonalStoreError;

#[derive(Debug, Error)]
pub enum GroupMembershipError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Group invitation error: {0}")]
    Invitation(#[from] GroupInvitationError),
    #[error("Sharing keypair error: {0}")]
    Sharing(#[from] SharingKeypairError),
    #[error("Group store error: {0}")]
    Store(#[from] GroupStoreError),
    #[error("Persona '{0}' has already departed group '{1}'")]
    AlreadyDeparted(String, String),
    #[error("User '{0}' has no registered sharing public key")]
    RecipientHasNoSharingKey(String),
    #[error("Could not generate random bytes: {0}")]
    RandomSource(String),
    #[error("Rotation '{0}' has a stored encrypted_group_key that is not valid hex")]
    CorruptStoredEnvelope(String),
    #[error("Rotation '{0}' decrypted to a group key of the wrong length")]
    CorruptGroupKey(String),
    #[error("Invalid departure reason '{0}' -- must be 'left' or 'removed'")]
    InvalidReason(String),
    #[error("Failed to durably persist group key: {0}")]
    PersonalStore(#[from] PersonalStoreError),
}

// ---------------------------------------------------------------------------
// Departure reason
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepartureReason {
    Left,
    Removed,
}

impl DepartureReason {
    fn as_str(self) -> &'static str {
        match self {
            DepartureReason::Left => "left",
            DepartureReason::Removed => "removed",
        }
    }
}

impl std::str::FromStr for DepartureReason {
    type Err = GroupMembershipError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(DepartureReason::Left),
            "removed" => Ok(DepartureReason::Removed),
            other => Err(GroupMembershipError::InvalidReason(other.to_owned())),
        }
    }
}

// ---------------------------------------------------------------------------
// DB opener (shared.db -- unencrypted)
// ---------------------------------------------------------------------------

async fn open_shared_db() -> Result<SqliteConnection, GroupMembershipError> {
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

fn hex_decode(context: &str, s: &str) -> Result<Vec<u8>, GroupMembershipError> {
    if !s.len().is_multiple_of(2) {
        return Err(GroupMembershipError::CorruptStoredEnvelope(
            context.to_owned(),
        ));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| GroupMembershipError::CorruptStoredEnvelope(context.to_owned()))
        })
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Roster derivation
// ---------------------------------------------------------------------------

/// "Remaining members" for a group, excluding `excluding_persona_id` -- see
/// this module's own header (ROSTER DERIVATION) for the full reasoning and
/// its known false-negative (a group's creator never appears here).
pub(crate) async fn remaining_members(
    group_id: &str,
    excluding_persona_id: &str,
    conn: &mut SqliteConnection,
) -> Result<Vec<String>, GroupMembershipError> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT recipient_persona_id FROM pending_group_invitations
         WHERE group_id = ? AND status = 'accepted'
         AND recipient_persona_id NOT IN (
             SELECT persona_id FROM group_departures WHERE group_id = ?
         )
         AND recipient_persona_id != ?",
    )
    .bind(group_id)
    .bind(group_id)
    .bind(excluding_persona_id)
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows.into_iter().map(|(id,)| id).collect())
}

// ---------------------------------------------------------------------------
// Remove member (initiator's install)
// ---------------------------------------------------------------------------

/// Record `departing_persona_id`'s departure from `group_id`, generate a
/// fresh group key, and queue one encrypted rotation envelope per remaining
/// member. Does NOT rekey any local group.db file -- see this module's own
/// header (TWO HALVES) for why that's apply_pending_rotations' job, run
/// separately on each remaining member's own install.
///
/// IDEMPOTENCY: a second call for an already-departed persona returns
/// AlreadyDeparted rather than silently re-rotating (which would generate
/// and redistribute a second new key, an unnecessary second rotation).
///
/// ATOMICITY: the departure-record insert and every rotation-row insert run
/// inside one SAVEPOINT -- either every remaining member gets queued a
/// rotation envelope, or (on any failure) none do and the departure itself
/// rolls back too, rather than leaving some members redistributed and
/// others silently stranded with only the old key. Mirrors migrations.rs's
/// own bootstrap_lock_table SAVEPOINT-then-ROLLBACK-TO pattern.
/// GroupKeyRegistry::clear (below, outside the SAVEPOINT) is a separate
/// storage system entirely -- no SAVEPOINT could make shared.db and
/// in-memory state atomic together regardless (same reasoning
/// accept_invitation's own doc comment gives for its registry-then-DB
/// ordering).
///
/// PERSONAL.DB SYNC ON EVICT (items.id=290, decisions.id=718): right after
/// evicting the registry entry, this also attempts to delete the matching
/// durable row from personal.db's group_keys table, so the two don't drift
/// apart. Deliberately best-effort (log on failure, never propagated) for a
/// structural reason, not convenience: `remove_member` runs on the
/// *initiator's* install, and `departing_persona_id`'s owning account's
/// master key is only ever resident here (via `key_registry`) when the
/// departing persona IS the currently logged-in account -- true
/// self-departure. For every other case (this account removing someone
/// else's persona), there's nothing local to delete, by construction, not
/// by failure -- see this module's own header ("a household's members each
/// run their own QR install").
pub async fn remove_member(
    group_id: &str,
    departing_persona_id: &str,
    reason: DepartureReason,
    sender_label: &str,
    group_key_registry: &GroupKeyRegistry,
    key_registry: &KeyRegistry,
) -> Result<(), GroupMembershipError> {
    let mut conn = open_shared_db().await?;

    sqlx::query("SAVEPOINT remove_member")
        .execute(&mut conn)
        .await?;

    let result = remove_member_inner(
        &mut conn,
        group_id,
        departing_persona_id,
        reason,
        sender_label,
    )
    .await;

    match &result {
        Ok(()) => {
            sqlx::query("RELEASE remove_member")
                .execute(&mut conn)
                .await?;
        }
        Err(_) => {
            let _ = sqlx::query("ROLLBACK TO remove_member")
                .execute(&mut conn)
                .await;
        }
    }

    result?;

    // Outside the SAVEPOINT deliberately -- see this fn's own doc comment
    // (ATOMICITY) on why shared.db and GroupKeyRegistry can't be made
    // atomic together regardless.
    group_key_registry
        .clear(departing_persona_id, group_id)
        .await;

    // See this fn's own doc comment (PERSONAL.DB SYNC ON EVICT) -- only
    // ever reachable when departing_persona_id is this account's own
    // persona; a no-op everywhere else.
    if let Some((current_user_id, current_key_hex)) = key_registry
        .with_key(|k| (k.user_id.clone(), crate::auth::registry::key_hex(&k.master_key)))
        .await
    {
        match group_invitations::resolve_persona_owner(departing_persona_id, &mut conn).await {
            Ok(owner_user_id) if owner_user_id == current_user_id => {
                if let Err(e) = group_key_store::delete_group_key(
                    &current_user_id,
                    departing_persona_id,
                    &current_key_hex,
                    group_id,
                )
                .await
                {
                    log::warn!(
                        "remove_member: couldn't delete stale personal.db group_keys row for \
                         persona={departing_persona_id} group={group_id}: {e}"
                    );
                }
            }
            _ => {} // not this account's own departure -- nothing local to clean up
        }
    }

    Ok(())
}

async fn remove_member_inner(
    conn: &mut SqliteConnection,
    group_id: &str,
    departing_persona_id: &str,
    reason: DepartureReason,
    sender_label: &str,
) -> Result<(), GroupMembershipError> {
    let already_departed: Option<(String,)> = sqlx::query_as(
        "SELECT persona_id FROM group_departures WHERE group_id = ? AND persona_id = ?",
    )
    .bind(group_id)
    .bind(departing_persona_id)
    .fetch_optional(&mut *conn)
    .await?;
    if already_departed.is_some() {
        return Err(GroupMembershipError::AlreadyDeparted(
            departing_persona_id.to_owned(),
            group_id.to_owned(),
        ));
    }

    let remaining = remaining_members(group_id, departing_persona_id, conn).await?;

    sqlx::query(
        "INSERT INTO group_departures (group_id, persona_id, departed_at, reason)
         VALUES (?, ?, ?, ?)",
    )
    .bind(group_id)
    .bind(departing_persona_id)
    .bind(crate::providers::utils::now())
    .bind(reason.as_str())
    .execute(&mut *conn)
    .await?;

    let mut new_group_key = [0u8; kdf::MASTER_KEY_LEN];
    getrandom::fill(&mut new_group_key)
        .map_err(|e| GroupMembershipError::RandomSource(e.to_string()))?;

    for member_persona_id in remaining {
        let owner_user_id =
            group_invitations::resolve_persona_owner(&member_persona_id, conn).await?;
        let recipient_public_key = sharing_keypair::get_public_key(&owner_user_id)
            .await?
            .ok_or_else(|| {
                GroupMembershipError::RecipientHasNoSharingKey(owner_user_id.clone())
            })?;
        let envelope =
            sharing_keypair::encrypt_to_public_key(&recipient_public_key, &new_group_key)?;

        sqlx::query(
            "INSERT INTO pending_group_key_rotations
             (id, recipient_persona_id, group_id, encrypted_group_key, sender_label,
              status, created_at)
             VALUES (?, ?, ?, ?, ?, 'pending', ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&member_persona_id)
        .bind(group_id)
        .bind(hex_encode(&envelope))
        .bind(sender_label)
        .bind(crate::providers::utils::now())
        .execute(&mut *conn)
        .await?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Apply pending rotations (each remaining member's own install)
// ---------------------------------------------------------------------------

/// Decrypt and apply every rotation envelope currently pending for
/// `persona_id`, across all of that persona's groups. Called from main.rs's
/// periodic poll loop, once per resident persona per tick, before that
/// tick's folder-sync pull sweep -- see this module's own header (TRIGGER)
/// for the full cadence reasoning.
///
/// A rotation whose OLD key isn't currently resident in
/// `group_key_registry` is skipped, not errored -- it stays 'pending' and
/// is retried next poll. See this module's own header (TRIGGER) for why
/// this inherits items.id=290's already-accepted limitation rather than
/// introducing a new failure mode.
///
/// Per-rotation failures (a corrupt envelope, a rekey I/O error) are
/// returned as Err immediately, stopping the sweep for this persona at that
/// row -- unlike group_sync::engine's push/pull, which never surfaces
/// individual-item failures as Err. Deliberate difference: a bad rotation
/// row is a correctness problem (this persona's local group.db key state
/// could end up wrong) worth surfacing to the caller's log, not a routine
/// "peer hasn't pushed yet" condition to silently skip past.
///
/// PERSONAL.DB DURABLE WRITE (items.id=290, decisions.id=718): each
/// iteration now also durably persists the freshly-decrypted key to
/// personal.db (group_key_store::save_group_key) BEFORE calling
/// `rekey_group_db` below -- not merely before the status UPDATE.
/// `rekey_group_db` physically rewrites the group.db file under the new
/// key, an irreversible step that is not safely re-runnable: if the
/// personal.db write were placed after it and then failed, a retry would
/// still be holding the now-stale `old_key_hex` (the registry hasn't been
/// updated yet either) and would try to open a file already rewritten under
/// the new key, and fail. Placing the durable write first keeps every step
/// safely re-runnable on failure -- decrypt is a pure function of the
/// stored envelope, and save_group_key upserts on group_id, so retrying
/// from a `write failed` state reproduces the identical key and simply
/// tries the write again.
pub async fn apply_pending_rotations(
    persona_id: &str,
    group_key_registry: &GroupKeyRegistry,
    sharing_private_key: &StaticSecret,
    personal_key_hex: &str,
) -> Result<(), GroupMembershipError> {
    let mut conn = open_shared_db().await?;

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, group_id, encrypted_group_key FROM pending_group_key_rotations
         WHERE recipient_persona_id = ? AND status = 'pending'",
    )
    .bind(persona_id)
    .fetch_all(&mut conn)
    .await?;

    let owner_user_id = group_invitations::resolve_persona_owner(persona_id, &mut conn).await?;

    for (rotation_id, group_id, encrypted_hex) in rows {
        let Some(old_key_hex) = group_key_registry.key_hex_for(persona_id, &group_id).await
        else {
            continue;
        };

        let envelope = hex_decode(&rotation_id, &encrypted_hex)?;
        let plaintext = sharing_keypair::decrypt_own_envelope(sharing_private_key, &envelope)?;
        let new_group_key: [u8; kdf::MASTER_KEY_LEN] = plaintext
            .try_into()
            .map_err(|_| GroupMembershipError::CorruptGroupKey(rotation_id.clone()))?;
        let new_key_hex = crate::auth::registry::key_hex(&new_group_key);

        // See this fn's own doc comment (PERSONAL.DB DURABLE WRITE) -- must
        // run before rekey_group_db, not after.
        group_key_store::save_group_key(
            &owner_user_id,
            persona_id,
            personal_key_hex,
            &group_id,
            &new_key_hex,
            &crate::providers::utils::now(),
        )
        .await?;

        group_store::rekey_group_db(persona_id, &group_id, &old_key_hex, &new_key_hex).await?;

        group_key_registry
            .replace(
                persona_id,
                &group_id,
                UnlockedGroupKey {
                    group_id: group_id.clone(),
                    group_key: new_group_key,
                    unlocked_at: crate::providers::utils::now(),
                },
            )
            .await;

        sqlx::query(
            "UPDATE pending_group_key_rotations SET status = 'applied', applied_at = ? \
             WHERE id = ?",
        )
        .bind(crate::providers::utils::now())
        .bind(&rotation_id)
        .execute(&mut conn)
        .await?;

        // Best-effort, same failure-handling contract as every other
        // group_sync call site -- a re-push failure must not turn an
        // otherwise-successful rotation apply into an Err.
        if let Err(e) =
            group_sync_engine::republish_owned_documents(persona_id, &group_id, &new_key_hex)
                .await
        {
            log::warn!(
                "apply_pending_rotations: republish_owned_documents failed for \
                 persona={persona_id} group={group_id}: {e}"
            );
        }
    }

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

    /// Same fixture shape as group_invitations.rs's own test module --
    /// duplicated deliberately (test-only, zero-divergence-risk).
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

    // -- remaining_members ---------------------------------------------------

    #[tokio::test]
    async fn remaining_members_excludes_departed_and_self() {
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Sender", 0x11).await;
        let (_b_user, b_persona, _) = make_user_with_persona("Bob", 0x22).await;
        let (_c_user, c_persona, _) = make_user_with_persona("Carol", 0x33).await;

        let group_id = "group-roster-1";
        let group_key = [0x55u8; kdf::MASTER_KEY_LEN];

        let mut conn = open_shared_db().await.unwrap();

        for persona in [&b_persona, &c_persona] {
            let invitation_id = group_invitations::send_invitation(
                persona,
                group_id,
                "Test Group",
                &sender_persona,
                &group_key,
            )
            .await
            .unwrap();
            sqlx::query("UPDATE pending_group_invitations SET status = 'accepted' WHERE id = ?")
                .bind(&invitation_id)
                .execute(&mut conn)
                .await
                .unwrap();
        }

        sqlx::query(
            "INSERT INTO group_departures (group_id, persona_id, departed_at, reason)
             VALUES (?, ?, ?, ?)",
        )
        .bind(group_id)
        .bind(&b_persona)
        .bind(crate::providers::utils::now())
        .bind("left")
        .execute(&mut conn)
        .await
        .unwrap();

        let roster = remaining_members(group_id, "someone-else", &mut conn)
            .await
            .expect("remaining_members must succeed");
        assert_eq!(
            roster,
            vec![c_persona.clone()],
            "departed Bob must be excluded, Carol must remain"
        );

        let roster_excluding_carol = remaining_members(group_id, &c_persona, &mut conn)
            .await
            .unwrap();
        assert!(
            roster_excluding_carol.is_empty(),
            "excluding_persona_id must also be excluded from its own roster"
        );
    }

    // -- remove_member --------------------------------------------------------

    #[tokio::test]
    async fn remove_member_is_idempotent_for_already_departed_persona() {
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Sender", 0x44).await;
        let (_b_user, b_persona, _) = make_user_with_persona("Bob", 0x55).await;

        let group_id = "group-idempotent-1";
        let registry = GroupKeyRegistry::default();

        remove_member(
            group_id,
            &b_persona,
            DepartureReason::Left,
            &sender_persona,
            &registry,
            &KeyRegistry::default(),
        )
        .await
        .expect("first remove_member must succeed");

        let second = remove_member(
            group_id,
            &b_persona,
            DepartureReason::Left,
            &sender_persona,
            &registry,
            &KeyRegistry::default(),
        )
        .await;
        assert!(matches!(
            second,
            Err(GroupMembershipError::AlreadyDeparted(_, _))
        ));
    }

    #[tokio::test]
    async fn remove_member_queues_rotation_for_remaining_members_and_evicts_registry() {
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Sender", 0x66).await;
        let (_b_user, b_persona, b_private_key) = make_user_with_persona("Bob", 0x77).await;
        let (_c_user, c_persona, c_private_key) = make_user_with_persona("Carol", 0x88).await;

        let group_id = "group-remove-1";
        let group_key = [0x99u8; kdf::MASTER_KEY_LEN];
        let registry = GroupKeyRegistry::default();

        let b_invitation = group_invitations::send_invitation(
            &b_persona,
            group_id,
            "Test Group",
            &sender_persona,
            &group_key,
        )
        .await
        .unwrap();
        group_invitations::accept_invitation(
            &b_invitation,
            &b_persona,
            &b_private_key,
            &registry,
            PERSONAL_KEY_HEX,
        )
        .await
        .unwrap();

        let c_invitation = group_invitations::send_invitation(
            &c_persona,
            group_id,
            "Test Group",
            &sender_persona,
            &group_key,
        )
        .await
        .unwrap();
        group_invitations::accept_invitation(
            &c_invitation,
            &c_persona,
            &c_private_key,
            &registry,
            PERSONAL_KEY_HEX,
        )
        .await
        .unwrap();

        assert!(registry.is_occupied(&b_persona, group_id).await);
        assert!(registry.is_occupied(&c_persona, group_id).await);

        remove_member(
            group_id,
            &b_persona,
            DepartureReason::Removed,
            &sender_persona,
            &registry,
            &KeyRegistry::default(),
        )
        .await
        .expect("remove_member must succeed");

        assert!(
            !registry.is_occupied(&b_persona, group_id).await,
            "the departed persona's key must be evicted locally"
        );
        assert!(
            registry.is_occupied(&c_persona, group_id).await,
            "Carol's OLD key stays resident until apply_pending_rotations runs -- \
             remove_member only queues redistribution"
        );

        let mut conn = open_shared_db().await.unwrap();
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT recipient_persona_id, encrypted_group_key \
             FROM pending_group_key_rotations WHERE group_id = ?",
        )
        .bind(group_id)
        .fetch_all(&mut conn)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "only Carol should receive a rotation envelope");
        assert_eq!(rows[0].0, c_persona);

        let envelope = hex_decode("test", &rows[0].1).unwrap();
        let decrypted = sharing_keypair::decrypt_own_envelope(&c_private_key, &envelope).unwrap();
        assert_ne!(
            decrypted, group_key,
            "rotated key must be freshly generated, not the original group key resent"
        );
        assert_eq!(decrypted.len(), kdf::MASTER_KEY_LEN);
    }

    #[tokio::test]
    async fn remove_member_self_departure_deletes_personal_db_row() {
        // items.id=290, decisions.id=718: the departing persona's own
        // account (its master key resident in KeyRegistry) must have its
        // durable group_keys row cleaned up alongside the in-memory
        // eviction -- see remove_member's own doc comment (PERSONAL.DB
        // SYNC ON EVICT).
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Sender", 0xE0).await;

        let departing_master_key = [0xE1u8; kdf::MASTER_KEY_LEN];
        let (departing_user, departing_persona, departing_private_key) =
            make_user_with_persona("Departing", 0xE1).await;
        let departing_key_hex = crate::auth::registry::key_hex(&departing_master_key);

        let group_id = "group-self-departure-1";
        let group_key = [0xE2u8; kdf::MASTER_KEY_LEN];
        let registry = GroupKeyRegistry::default();

        let invitation = group_invitations::send_invitation(
            &departing_persona,
            group_id,
            "Test Group",
            &sender_persona,
            &group_key,
        )
        .await
        .unwrap();
        group_invitations::accept_invitation(
            &invitation,
            &departing_persona,
            &departing_private_key,
            &registry,
            &departing_key_hex,
        )
        .await
        .unwrap();

        let rows_before = crate::persistence::group_key_store::list_group_keys(
            &departing_user,
            &departing_persona,
            &departing_key_hex,
        )
        .await
        .unwrap();
        assert_eq!(rows_before.len(), 1, "durable row must exist before departure");

        let key_registry = KeyRegistry::default();
        key_registry
            .replace(crate::auth::registry::UnlockedKey {
                user_id: departing_user.clone(),
                master_key: departing_master_key,
                sharing_private_key: departing_private_key.to_bytes(),
                unlocked_at: "2026-01-01T00:00:00Z".to_owned(),
            })
            .await;

        remove_member(
            group_id,
            &departing_persona,
            DepartureReason::Left,
            &sender_persona,
            &registry,
            &key_registry,
        )
        .await
        .expect("remove_member must succeed");

        let rows_after = crate::persistence::group_key_store::list_group_keys(
            &departing_user,
            &departing_persona,
            &departing_key_hex,
        )
        .await
        .unwrap();
        assert!(
            rows_after.is_empty(),
            "self-departure must delete the durable personal.db row"
        );
    }

    #[tokio::test]
    async fn remove_member_of_someone_else_does_not_touch_their_personal_db() {
        // The structural no-op case (this module's own doc comment) -- the
        // initiator's install has no way to reach a different account's
        // personal.db (no resident key for it), so the delete attempt must
        // be silently skipped, not error, when key_registry holds a
        // DIFFERENT account than departing_persona_id's owner.
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Sender", 0xE3).await;

        let departing_master_key = [0xE4u8; kdf::MASTER_KEY_LEN];
        let (departing_user, departing_persona, departing_private_key) =
            make_user_with_persona("Departing", 0xE4).await;
        let departing_key_hex = crate::auth::registry::key_hex(&departing_master_key);

        let group_id = "group-remote-departure-1";
        let group_key = [0xE5u8; kdf::MASTER_KEY_LEN];
        let registry = GroupKeyRegistry::default();

        let invitation = group_invitations::send_invitation(
            &departing_persona,
            group_id,
            "Test Group",
            &sender_persona,
            &group_key,
        )
        .await
        .unwrap();
        group_invitations::accept_invitation(
            &invitation,
            &departing_persona,
            &departing_private_key,
            &registry,
            &departing_key_hex,
        )
        .await
        .unwrap();

        // The initiator's own resident session -- a DIFFERENT account from
        // the departing persona's owner.
        let (initiator_user, _initiator_persona, initiator_private_key) =
            make_user_with_persona("Initiator", 0xE6).await;
        let initiator_master_key = [0xE6u8; kdf::MASTER_KEY_LEN];
        let key_registry = KeyRegistry::default();
        key_registry
            .replace(crate::auth::registry::UnlockedKey {
                user_id: initiator_user,
                master_key: initiator_master_key,
                sharing_private_key: initiator_private_key.to_bytes(),
                unlocked_at: "2026-01-01T00:00:00Z".to_owned(),
            })
            .await;

        remove_member(
            group_id,
            &departing_persona,
            DepartureReason::Removed,
            &sender_persona,
            &registry,
            &key_registry,
        )
        .await
        .expect("remove_member must succeed even when it can't reach the departing persona's own personal.db");

        let rows_after = crate::persistence::group_key_store::list_group_keys(
            &departing_user,
            &departing_persona,
            &departing_key_hex,
        )
        .await
        .unwrap();
        assert_eq!(
            rows_after.len(),
            1,
            "a different account's own personal.db row must be untouched"
        );
    }

    // -- apply_pending_rotations -----------------------------------------------

    #[tokio::test]
    async fn apply_pending_rotations_rekeys_group_db_and_swaps_registry_key() {
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Sender", 0xAA).await;
        let (_recipient_user, recipient_persona, recipient_private_key) =
            make_user_with_persona("Recipient", 0xBB).await;
        let (_departing_user, departing_persona, departing_private_key) =
            make_user_with_persona("Departing", 0xCC).await;

        let group_id = "group-apply-1";
        let old_group_key = [0x11u8; kdf::MASTER_KEY_LEN];
        let registry = GroupKeyRegistry::default();

        let recipient_invitation = group_invitations::send_invitation(
            &recipient_persona,
            group_id,
            "Test Group",
            &sender_persona,
            &old_group_key,
        )
        .await
        .unwrap();
        group_invitations::accept_invitation(
            &recipient_invitation,
            &recipient_persona,
            &recipient_private_key,
            &registry,
            PERSONAL_KEY_HEX,
        )
        .await
        .unwrap();
        let old_key_hex = registry
            .key_hex_for(&recipient_persona, group_id)
            .await
            .unwrap();
        group_store::create_document(
            &recipient_persona,
            group_id,
            &old_key_hex,
            &recipient_persona,
            "Doc",
            "v1",
        )
        .await
        .unwrap();

        let departing_invitation = group_invitations::send_invitation(
            &departing_persona,
            group_id,
            "Test Group",
            &sender_persona,
            &old_group_key,
        )
        .await
        .unwrap();
        group_invitations::accept_invitation(
            &departing_invitation,
            &departing_persona,
            &departing_private_key,
            &registry,
            PERSONAL_KEY_HEX,
        )
        .await
        .unwrap();

        remove_member(
            group_id,
            &departing_persona,
            DepartureReason::Left,
            &sender_persona,
            &registry,
            &KeyRegistry::default(),
        )
        .await
        .unwrap();

        apply_pending_rotations(
            &recipient_persona,
            &registry,
            &recipient_private_key,
            PERSONAL_KEY_HEX,
        )
        .await
        .expect("apply_pending_rotations must succeed");

        let new_key_hex = registry
            .key_hex_for(&recipient_persona, group_id)
            .await
            .expect("registry must still hold a key for recipient after rotation");
        assert_ne!(
            new_key_hex, old_key_hex,
            "registry must hold the NEW key after rotation, not the old one"
        );

        let docs = group_store::list_documents(&recipient_persona, group_id, &new_key_hex)
            .await
            .expect("group.db must open under the new key after rekey");
        assert_eq!(docs.len(), 1);
        assert_eq!(
            docs[0].content, "v1",
            "the document written under the old key must survive the rekey"
        );

        let mut conn = open_shared_db().await.unwrap();
        let status: (String,) = sqlx::query_as(
            "SELECT status FROM pending_group_key_rotations \
             WHERE recipient_persona_id = ? AND group_id = ?",
        )
        .bind(&recipient_persona)
        .bind(group_id)
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(status.0, "applied");
    }

    #[tokio::test]
    async fn apply_pending_rotations_skips_when_old_key_not_resident() {
        let _env = setup().await;
        let (_sender_user, sender_persona, _) = make_user_with_persona("Sender", 0xDD).await;
        let (_recipient_user, recipient_persona, recipient_private_key) =
            make_user_with_persona("Recipient", 0xEE).await;
        let (_departing_user, departing_persona, departing_private_key) =
            make_user_with_persona("Departing", 0xFF).await;

        let group_id = "group-apply-2";
        let old_group_key = [0x22u8; kdf::MASTER_KEY_LEN];
        let registry = GroupKeyRegistry::default();

        let recipient_invitation = group_invitations::send_invitation(
            &recipient_persona,
            group_id,
            "Test Group",
            &sender_persona,
            &old_group_key,
        )
        .await
        .unwrap();
        group_invitations::accept_invitation(
            &recipient_invitation,
            &recipient_persona,
            &recipient_private_key,
            &registry,
            PERSONAL_KEY_HEX,
        )
        .await
        .unwrap();

        let departing_invitation = group_invitations::send_invitation(
            &departing_persona,
            group_id,
            "Test Group",
            &sender_persona,
            &old_group_key,
        )
        .await
        .unwrap();
        group_invitations::accept_invitation(
            &departing_invitation,
            &departing_persona,
            &departing_private_key,
            &registry,
            PERSONAL_KEY_HEX,
        )
        .await
        .unwrap();

        remove_member(
            group_id,
            &departing_persona,
            DepartureReason::Left,
            &sender_persona,
            &registry,
            &KeyRegistry::default(),
        )
        .await
        .unwrap();

        // Simulate recipient's key not being resident this session (e.g.
        // app restarted) by evicting it before polling.
        registry.clear(&recipient_persona, group_id).await;

        apply_pending_rotations(
            &recipient_persona,
            &registry,
            &recipient_private_key,
            PERSONAL_KEY_HEX,
        )
        .await
        .expect("apply_pending_rotations must not error when the old key isn't resident");

        let mut conn = open_shared_db().await.unwrap();
        let status: (String,) = sqlx::query_as(
            "SELECT status FROM pending_group_key_rotations \
             WHERE recipient_persona_id = ? AND group_id = ?",
        )
        .bind(&recipient_persona)
        .bind(group_id)
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(
            status.0, "pending",
            "a rotation must stay pending, retried next poll, when the old key isn't resident"
        );
    }

    #[test]
    fn departure_reason_round_trips_through_str() {
        assert_eq!("left".parse::<DepartureReason>().unwrap(), DepartureReason::Left);
        assert_eq!(
            "removed".parse::<DepartureReason>().unwrap(),
            DepartureReason::Removed
        );
        assert!(matches!(
            "bogus".parse::<DepartureReason>(),
            Err(GroupMembershipError::InvalidReason(_))
        ));
    }
}
