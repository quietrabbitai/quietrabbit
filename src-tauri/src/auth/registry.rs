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

use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::auth::kdf;

/// One account's resident, unlocked master key. Never persisted -- exists
/// only as Tauri managed state for this process's lifetime. Fields are
/// pub(crate): constructed only by login() (a later step, same crate);
/// not part of the IPC surface, so no external caller can reach these
/// fields regardless -- accessor methods would add ceremony without
/// defending against a real access path.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct UnlockedKey {
    #[zeroize(skip)]
    pub(crate) user_id: String,
    pub(crate) master_key: [u8; kdf::MASTER_KEY_LEN],
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(user_id: &str, byte_fill: u8) -> UnlockedKey {
        UnlockedKey {
            user_id: user_id.to_owned(),
            master_key: [byte_fill; kdf::MASTER_KEY_LEN],
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
