// src-tauri/src/commands/provider_catalog.rs
//
// Group 27 -- the signed provider catalog update (items.id=713,
// decisions.id=875 point 6, spec Part 8f). Four commands:
//   get_catalog_status      local read
//   check_catalog_update    USER-CLICKED NETWORK STEP: fetch, verify, stage,
//                           preview. Writes no provider or model row.
//   apply_catalog_update    admin only: apply the previewed catalog
//   rollback_catalog        admin only: undo the newest applied catalog
//
// CONTRACT NOTES (the frontend's contract; the handoff repeats them):
//   - Every statement is a stable CODE (provider_catalog::codes), optionally
//     followed by `:` and a short detail (ids and field names only). No
//     user-facing prose is produced here.
//   - Versions are YYYYMMDDNN numbers that fit 32 bits.
//   - `check_catalog_update` contacts the QR repository's release host and
//     nothing else. It sends no identifier, no query string, no app or
//     catalog version and no user content. The screen must say that the host
//     (GitHub) sees the user's IP address.
//   - Apply and rollback change data shared by every account on this install,
//     so they need an admin account (users.role = 'admin').
//   - Retiring a model, adding a model, or a price that changes a model's
//     cost class makes every user of that provider consent again; the preview
//     lists those providers with `catalog.consent.*` reasons before the click.
//   - Until the production public key is compiled in, check_catalog_update
//     returns `catalog.error.no_trusted_key` without any network traffic.
//
// serde_json::Value is NOT used in any exported type (specta's TypeScript
// exporter does not terminate on it; see commands::provider_spend).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::auth::user_store::find_user_by_id;
use crate::persistence::provider_catalog_store as store;
use crate::pinned_asset::{PinnedFetcher, GITHUB_RELEASE_REDIRECT_HOSTS};
use crate::provider_catalog::apply::{Change, ConsentEffect, Outcome, Skip};
use crate::provider_catalog::flow::{
    self, check_for_update, load_staged, CheckResult, Failure, Staged,
};
use crate::provider_catalog::verify::TRUSTED_PUBLIC_KEYS;
use crate::provider_catalog::{baseline_version, codes, RELEASE_BASE_URL};

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Type)]
pub struct CatalogChangeDto {
    pub provider_id: String,
    pub model_id: Option<String>,
    /// The field that changes (`pricing`, `status`, `new_model`, ...), or
    /// `restored` / `removed` for a rollback.
    pub field: String,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct CatalogSkipDto {
    /// `catalog.skip.*`: the catalog carried an older value than the one in
    /// force and was not applied for that field.
    pub code: String,
    pub provider_id: String,
    pub model_id: Option<String>,
    pub field: String,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct CatalogConsentEffectDto {
    pub provider_id: String,
    /// `catalog.consent.model_added | model_retired | cost_class_changed`.
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct CatalogPreviewDto {
    pub version: u32,
    /// Pass this back to `apply_catalog_update`: what is applied is exactly
    /// what was previewed.
    pub sha256: String,
    pub changes: Vec<CatalogChangeDto>,
    pub skipped: Vec<CatalogSkipDto>,
    pub consent_effects: Vec<CatalogConsentEffectDto>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct CatalogCheckDto {
    /// `catalog.state.up_to_date` or `catalog.state.update_available`.
    pub state: String,
    pub preview: Option<CatalogPreviewDto>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct CatalogStatusDto {
    /// The catalog in force; None = none applied (the release's own data).
    pub applied_version: Option<u32>,
    pub applied_at: Option<String>,
    /// The highest version the user rolled back, if any.
    pub declined_version: Option<u32>,
    /// The catalog version this release carries.
    pub baseline_version: u32,
    pub can_roll_back: bool,
    /// False until the production public key is compiled in; updates are
    /// unavailable while it is false.
    pub trusted_key_configured: bool,
    /// The only host a check contacts (for the screen's disclosure line).
    pub update_source_host: String,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct CatalogApplyDto {
    /// The version now in force (None after rolling back the only catalog).
    pub version: Option<u32>,
    pub changes: Vec<CatalogChangeDto>,
    pub consent_effects: Vec<CatalogConsentEffectDto>,
}

fn change_dto(c: &Change) -> CatalogChangeDto {
    CatalogChangeDto {
        provider_id: c.provider_id.clone(),
        model_id: c.model_id.clone(),
        field: c.field.to_owned(),
    }
}

fn skip_dto(s: &Skip) -> CatalogSkipDto {
    CatalogSkipDto {
        code: s.code.to_owned(),
        provider_id: s.provider_id.clone(),
        model_id: s.model_id.clone(),
        field: s.field.to_owned(),
    }
}

fn effect_dto(e: &ConsentEffect) -> CatalogConsentEffectDto {
    CatalogConsentEffectDto {
        provider_id: e.provider_id.clone(),
        reasons: e.reasons.iter().map(|r| (*r).to_owned()).collect(),
    }
}

fn version_u32(v: u64) -> Option<u32> {
    u32::try_from(v).ok()
}

fn error_text(f: &Failure) -> String {
    if f.detail.is_empty() {
        f.code.to_owned()
    } else {
        format!("{}:{}", f.code, f.detail)
    }
}

fn internal(what: &str, e: impl std::fmt::Display) -> String {
    log::warn!("provider_catalog: {what}: {e}");
    codes::ERROR_INTERNAL.to_owned()
}

// ---------------------------------------------------------------------------
// Session and role
// ---------------------------------------------------------------------------

async fn session_user(key_registry: &KeyRegistry) -> Result<String, String> {
    key_registry
        .with_key(|k| k.user_id.clone())
        .await
        .ok_or_else(|| codes::ERROR_NOT_LOGGED_IN.to_owned())
}

pub(crate) async fn require_admin(pool: &sqlx::SqlitePool, user_id: &str) -> Result<(), String> {
    let user = find_user_by_id(pool, user_id)
        .await
        .map_err(|e| internal("user lookup", e))?;
    match user {
        Some(u) if u.role == "admin" => Ok(()),
        _ => Err(codes::ERROR_NOT_ADMIN.to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------

fn fetcher() -> &'static PinnedFetcher {
    static F: OnceLock<PinnedFetcher> = OnceLock::new();
    F.get_or_init(|| PinnedFetcher::new(GITHUB_RELEASE_REDIRECT_HOSTS))
}

fn stage_dir() -> PathBuf {
    crate::providers::utils::get_data_root()
        .join("catalog")
        .join("staged")
}

fn today_local() -> NaiveDate {
    chrono::Local::now().date_naive()
}

fn source_host() -> String {
    reqwest::Url::parse(RELEASE_BASE_URL)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default()
}

fn preview_dto(staged: &Staged, outcome: &Outcome) -> CatalogPreviewDto {
    CatalogPreviewDto {
        version: version_u32(staged.manifest.catalog_version).unwrap_or(0),
        sha256: staged.sha256.clone(),
        changes: outcome.changes.iter().map(change_dto).collect(),
        skipped: outcome.skipped.iter().map(skip_dto).collect(),
        consent_effects: outcome.consent_effects.iter().map(effect_dto).collect(),
    }
}

// ---------------------------------------------------------------------------
// get_catalog_status
// ---------------------------------------------------------------------------

pub(crate) async fn status_impl(
    pool: &sqlx::SqlitePool,
    keys: &[&str],
) -> Result<CatalogStatusDto, String> {
    let mut conn = pool.acquire().await.map_err(|e| internal("pool", e))?;
    let state = store::get_state(&mut conn)
        .await
        .map_err(|e| internal("state", e))?;
    let latest = store::latest_history(&mut conn)
        .await
        .map_err(|e| internal("history", e))?;
    Ok(CatalogStatusDto {
        applied_version: (state.applied_version > 0)
            .then(|| version_u32(state.applied_version))
            .flatten(),
        applied_at: state.applied_at,
        declined_version: (state.declined_version > 0)
            .then(|| version_u32(state.declined_version))
            .flatten(),
        baseline_version: version_u32(baseline_version()).unwrap_or(0),
        can_roll_back: latest.is_some_and(|l| l.version == state.applied_version),
        trusted_key_configured: !keys.is_empty(),
        update_source_host: source_host(),
    })
}

#[tauri::command]
#[specta::specta]
pub async fn get_catalog_status(
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<CatalogStatusDto, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    session_user(&key_registry).await?;
    status_impl(&pool, TRUSTED_PUBLIC_KEYS).await
}

// ---------------------------------------------------------------------------
// check_catalog_update
// ---------------------------------------------------------------------------

pub(crate) async fn check_impl(
    pool: &sqlx::SqlitePool,
    fetcher: &PinnedFetcher,
    base_url: &str,
    keys: &[&str],
    stage_dir: &Path,
    today: NaiveDate,
) -> Result<CatalogCheckDto, String> {
    let (state, schema) = {
        let mut conn = pool.acquire().await.map_err(|e| internal("pool", e))?;
        let state = store::get_state(&mut conn)
            .await
            .map_err(|e| internal("state", e))?;
        let schema = store::shared_schema_version(&mut conn)
            .await
            .map_err(|e| internal("schema version", e))?;
        (state, schema)
    };
    let result = check_for_update(fetcher, base_url, keys, &state, schema, stage_dir, today)
        .await
        .map_err(|f| error_text(&f))?;
    match result {
        CheckResult::UpToDate => Ok(CatalogCheckDto {
            state: codes::STATE_UP_TO_DATE.to_owned(),
            preview: None,
        }),
        CheckResult::Available(staged) => {
            let outcome = flow::preview(pool, &staged.doc, today)
                .await
                .map_err(|f| error_text(&f))?;
            Ok(CatalogCheckDto {
                state: codes::STATE_UPDATE_AVAILABLE.to_owned(),
                preview: Some(preview_dto(&staged, &outcome)),
            })
        }
    }
}

/// USER-CLICKED. The only command that reaches the network.
#[tauri::command]
#[specta::specta]
pub async fn check_catalog_update(
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<CatalogCheckDto, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    session_user(&key_registry).await?;
    check_impl(
        &pool,
        fetcher(),
        RELEASE_BASE_URL,
        TRUSTED_PUBLIC_KEYS,
        &stage_dir(),
        today_local(),
    )
    .await
}

// ---------------------------------------------------------------------------
// apply_catalog_update
// ---------------------------------------------------------------------------

pub(crate) async fn apply_staged_impl(
    pool: &sqlx::SqlitePool,
    staged: &Staged,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> Result<CatalogApplyDto, String> {
    let outcome = flow::apply_staged(pool, staged, today, now)
        .await
        .map_err(|f| error_text(&f))?;
    Ok(CatalogApplyDto {
        version: version_u32(staged.manifest.catalog_version),
        changes: outcome.changes.iter().map(change_dto).collect(),
        consent_effects: outcome.consent_effects.iter().map(effect_dto).collect(),
    })
}

#[tauri::command]
#[specta::specta]
pub async fn apply_catalog_update(
    sha256: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<CatalogApplyDto, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let user_id = session_user(&key_registry).await?;
    require_admin(&pool, &user_id).await?;
    let dir = stage_dir();
    let staged = load_staged(TRUSTED_PUBLIC_KEYS, &dir, &sha256)
        .await
        .map_err(|f| error_text(&f))?;
    let dto = apply_staged_impl(&pool, &staged, today_local(), Utc::now()).await?;
    // The staged copy has served its purpose.
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(dto)
}

// ---------------------------------------------------------------------------
// rollback_catalog
// ---------------------------------------------------------------------------

pub(crate) async fn rollback_impl(
    pool: &sqlx::SqlitePool,
    now: DateTime<Utc>,
) -> Result<CatalogApplyDto, String> {
    let outcome = flow::rollback_latest(pool, now)
        .await
        .map_err(|f| error_text(&f))?;
    let mut conn = pool.acquire().await.map_err(|e| internal("pool", e))?;
    let state = store::get_state(&mut conn)
        .await
        .map_err(|e| internal("state", e))?;
    Ok(CatalogApplyDto {
        version: (state.applied_version > 0)
            .then(|| version_u32(state.applied_version))
            .flatten(),
        changes: outcome.changes.iter().map(change_dto).collect(),
        consent_effects: outcome.consent_effects.iter().map(effect_dto).collect(),
    })
}

#[tauri::command]
#[specta::specta]
pub async fn rollback_catalog(
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<CatalogApplyDto, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let user_id = session_user(&key_registry).await?;
    require_admin(&pool, &user_id).await?;
    rollback_impl(&pool, Utc::now()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    use crate::pinned_asset::PinnedFetcher;
    use crate::provider_catalog::format::{parse_catalog, Manifest, CATALOG_ASSET_NAME};
    use crate::providers::loopback_test_server::{Canned, LoopbackServer};

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 20).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 12, 0, 0).unwrap()
    }

    async fn with_pool<F, Fut>(f: F)
    where
        F: FnOnce(sqlx::SqlitePool) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved = std::env::var("QR_DATA_ROOT").ok();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", dir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("migrate");
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("pool");
        f(pool).await;
        match saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    async fn add_user(pool: &sqlx::SqlitePool, id: &str, role: &str) {
        sqlx::query(
            "INSERT INTO users (id, display_name, role, is_primary, auth_enabled, created_at) \
             VALUES (?, ?, ?, 0, 1, '2026-01-01T00:00:00Z')",
        )
        .bind(id)
        .bind(id)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
    }

    fn staged_for(doc_json: serde_json::Value) -> Staged {
        let text = doc_json.to_string();
        let doc = parse_catalog(text.as_bytes()).unwrap();
        Staged {
            manifest: Manifest {
                format: 1,
                catalog_version: doc.catalog_version,
                min_shared_schema: 1,
                asset_name: CATALOG_ASSET_NAME.to_owned(),
                size: text.len() as u64,
                sha256: "0".repeat(64),
            },
            doc,
            sha256: "0".repeat(64),
            body: text,
        }
    }

    /// A hosted provider and one model, found in the migrated seed data.
    async fn a_seeded_model(pool: &sqlx::SqlitePool) -> (String, String) {
        let row: (String, String) = sqlx::query_as(
            "SELECT m.provider_id, m.model_id FROM provider_models m
               JOIN providers p ON p.id = m.provider_id
              WHERE p.is_local = 0 AND p.api_dialect IS NOT NULL AND m.is_default = 1
              ORDER BY m.provider_id LIMIT 1",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        row
    }

    #[tokio::test]
    async fn only_an_admin_may_apply_or_roll_back() {
        with_pool(|pool| async move {
            add_user(&pool, "u-admin", "admin").await;
            add_user(&pool, "u-plain", "user").await;
            assert_eq!(require_admin(&pool, "u-admin").await, Ok(()));
            assert_eq!(
                require_admin(&pool, "u-plain").await,
                Err(codes::ERROR_NOT_ADMIN.to_owned())
            );
            assert_eq!(
                require_admin(&pool, "no-such-user").await,
                Err(codes::ERROR_NOT_ADMIN.to_owned())
            );
        })
        .await;
    }

    #[tokio::test]
    async fn status_reports_the_state_the_release_and_the_key() {
        with_pool(|pool| async move {
            let s = status_impl(&pool, &[]).await.unwrap();
            assert_eq!(s.applied_version, None);
            assert_eq!(s.declined_version, None);
            assert_eq!(s.baseline_version as u64, baseline_version());
            assert!(!s.can_roll_back);
            assert!(
                !s.trusted_key_configured,
                "no production key is compiled in"
            );
            assert_eq!(s.update_source_host, "github.com");
            assert!(
                status_impl(&pool, &["k"])
                    .await
                    .unwrap()
                    .trusted_key_configured
            );
        })
        .await;
    }

    #[tokio::test]
    async fn apply_then_roll_back_through_the_command_logic() {
        with_pool(|pool| async move {
            let (provider, model) = a_seeded_model(&pool).await;
            let staged = staged_for(json!({
                "format": 1, "catalog_version": 2026101501u64,
                "models": [{"provider_id": provider, "model_id": model,
                    "pricing": {"input_per_mtok": "0.11", "output_per_mtok": "0.22",
                                "as_of": "2026-10-15"}}]}));
            let applied = apply_staged_impl(&pool, &staged, today(), now())
                .await
                .unwrap();
            assert_eq!(applied.version, Some(2026101501));
            assert!(applied.changes.iter().any(|c| c.field == "pricing"));
            let s = status_impl(&pool, &[]).await.unwrap();
            assert_eq!(s.applied_version, Some(2026101501));
            assert!(s.can_roll_back);

            let back = rollback_impl(&pool, now()).await.unwrap();
            assert_eq!(back.version, None);
            assert!(back.changes.iter().any(|c| c.field == "restored"));
            let s = status_impl(&pool, &[]).await.unwrap();
            assert_eq!(s.applied_version, None);
            assert_eq!(s.declined_version, Some(2026101501));
            assert!(!s.can_roll_back);
            assert_eq!(
                rollback_impl(&pool, now()).await.unwrap_err(),
                codes::ERROR_NOTHING_TO_ROLL_BACK
            );
            // The rolled-back version is not newer than the floor any more.
            assert_eq!(
                apply_staged_impl(&pool, &staged, today(), now())
                    .await
                    .unwrap_err(),
                codes::ERROR_NOT_NEWER
            );
        })
        .await;
    }

    #[tokio::test]
    async fn the_check_command_logic_fails_closed_without_a_key_and_touches_nothing() {
        with_pool(|pool| async move {
            let server = LoopbackServer::start(vec![Canned::bytes(200, b"{}".to_vec())]);
            let dir = tempfile::tempdir().unwrap();
            let fetcher = PinnedFetcher::for_loopback_tests(&["127.0.0.1"]);
            let err = check_impl(&pool, &fetcher, &server.base(), &[], dir.path(), today())
                .await
                .unwrap_err();
            assert_eq!(err, codes::ERROR_NO_TRUSTED_KEY);
            assert_eq!(server.connections(), 0);
            // The compiled-in list is what the command passes, and it is empty.
            assert!(TRUSTED_PUBLIC_KEYS.is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn a_check_with_a_bad_signature_changes_nothing() {
        with_pool(|pool| async move {
            let server = LoopbackServer::start(vec![
                Canned::bytes(200, b"{\"format\":1}".to_vec()),
                Canned::bytes(200, b"not a signature".to_vec()),
            ]);
            let dir = tempfile::tempdir().unwrap();
            let fetcher = PinnedFetcher::for_loopback_tests(&["127.0.0.1"]);
            let err = check_impl(
                &pool,
                &fetcher,
                &server.base(),
                &["RWQ0000000000000000000000000000000000000000000000000"],
                dir.path(),
                today(),
            )
            .await
            .unwrap_err();
            assert_eq!(err, codes::ERROR_SIGNATURE_INVALID);
            // Only the manifest and its signature were requested, never the body.
            assert_eq!(server.connections(), 2);
            let s = status_impl(&pool, &[]).await.unwrap();
            assert_eq!(s.applied_version, None);
        })
        .await;
    }

    #[test]
    fn errors_are_codes_with_an_optional_detail() {
        assert_eq!(
            error_text(&Failure {
                code: codes::REJECT_BAD_PRICE,
                detail: "m:1".to_owned()
            }),
            "catalog.reject.bad_price:m:1"
        );
        assert_eq!(
            error_text(&Failure {
                code: codes::ERROR_NOT_ADMIN,
                detail: String::new()
            }),
            "catalog.error.not_admin"
        );
    }
}
