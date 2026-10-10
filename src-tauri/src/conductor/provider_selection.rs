// src-tauri/src/conductor/provider_selection.rs
//
// Which hosted provider (Groq, Mistral, ...) a step may be sent to, given the
// step's ExternalAccess ceiling and the user's provider preference
// (items.id=692, decisions.id=870; spec PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md
// Parts 6b/6f).
//
// Two questions are answered here, in this order:
//   1. Which cloud_inference_api providers does the ceiling permit?
//      (focus_provider_criteria_store::providers_permitted_by_ceiling --
//      hard filter at anonymous_required, anonymous-first ranking at
//      anonymous_preferred.)
//   2. Which permitted provider has the user explicitly preferred?
//      (user_provider_preference_store, Focus -> Persona -> account.)
//
// An explicit user preference always wins among permitted providers; the
// ceiling's ranking never overrides it. There is still no prescribed default:
// no preference means NotConfigured, never an automatic pick, so ranking is
// currently an ordering of the permitted list only.
//
// ExcludedByCeiling is the one case that needs a distinct answer: the user
// HAS chosen a hosted provider, but this Focus's ceiling forbids it. That is
// a setting to change, not a missing setup step, and callers (executor's
// Step 4.5, FailureHandler's escalation offers) treat it differently from
// NotConfigured.

use crate::conductor::tokens::ExternalAccess;
use crate::persistence::focus_provider_criteria_store::providers_permitted_by_ceiling;
use crate::persistence::provider_store;
use crate::persistence::user_provider_preference_store::preferred_providers;

/// provider_type of the hosted-inference providers QR itself can call over an
/// API (Groq, Mistral). Cloud Chat panes (split_screen_web/external_service)
/// are never candidates here -- those are user-driven and not gated by the
/// ceiling (decisions.id=680).
const HOSTED_PROVIDER_TYPE: &str = "cloud_inference_api";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalProviderResolution {
    /// A permitted provider the user explicitly preferred.
    Selected(String),
    /// No usable preference: unset, ambiguous (several Preferred), or the
    /// lookup failed. Surfaces as the "no provider set up" failure.
    NotConfigured,
    /// The user's one preferred hosted provider exists, but the ceiling does
    /// not permit it. `provider_name` is its display name.
    ExcludedByCeiling { provider_name: String },
}

/// Resolve the hosted provider for a step whose effective ceiling is
/// `access`. Callers only ask for steps with `access != LocalOnly`; a
/// LocalOnly ceiling permits nothing, so it can never return `Selected`.
/// Database failures collapse to `NotConfigured`, matching the previous
/// inline behavior in lifecycle.rs (never guess a provider).
pub async fn resolve_external_provider(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    access: ExternalAccess,
) -> ExternalProviderResolution {
    let candidates = provider_store::list_providers_by_type(pool, HOSTED_PROVIDER_TYPE)
        .await
        .unwrap_or_default();

    let eligible = providers_permitted_by_ceiling(candidates.clone(), access);
    let eligible_ids: Vec<String> = eligible.iter().map(|p| p.id.clone()).collect();
    let all_ids: Vec<String> = candidates.iter().map(|p| p.id.clone()).collect();

    // Explicit preference among permitted providers (exactly one, as before).
    if let Ok(preferred) = preferred_providers(
        pool,
        user_id,
        Some(persona_id),
        Some(focus_id),
        &eligible_ids,
    )
    .await
    {
        if let [only] = preferred.as_slice() {
            return ExternalProviderResolution::Selected(only.clone());
        }
        if preferred.len() > 1 {
            return ExternalProviderResolution::NotConfigured;
        }
    } else {
        return ExternalProviderResolution::NotConfigured;
    }

    // Nothing permitted is preferred. Is the user's single preferred provider
    // one the ceiling excluded?
    if let Ok(preferred_all) =
        preferred_providers(pool, user_id, Some(persona_id), Some(focus_id), &all_ids).await
    {
        if let [only] = preferred_all.as_slice() {
            if let Some(p) = candidates.iter().find(|p| &p.id == only) {
                return ExternalProviderResolution::ExcludedByCeiling {
                    provider_name: p.display_name.clone(),
                };
            }
        }
    }
    ExternalProviderResolution::NotConfigured
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::user_provider_preference_store::{
        upsert_preference, NewUserProviderPreference, UserPreference,
    };

    const USER: &str = "u1";
    const PERSONA: &str = "p1";
    const FOCUS: &str = "f1";

    async fn setup_real_db() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("migrate_shared_db must succeed");
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool must connect");
        // user_provider_preference.user_id is a foreign key.
        sqlx::query(
            "INSERT INTO users (id, display_name, role, is_primary, auth_enabled, created_at) \
             VALUES ('u1', 'Test User', 'user', 1, 1, '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .expect("seed user");
        (tempdir, pool)
    }

    async fn prefer(pool: &sqlx::SqlitePool, provider_id: &str) {
        upsert_preference(
            pool,
            NewUserProviderPreference {
                user_id: USER,
                persona_id: None,
                focus_id: None,
                provider_id,
                login_available: true,
                user_preference: UserPreference::Preferred,
                local_model_version: None,
                subscription_status: None,
            },
        )
        .await
        .expect("upsert_preference must succeed");
    }

    async fn resolve(
        pool: &sqlx::SqlitePool,
        access: ExternalAccess,
    ) -> ExternalProviderResolution {
        resolve_external_provider(pool, USER, PERSONA, FOCUS, access).await
    }

    /// Runs `body` against a fresh real shared.db, restoring QR_DATA_ROOT.
    async fn with_db<F, Fut>(body: F)
    where
        F: FnOnce(sqlx::SqlitePool) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;
        body(pool).await;
        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn anonymous_required_excludes_a_preferred_groq() {
        with_db(|pool| async move {
            prefer(&pool, "groq").await;
            assert_eq!(
                resolve(&pool, ExternalAccess::AnonymousRequired).await,
                ExternalProviderResolution::ExcludedByCeiling {
                    provider_name: "Groq".to_owned()
                }
            );
        })
        .await;
    }

    #[tokio::test]
    async fn anonymous_required_excludes_a_preferred_mistral() {
        with_db(|pool| async move {
            prefer(&pool, "mistral").await;
            assert_eq!(
                resolve(&pool, ExternalAccess::AnonymousRequired).await,
                ExternalProviderResolution::ExcludedByCeiling {
                    provider_name: "Mistral".to_owned()
                }
            );
        })
        .await;
    }

    #[tokio::test]
    async fn anonymous_preferred_allows_the_users_preferred_provider() {
        with_db(|pool| async move {
            prefer(&pool, "groq").await;
            assert_eq!(
                resolve(&pool, ExternalAccess::AnonymousPreferred).await,
                ExternalProviderResolution::Selected("groq".to_owned())
            );
        })
        .await;
    }

    #[tokio::test]
    async fn unrestricted_allows_the_users_preferred_provider() {
        with_db(|pool| async move {
            prefer(&pool, "mistral").await;
            assert_eq!(
                resolve(&pool, ExternalAccess::Unrestricted).await,
                ExternalProviderResolution::Selected("mistral".to_owned())
            );
        })
        .await;
    }

    #[tokio::test]
    async fn no_preference_is_not_configured_at_every_external_ceiling() {
        with_db(|pool| async move {
            for access in [
                ExternalAccess::AnonymousRequired,
                ExternalAccess::AnonymousPreferred,
                ExternalAccess::Unrestricted,
            ] {
                assert_eq!(
                    resolve(&pool, access).await,
                    ExternalProviderResolution::NotConfigured,
                    "{access:?}: an unset preference is never auto-picked"
                );
            }
        })
        .await;
    }

    #[tokio::test]
    async fn two_preferred_providers_stay_ambiguous() {
        with_db(|pool| async move {
            prefer(&pool, "groq").await;
            prefer(&pool, "mistral").await;
            assert_eq!(
                resolve(&pool, ExternalAccess::Unrestricted).await,
                ExternalProviderResolution::NotConfigured
            );
        })
        .await;
    }

    #[tokio::test]
    async fn local_only_never_selects_a_provider() {
        with_db(|pool| async move {
            prefer(&pool, "groq").await;
            // The executor never asks for LocalOnly steps; if asked anyway
            // the ceiling permits nothing.
            assert!(!matches!(
                resolve(&pool, ExternalAccess::LocalOnly).await,
                ExternalProviderResolution::Selected(_)
            ));
        })
        .await;
    }
}
