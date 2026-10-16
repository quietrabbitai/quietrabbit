// src-tauri/src/conductor/model_selection.rs
//
// Which model a hosted step is sent to (items.id=711; decisions.id=875
// point 5; spec Part 8f). The stored selection (Focus, then Persona, then
// account) wins over the provider's default model.
//
// WHAT A SELECTION MAY POINT AT
//   - a curated `provider_models` row (global, reviewed); or
//   - a model from the user's own refreshed live list (`provider_live_models`
//     in the encrypted per-user store): "available, not reviewed". It has no
//     price (the spend gate stops until the user sets an override), its
//     privacy is unverified, and it needs an output token limit the user
//     chose, because the client sends nothing without one.
//
// NO SILENT SUBSTITUTION. A selection that points at a model that is retired,
// or is in neither place, is an error code, never a quiet fall back to the
// default: that would change the cost and the privacy of a run the user set
// up. A model that is only deprecated, or missing from the latest list, still
// runs and is reported as a notice.
//
// An uncurated model carries no options of its own except the output limit.
// The limit KEY NAME (`max_tokens` or `max_completion_tokens`) is taken from
// the provider's default curated row, since the provider accepts that name;
// no other option of that row is inherited. Provider-wide required options
// (a data-collection restriction, say) are merged by the request builder for
// every model, curated or not (providers.required_request_options).

use serde_json::json;

use crate::persistence::provider_models_store::{get_live_model_conn, LiveModelRow};
use crate::persistence::provider_spend_store as spend_store;
use crate::persistence::provider_store::{self, ModelStatus, ProviderModel};
use crate::persistence::user_provider_preference_store::{
    resolve_model_selection, SelectionSource,
};
use crate::spend::codes::Code;

/// Context window recorded for a live model whose list did not report one.
/// The pre-call context check then never trips and the provider enforces its
/// own limit. The real value is unknown, not large.
pub const CONTEXT_UNKNOWN: u32 = u32::MAX;

/// Where the model came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelOrigin {
    /// The provider's default curated model (no selection stored).
    Default,
    /// A curated model the user selected.
    Curated,
    /// A model from the user's live list that QR has not reviewed.
    Uncurated,
}

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: ProviderModel,
    pub origin: ModelOrigin,
    /// The level the stored selection came from; `None` for the default.
    pub source: Option<SelectionSource>,
    /// The latest provider list no longer includes this model.
    pub may_be_retired: bool,
    pub deprecated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    /// Nothing selected and the provider has no default model.
    NoModel,
    Retired {
        model_id: String,
    },
    Unavailable {
        model_id: String,
    },
    OutputLimitRequired {
        model_id: String,
    },
    /// The shared catalog or the per-user store could not be read.
    Store,
}

impl ModelError {
    /// The code and its parameters, for the failure metadata.
    pub fn block(&self) -> (Code, serde_json::Value) {
        match self {
            ModelError::NoModel => (Code::BlockModelUnavailable, json!({"model_id": null})),
            ModelError::Retired { model_id } => {
                (Code::BlockModelRetired, json!({"model_id": model_id}))
            }
            ModelError::Unavailable { model_id } => {
                (Code::BlockModelUnavailable, json!({"model_id": model_id}))
            }
            ModelError::OutputLimitRequired { model_id } => (
                Code::BlockModelOutputLimitRequired,
                json!({"model_id": model_id}),
            ),
            ModelError::Store => (Code::BlockLedgerUnavailable, json!({})),
        }
    }
}

/// The request-options key a provider accepts for the output limit, read from
/// its default curated row. `max_tokens` when the row names neither.
pub fn output_limit_key(default_model: Option<&ProviderModel>) -> &'static str {
    let has = |k: &str| {
        default_model
            .and_then(|m| m.request_options.as_ref())
            .and_then(|o| o.get(k))
            .is_some_and(|v| !v.is_null())
    };
    if has("max_completion_tokens") {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }
}

/// A `ProviderModel` for a model from the live list. Priced nothing, claims
/// nothing: multipliers neutral, no recommendation, no privacy override.
pub fn synthesize_live_model(
    default_model: Option<&ProviderModel>,
    live: &LiveModelRow,
) -> ProviderModel {
    let request_options = live
        .user_output_limit
        .map(|n| json!({ output_limit_key(default_model): n }));
    ProviderModel {
        id: format!("{}:{}", live.provider_id, live.model_id),
        provider_id: live.provider_id.clone(),
        model_id: live.model_id.clone(),
        context_window_tokens: live.context_tokens.unwrap_or(CONTEXT_UNKNOWN),
        is_default: false,
        created_at: live.first_seen_at.clone(),
        pricing: None,
        request_options,
        token_factor: 1.0,
        output_overhead: 1.0,
        status: ModelStatus::Active,
        retires_at: live.retires_at.clone(),
        requires_paid: false,
        qr_recommended: false,
        qr_recommendation_rank: None,
        retains_data_override: None,
        trains_on_data_override: None,
        retention_days_override: None,
        context_window_source: live.context_tokens.map(|_| "provider_api".to_owned()),
        context_window_as_of: live.context_as_of.clone(),
    }
}

/// Resolve the model for one hosted step.
pub async fn resolve_hosted_model(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: Option<&str>,
    focus_id: Option<&str>,
    provider_id: &str,
) -> Result<ResolvedModel, ModelError> {
    let selection = resolve_model_selection(pool, user_id, persona_id, focus_id, provider_id)
        .await
        .map_err(|e| {
            log::warn!("model selection lookup failed: {e}");
            ModelError::Store
        })?;

    let default_model = provider_store::get_default_model(pool, provider_id)
        .await
        .map_err(|e| {
            log::warn!("default model lookup failed: {e}");
            ModelError::Store
        })?;

    let Some(selection) = selection else {
        let model = default_model.ok_or(ModelError::NoModel)?;
        return curated(model, ModelOrigin::Default, None);
    };

    resolve_model_id(
        pool,
        user_id,
        key_hex,
        provider_id,
        default_model.as_ref(),
        &selection.model_id,
        Some(selection.source),
    )
    .await
}

/// The model a conversation is bound to (items.id=716). An API Chat binds a
/// provider and model at creation, so there is no preference to look up and no
/// default to fall back to: the model is resolved by id through exactly the
/// rules a stored selection follows (curated row, else the user's own live
/// list with a user-chosen output limit). A retired or missing model is an
/// error code, never a quiet switch to the provider's default, which would
/// change the cost and the privacy of a conversation the user set up.
pub async fn resolve_bound_model(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    provider_id: &str,
    model_id: &str,
) -> Result<ResolvedModel, ModelError> {
    let default_model = provider_store::get_default_model(pool, provider_id)
        .await
        .map_err(|e| {
            log::warn!("default model lookup failed: {e}");
            ModelError::Store
        })?;
    resolve_model_id(
        pool,
        user_id,
        key_hex,
        provider_id,
        default_model.as_ref(),
        model_id,
        None,
    )
    .await
}

/// The shared tail of `resolve_hosted_model` and `resolve_bound_model`: a model
/// id, wherever it came from, to a runnable row.
async fn resolve_model_id(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    provider_id: &str,
    default_model: Option<&ProviderModel>,
    model_id: &str,
    source: Option<SelectionSource>,
) -> Result<ResolvedModel, ModelError> {
    let curated_row =
        provider_store::get_model_by_provider_and_model_id(pool, provider_id, model_id)
            .await
            .map_err(|e| {
                log::warn!("selected model lookup failed: {e}");
                ModelError::Store
            })?;
    if let Some(model) = curated_row {
        return curated(model, ModelOrigin::Curated, source);
    }

    // Not a curated row: it must be in the user's own live list.
    if key_hex.is_empty() {
        return Err(ModelError::Unavailable {
            model_id: model_id.to_owned(),
        });
    }
    let mut conn = spend_store::open(user_id, key_hex).await.map_err(|e| {
        log::warn!("model selection: could not open the per-user store: {e}");
        ModelError::Store
    })?;
    let live = get_live_model_conn(&mut conn, provider_id, model_id)
        .await
        .map_err(|e| {
            log::warn!("live model lookup failed: {e}");
            ModelError::Store
        })?
        .ok_or_else(|| ModelError::Unavailable {
            model_id: model_id.to_owned(),
        })?;
    if live.user_output_limit.is_none() {
        return Err(ModelError::OutputLimitRequired {
            model_id: model_id.to_owned(),
        });
    }
    Ok(ResolvedModel {
        model: synthesize_live_model(default_model, &live),
        origin: ModelOrigin::Uncurated,
        source,
        may_be_retired: !live.present,
        deprecated: false,
    })
}

fn curated(
    model: ProviderModel,
    origin: ModelOrigin,
    source: Option<SelectionSource>,
) -> Result<ResolvedModel, ModelError> {
    match model.status {
        ModelStatus::Retired => Err(ModelError::Retired {
            model_id: model.model_id,
        }),
        status => Ok(ResolvedModel {
            deprecated: status == ModelStatus::Deprecated,
            model,
            origin,
            source,
            may_be_retired: false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::provider_models_store::{
        record_refresh_conn, set_user_output_limit_conn,
    };
    use crate::persistence::user_provider_preference_store::{set_model_selection, SelectionScope};
    use crate::providers::hosted_client::LiveModel;

    const USER: &str = "sel-user";
    const KEY: &str = "0404040404040404040404040404040404040404040404040404040404040404";

    struct Rig {
        pool: sqlx::SqlitePool,
        provider: String,
        default_model: String,
    }

    /// A real migrated shared.db with the seeded rows. The provider is picked
    /// by field (a hosted provider with a default model), so no test names one
    /// (decisions.id=872).
    async fn with_rig<F, Fut>(f: F)
    where
        F: FnOnce(Rig) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved = std::env::var("QR_DATA_ROOT").ok();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", dir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("migrate shared.db");
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("pool");
        sqlx::query(
            "INSERT INTO users (id, display_name, role, is_primary, auth_enabled, created_at) \
             VALUES (?, 'Fixture', 'user', 1, 1, '2026-01-01T00:00:00Z')",
        )
        .bind(USER)
        .execute(&pool)
        .await
        .expect("user");
        for persona in ["p1", "p2"] {
            sqlx::query(
                "INSERT INTO personas (id, display_name, persona_type, created_at) \
                 VALUES (?, 'Fixture', 'personal', '2026-01-01T00:00:00Z')",
            )
            .bind(persona)
            .execute(&pool)
            .await
            .expect("persona");
        }
        let (provider, default_model): (String, String) = sqlx::query_as(
            "SELECT m.provider_id, m.model_id FROM provider_models m \
             JOIN providers p ON p.id = m.provider_id \
             WHERE m.is_default = 1 AND p.api_dialect IS NOT NULL ORDER BY m.provider_id LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("a seeded hosted provider with a default model");
        f(Rig {
            pool,
            provider,
            default_model,
        })
        .await;
        match saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    impl Rig {
        async fn add_curated(&self, model_id: &str, status: &str) {
            sqlx::query(
                "INSERT INTO provider_models \
                    (id, provider_id, model_id, context_window_tokens, is_default, created_at, status) \
                 VALUES (?, ?, ?, 4096, 0, '2026-01-01T00:00:00Z', ?)",
            )
            .bind(format!("{}:{model_id}", self.provider))
            .bind(&self.provider)
            .bind(model_id)
            .bind(status)
            .execute(&self.pool)
            .await
            .expect("curated fixture");
        }

        async fn select(&self, scope: SelectionScope, model: Option<&str>) {
            set_model_selection(&self.pool, USER, &scope, None, &self.provider, model)
                .await
                .expect("set selection");
        }

        async fn resolve(
            &self,
            persona: Option<&str>,
            focus: Option<&str>,
        ) -> Result<ResolvedModel, ModelError> {
            resolve_hosted_model(&self.pool, USER, KEY, persona, focus, &self.provider).await
        }

        async fn live(&self, ids: &[(&str, Option<u32>)], present_only: bool) {
            let mut conn = spend_store::open(USER, KEY).await.unwrap();
            let models: Vec<LiveModel> = ids
                .iter()
                .map(|(id, ctx)| LiveModel {
                    id: (*id).to_owned(),
                    context_tokens: *ctx,
                    retires_at: None,
                })
                .collect();
            record_refresh_conn(&mut conn, &self.provider, &models, chrono::Utc::now())
                .await
                .unwrap();
            if !present_only {
                record_refresh_conn(&mut conn, &self.provider, &[], chrono::Utc::now())
                    .await
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn no_selection_uses_the_provider_default() {
        with_rig(|rig| async move {
            let r = rig.resolve(Some("p1"), Some("f1")).await.unwrap();
            assert_eq!(r.origin, ModelOrigin::Default);
            assert_eq!(r.model.model_id, rig.default_model);
            assert!(r.source.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn focus_beats_persona_beats_account_and_a_null_falls_through() {
        with_rig(|rig| async move {
            for m in ["m-account", "m-persona", "m-focus"] {
                rig.add_curated(m, "active").await;
            }
            rig.select(SelectionScope::Account, Some("m-account")).await;
            assert_eq!(
                rig.resolve(Some("p1"), Some("f1"))
                    .await
                    .unwrap()
                    .model
                    .model_id,
                "m-account"
            );
            rig.select(
                SelectionScope::Persona {
                    persona_id: "p1".into(),
                },
                Some("m-persona"),
            )
            .await;
            rig.select(
                SelectionScope::Focus {
                    focus_id: "f1".into(),
                },
                Some("m-focus"),
            )
            .await;
            let r = rig.resolve(Some("p1"), Some("f1")).await.unwrap();
            assert_eq!(
                (r.model.model_id.as_str(), r.origin),
                ("m-focus", ModelOrigin::Curated)
            );
            assert_eq!(r.source, Some(SelectionSource::Focus));
            // Another Focus of the same Persona gets the Persona's choice,
            // another Persona gets the account's.
            assert_eq!(
                rig.resolve(Some("p1"), Some("f2"))
                    .await
                    .unwrap()
                    .model
                    .model_id,
                "m-persona"
            );
            assert_eq!(
                rig.resolve(Some("p2"), Some("f3"))
                    .await
                    .unwrap()
                    .model
                    .model_id,
                "m-account"
            );
            // Clearing the Focus level falls through to the Persona's, per
            // column, even though the Focus row still exists.
            rig.select(
                SelectionScope::Focus {
                    focus_id: "f1".into(),
                },
                None,
            )
            .await;
            assert_eq!(
                rig.resolve(Some("p1"), Some("f1"))
                    .await
                    .unwrap()
                    .model
                    .model_id,
                "m-persona"
            );
            // Clearing everything returns to the default.
            rig.select(
                SelectionScope::Persona {
                    persona_id: "p1".into(),
                },
                None,
            )
            .await;
            rig.select(SelectionScope::Account, None).await;
            assert_eq!(
                rig.resolve(Some("p1"), Some("f1")).await.unwrap().origin,
                ModelOrigin::Default
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_retired_selection_stops_and_a_deprecated_one_runs() {
        with_rig(|rig| async move {
            rig.add_curated("m-retired", "retired").await;
            rig.add_curated("m-deprecated", "deprecated").await;
            rig.select(SelectionScope::Account, Some("m-retired")).await;
            assert_eq!(
                rig.resolve(None, None).await.unwrap_err(),
                ModelError::Retired {
                    model_id: "m-retired".into()
                },
                "never a quiet fall back to the default"
            );
            rig.select(SelectionScope::Account, Some("m-deprecated"))
                .await;
            let r = rig.resolve(None, None).await.unwrap();
            assert!(r.deprecated);
            assert_eq!(r.model.model_id, "m-deprecated");
        })
        .await;
    }

    #[tokio::test]
    async fn a_selection_that_exists_nowhere_stops_and_does_not_use_the_default() {
        with_rig(|rig| async move {
            rig.select(SelectionScope::Account, Some("ghost")).await;
            assert_eq!(
                rig.resolve(None, None).await.unwrap_err(),
                ModelError::Unavailable {
                    model_id: "ghost".into()
                }
            );
            // With no unlocked session there is no per-user store to look in.
            let no_key = resolve_hosted_model(&rig.pool, USER, "", None, None, &rig.provider).await;
            assert_eq!(
                no_key.unwrap_err(),
                ModelError::Unavailable {
                    model_id: "ghost".into()
                }
            );
        })
        .await;
    }

    #[tokio::test]
    async fn an_uncurated_model_needs_a_user_chosen_output_limit() {
        with_rig(|rig| async move {
            rig.live(&[("live-a", Some(32_000)), ("live-b", None)], true)
                .await;
            rig.select(SelectionScope::Account, Some("live-a")).await;
            assert_eq!(
                rig.resolve(None, None).await.unwrap_err(),
                ModelError::OutputLimitRequired {
                    model_id: "live-a".into()
                }
            );

            let mut conn = spend_store::open(USER, KEY).await.unwrap();
            assert!(
                set_user_output_limit_conn(&mut conn, &rig.provider, "live-a", Some(777))
                    .await
                    .unwrap()
            );
            let r = rig.resolve(None, None).await.unwrap();
            assert_eq!(r.origin, ModelOrigin::Uncurated);
            assert_eq!(r.model.context_window_tokens, 32_000);
            assert_eq!(
                r.model.context_window_source.as_deref(),
                Some("provider_api")
            );
            assert!(
                r.model.pricing.is_none(),
                "an uncurated model carries no price"
            );
            assert_eq!((r.model.token_factor, r.model.output_overhead), (1.0, 1.0));
            assert!(!r.may_be_retired);
            // Only the limit, under the key name the provider's default row uses.
            let opts = r
                .model
                .request_options
                .as_ref()
                .unwrap()
                .as_object()
                .unwrap();
            assert_eq!(opts.len(), 1);
            let default = provider_store::get_default_model(&rig.pool, &rig.provider)
                .await
                .unwrap();
            assert_eq!(opts[output_limit_key(default.as_ref())], 777);

            // No reported context: no pre-call context check, the provider enforces.
            rig.select(SelectionScope::Account, Some("live-b")).await;
            set_user_output_limit_conn(&mut conn, &rig.provider, "live-b", Some(100))
                .await
                .unwrap();
            let r = rig.resolve(None, None).await.unwrap();
            assert_eq!(r.model.context_window_tokens, CONTEXT_UNKNOWN);
            assert!(r.model.context_window_source.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn a_live_model_missing_from_the_latest_list_still_runs_with_a_notice() {
        with_rig(|rig| async move {
            rig.live(&[("live-a", Some(8000))], false).await;
            let mut conn = spend_store::open(USER, KEY).await.unwrap();
            set_user_output_limit_conn(&mut conn, &rig.provider, "live-a", Some(50))
                .await
                .unwrap();
            rig.select(SelectionScope::Account, Some("live-a")).await;
            let r = rig.resolve(None, None).await.unwrap();
            assert!(r.may_be_retired);
            assert_eq!(r.model.model_id, "live-a");
        })
        .await;
    }

    // -- items.id=716: a bound conversation names its model; nothing falls back

    impl Rig {
        async fn bound(&self, model_id: &str) -> Result<ResolvedModel, ModelError> {
            resolve_bound_model(&self.pool, USER, KEY, &self.provider, model_id).await
        }
    }

    #[tokio::test]
    async fn a_bound_model_resolves_by_id_ignoring_any_stored_selection() {
        with_rig(|rig| async move {
            rig.add_curated("m-bound", "active").await;
            rig.add_curated("m-selected", "active").await;
            rig.select(SelectionScope::Account, Some("m-selected"))
                .await;

            let r = rig.bound("m-bound").await.unwrap();
            assert_eq!(r.model.model_id, "m-bound");
            assert_eq!(r.origin, ModelOrigin::Curated);
            assert!(r.source.is_none(), "no selection level produced it");

            // The default model is also reachable by its own id.
            let d = rig.bound(&rig.default_model).await.unwrap();
            assert_eq!(d.model.model_id, rig.default_model);
        })
        .await;
    }

    #[tokio::test]
    async fn a_retired_or_unknown_bound_model_stops_and_never_uses_the_default() {
        with_rig(|rig| async move {
            rig.add_curated("m-retired", "retired").await;
            assert_eq!(
                rig.bound("m-retired").await.unwrap_err(),
                ModelError::Retired {
                    model_id: "m-retired".into()
                }
            );
            assert_eq!(
                rig.bound("ghost").await.unwrap_err(),
                ModelError::Unavailable {
                    model_id: "ghost".into()
                }
            );
            // No unlocked session: nothing to look in beyond the catalog.
            let no_key = resolve_bound_model(&rig.pool, USER, "", &rig.provider, "ghost").await;
            assert_eq!(
                no_key.unwrap_err(),
                ModelError::Unavailable {
                    model_id: "ghost".into()
                }
            );
            // A deprecated one still runs, with the flag.
            rig.add_curated("m-deprecated", "deprecated").await;
            assert!(rig.bound("m-deprecated").await.unwrap().deprecated);
        })
        .await;
    }

    #[tokio::test]
    async fn a_bound_uncurated_model_needs_the_users_output_limit() {
        with_rig(|rig| async move {
            rig.live(&[("live-a", Some(32_000))], true).await;
            assert_eq!(
                rig.bound("live-a").await.unwrap_err(),
                ModelError::OutputLimitRequired {
                    model_id: "live-a".into()
                }
            );
            let mut conn = spend_store::open(USER, KEY).await.unwrap();
            set_user_output_limit_conn(&mut conn, &rig.provider, "live-a", Some(300))
                .await
                .unwrap();
            let r = rig.bound("live-a").await.unwrap();
            assert_eq!(r.origin, ModelOrigin::Uncurated);
            assert!(r.model.pricing.is_none());
        })
        .await;
    }

    #[test]
    fn the_limit_key_follows_the_provider_default_row() {
        let mut m = synthesize_live_model(
            None,
            &LiveModelRow {
                provider_id: "p".into(),
                model_id: "m".into(),
                context_tokens: None,
                context_as_of: None,
                retires_at: None,
                first_seen_at: "t".into(),
                last_seen_at: "t".into(),
                present: true,
                user_output_limit: Some(9),
            },
        );
        assert_eq!(m.request_options.as_ref().unwrap()["max_tokens"], 9);
        m.request_options = Some(json!({"max_completion_tokens": 5, "reasoning_effort": "low"}));
        assert_eq!(output_limit_key(Some(&m)), "max_completion_tokens");
        m.request_options = Some(json!({"max_tokens": 5}));
        assert_eq!(output_limit_key(Some(&m)), "max_tokens");
        assert_eq!(output_limit_key(None), "max_tokens");
    }

    #[test]
    fn every_error_maps_to_a_registered_code() {
        let cases = [
            (ModelError::NoModel, Code::BlockModelUnavailable),
            (
                ModelError::Retired {
                    model_id: "m".into(),
                },
                Code::BlockModelRetired,
            ),
            (
                ModelError::Unavailable {
                    model_id: "m".into(),
                },
                Code::BlockModelUnavailable,
            ),
            (
                ModelError::OutputLimitRequired {
                    model_id: "m".into(),
                },
                Code::BlockModelOutputLimitRequired,
            ),
            (ModelError::Store, Code::BlockLedgerUnavailable),
        ];
        for (e, code) in cases {
            assert_eq!(e.block().0, code);
        }
    }
}
