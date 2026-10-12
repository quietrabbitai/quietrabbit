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
// get_provider_recommendation: items.id=607, items.id=712 -- Step 1 engine
//   (see recommendation.rs). Callable before login: like get_health it only
//   needs the shared.db pool; without a resident session the key-presence
//   and plan lookups degrade to "none" instead of failing.

use serde::Serialize;
use specta::Type;
use tokio::sync::RwLock;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::hardware_probe::{self, HardwareProfile, RamClass};
use crate::ollama_sidecar::SidecarStartup;
use crate::persistence::integration_keys_store::{self, QR_HOSTED_KEY_TYPE};
use crate::persistence::provider_store;
use crate::persistence::user_provider_preference_store;
use crate::providers::ollama_client::OllamaClient;
use crate::providers::types::{ProviderHealth, ProviderStatus};
use crate::recommendation::{self, Candidate, CandidateKind, ProviderRecommendation, UserContext};

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
    /// hardcoded provider-id array before this) -- a capability-status
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
/// provider-id array, so a future qr_hosted provider is picked up
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

/// Reduces an is_local providers row to an engine candidate. A local
/// candidate's hardware_requirement must carry a parsable min_ram_class; one
/// that does not is skipped with a warning rather than guessed at. Hosted
/// candidates are reduced by recommendation::hosted_candidate (items.id=712).
fn local_candidate(p: &provider_store::Provider) -> Option<Candidate> {
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
    Some(Candidate {
        provider_id: p.id.clone(),
        kind: CandidateKind::LocalModel,
        rank: p.qr_recommendation_rank,
        min_ram_class: Some(min_ram_class),
        min_vram_mb,
        installed: p.installed,
        has_key: false,
        hosted: None,
    })
}

/// A failed lookup must not fail the recommendation: it degrades to the empty
/// value (no key configured, no plan known -- the user is simply asked) and is
/// logged.
fn lookup_or_default<T: Default, E: std::fmt::Display>(what: &str, result: Result<T, E>) -> T {
    match result {
        Ok(found) => found,
        Err(e) => {
            log::warn!("{what} lookup failed, treating as none: {e}");
            T::default()
        }
    }
}

/// The per-user ranking facts: which hosted providers have an active key
/// (presence only, the credential column is never read) and the user's plan
/// at each provider (account-wide preference rows). Empty with no resident
/// session.
async fn load_user_context(
    pool: &sqlx::SqlitePool,
    session: Option<&(String, String)>,
) -> UserContext {
    let Some((user_id, key_hex_str)) = session else {
        return UserContext::default();
    };
    let configured = lookup_or_default(
        "hosted key presence",
        integration_keys_store::active_key_provider_ids(user_id, key_hex_str, QR_HOSTED_KEY_TYPE)
            .await,
    );
    let prefs = lookup_or_default(
        "plan keys",
        user_provider_preference_store::list_preferences_for_user(pool, user_id).await,
    );
    let plan_keys = prefs
        .into_iter()
        .filter(|p| p.persona_id.is_none() && p.focus_id.is_none())
        .filter_map(|p| p.plan_key.map(|k| (p.provider_id, k)))
        .collect();
    UserContext {
        configured_provider_ids: configured.into_iter().collect(),
        plan_keys,
    }
}

/// The engine catalog: local rows plus the hosted eligible set, with each
/// hosted row's default model and raw pricing text read for it and nothing
/// else.
async fn load_catalog(
    pool: &sqlx::SqlitePool,
    ctx: &UserContext,
    today: chrono::NaiveDate,
) -> Result<Vec<Candidate>, String> {
    let providers = provider_store::list_active_providers(pool)
        .await
        .map_err(|e| e.to_string())?;

    let mut catalog = Vec::new();
    for p in &providers {
        if p.is_local {
            catalog.extend(local_candidate(p));
            continue;
        }
        if p.provider_type != recommendation::HOSTED_PROVIDER_TYPE {
            continue;
        }
        let model = provider_store::get_default_model(pool, &p.id)
            .await
            .map_err(|e| e.to_string())?;
        let pricing = provider_store::list_model_pricing_text(pool, &p.id)
            .await
            .map_err(|e| e.to_string())?;
        let pricing_text = model.as_ref().and_then(|m| {
            pricing
                .iter()
                .find(|(id, _)| *id == m.model_id)
                .and_then(|(_, t)| t.as_deref())
        });
        catalog.extend(recommendation::hosted_candidate(
            p,
            model.as_ref(),
            pricing_text,
            ctx,
            today,
        ));
    }
    Ok(catalog)
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
    let ctx = load_user_context(&pool, session.as_ref()).await;
    let catalog = load_catalog(&pool, &ctx, chrono::Local::now().date_naive()).await?;
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
    fn lookup_error_degrades_to_none() {
        assert!(lookup_or_default::<Vec<String>, _>("p", Err("db unreadable")).is_empty());
        assert_eq!(
            lookup_or_default::<Vec<String>, &str>("p", Ok(vec!["x".to_owned()])),
            vec!["x".to_owned()]
        );
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
        // finds the seeded hosted rows.
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

    /// The seeded hosted provider ids, by field (decisions.id=872: no test
    /// names a provider). Ordered as list_providers_by_type returns them.
    async fn hosted_ids(pool: &sqlx::SqlitePool) -> Vec<String> {
        let ids: Vec<String> = provider_store::list_providers_by_type(pool, "cloud_inference_api")
            .await
            .expect("list_providers_by_type must succeed")
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert!(ids.len() >= 2, "at least two seeded hosted providers");
        ids
    }

    #[tokio::test]
    async fn true_when_the_first_hosted_provider_is_configured() {
        let master_key = [0x44u8; crate::auth::kdf::MASTER_KEY_LEN];
        let _env = setup("user-d", &master_key).await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, "user-d", master_key).await;
        let pool = app.state::<sqlx::SqlitePool>();
        let first = hosted_ids(&pool).await.remove(0);

        integration_keys_store::upsert_key(
            "user-d",
            &key_hex(&master_key),
            &first,
            QR_HOSTED_KEY_TYPE,
            "fixture_secret_value",
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
    async fn true_when_only_the_last_hosted_provider_is_configured() {
        // Aggregate-across-providers behavior: every candidate but the last
        // is unset, the last is set -- must still report true. Guards against
        // a scan that only ever checked the first provider in the
        // cloud_inference_api candidate list.
        let master_key = [0x55u8; crate::auth::kdf::MASTER_KEY_LEN];
        let _env = setup("user-e", &master_key).await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, "user-e", master_key).await;
        let pool = app.state::<sqlx::SqlitePool>();
        let last = hosted_ids(&pool).await.pop().expect("hosted ids");

        integration_keys_store::upsert_key(
            "user-e",
            &key_hex(&master_key),
            &last,
            QR_HOSTED_KEY_TYPE,
            "fixture_secret_value",
            None,
            Some("api_key"),
            None,
        )
        .await
        .expect("upsert_key must succeed in test setup");

        let result = qr_hosted_is_configured(&pool, &registry).await;
        assert_eq!(result, Ok(true));
    }

    // items.id=712: the IPC-layer loading, against the real encrypted keys
    // store and the migrated, seeded shared.db.

    #[tokio::test]
    async fn user_context_is_empty_with_no_session() {
        let pool = sqlx::SqlitePool::connect_lazy_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(":memory:"),
        );
        let ctx = load_user_context(&pool, None).await;
        assert!(ctx.configured_provider_ids.is_empty());
        assert!(ctx.plan_keys.is_empty());
    }

    #[tokio::test]
    async fn user_context_carries_key_presence_and_account_wide_plan_keys() {
        let master_key = [0x66u8; crate::auth::kdf::MASTER_KEY_LEN];
        let _env = setup("user-f", &master_key).await;
        let ids = hosted_ids(&_env.pool).await;
        let (keyed, planned, scoped) = (ids[0].clone(), ids[1].clone(), ids[2].clone());

        integration_keys_store::upsert_key(
            "user-f",
            &key_hex(&master_key),
            &keyed,
            QR_HOSTED_KEY_TYPE,
            "fixture_secret_value",
            None,
            Some("api_key"),
            None,
        )
        .await
        .expect("upsert_key must succeed in test setup");

        sqlx::query(
            "INSERT INTO users (id, display_name, role, is_primary, auth_enabled, created_at) \
             VALUES ('user-f', 'Fixture User', 'user', 1, 1, '2026-01-01T00:00:00Z')",
        )
        .execute(&_env.pool)
        .await
        .expect("fixture user");
        sqlx::query(
            "INSERT INTO personas (id, display_name, persona_type, created_at) \
             VALUES ('fixture-persona', 'Fixture', 'personal', '2026-01-01T00:00:00Z')",
        )
        .execute(&_env.pool)
        .await
        .expect("fixture persona");
        user_provider_preference_store::upsert_preference(
            &_env.pool,
            user_provider_preference_store::NewUserProviderPreference {
                user_id: "user-f",
                persona_id: None,
                focus_id: None,
                provider_id: &planned,
                login_available: false,
                user_preference: user_provider_preference_store::UserPreference::Allowed,
                local_model_version: None,
                subscription_status: None,
                plan_key: Some("fixture-plan"),
                model_id: None,
            },
        )
        .await
        .expect("account-wide preference row");
        // A Persona-scoped plan is not the account-wide plan.
        user_provider_preference_store::upsert_preference(
            &_env.pool,
            user_provider_preference_store::NewUserProviderPreference {
                user_id: "user-f",
                persona_id: Some("fixture-persona"),
                focus_id: None,
                provider_id: &scoped,
                login_available: false,
                user_preference: user_provider_preference_store::UserPreference::Allowed,
                local_model_version: None,
                subscription_status: None,
                plan_key: Some("fixture-plan"),
                model_id: None,
            },
        )
        .await
        .expect("persona-scoped preference row");

        let session = ("user-f".to_owned(), key_hex(&master_key));
        let ctx = load_user_context(&_env.pool, Some(&session)).await;
        assert_eq!(
            ctx.configured_provider_ids.iter().collect::<Vec<_>>(),
            vec![&keyed]
        );
        assert_eq!(ctx.plan_keys.len(), 1);
        assert_eq!(
            ctx.plan_keys.get(&planned).map(String::as_str),
            Some("fixture-plan")
        );
    }

    #[tokio::test]
    async fn the_seeded_catalog_yields_only_clean_flagged_recommendations() {
        let master_key = [0x77u8; crate::auth::kdf::MASTER_KEY_LEN];
        let _env = setup("user-g", &master_key).await;
        let catalog = load_catalog(
            &_env.pool,
            &UserContext::default(),
            chrono::NaiveDate::from_ymd_opt(2026, 10, 12).unwrap(),
        )
        .await
        .expect("catalog");
        let hosted: Vec<&Candidate> = catalog
            .iter()
            .filter(|c| c.kind == CandidateKind::HostedApi)
            .collect();
        assert!(!hosted.is_empty(), "the curated hosted rows are eligible");

        // Weak hardware, so a hosted entry leads if any is recommendable.
        let profile = HardwareProfile {
            profile_version: hardware_probe::PROFILE_VERSION,
            ram_mb: 4_000,
            ram_class: RamClass::from_mb(4_000),
            cpu_cores: 4,
            gpus: Vec::new(),
            primary_gpu: None,
        };
        let r = recommendation::recommend(&profile, &catalog);
        let providers = provider_store::list_active_providers(&_env.pool)
            .await
            .unwrap();
        let row = |id: &str| providers.iter().find(|p| p.id == id).unwrap();

        for entry in r.recommended.iter().chain(r.alternatives.iter()) {
            if entry.kind != CandidateKind::HostedApi {
                continue;
            }
            let p = row(&entry.provider_id);
            assert!(p.qr_recommended, "flag required");
            assert_eq!(
                p.verification_status,
                provider_store::VerificationStatus::Verified
            );
            assert!(!p.trains_on_data_by_default && p.risk_rating <= 1);
        }
        // Every hosted candidate is accounted for: the recommendation, an
        // alternative or an other provider. Nothing is dropped.
        let is_hosted = |e: &&recommendation::Recommendation| e.kind == CandidateKind::HostedApi;
        let listed = r.recommended.iter().filter(is_hosted).count()
            + r.alternatives.iter().filter(is_hosted).count()
            + r.other_providers.len();
        assert_eq!(listed, hosted.len());
        assert!(r.other_providers.iter().all(|o| o.cost_class.is_some()));
        // Curated ranks are unique, so nothing is tied.
        assert!(r
            .recommended
            .iter()
            .chain(r.alternatives.iter())
            .chain(r.other_providers.iter())
            .all(|e| !e.tied));
    }
}
