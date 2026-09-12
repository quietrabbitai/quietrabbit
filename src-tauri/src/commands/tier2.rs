// src-tauri/src/commands/tier2.rs
//
// Group 9 — Tier 2 configuration.
// Commands: get_tier2_config, set_tier2_provider.
//
// Wired (items.id=185, 2026-08-02) against persistence::integration_keys_store
// and auth::registry::KeyRegistry, per Architecture/AUTH_MULTIUSER_ARCHITECTURE.md
// Section 4.2: "every encrypted-store open() call reads key_hex from this
// registry instead of taking it as a bare caller-supplied parameter." This
// item is a deliberate narrow first slice of that contract (tier2.rs only) --
// every other store in this codebase still takes key_hex as a bare parameter,
// confirmed unchanged this session; that is a separate, larger, later
// migration, not part of this item.
//
// SCOPE, deliberate: both commands operate on user-global keys only
// (integration_keys_store::get_active_key/upsert_key called with
// persona_id=None) -- neither command signature carries a persona_id
// parameter today (the stub this replaces did not have one, and no
// frontend caller exists yet to have specified one). Persona-scoped Tier 2
// keys are supported by the underlying schema and store (keys_001.sql's
// persona_id column, integration_keys_store::get_active_key's persona_id
// parameter) but adding that to the IPC surface is a frontend-contract
// decision outside this item's scope -- flagged in this session's handoff.
//
// key_type is hardcoded to "tier2" in both commands -- this module's only
// concern is Tier 2 provider configuration (Groq, Mistral); Tier 3 and
// future non-AI integrations use the same table via a different key_type,
// through their own future command modules.
//
// api_key/credential must NEVER be returned to the frontend (write-only per
// this module's own original spec comment) -- Tier2Config below carries no
// credential field; get_tier2_config only reports whether a key is
// configured and which provider is active, never the key value itself.
//
// FOLLOW-ON NOT DONE HERE (flagged to Chat-PM in this session's handoff):
// providers/groq.rs::get_api_key() still reads GROQ_API_KEY from the
// environment (Layer 6 dev bridge) -- its own doc comment names swapping
// to integration_keys_store retrieval as a distinct future "Layer 8" step
// with a stable signature/error contract. That function is synchronous;
// this module's store calls are async. Wiring it now would force an
// async-fn-vs-block-on-async decision this item does not scope.

use tauri::State;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::persistence::integration_keys_store;
use crate::persistence::provider_store;
use crate::persistence::user_provider_preference_store::{
    self, NewUserProviderPreference, UserPreference, UserProviderPreference,
};

const TIER2_KEY_TYPE: &str = "tier2";

// ---------------------------------------------------------------------------
// IPC types
// ---------------------------------------------------------------------------

/// Non-secret Tier 2 configuration state -- never carries the credential
/// itself. `configured` is true iff an active user-global tier2 key exists
/// for `provider`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct Tier2Config {
    pub provider: String,
    pub configured: bool,
    pub expires_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Report configuration state for a Tier 2 provider. Returns
/// configured=false (not an error) when no key is set yet -- an
/// unconfigured provider is a normal, expected state, not a failure.
#[tauri::command]
#[specta::specta]
pub async fn get_tier2_config(
    provider: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<Tier2Config, String> {
    let session = key_registry
        .with_key(|k| (k.user_id.clone(), key_hex(&k.master_key)))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;
    let (user_id, key_hex_str) = session;

    let existing = integration_keys_store::get_active_key(
        &user_id,
        &key_hex_str,
        &provider,
        TIER2_KEY_TYPE,
        None,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(match existing {
        Some(key) => Tier2Config {
            provider: key.provider,
            configured: true,
            expires_at: key.expires_at,
        },
        None => Tier2Config {
            provider,
            configured: false,
            expires_at: None,
        },
    })
}

/// Set (or replace) the credential for a Tier 2 provider, user-global scope.
#[tauri::command]
#[specta::specta]
pub async fn set_tier2_provider(
    provider: String,
    api_key: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<(), String> {
    let session = key_registry
        .with_key(|k| (k.user_id.clone(), key_hex(&k.master_key)))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;
    let (user_id, key_hex_str) = session;

    integration_keys_store::upsert_key(
        &user_id,
        &key_hex_str,
        &provider,
        TIER2_KEY_TYPE,
        &api_key,
        None,
        Some("api_key"),
        None,
    )
    .await
    .map_err(|e| e.to_string())
}

/// Set (or clear, with `provider: None`) the current user's Tier 2 provider
/// preference -- distinct from set_tier2_provider above, which stores a
/// credential. This is the "which provider should QR actually use" choice.
///
/// items.id=432: lifecycle.rs resolves an account-wide user_provider_preference
/// row via find_preferred_provider()/resolve_preference() (items.id=428/432)
/// instead of the legacy users.tier2_provider_preference column (dropped by
/// items.id=433 -- this command wrote it as a dual-write in the interim, now
/// removed along with the column). Selecting a provider marks its
/// account-wide row Preferred and downgrades any OTHER Tier 1.5 candidate's
/// account-wide row that was previously Preferred to Allowed, preserving
/// find_preferred_provider()'s "at most one Preferred" assumption. Clearing
/// (`provider: None`) downgrades any currently-Preferred candidate the same
/// way, without picking a new one.
#[tauri::command]
#[specta::specta]
pub async fn set_tier2_provider_preference(
    provider: Option<String>,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<(), String> {
    let user_id = key_registry
        .with_key(|k| k.user_id.clone())
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let candidates = provider_store::list_providers_by_type(&pool, "cloud_inference_api")
        .await
        .map_err(|e| e.to_string())?;

    if let Some(p) = &provider {
        if !candidates.iter().any(|c| c.id == *p) {
            let valid = candidates
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "invalid Tier 2 provider: {p}. Valid: {valid}, or null to clear"
            ));
        }
    }

    for candidate in &candidates {
        let existing = user_provider_preference_store::get_preference_at_scope(
            &pool,
            &user_id,
            None,
            None,
            &candidate.id,
        )
        .await
        .map_err(|e| e.to_string())?;

        let is_selected = provider.as_deref() == Some(candidate.id.as_str());
        let was_preferred = existing
            .as_ref()
            .map(|e| e.user_preference == UserPreference::Preferred)
            .unwrap_or(false);

        let new_preference = if is_selected {
            UserPreference::Preferred
        } else if was_preferred {
            UserPreference::Allowed
        } else {
            // Not the selected candidate and never Preferred -- nothing to
            // change (avoids creating a fresh 'allowed' row for every
            // untouched candidate on every call).
            continue;
        };

        let login_available = existing
            .as_ref()
            .map(|e| e.login_available)
            .unwrap_or(false);
        let local_model_version = existing
            .as_ref()
            .and_then(|e| e.local_model_version.clone());
        let subscription_status = existing.as_ref().and_then(|e| e.subscription_status);

        user_provider_preference_store::upsert_preference(
            &pool,
            NewUserProviderPreference {
                user_id: &user_id,
                persona_id: None,
                focus_id: None,
                provider_id: &candidate.id,
                login_available,
                user_preference: new_preference,
                local_model_version: local_model_version.as_deref(),
                subscription_status,
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

/// List every provider preference row for the current user, across every
/// scope (account-wide, Persona-wide, Focus-specific) -- for a future
/// settings-surface listing. items.id=254 Part 1.
#[tauri::command]
#[specta::specta]
pub async fn get_tier2_provider_preferences(
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<Vec<UserProviderPreference>, String> {
    let user_id = key_registry
        .with_key(|k| k.user_id.clone())
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    user_provider_preference_store::list_preferences_for_user(&pool, &user_id)
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{mock_app_with_registry, populate_registry, ENV_MUTEX};
    use tauri::Manager;

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
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

    /// Sets up a tempdir QR_DATA_ROOT and a real, migrated
    /// integration_keys.db for `user_id` under `master_key` -- mirrors
    /// commands/auth.rs's own test setup() pattern (bootstrap via the
    /// real migration path, not a hand-built schema), so this test
    /// exercises the actual production code path end to end: real
    /// SQLCipher file, real KeyRegistry, real store queries.
    fn dummy_pool() -> sqlx::SqlitePool {
        sqlx::SqlitePool::connect_lazy_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(":memory:"),
        )
    }

    async fn setup(user_id: &str, master_key: &[u8; crate::auth::kdf::MASTER_KEY_LEN]) -> TestEnv {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();

        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_keys_db(user_id, &key_hex(master_key))
            .await
            .expect("integration_keys.db migration must succeed in test setup");

        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
        }
    }

    #[tokio::test]
    async fn get_tier2_config_reports_unconfigured_when_no_key_set() {
        let master_key = [0x11u8; crate::auth::kdf::MASTER_KEY_LEN];
        let _env = setup("user-a", &master_key).await;
        let app = mock_app_with_registry(dummy_pool());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, "user-a", master_key).await;

        let config = get_tier2_config("groq".to_owned(), registry.clone())
            .await
            .expect("get_tier2_config must succeed when logged in");

        assert!(!config.configured);
        assert_eq!(config.provider, "groq");
        assert_eq!(config.expires_at, None);
    }

    #[tokio::test]
    async fn set_then_get_round_trips_and_never_exposes_the_key() {
        let master_key = [0x22u8; crate::auth::kdf::MASTER_KEY_LEN];
        let _env = setup("user-b", &master_key).await;
        let app = mock_app_with_registry(dummy_pool());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, "user-b", master_key).await;

        set_tier2_provider(
            "groq".to_owned(),
            "gsk_super_secret_value".to_owned(),
            registry.clone(),
        )
        .await
        .expect("set_tier2_provider must succeed when logged in");

        let config = get_tier2_config("groq".to_owned(), registry.clone())
            .await
            .expect("get_tier2_config must succeed after set_tier2_provider");

        assert!(config.configured);
        assert_eq!(config.provider, "groq");
        // Tier2Config has no field capable of carrying the credential --
        // a structural guarantee checked at compile time by the struct
        // definition itself, not something this test could violate even
        // if it tried. Asserted here as documentation of that intent.
    }

    #[tokio::test]
    async fn set_tier2_provider_fails_cleanly_when_not_logged_in() {
        let app = tauri::test::mock_app();
        app.manage(KeyRegistry::default());
        let registry = app.state::<KeyRegistry>();

        let result = set_tier2_provider("groq".to_owned(), "some-key".to_owned(), registry).await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // set_tier2_provider_preference tests
    //
    // This command touches shared.db (via auth::user_store), not
    // integration_keys.db -- the TestEnv/setup() above (which migrates the
    // SQLCipher keys db) doesn't apply here. This is a separate fixture that
    // mirrors auth/user_store.rs's own setup(): migrate_shared_db() + a real
    // user row via create_user(), combined with this file's existing
    // mock_app_with_registry()/populate_registry() for the KeyRegistry half.
    // -----------------------------------------------------------------------

    struct SharedDbTestEnv {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
        pool: sqlx::SqlitePool,
    }

    impl Drop for SharedDbTestEnv {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn setup_shared_db(user_id: &str) -> SharedDbTestEnv {
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

        crate::auth::user_store::create_user(
            &pool,
            user_id,
            "Alice",
            "admin",
            true,
            b"salt1234",
            1024,
            1,
            1,
            &[0u8; 32],
        )
        .await
        .expect("create_user must succeed in test setup");

        SharedDbTestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
            pool,
        }
    }

    /// items.id=432/433: set_tier2_provider_preference must write an
    /// account-wide Preferred row in user_provider_preference -- the table
    /// lifecycle.rs::find_preferred_provider() actually reads. Switching
    /// the choice must downgrade the previously-Preferred candidate to
    /// Allowed rather than leaving two candidates both Preferred (which
    /// would make find_preferred_provider() return an ambiguous None).
    #[tokio::test]
    async fn set_tier2_provider_preference_downgrades_other_candidate() {
        let master_key = [0x66u8; crate::auth::kdf::MASTER_KEY_LEN];
        let _env = setup_shared_db("user-f").await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, "user-f", master_key).await;
        let pool = app.state::<sqlx::SqlitePool>();

        set_tier2_provider_preference(Some("groq".to_owned()), registry.clone(), pool.clone())
            .await
            .expect("set_tier2_provider_preference must succeed when logged in");

        let groq_pref = user_provider_preference_store::get_preference_at_scope(
            &pool, "user-f", None, None, "groq",
        )
        .await
        .unwrap()
        .expect("an account-wide groq row must exist after selecting groq");
        assert_eq!(groq_pref.user_preference, UserPreference::Preferred);

        // Switch to mistral -- groq's row must be downgraded to Allowed, not
        // left Preferred (which would make find_preferred_provider return
        // an ambiguous None instead of "mistral").
        set_tier2_provider_preference(Some("mistral".to_owned()), registry.clone(), pool.clone())
            .await
            .expect("switching provider must succeed");

        let groq_after = user_provider_preference_store::get_preference_at_scope(
            &pool, "user-f", None, None, "groq",
        )
        .await
        .unwrap()
        .expect("groq's row must still exist, just downgraded");
        assert_eq!(groq_after.user_preference, UserPreference::Allowed);

        let mistral_after = user_provider_preference_store::get_preference_at_scope(
            &pool, "user-f", None, None, "mistral",
        )
        .await
        .unwrap()
        .expect("an account-wide mistral row must exist after selecting mistral");
        assert_eq!(mistral_after.user_preference, UserPreference::Preferred);

        let preferred = user_provider_preference_store::find_preferred_provider(
            &pool,
            "user-f",
            None,
            None,
            &["groq".to_owned(), "mistral".to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(preferred, Some("mistral".to_owned()));
    }

    #[tokio::test]
    async fn set_tier2_provider_preference_rejects_unknown_provider() {
        let master_key = [0x44u8; crate::auth::kdf::MASTER_KEY_LEN];
        let _env = setup_shared_db("user-d").await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, "user-d", master_key).await;
        let pool = app.state::<sqlx::SqlitePool>();

        let result = set_tier2_provider_preference(Some("openai".to_owned()), registry, pool).await;
        let err = result.expect_err("openai is not a valid Tier 2 provider");
        assert!(err.contains("invalid Tier 2 provider"), "got: {err}");
    }

    #[tokio::test]
    async fn set_tier2_provider_preference_fails_cleanly_when_not_logged_in() {
        let app = tauri::test::mock_app();
        app.manage(KeyRegistry::default());
        app.manage(dummy_pool());
        let registry = app.state::<KeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        let result = set_tier2_provider_preference(Some("groq".to_owned()), registry, pool).await;
        assert!(result.is_err());
    }
}
