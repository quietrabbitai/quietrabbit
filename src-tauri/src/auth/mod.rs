// src-tauri/src/auth/mod.rs
//
// Auth module -- cryptographic and lifecycle logic for QR's account/session
// model (items.id=205, Architecture/AUTH_MULTIUSER_ARCHITECTURE.md).
// Deliberately separate from commands/auth.rs, which holds only the thin
// Tauri IPC surface (login/logout/get_recovery_key_display) -- same
// separation persistence/personal_store.rs already uses relative to
// commands/personal.rs.

pub mod group_creation;
pub mod group_invitations;
pub mod group_membership;
pub mod idle_timeout;
pub mod kdf;
pub mod persona_sharing;
pub mod registry;
pub mod sharing_keypair;
pub mod user_store;

use crate::auth::registry::GroupKeyRegistry;
use crate::persistence::persona_store;

/// Evict every group key resident for one account's Personas -- the
/// session-boundary counterpart to KeyRegistry::clear() (items.id=469).
/// Call this alongside key_registry.clear() at every point a session ends:
/// commands::auth::logout() and auth::idle_timeout::run_periodic_check().
/// registry::GroupKeyRegistry::clear_persona() already existed and worked
/// correctly for exactly this case, but nothing called it -- group keys
/// outlived logout/idle-timeout until the process itself exited.
///
/// ENUMERATION: persona_store::list_personas_for_user(user_id) -- the same
/// query commands::auth::finish_login already uses to rehydrate
/// GroupKeyRegistry on login, run in reverse here. No dedicated "all
/// personas under this user_id, for eviction" query was needed; this one
/// already returns exactly that set.
///
/// SAFE BY user_id ALONE: KeyRegistry is single-slot (registry.rs's own
/// header) -- at most one account's master key is ever resident in this
/// process at a time, so there is never a second, still-logged-in account
/// whose Personas this could accidentally clear.
///
/// Best-effort, matching finish_login's own posture on this identical
/// query: a lookup failure is logged and treated as nothing-to-clear rather
/// than propagated -- failing a logout or idle-timeout clear over an
/// ancillary lookup would be worse than leaving one account's group keys
/// resident an extra cycle.
pub async fn clear_group_keys_for_user(
    pool: &sqlx::SqlitePool,
    group_key_registry: &GroupKeyRegistry,
    user_id: &str,
) {
    match persona_store::list_personas_for_user(pool, user_id).await {
        Ok(personas) => {
            for persona in personas {
                group_key_registry.clear_persona(&persona.id).await;
            }
        }
        Err(e) => {
            log::warn!(
                "clear_group_keys_for_user: couldn't list personas for user={user_id}: {e} -- \
                 this account's group keys may remain resident until the next successful call \
                 or process exit"
            );
        }
    }
}
