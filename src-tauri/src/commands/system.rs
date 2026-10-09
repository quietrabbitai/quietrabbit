// src-tauri/src/commands/system.rs
//
// Group 12 — System.
// Commands: get_health, get_capability_profile, get_hardware_profile,
// get_provider_recommendation.
//
// get_health: checks Ollama availability and returns provider health status.
//   ollama_source: "system" | "sidecar" | "unavailable" — written during app
//   setup by OllamaSidecar::ensure_available(); read from RwLock<OllamaSource>.
//   Returns "unavailable" during the brief startup detection window.
//   qr_hosted_configured wired items.id=229 (2026-08-10) against
//   integration_keys_store::get_active_key, the same lookup qr_hosted.rs's
//   get_qr_hosted_config already uses (items.id=185, 2026-08-02) -- see that
//   command's read path just below for the full aggregate-vs-per-provider
//   and no-session reasoning.
// get_capability_profile: returns installed models and benchmark status.
//   recommended_routing omitted -- evaluation/scores DB not yet ported.
//   Release 1 benchmark_status values: "pending" (models present, no scores
//   yet) or "unavailable" (no models detected). "complete" requires scores DB.
// get_hardware_profile: items.id=435 -- cached RAM/CPU/GPU capability probe
//   (hardware_probe::get_or_detect), matchable against providers.
//   hardware_requirement (Part 4a). No caller in this app yet -- this
//   command exists so items.id=437's onboarding UI has something to call.
// get_provider_recommendation: items.id=607 -- Step 1 engine (see
//   recommendation.rs). Callable before login: like get_health it only
//   needs the shared.db pool; without a resident session the hosted
//   needs_api_key lookup degrades to "no key" instead of failing.

use serde::Serialize;
use specta::Type;
use tokio::sync::RwLock;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::hardware_probe::{self, HardwareProfile, RamClass};
use crate::ollama_sidecar::SidecarStartup;
use crate::persistence::{integration_keys_store, provider_store};
use crate::providers::ollama_client::OllamaClient;
use crate::providers::types::{ProviderHealth, ProviderStatus};
use crate::recommendation::{self, Candidate, CandidateKind, ProviderRecommendation};

const QR_HOSTED_KEY_TYPE: &str = "qr_hosted";

// ---------------------------------------------------------------------------
// Response structs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Type)]
pub struct HealthResponse {
    pub ollama: ProviderHealth,
    /// "sidecar" | "unavailable" -- no "system" value any more
    /// (decisions.id=840, items.id=436): QR always starts its own sidecar
    /// regardless of what's detected on 11434, so this field only ever
    /// describes QR's own sidecar outcome now. Set during app setup by
    /// OllamaSidecar::ensure_available(). "unavailable" is returned during
    /// the brief startup detection window.
    pub ollama_source: String,
    /// items.id=436: true iff a separate, untouched user Ollama was also
    /// seen on 127.0.0.1:11434 at startup -- a contention warning only
    /// (possible shared GPU/RAM load from two Ollama processes), never a
    /// signal that QR is using or trusting that instance's models.
    pub system_ollama_contention: bool,
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
    pub qr_hosted_configured: bool,
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
    sidecar_startup: tauri::State<'_, RwLock<SidecarStartup>>,
    key_registry: tauri::State<'_, KeyRegistry>,
    pool: tauri::State<'_, sqlx::SqlitePool>,
) -> Result<HealthResponse, String> {
    let ollama = client.check_health().await;
    let startup = sidecar_startup.read().await.clone();
    let qr_hosted_configured = qr_hosted_is_configured(&pool, &key_registry).await?;

    Ok(HealthResponse {
        ollama,
        ollama_source: startup.source.as_str().to_owned(),
        system_ollama_contention: startup.system_ollama_contention,
        qr_hosted_configured,
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
async fn qr_hosted_is_configured(
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

#[tauri::command]
#[specta::specta]
pub async fn get_hardware_profile(
    pool: tauri::State<'_, sqlx::SqlitePool>,
) -> Result<HardwareProfile, String> {
    Ok(hardware_probe::get_or_detect(&pool).await)
}

/// Decimal megabytes per curated min_vram_gb (probe vram_mb is decimal MB).
const MB_PER_GB: u32 = 1000;

/// Reduces a providers row to an engine candidate. Local candidates are
/// is_local rows whose hardware_requirement carries a parsable
/// min_ram_class; one that does not is skipped with a warning rather than
/// guessed at. Hosted candidates are qr_recommended cloud_inference_api rows
/// (the per-type recommendation slot recorded in shared_016.sql).
fn to_candidate(p: &provider_store::Provider, has_key: bool) -> Option<Candidate> {
    if p.is_local {
        let req = p.hardware_requirement.as_ref()?;
        let min_ram_class = req
            .get("min_ram_class")
            .and_then(|v| serde_json::from_value::<RamClass>(v.clone()).ok());
        let Some(min_ram_class) = min_ram_class else {
            log::warn!(
                "provider '{}' hardware_requirement has no usable min_ram_class, skipped",
                p.id
            );
            return None;
        };
        let min_vram_mb = req
            .get("min_vram_gb")
            .and_then(|v| v.as_u64())
            .and_then(|gb| u32::try_from(gb).ok())
            .and_then(|gb| gb.checked_mul(MB_PER_GB));
        return Some(Candidate {
            provider_id: p.id.clone(),
            kind: CandidateKind::LocalModel,
            rank: p.qr_recommendation_rank,
            min_ram_class: Some(min_ram_class),
            min_vram_mb,
            installed: p.installed,
            has_key: false,
        });
    }
    if is_hosted_candidate(p) {
        return Some(Candidate {
            provider_id: p.id.clone(),
            kind: CandidateKind::HostedApi,
            rank: p.qr_recommendation_rank,
            min_ram_class: None,
            min_vram_mb: None,
            installed: false,
            has_key,
        });
    }
    None
}

/// Hosted candidate = qr_recommended cloud_inference_api row (the
/// per-type recommendation slot recorded in shared_016.sql).
fn is_hosted_candidate(p: &provider_store::Provider) -> bool {
    !p.is_local && p.qr_recommended && p.provider_type == "cloud_inference_api"
}

/// A failed key lookup must not fail the recommendation: it degrades to "no
/// key" (the user is simply asked for one) and is logged.
fn key_lookup_or_false<T, E: std::fmt::Display>(
    provider_id: &str,
    result: Result<Option<T>, E>,
) -> bool {
    match result {
        Ok(found) => found.is_some(),
        Err(e) => {
            log::warn!("key lookup for provider '{provider_id}' failed, treating as no key: {e}");
            false
        }
    }
}

#[tauri::command]
#[specta::specta]
pub async fn get_provider_recommendation(
    pool: tauri::State<'_, sqlx::SqlitePool>,
    key_registry: tauri::State<'_, KeyRegistry>,
) -> Result<ProviderRecommendation, String> {
    let profile = hardware_probe::get_or_detect(&pool).await;
    let session = key_registry
        .with_key(|k| (k.user_id.clone(), key_hex(&k.master_key)))
        .await;
    let providers = provider_store::list_active_providers(&pool)
        .await
        .map_err(|e| e.to_string())?;

    let mut catalog = Vec::new();
    for p in &providers {
        // The key lookup is only for hosted candidates; local rows and
        // non-candidates never touch integration_keys.db.
        let has_key = match &session {
            Some((user_id, key_hex_str)) if is_hosted_candidate(p) => key_lookup_or_false(
                &p.id,
                integration_keys_store::get_active_key(
                    user_id,
                    key_hex_str,
                    &p.id,
                    QR_HOSTED_KEY_TYPE,
                    None,
                )
                .await,
            ),
            _ => false,
        };
        if let Some(c) = to_candidate(p, has_key) {
            catalog.push(c);
        }
    }
    Ok(recommendation::recommend(&profile, &catalog))
}

// Tests target qr_hosted_is_configured() directly rather than the get_health
// command -- it's the only new logic here; get_health itself is glue over
// OllamaClient::check_health() (a real network call, irrelevant to this
// field) plus this function. Harness mirrors qr_hosted.rs's own test module
// (setup/mock_app_with_registry/populate_registry) -- same real
// SQLCipher-file-backed integration_keys.db, same reasoning for why.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{mock_app_with_registry, populate_registry, ENV_MUTEX};
    use tauri::Manager;

    #[test]
    fn key_lookup_error_degrades_to_no_key() {
        assert!(!key_lookup_or_false::<(), _>("p", Err("db unreadable")));
        assert!(!key_lookup_or_false::<(), &str>("p", Ok(None)));
        assert!(key_lookup_or_false::<_, &str>("p", Ok(Some(()))));
    }

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
        // items.id=430: qr_hosted_is_configured() now reads the qr_hosted
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
        let result = qr_hosted_is_configured(&pool, &registry).await;
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

        let result = qr_hosted_is_configured(&pool, &registry).await;
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

        let result = qr_hosted_is_configured(&pool, &registry).await;
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

        let result = qr_hosted_is_configured(&pool, &registry).await;
        assert_eq!(result, Ok(true));
    }
}
