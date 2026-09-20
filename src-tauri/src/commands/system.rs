// src-tauri/src/commands/system.rs
//
// Group 12 — System.
// Commands: get_health, get_capability_profile.
//
// get_health: checks Ollama availability and returns provider health status.
//   ollama_source: "system" | "sidecar" | "unavailable" — written during app
//   setup by OllamaSidecar::ensure_available(); read from RwLock<OllamaSource>.
//   Returns "unavailable" during the brief startup detection window.
//   tier2_configured wired items.id=229 (2026-08-10) against
//   integration_keys_store::get_active_key, the same lookup tier2.rs's
//   get_tier2_config already uses (items.id=185, 2026-08-02) -- see that
//   command's read path just below for the full aggregate-vs-per-provider
//   and no-session reasoning.
// get_capability_profile: returns installed models and benchmark status.
//   recommended_routing omitted -- evaluation/scores DB not yet ported.
//   Release 1 benchmark_status values: "pending" (models present, no scores
//   yet) or "unavailable" (no models detected). "complete" requires scores DB.

use serde::Serialize;
use specta::Type;
use tokio::sync::RwLock;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::ollama_sidecar::OllamaSource;
use crate::persistence::{integration_keys_store, provider_store};
use crate::providers::ollama_client::OllamaClient;
use crate::providers::types::{ProviderHealth, ProviderStatus};

const QR_HOSTED_KEY_TYPE: &str = "qr_hosted";

// ---------------------------------------------------------------------------
// Response structs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Type)]
pub struct HealthResponse {
    pub ollama: ProviderHealth,
    /// "system" | "sidecar" | "unavailable".
    /// Set during app setup by OllamaSidecar::ensure_available().
    /// "unavailable" is returned during the brief startup detection window.
    pub ollama_source: String,
    /// True iff an active user-global key exists for ANY qr_hosted provider
    /// (providers.provider_type='cloud_inference_api' -- items.id=430; was a
    /// hardcoded ["mistral","groq"] array before this) -- a capability-status
    /// signal ("is qr_hosted usable at all," e.g. for an onboarding nudge),
    /// not a report of which provider is active. Provider *selection* at
    /// execution time is a separate concern, wired through
    /// user_provider_preference_store::resolve_preference() (items.id=432) --
    /// out of scope for this field, there is no per-provider consumer
    /// downstream to feed. False, not an error, when no session is resident
    /// -- get_health must stay callable pre-login (Ollama status has no such
    /// requirement).
    pub tier2_configured: bool,
}

#[derive(Debug, Serialize, Type)]
pub struct CapabilityProfileResponse {
    pub installed_models: Vec<String>,
    /// Release 1: "pending" | "unavailable" only.
    /// "complete" requires evaluation/scores DB port (post-Release 1).
    /// STUB: recommended_routing omitted until scores DB is ported.
    pub benchmark_status: String,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn get_health(
    client: tauri::State<'_, OllamaClient>,
    ollama_source: tauri::State<'_, RwLock<OllamaSource>>,
    key_registry: tauri::State<'_, KeyRegistry>,
    pool: tauri::State<'_, sqlx::SqlitePool>,
) -> Result<HealthResponse, String> {
    let ollama = client.check_health().await;
    let source = ollama_source.read().await.as_str().to_owned();
    let tier2_configured = tier2_is_configured(&pool, &key_registry).await?;

    Ok(HealthResponse {
        ollama,
        ollama_source: source,
        tier2_configured,
    })
}

/// False (not an error) with no resident session -- see HealthResponse's
/// own doc comment on why get_health must stay usable pre-login. True as
/// soon as ANY qr_hosted provider has an active user-global key;
/// short-circuits on the first hit rather than checking every candidate
/// unconditionally. items.id=430: the candidate set is read from
/// providers.provider_type='cloud_inference_api' instead of a hardcoded
/// ["mistral","groq"] array, so a future qr_hosted provider is picked up
/// automatically once curated into the providers table.
async fn tier2_is_configured(
    pool: &sqlx::SqlitePool,
    key_registry: &KeyRegistry,
) -> Result<bool, String> {
    let session = key_registry
        .with_key(|k| (k.user_id.clone(), key_hex(&k.master_key)))
        .await;
    let Some((user_id, key_hex_str)) = session else {
        return Ok(false);
    };

    let candidates = provider_store::list_providers_by_type(pool, "cloud_inference_api")
        .await
        .map_err(|e| e.to_string())?;
    for provider in candidates {
        let found = integration_keys_store::get_active_key(
            &user_id,
            &key_hex_str,
            &provider.id,
            QR_HOSTED_KEY_TYPE,
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
        if found.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[tauri::command]
#[specta::specta]
pub async fn get_capability_profile(
    client: tauri::State<'_, OllamaClient>,
) -> Result<CapabilityProfileResponse, String> {
    let health = client.check_health().await;

    let installed_models = if health.status == ProviderStatus::Available {
        health.available_models
    } else {
        vec![]
    };

    let benchmark_status = if installed_models.is_empty() {
        "unavailable".to_string()
    } else {
        // Cached scores only in Release 1 -- no live benchmark trigger via IPC.
        // Returns "pending" until evaluation/scores DB is ported.
        "pending".to_string()
    };

    Ok(CapabilityProfileResponse {
        installed_models,
        benchmark_status,
    })
}

// Tests target tier2_is_configured() directly rather than the get_health
// command -- it's the only new logic here; get_health itself is glue over
// OllamaClient::check_health() (a real network call, irrelevant to this
// field) plus this function. Harness mirrors tier2.rs's own test module
// (setup/mock_app_with_registry/populate_registry) -- same real
// SQLCipher-file-backed integration_keys.db, same reasoning for why.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{mock_app_with_registry, populate_registry, ENV_MUTEX};
    use tauri::Manager;

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

    async fn setup(user_id: &str, master_key: &[u8; crate::auth::kdf::MASTER_KEY_LEN]) -> TestEnv {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();

        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_keys_db(user_id, &key_hex(master_key))
            .await
            .expect("integration_keys.db migration must succeed in test setup");
        // items.id=430: tier2_is_configured() now reads the qr_hosted
        // candidate set from providers (shared.db) instead of a hardcoded
        // array -- shared.db must be migrated too so list_providers_by_type
        // finds the seeded groq/mistral rows.
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

    #[tokio::test]
    async fn false_with_no_resident_session_not_an_error() {
        let pool = sqlx::SqlitePool::connect_lazy_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(":memory:"),
        );
        let registry = KeyRegistry::default();
        let result = tier2_is_configured(&pool, &registry).await;
        assert_eq!(result, Ok(false));
    }

    #[tokio::test]
    async fn false_when_logged_in_but_no_provider_configured() {
        let master_key = [0x33u8; crate::auth::kdf::MASTER_KEY_LEN];
        let _env = setup("user-c", &master_key).await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, "user-c", master_key).await;
        let pool = app.state::<sqlx::SqlitePool>();

        let result = tier2_is_configured(&pool, &registry).await;
        assert_eq!(result, Ok(false));
    }

    #[tokio::test]
    async fn true_when_groq_is_configured() {
        let master_key = [0x44u8; crate::auth::kdf::MASTER_KEY_LEN];
        let _env = setup("user-d", &master_key).await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, "user-d", master_key).await;
        let pool = app.state::<sqlx::SqlitePool>();

        integration_keys_store::upsert_key(
            "user-d",
            &key_hex(&master_key),
            "groq",
            QR_HOSTED_KEY_TYPE,
            "gsk_super_secret_value",
            None,
            Some("api_key"),
            None,
        )
        .await
        .expect("upsert_key must succeed in test setup");

        let result = tier2_is_configured(&pool, &registry).await;
        assert_eq!(result, Ok(true));
    }

    #[tokio::test]
    async fn true_when_only_mistral_is_configured() {
        // Aggregate-across-providers behavior: groq unset, mistral set --
        // must still report true. Guards against a scan that only ever
        // checked the first provider in the cloud_inference_api candidate
        // list.
        let master_key = [0x55u8; crate::auth::kdf::MASTER_KEY_LEN];
        let _env = setup("user-e", &master_key).await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, "user-e", master_key).await;
        let pool = app.state::<sqlx::SqlitePool>();

        integration_keys_store::upsert_key(
            "user-e",
            &key_hex(&master_key),
            "mistral",
            QR_HOSTED_KEY_TYPE,
            "mistral_super_secret_value",
            None,
            Some("api_key"),
            None,
        )
        .await
        .expect("upsert_key must succeed in test setup");

        let result = tier2_is_configured(&pool, &registry).await;
        assert_eq!(result, Ok(true));
    }
}
