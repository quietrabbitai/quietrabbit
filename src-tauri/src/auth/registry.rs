// src-tauri/src/auth/registry.rs
//
// Single-slot in-memory key registry (items.id=205, Architecture/
// AUTH_MULTIUSER_ARCHITECTURE.md Section 4.2). Tauri-managed state --
// never persisted, volatile only, at most one account's key resident in
// memory at any instant, for the process's entire lifetime.
//
// ARCHITECTURAL DEVIATION FROM SECTION 4.2's LITERAL STRUCT: the doc gives
// master_key: Vec<u8>. This module uses [u8; kdf::MASTER_KEY_LEN] (32
// bytes) instead. A fixed-size array enforces the invariant stated
// elsewhere in the document (Section 4.1: master key is always "raw 32
// bytes") while eliminating heap allocation and reallocation exposure that
// Vec<u8> carries -- kdf::derive_master_key() already returns this exact
// array type, so no conversion is needed either way.
//
// SECURITY RATIONALE -- ZEROIZE-ON-DROP: derive(Zeroize, ZeroizeOnDrop) is
// required together -- Zeroize alone only provides a callable .zeroize()
// method; it does not run automatically on drop unless ZeroizeOnDrop is
// also derived (an older crate version auto-derived Drop from Zeroize
// alone; current versions, including the 1.8/1.9 range this codebase
// resolves to, do not -- verified against the crate's current docs this
// session).
//
// SECURITY RATIONALE -- ENCAPSULATION: the slot's inner Mutex is private,
// not exposed as a type alias -- callers can only reach it through
// replace()/clear()/is_occupied()/with_key() below. This makes "never
// mutate an UnlockedKey in place, always replace the whole Option" an
// enforced property of the API, not just a documented convention: there is
// no path by which calling code can obtain &mut access to a resident
// UnlockedKey's fields at all.
//
// IMPLEMENTATION NOTE -- EXECUTION MODEL: auth commands are async
// #[tauri::command]s, so this registry uses Tokio's async Mutex to match --
// the same pattern main.rs already uses for Mutex<OllamaSidecar>. A
// std::sync::MutexGuard held across an .await point is not Send and will
// not compile in this position, so this is also the only Mutex type that
// actually works here, not solely a style preference.
//
// IMPLEMENTATION NOTE -- TRANSIENT COPIES NOT COVERED HERE: the
// replace-not-mutate invariant above protects the long-lived copy once a
// key is resident in this registry. It does NOT cover transient copies
// created before that point -- kdf::derive_master_key()'s stack buffer,
// or a freshly-constructed UnlockedKey's fields before it's moved into
// replace(). Whoever writes login() (a later step) needs to verify that
// path separately; this module's guarantee starts once a key is inside it.

use std::collections::HashMap;

use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::auth::kdf;

/// Hex-encode a resident master key for use as a store's `key_hex: &str`
/// parameter (e.g. the SQLCipher PRAGMA key value). Shared implementation --
/// items.id=268 found three independent byte-identical private copies
/// (commands/tier2.rs, commands/system.rs, commands/tier3_pane.rs) that had
/// accreted before this was ever unified; those three, plus every command
/// migrated in that item, call this one instead.
pub(crate) fn key_hex(key: &[u8; kdf::MASTER_KEY_LEN]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

/// One account's resident, unlocked master key. Never persisted -- exists
/// only as Tauri managed state for this process's lifetime. Fields are
/// pub(crate): constructed only by login() (a later step, same crate);
/// not part of the IPC surface, so no external caller can reach these
/// fields regardless -- accessor methods would add ceremony without
/// defending against a real access path.
///
/// sharing_private_key (items.id=289, decisions.id=677): the account's
/// X25519 sharing keypair's private half, HKDF-derived from master_key (see
/// auth::sharing_keypair::derive_sharing_keypair). Follows master_key's own
/// single-slot lifecycle deliberately -- not a second standing secret, it's
/// re-derived alongside master_key on every login and evicted alongside it
/// on clear().
///
/// Stored as raw [u8; 32] rather than x25519_dalek::StaticSecret directly --
/// tried the latter first, but StaticSecret (even with x25519-dalek's own
/// "zeroize" feature enabled) only implements ZeroizeOnDrop internally, not
/// the callable Zeroize trait #[derive(Zeroize)] requires of every
/// non-skipped field (confirmed via a real compile error this session:
/// "StaticSecret: DefaultIsZeroes ... required by StaticSecret: Zeroize").
/// Raw bytes get the exact same #[derive(Zeroize)] treatment master_key
/// already has here, and StaticSecret::from(bytes) reconstructs an
/// equivalent key on demand wherever one is actually needed (e.g.
/// auth::sharing_keypair::decrypt_own_envelope).
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct UnlockedKey {
    #[zeroize(skip)]
    pub(crate) user_id: String,
    pub(crate) master_key: [u8; kdf::MASTER_KEY_LEN],
    pub(crate) sharing_private_key: [u8; 32],
    #[zeroize(skip)]
    pub(crate) unlocked_at: String,
}

/// Single-slot key registry. Encapsulated -- see module header on why the
/// inner Mutex is not exposed directly.
#[derive(Default)]
pub struct KeyRegistry {
    slot: tokio::sync::Mutex<Option<UnlockedKey>>,
}

impl KeyRegistry {
    /// Populate the slot with a newly-unlocked account's key, replacing
    /// whatever was previously resident (if anything). The only writer
    /// path that installs a key.
    pub async fn replace(&self, new_value: UnlockedKey) {
        let mut guard = self.slot.lock().await;
        *guard = Some(new_value);
    }

    /// Clear the slot entirely -- logout, idle-timeout fire, sleep/suspend
    /// detection, or rekey completion. Dropping the old Some(..) value
    /// here is what fires ZeroizeOnDrop on the outgoing UnlockedKey.
    pub async fn clear(&self) {
        let mut guard = self.slot.lock().await;
        *guard = None;
    }

    /// Whether any account's key is currently resident. Used by callers
    /// that need to check session state without needing the key itself
    /// (e.g. an is-logged-in check).
    pub async fn is_occupied(&self) -> bool {
        self.slot.lock().await.is_some()
    }

    /// Run a closure with read access to the resident key, if any. The
    /// closure receives &UnlockedKey (never &mut) -- this is the only
    /// sanctioned read path, and it structurally cannot be used to mutate
    /// a field in place.
    pub async fn with_key<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&UnlockedKey) -> R,
    {
        let guard = self.slot.lock().await;
        guard.as_ref().map(f)
    }

    /// Resolve the resident account's sharing private key as raw bytes, or
    /// None if no account is currently unlocked. Convenience for callers
    /// outside this crate's own module tree -- UnlockedKey.sharing_private_key
    /// is pub(crate), not reachable from the main.rs binary crate (a
    /// separate crate root from this lib despite sharing one Cargo package,
    /// same boundary GroupKeyRegistry::key_hex_for's own doc comment below
    /// already explains). main.rs's periodic rotation-apply loop
    /// (items.id=288) is the first such caller. Returns raw bytes rather
    /// than StaticSecret directly -- StaticSecret is UnlockedKey's own
    /// internal representation choice (see this struct's field-level doc
    /// comment on why raw bytes were chosen there over StaticSecret); the
    /// caller reconstructs StaticSecret::from(bytes) itself, matching how
    /// auth::group_invitations::accept_invitation's own caller already does
    /// this same reconstruction.
    pub async fn sharing_private_key(&self) -> Option<[u8; 32]> {
        self.with_key(|k| k.sharing_private_key).await
    }

    /// Resolve the resident account's master key as key_hex, or None if no
    /// account is currently unlocked. Same convenience-for-outside-crate
    /// reasoning as sharing_private_key above -- key_hex() itself is
    /// pub(crate), not reachable from the main.rs binary crate. items.id=290
    /// (decisions.id=718): main.rs's periodic rotation-apply loop needs this
    /// alongside sharing_private_key every tick, to feed
    /// auth::group_membership::apply_pending_rotations' own personal.db
    /// durable-write step (group_key_store::save_group_key).
    pub async fn personal_key_hex(&self) -> Option<String> {
        self.with_key(|k| key_hex(&k.master_key)).await
    }
}

// ---------------------------------------------------------------------------
// GroupKeyRegistry (items.id=283, GROUP_DB_DESIGN_20260802.md Section 2.1)
// ---------------------------------------------------------------------------
//
// Alongside KeyRegistry above, not a replacement for it -- the single-slot
// account master key registry is unchanged. This is a separate, additive
// structure holding zero or more resident GROUP symmetric keys, keyed by
// (persona_id, group_id): group membership is per-Persona, not per-account
// (design doc Section 2.1), so the same person's different Personas never
// share group-key state, and one Persona can hold keys for more than one
// group at once.
//
// KEY SIZE: [u8; kdf::MASTER_KEY_LEN], same as UnlockedKey.master_key.
// group.db is encrypted the same way personal.db is (design doc Section
// 2.1), and migrate_group_db's key_hex feeds the same SQLCipher PRAGMA key
// mechanism migrate_personal_db does -- no basis found for a different
// size. Tied to the constant, not a hardcoded 32, so it tracks the one
// real invariant if that ever changes.
//
// ID TYPING: persona_id/group_id are plain String, not dedicated newtypes.
// Checked first -- no PersonaId/GroupId type exists anywhere in this
// codebase; every existing ID (UnlockedKey.user_id included) is a bare
// String/&str. Matches that convention rather than inventing one just for
// this struct.
//
// ENCAPSULATION: same discipline as KeyRegistry -- the inner Mutex<HashMap>
// is private; replace()/with_key()/is_occupied()/clear()/clear_persona()
// are the only ways in. No path exists for calling code to get &mut access
// to a resident UnlockedGroupKey's fields.
//
// REMOVAL API -- two granularities, both provided:
//   clear(persona_id, group_id): evict exactly one group's key -- e.g.
//     leaving a specific group, or a future key-rotation replacement
//     (items.id=288) without disturbing the Persona's other group keys.
//   clear_persona(persona_id): evict every group key resident for one
//     Persona -- e.g. that Persona's own account session ending. Mirrors
//     the lifecycle discipline KeyRegistry::clear() already gives the
//     master key on logout, rather than leaving orphaned group keys
//     resident after the owning account has logged out (design doc Section
//     4 item 3 flags exactly this as something the implementation must not
//     assume away).
// Neither is wired into any real call site here (no logout code is
// touched in this item) -- these are the primitives only.

/// One Persona's resident, unlocked symmetric key for one group. Never
/// persisted -- exists only as Tauri managed state for this process's
/// lifetime, same as UnlockedKey. Fields are pub(crate): constructed only
/// by the (later) invitation-accept / group-unlock path, same crate; not
/// part of the IPC surface.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct UnlockedGroupKey {
    #[zeroize(skip)]
    pub(crate) group_id: String,
    pub(crate) group_key: [u8; kdf::MASTER_KEY_LEN],
    #[zeroize(skip)]
    pub(crate) unlocked_at: String,
}

/// Multi-key group registry, keyed by (persona_id, group_id). Encapsulated
/// -- see module header above for why the inner Mutex is not exposed
/// directly.
#[derive(Default)]
pub struct GroupKeyRegistry {
    keys: tokio::sync::Mutex<HashMap<(String, String), UnlockedGroupKey>>,
}

impl GroupKeyRegistry {
    /// Populate (or replace) the entry for (persona_id, group_id) with a
    /// newly-unlocked group key. The only writer path that installs a key.
    pub async fn replace(&self, persona_id: &str, group_id: &str, new_value: UnlockedGroupKey) {
        let mut guard = self.keys.lock().await;
        guard.insert((persona_id.to_owned(), group_id.to_owned()), new_value);
    }

    /// Evict exactly one (persona_id, group_id) entry, if present. Dropping
    /// the outgoing UnlockedGroupKey here is what fires ZeroizeOnDrop on it.
    pub async fn clear(&self, persona_id: &str, group_id: &str) {
        let mut guard = self.keys.lock().await;
        guard.remove(&(persona_id.to_owned(), group_id.to_owned()));
    }

    /// Evict every entry resident for one Persona, across all of that
    /// Persona's groups -- e.g. that Persona's own account session ending.
    /// Other Personas' entries (including other Personas held by the same
    /// account) are untouched.
    pub async fn clear_persona(&self, persona_id: &str) {
        let mut guard = self.keys.lock().await;
        guard.retain(|(pid, _), _| pid != persona_id);
    }

    /// Whether a key is currently resident for this (persona_id, group_id)
    /// pair. Used by callers that need to check residency without needing
    /// the key itself (e.g. deciding whether to prompt for group unlock).
    pub async fn is_occupied(&self, persona_id: &str, group_id: &str) -> bool {
        let guard = self.keys.lock().await;
        guard.contains_key(&(persona_id.to_owned(), group_id.to_owned()))
    }

    /// Run a closure with read access to one resident group key, if any.
    /// The closure receives &UnlockedGroupKey (never &mut) -- the only
    /// sanctioned read path, same shape as KeyRegistry::with_key.
    pub async fn with_key<F, R>(&self, persona_id: &str, group_id: &str, f: F) -> Option<R>
    where
        F: FnOnce(&UnlockedGroupKey) -> R,
    {
        let guard = self.keys.lock().await;
        guard
            .get(&(persona_id.to_owned(), group_id.to_owned()))
            .map(f)
    }

    /// Resolve one resident group key straight to its key_hex form, or
    /// None if not resident. Convenience combining with_key() + key_hex()
    /// for callers outside this crate's own module tree (key_hex itself is
    /// pub(crate), not reachable from the main.rs binary crate, which is a
    /// separate crate root from this lib despite sharing one Cargo
    /// package) -- items.id=287's periodic pull loop (main.rs) is the
    /// first such caller.
    pub async fn key_hex_for(&self, persona_id: &str, group_id: &str) -> Option<String> {
        self.with_key(persona_id, group_id, |k| key_hex(&k.group_key))
            .await
    }

    /// Every (persona_id, group_id) pair currently resident, in no
    /// particular order. Returns owned identifiers only, never key
    /// material -- items.id=287's periodic folder-sync pull loop uses this
    /// to know *what* to poll (it cannot discover "all groups this persona
    /// belongs to" any other way -- there is no durable group-membership
    /// table yet, items.id=290), then calls with_key() separately per pair
    /// to get the actual key right before doing real I/O. The lock is
    /// acquired, cloned out of, and released before this function returns --
    /// never held across an .await beyond that, matching
    /// ConductorScheduler's documented "no Mutex guard held across .await"
    /// rule (conductor/concurrency.rs).
    pub async fn resident_keys(&self) -> Vec<(String, String)> {
        let guard = self.keys.lock().await;
        guard.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(user_id: &str, byte_fill: u8) -> UnlockedKey {
        let master_key = [byte_fill; kdf::MASTER_KEY_LEN];
        let (sharing_private_key, _) =
            crate::auth::sharing_keypair::derive_sharing_keypair(&master_key, user_id);
        UnlockedKey {
            user_id: user_id.to_owned(),
            master_key,
            sharing_private_key: sharing_private_key.to_bytes(),
            unlocked_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    #[tokio::test]
    async fn empty_registry_is_not_occupied() {
        let registry = KeyRegistry::default();
        assert!(!registry.is_occupied().await);
    }

    #[tokio::test]
    async fn replace_populates_the_slot() {
        let registry = KeyRegistry::default();
        registry.replace(make_key("user-a", 0xAA)).await;
        assert!(registry.is_occupied().await);
    }

    #[tokio::test]
    async fn replace_overwrites_rather_than_stacking() {
        // Single-slot invariant: a second replace() must fully supersede
        // the first, not somehow accumulate -- confirmed by reading back
        // via with_key() and checking it's user-b's data, not user-a's.
        let registry = KeyRegistry::default();
        registry.replace(make_key("user-a", 0xAA)).await;
        registry.replace(make_key("user-b", 0xBB)).await;

        let user_id = registry.with_key(|k| k.user_id.clone()).await;
        assert_eq!(user_id, Some("user-b".to_owned()));
    }

    #[tokio::test]
    async fn clear_empties_the_slot() {
        let registry = KeyRegistry::default();
        registry.replace(make_key("user-a", 0xAA)).await;
        assert!(registry.is_occupied().await);

        registry.clear().await;
        assert!(!registry.is_occupied().await);
    }

    #[tokio::test]
    async fn clear_on_empty_registry_is_a_no_op() {
        let registry = KeyRegistry::default();
        registry.clear().await; // must not panic
        assert!(!registry.is_occupied().await);
    }

    #[tokio::test]
    async fn with_key_returns_none_when_empty() {
        let registry = KeyRegistry::default();
        let result = registry.with_key(|k| k.user_id.clone()).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn with_key_reads_resident_key_data() {
        let registry = KeyRegistry::default();
        registry.replace(make_key("user-a", 0x42)).await;

        let master_key = registry.with_key(|k| k.master_key).await;
        assert_eq!(master_key, Some([0x42u8; kdf::MASTER_KEY_LEN]));
    }

    #[tokio::test]
    async fn sharing_private_key_matches_with_key_and_is_none_when_empty() {
        let registry = KeyRegistry::default();
        assert_eq!(registry.sharing_private_key().await, None);

        registry.replace(make_key("user-a", 0x42)).await;

        let via_with_key = registry.with_key(|k| k.sharing_private_key).await;
        assert_eq!(registry.sharing_private_key().await, via_with_key);
    }

    #[test]
    fn unlocked_key_implements_zeroize_on_drop() {
        // NOT a raw-pointer memory inspection -- an earlier version of this
        // test tried that (capture master_key's address, drop the value,
        // read the same address, assert zero) and failed unreliably,
        // investigated this session: a ZeroizeOnDrop value's stack
        // representation can be moved/copied by the compiler before Drop
        // glue actually runs (confirmed via independent research this
        // session -- this is a documented, general Rust phenomenon, "A
        // stack value implementing ZeroizeOnDrop is moved... only the
        // [moved] value is zeroized; the old copy... may remain,"
        // appsec.guide/docs/languages/rust/memory-zeroization -- not a bug
        // in this module). A captured pointer to the pre-move location is
        // therefore not a reliable way to observe the derive macro's
        // actual behavior, and continuing to chase it would be testing
        // compiler/optimizer behavior this codebase does not control,
        // rather than testing this module's own code.
        //
        // What this module IS responsible for, and what this test actually
        // checks: that UnlockedKey correctly derives ZeroizeOnDrop at all
        // (a type-level property the compiler verifies for us) and that
        // doing so does not silently fail to compile or get skipped due to
        // the #[zeroize(skip)] fields -- core::mem::needs_drop confirms a
        // real Drop impl exists for the type, which is what
        // #[derive(Zeroize, ZeroizeOnDrop)] is supposed to generate.
        assert!(
            std::mem::needs_drop::<UnlockedKey>(),
            "UnlockedKey must have a real Drop impl from #[derive(Zeroize, ZeroizeOnDrop)] -- \
             plain String/[u8; N] fields alone would not need_drop, so this failing would mean \
             the derive did not generate the expected Drop glue"
        );
    }
}

#[cfg(test)]
mod group_key_registry_tests {
    use super::*;

    fn make_group_key(group_id: &str, byte_fill: u8) -> UnlockedGroupKey {
        UnlockedGroupKey {
            group_id: group_id.to_owned(),
            group_key: [byte_fill; kdf::MASTER_KEY_LEN],
            unlocked_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    #[tokio::test]
    async fn empty_registry_is_not_occupied() {
        let registry = GroupKeyRegistry::default();
        assert!(!registry.is_occupied("persona-a", "group-1").await);
    }

    #[tokio::test]
    async fn replace_populates_the_entry() {
        let registry = GroupKeyRegistry::default();
        registry
            .replace("persona-a", "group-1", make_group_key("group-1", 0xAA))
            .await;
        assert!(registry.is_occupied("persona-a", "group-1").await);
    }

    #[tokio::test]
    async fn replace_for_a_different_key_does_not_clobber_the_first() {
        // The real point of the HashMap over KeyRegistry's single slot:
        // multiple (persona_id, group_id) entries coexist. Covers both
        // axes -- a different group for the same persona, and the same
        // group for a different persona.
        let registry = GroupKeyRegistry::default();
        registry
            .replace("persona-a", "group-1", make_group_key("group-1", 0xAA))
            .await;
        registry
            .replace("persona-a", "group-2", make_group_key("group-2", 0xBB))
            .await;
        registry
            .replace("persona-b", "group-1", make_group_key("group-1", 0xCC))
            .await;

        assert!(registry.is_occupied("persona-a", "group-1").await);
        assert!(registry.is_occupied("persona-a", "group-2").await);
        assert!(registry.is_occupied("persona-b", "group-1").await);

        let a1 = registry
            .with_key("persona-a", "group-1", |k| k.group_key)
            .await;
        let a2 = registry
            .with_key("persona-a", "group-2", |k| k.group_key)
            .await;
        let b1 = registry
            .with_key("persona-b", "group-1", |k| k.group_key)
            .await;
        assert_eq!(a1, Some([0xAAu8; kdf::MASTER_KEY_LEN]));
        assert_eq!(a2, Some([0xBBu8; kdf::MASTER_KEY_LEN]));
        assert_eq!(b1, Some([0xCCu8; kdf::MASTER_KEY_LEN]));
    }

    #[tokio::test]
    async fn replace_on_same_pair_overwrites_rather_than_stacking() {
        let registry = GroupKeyRegistry::default();
        registry
            .replace("persona-a", "group-1", make_group_key("group-1", 0xAA))
            .await;
        registry
            .replace("persona-a", "group-1", make_group_key("group-1", 0xBB))
            .await;

        let key = registry
            .with_key("persona-a", "group-1", |k| k.group_key)
            .await;
        assert_eq!(key, Some([0xBBu8; kdf::MASTER_KEY_LEN]));
    }

    #[tokio::test]
    async fn clear_removes_only_the_targeted_entry() {
        let registry = GroupKeyRegistry::default();
        registry
            .replace("persona-a", "group-1", make_group_key("group-1", 0xAA))
            .await;
        registry
            .replace("persona-a", "group-2", make_group_key("group-2", 0xBB))
            .await;

        registry.clear("persona-a", "group-1").await;

        assert!(!registry.is_occupied("persona-a", "group-1").await);
        assert!(registry.is_occupied("persona-a", "group-2").await);
    }

    #[tokio::test]
    async fn clear_on_missing_entry_is_a_no_op() {
        let registry = GroupKeyRegistry::default();
        registry.clear("persona-a", "group-1").await; // must not panic
        assert!(!registry.is_occupied("persona-a", "group-1").await);
    }

    #[tokio::test]
    async fn clear_persona_evicts_all_of_that_personas_entries_but_not_others() {
        let registry = GroupKeyRegistry::default();
        registry
            .replace("persona-a", "group-1", make_group_key("group-1", 0xAA))
            .await;
        registry
            .replace("persona-a", "group-2", make_group_key("group-2", 0xBB))
            .await;
        registry
            .replace("persona-b", "group-1", make_group_key("group-1", 0xCC))
            .await;

        registry.clear_persona("persona-a").await;

        assert!(!registry.is_occupied("persona-a", "group-1").await);
        assert!(!registry.is_occupied("persona-a", "group-2").await);
        assert!(
            registry.is_occupied("persona-b", "group-1").await,
            "clear_persona must not touch a different persona's entries"
        );
    }

    #[tokio::test]
    async fn with_key_returns_none_when_absent() {
        let registry = GroupKeyRegistry::default();
        let result = registry
            .with_key("persona-a", "group-1", |k| k.group_id.clone())
            .await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn with_key_reads_resident_key_data() {
        let registry = GroupKeyRegistry::default();
        registry
            .replace("persona-a", "group-1", make_group_key("group-1", 0x42))
            .await;

        let group_key = registry
            .with_key("persona-a", "group-1", |k| k.group_key)
            .await;
        assert_eq!(group_key, Some([0x42u8; kdf::MASTER_KEY_LEN]));
    }

    #[tokio::test]
    async fn key_hex_for_matches_with_key_and_is_none_when_absent() {
        let registry = GroupKeyRegistry::default();
        assert_eq!(registry.key_hex_for("persona-a", "group-1").await, None);

        registry
            .replace("persona-a", "group-1", make_group_key("group-1", 0x42))
            .await;

        let via_with_key = registry
            .with_key("persona-a", "group-1", |k| key_hex(&k.group_key))
            .await;
        assert_eq!(registry.key_hex_for("persona-a", "group-1").await, via_with_key);
    }

    #[tokio::test]
    async fn resident_keys_reflects_replace_and_clear() {
        let registry = GroupKeyRegistry::default();
        assert_eq!(registry.resident_keys().await, Vec::new());

        registry
            .replace("persona-a", "group-1", make_group_key("group-1", 0xAA))
            .await;
        registry
            .replace("persona-a", "group-2", make_group_key("group-2", 0xBB))
            .await;
        registry
            .replace("persona-b", "group-1", make_group_key("group-1", 0xCC))
            .await;

        let mut pairs = registry.resident_keys().await;
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("persona-a".to_owned(), "group-1".to_owned()),
                ("persona-a".to_owned(), "group-2".to_owned()),
                ("persona-b".to_owned(), "group-1".to_owned()),
            ]
        );

        registry.clear("persona-a", "group-1").await;
        let mut pairs_after_clear = registry.resident_keys().await;
        pairs_after_clear.sort();
        assert_eq!(
            pairs_after_clear,
            vec![
                ("persona-a".to_owned(), "group-2".to_owned()),
                ("persona-b".to_owned(), "group-1".to_owned()),
            ]
        );

        registry.clear_persona("persona-b").await;
        assert_eq!(
            registry.resident_keys().await,
            vec![("persona-a".to_owned(), "group-2".to_owned())]
        );
    }

    #[test]
    fn unlocked_group_key_implements_zeroize_on_drop() {
        // Same reasoning as unlocked_key_implements_zeroize_on_drop above --
        // needs_drop confirms the derive actually generated Drop glue,
        // rather than attempting an unreliable raw-pointer memory inspection.
        assert!(
            std::mem::needs_drop::<UnlockedGroupKey>(),
            "UnlockedGroupKey must have a real Drop impl from \
             #[derive(Zeroize, ZeroizeOnDrop)] -- plain String/[u8; N] fields \
             alone would not need_drop, so this failing would mean the derive \
             did not generate the expected Drop glue"
        );
    }
}
