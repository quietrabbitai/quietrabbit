// src-tauri/src/conductor/provider_selection.rs
//
// Which hosted provider a step may be sent to, given the
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
/// API. Cloud Chat panes (split_screen_web/external_service)
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
///
/// The preferences live in the user's encrypted integration_keys.db
/// (items.id=661), so this needs the data key: an empty key, or a file that
/// cannot be opened, is `NotConfigured` too. That is the fail-closed result a
/// run that outlives its session gets once the key is gone. The catalog
/// (providers) is still read from shared.db through `pool`.
pub async fn resolve_external_provider(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
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

    // One open of the encrypted file for both lookups below.
    if key_hex.is_empty() {
        return ExternalProviderResolution::NotConfigured;
    }
    let Ok(mut conn) = crate::persistence::provider_spend_store::open(user_id, key_hex).await
    else {
        return ExternalProviderResolution::NotConfigured;
    };

    // Explicit preference among permitted providers (exactly one, as before).
    if let Ok(preferred) = preferred_providers(
        &mut conn,
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
    if let Ok(preferred_all) = preferred_providers(
        &mut conn,
        user_id,
        Some(persona_id),
        Some(focus_id),
        &all_ids,
    )
    .await
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

/// The provider an API Chat conversation is bound to (items.id=716). The
/// binding is the user's explicit choice for this conversation, so no
/// preference is consulted; the ceiling still decides whether the bound
/// provider may be used: `ExcludedByCeiling` when it does not permit it,
/// `NotConfigured` when the bound id is not an active hosted provider (removed,
/// retired or not a hosted API row). Never substitutes another provider.
pub async fn resolve_bound_provider(
    pool: &sqlx::SqlitePool,
    provider_id: &str,
    access: ExternalAccess,
) -> ExternalProviderResolution {
    let candidates = provider_store::list_providers_by_type(pool, HOSTED_PROVIDER_TYPE)
        .await
        .unwrap_or_default();
    let Some(bound) = candidates.into_iter().find(|p| p.id == provider_id) else {
        return ExternalProviderResolution::NotConfigured;
    };
    let name = bound.display_name.clone();
    if providers_permitted_by_ceiling(vec![bound], access).is_empty() {
        return ExternalProviderResolution::ExcludedByCeiling {
            provider_name: name,
        };
    }
    ExternalProviderResolution::Selected(provider_id.to_owned())
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
    const KEY_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";

    /// The user's encrypted integration_keys.db, where the preferences live
    /// (items.id=661).
    async fn open_keys() -> sqlx::SqliteConnection {
        crate::persistence::provider_spend_store::open(USER, KEY_HEX)
            .await
            .expect("open integration_keys.db")
    }

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
        (tempdir, pool)
    }

    async fn prefer(provider_id: &str) {
        upsert_preference(
            &mut open_keys().await,
            NewUserProviderPreference {
                user_id: USER,
                persona_id: None,
                focus_id: None,
                provider_id,
                login_available: true,
                user_preference: UserPreference::Preferred,
                local_model_version: None,
                subscription_status: None,
                plan_key: None,
                model_id: None,
            },
        )
        .await
        .expect("upsert_preference must succeed");
    }

    /// Two seeded hosted API providers as (id, display_name), chosen by
    /// field so no test names a provider (decisions.id=872).
    async fn hosted_pair(pool: &sqlx::SqlitePool) -> [(String, String); 2] {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, display_name FROM providers \
             WHERE provider_type = 'cloud_inference_api' AND activation_status = 'active' \
             ORDER BY id LIMIT 2",
        )
        .fetch_all(pool)
        .await
        .expect("hosted rows query");
        rows.try_into()
            .expect("at least two seeded hosted providers")
    }

    async fn resolve(
        pool: &sqlx::SqlitePool,
        access: ExternalAccess,
    ) -> ExternalProviderResolution {
        resolve_external_provider(pool, USER, KEY_HEX, PERSONA, FOCUS, access).await
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
    async fn anonymous_required_excludes_a_preferred_hosted_provider() {
        with_db(|pool| async move {
            for (id, name) in hosted_pair(&pool).await {
                sqlx::query("DELETE FROM user_provider_preference")
                    .execute(&mut open_keys().await)
                    .await
                    .expect("clear preferences");
                prefer(&id).await;
                assert_eq!(
                    resolve(&pool, ExternalAccess::AnonymousRequired).await,
                    ExternalProviderResolution::ExcludedByCeiling {
                        provider_name: name
                    },
                    "{id}"
                );
            }
        })
        .await;
    }

    #[tokio::test]
    async fn anonymous_preferred_allows_the_users_preferred_provider() {
        with_db(|pool| async move {
            let [(first, _), _] = hosted_pair(&pool).await;
            prefer(&first).await;
            assert_eq!(
                resolve(&pool, ExternalAccess::AnonymousPreferred).await,
                ExternalProviderResolution::Selected(first)
            );
        })
        .await;
    }

    #[tokio::test]
    async fn unrestricted_allows_the_users_preferred_provider() {
        with_db(|pool| async move {
            let [_, (second, _)] = hosted_pair(&pool).await;
            prefer(&second).await;
            assert_eq!(
                resolve(&pool, ExternalAccess::Unrestricted).await,
                ExternalProviderResolution::Selected(second)
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
            let [(first, _), (second, _)] = hosted_pair(&pool).await;
            prefer(&first).await;
            prefer(&second).await;
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
            let [(first, _), _] = hosted_pair(&pool).await;
            prefer(&first).await;
            // The executor never asks for LocalOnly steps; if asked anyway
            // the ceiling permits nothing.
            assert!(!matches!(
                resolve(&pool, ExternalAccess::LocalOnly).await,
                ExternalProviderResolution::Selected(_)
            ));
        })
        .await;
    }

    // -- items.id=711 (Chat-PM ruling on Q2 = A): a model selection stored in a
    // narrower scope must not change which provider is chosen, and what a
    // stale copy of the preference does is pinned here, not asserted in prose.

    async fn set_account_preference(provider_id: &str, user_preference: UserPreference) {
        upsert_preference(
            &mut open_keys().await,
            NewUserProviderPreference {
                user_id: USER,
                persona_id: None,
                focus_id: None,
                provider_id,
                login_available: true,
                user_preference,
                local_model_version: None,
                subscription_status: None,
                plan_key: None,
                model_id: None,
            },
        )
        .await
        .expect("account preference");
    }

    #[tokio::test]
    async fn a_model_selection_in_a_narrower_scope_does_not_change_the_chosen_provider() {
        use crate::persistence::user_provider_preference_store::{
            set_model_selection, SelectionScope,
        };
        with_db(|pool| async move {
            let [(first, _), (second, _)] = hosted_pair(&pool).await;
            prefer(&first).await;
            let scope = SelectionScope::Persona {
                persona_id: PERSONA.to_owned(),
            };

            // The chosen provider: the new row copies Preferred, so the choice
            // is unchanged.
            set_model_selection(
                &mut open_keys().await,
                USER,
                &scope,
                None,
                &first,
                Some("m-1"),
            )
            .await
            .unwrap();
            assert_eq!(
                resolve(&pool, ExternalAccess::Unrestricted).await,
                ExternalProviderResolution::Selected(first.clone())
            );

            // A provider with no row anywhere: the new row is `allowed`, never
            // Preferred, so storing a model for it cannot make it the choice.
            set_model_selection(
                &mut open_keys().await,
                USER,
                &scope,
                None,
                &second,
                Some("m-2"),
            )
            .await
            .unwrap();
            assert_eq!(
                resolve(&pool, ExternalAccess::Unrestricted).await,
                ExternalProviderResolution::Selected(first)
            );
        })
        .await;
    }

    /// The known limitation of keeping the selection on the preference row:
    /// the narrower row holds a COPY of the preference. When the account-level
    /// choice later moves to another provider, the copy still says Preferred,
    /// two providers resolve as Preferred at that Persona, and the resolver
    /// returns NotConfigured. It fails closed (the step asks), and other
    /// Personas follow the new choice.
    #[tokio::test]
    async fn a_stale_copy_makes_two_preferred_providers_resolve_to_not_configured() {
        use crate::persistence::user_provider_preference_store::{
            set_model_selection, SelectionScope,
        };
        with_db(|pool| async move {
            let [(first, _), (second, _)] = hosted_pair(&pool).await;
            prefer(&first).await;
            set_model_selection(
                &mut open_keys().await,
                USER,
                &SelectionScope::Persona {
                    persona_id: PERSONA.to_owned(),
                },
                None,
                &first,
                Some("m-1"),
            )
            .await
            .unwrap();

            // The user moves the account-level choice to the other provider.
            set_account_preference(&first, UserPreference::Allowed).await;
            set_account_preference(&second, UserPreference::Preferred).await;

            // At the Persona that holds the stale copy both are Preferred.
            let preferred =
                crate::persistence::user_provider_preference_store::preferred_providers(
                    &mut open_keys().await,
                    USER,
                    Some(PERSONA),
                    Some(FOCUS),
                    &[first.clone(), second.clone()],
                )
                .await
                .unwrap();
            assert_eq!(preferred, vec![first.clone(), second.clone()]);
            assert_eq!(
                resolve(&pool, ExternalAccess::Unrestricted).await,
                ExternalProviderResolution::NotConfigured,
                "two Preferred providers fail closed and never pick one"
            );

            // Every other Persona follows the new account-level choice.
            assert_eq!(
                resolve_external_provider(
                    &pool,
                    USER,
                    KEY_HEX,
                    "p2",
                    FOCUS,
                    ExternalAccess::Unrestricted
                )
                .await,
                ExternalProviderResolution::Selected(second)
            );
        })
        .await;
    }

    // -- items.id=716: a bound conversation resolves its own provider --------

    #[tokio::test]
    async fn a_bound_provider_is_selected_without_any_preference() {
        with_db(|pool| async move {
            let [(first, _), _] = hosted_pair(&pool).await;
            // No preference row exists at all.
            assert_eq!(
                resolve_bound_provider(&pool, &first, ExternalAccess::Unrestricted).await,
                ExternalProviderResolution::Selected(first.clone())
            );
            // A preference for ANOTHER provider changes nothing.
            let [_, (second, _)] = hosted_pair(&pool).await;
            prefer(&second).await;
            assert_eq!(
                resolve_bound_provider(&pool, &first, ExternalAccess::Unrestricted).await,
                ExternalProviderResolution::Selected(first)
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_bound_provider_still_obeys_the_ceiling() {
        with_db(|pool| async move {
            for (id, name) in hosted_pair(&pool).await {
                assert_eq!(
                    resolve_bound_provider(&pool, &id, ExternalAccess::AnonymousRequired).await,
                    ExternalProviderResolution::ExcludedByCeiling {
                        provider_name: name.clone()
                    },
                    "{id}"
                );
                assert!(
                    !matches!(
                        resolve_bound_provider(&pool, &id, ExternalAccess::LocalOnly).await,
                        ExternalProviderResolution::Selected(_)
                    ),
                    "{id}: a local-only ceiling permits no provider"
                );
            }
        })
        .await;
    }

    #[tokio::test]
    async fn an_unknown_or_non_hosted_bound_provider_is_not_configured() {
        with_db(|pool| async move {
            assert_eq!(
                resolve_bound_provider(&pool, "no-such-provider", ExternalAccess::Unrestricted)
                    .await,
                ExternalProviderResolution::NotConfigured
            );
            let local: Option<String> = sqlx::query_scalar(
                "SELECT id FROM providers WHERE provider_type <> 'cloud_inference_api' LIMIT 1",
            )
            .fetch_optional(&pool)
            .await
            .expect("query");
            if let Some(local) = local {
                assert_eq!(
                    resolve_bound_provider(&pool, &local, ExternalAccess::Unrestricted).await,
                    ExternalProviderResolution::NotConfigured,
                    "a non-hosted row is never a bound API provider"
                );
            }
        })
        .await;
    }
}
