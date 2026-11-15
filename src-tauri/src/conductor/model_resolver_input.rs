// src-tauri/src/conductor/model_resolver_input.rs
//
// items.id=896 stage 3: the GATHERER that feeds the pure resolver
// (conductor::model_resolver). It reads what the resolver may choose from and
// maps it into the resolver's plain inputs. It decides nothing: which model is
// chosen is the resolver's answer, and every rule about readiness below only marks
// a candidate Ready or not.
//
// LOCAL candidates are the providers rows of type local_model that are curated for
// Focus use (focus_eligible) and whose tag is exactly one of the curated tags
// (providers::curated_models, the trust boundary for what QR may run). A row that
// is not installed or that the user turned off is still returned, marked
// NotInstalled or DisabledByUser, so the resolver knows it exists and passes over it.
//
// HOSTED candidates are the cloud_inference_api providers the run's ceiling permits
// (focus_provider_criteria_store::providers_permitted_by_ceiling: the hard
// anonymity filter), each with its models. Cloud Chat panes (split_screen_web and
// external_service) are user-driven and never candidates (decisions.id=680). A
// hosted model is Ready only with ALL of:
//   * a stored API key for its provider (the same lookup the call itself makes);
//   * a current consent whose attributes hash equals the provider's present hash
//     (a stale or revoked consent is NotConsented);
//   * a readable price, the user's override first (resolve_price). A model of unknown
//     price is PriceUnknown and never chosen automatically (decisions.id=942
//     addendum 2 d, decisions.id=941 point 4);
//   * a status that is not retired.
// The spend gate still decides, at call time, whether the call may go out (caps,
// limits, the unattended budget). The gatherer does not repeat that: a model it
// marks Ready can still be stopped there, and the run then parks with the spend
// gate's own code, as for any hosted step.
//
// COST TIER. A known price of exactly zero is Free, any other known price is
// Metered. PlanIncluded is never produced here: nothing records which plan a user
// holds (providers.plan_inclusion lists plans that include API use, not whether the
// user has one), so a hosted model is never treated as already paid for.
//
// DOCUMENT PROFILE. The run's selected document type is read from the pass's seeds
// under DOCUMENT_TYPE_NAME and mapped to its shipped profile
// (model_resolver::profile_for_document_type). It is the ONLY source of the
// profile: the block's own configuration is never read for it (items.id=896
// ruling). Absent means no profile, and the resolver then reads only the
// task-type-wide cell, which for the shipped data means not measured.
//
// FAIL CLOSED. A read that fails is logged (a count, never a value) and the
// candidate or table it would have supplied is simply missing, so the resolver
// sees fewer candidates, never an invented one.

use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::conductor::executor::get_context_window;
use crate::conductor::model_capability::{CapabilityRow, CapabilitySubject};
use crate::conductor::model_resolver::{Candidate, CostTier, Readiness};
use crate::conductor::provider_selection::HOSTED_PROVIDER_TYPE;
use crate::conductor::tokens::ExternalAccess;
use crate::persistence::focus_provider_criteria_store::providers_permitted_by_ceiling;
use crate::persistence::integration_keys_store::{self, QR_HOSTED_KEY_TYPE};
use crate::persistence::model_capability_store::list_capabilities;
use crate::persistence::provider_spend_store as spend_store;
use crate::persistence::provider_store::{self, ModelStatus, ProviderModel};
use crate::providers::curated_models::is_curated_local_model_tag;
use crate::spend::disclosure::attributes_hash;
use crate::spend::estimate::worst_case;
use crate::spend::price::{resolve_price, PriceQuote};

/// The bound session name that holds the run's selected document type id.
/// items.id=897 registers it as a bound name and guarantees it a value (a Focus
/// preset that fixes its type seeds the same name at seed time). Until then no
/// name seeds it and every model-block step has no profile.
pub const DOCUMENT_TYPE_NAME: &str = "document_type";

/// providers.provider_type of a curated local model row.
const LOCAL_PROVIDER_TYPE: &str = "local_model";

pub struct GatherInput<'a> {
    pub pool: &'a SqlitePool,
    pub user_id: &'a str,
    pub key_hex: &'a str,
    /// The run's ceiling, for the hosted anonymity filter.
    pub ceiling: ExternalAccess,
    pub local_allowed: bool,
    pub hosted_allowed: bool,
    /// The input tokens the cost estimate prices.
    pub est_input_tokens: u32,
    pub est_output_tokens: u32,
}

#[derive(Debug, Default)]
pub struct Gathered {
    pub candidates: Vec<Candidate>,
    pub capabilities: Vec<CapabilityRow>,
    /// The catalog row of each hosted candidate, by (provider id, model id), so a
    /// selection needs no second lookup.
    pub hosted_models: HashMap<(String, String), ProviderModel>,
}

pub async fn gather(input: &GatherInput<'_>) -> Gathered {
    let mut out = Gathered::default();

    match input.pool.acquire().await {
        Ok(mut conn) => match list_capabilities(&mut conn).await {
            Ok(t) => {
                if t.skipped > 0 {
                    log::warn!(
                        "model resolver: {} capability rows use a value this build does not know",
                        t.skipped
                    );
                }
                out.capabilities = t.rows;
            }
            Err(e) => log::warn!("model resolver: capability table unreadable: {e}"),
        },
        Err(e) => log::warn!("model resolver: shared database unavailable: {e}"),
    }

    if input.local_allowed {
        gather_local(input, &mut out).await;
    }
    if input.hosted_allowed {
        gather_hosted(input, &mut out).await;
    }
    out
}

async fn gather_local(input: &GatherInput<'_>, out: &mut Gathered) {
    let rows = match provider_store::list_providers_by_type(input.pool, LOCAL_PROVIDER_TYPE).await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("model resolver: local model rows unreadable: {e}");
            return;
        }
    };
    for p in rows {
        let Some(tag) = p.local_model_tag.clone() else {
            continue;
        };
        if !p.is_local || !p.focus_eligible || !is_curated_local_model_tag(&tag) {
            continue;
        }
        let readiness = if !p.installed {
            Readiness::NotInstalled
        } else if p.qr_disabled_by_user {
            Readiness::DisabledByUser
        } else {
            Readiness::Ready
        };
        out.candidates.push(Candidate {
            subject: CapabilitySubject::Local,
            provider_id: String::new(),
            context_window_tokens: get_context_window(input.pool, None, &tag).await,
            model_id: tag,
            readiness,
            cost_tier: CostTier::Local,
            est_cost_micros: 0,
            provider_is_anonymous: true,
        });
    }
}

async fn gather_hosted(input: &GatherInput<'_>, out: &mut Gathered) {
    let all = match provider_store::list_providers_by_type(input.pool, HOSTED_PROVIDER_TYPE).await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("model resolver: hosted provider rows unreadable: {e}");
            return;
        }
    };
    let permitted = providers_permitted_by_ceiling(all, input.ceiling);
    if permitted.is_empty() {
        return;
    }

    // One open of the user's encrypted spend store for every consent and price
    // override below. No unlocked session means nothing hosted is usable.
    let mut spend = if input.key_hex.is_empty() {
        None
    } else {
        match spend_store::open(input.user_id, input.key_hex).await {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("model resolver: spend store unavailable: {e}");
                None
            }
        }
    };

    for provider in permitted {
        let models = match provider_store::list_models_for_provider(input.pool, &provider.id).await
        {
            Ok(m) => m,
            Err(e) => {
                log::warn!("model resolver: models of a provider unreadable: {e}");
                continue;
            }
        };
        let pricing_texts = provider_store::list_model_pricing_text(input.pool, &provider.id)
            .await
            .unwrap_or_default();

        // Provider-level readiness: a stored key and a current consent.
        let key_present = if input.key_hex.is_empty() {
            false
        } else {
            matches!(
                integration_keys_store::get_active_key(
                    input.user_id,
                    input.key_hex,
                    &provider.id,
                    QR_HOSTED_KEY_TYPE,
                    None,
                )
                .await,
                Ok(Some(_))
            )
        };
        let consented = match spend.as_mut() {
            None => false,
            Some(conn) => {
                let current = attributes_hash(&provider, &models, &pricing_texts);
                match spend_store::consent_standing_conn(conn, &provider.id).await {
                    Ok(s) => s.current.is_some_and(|c| c.attributes_hash == current),
                    Err(e) => {
                        log::warn!("model resolver: consent unreadable: {e}");
                        false
                    }
                }
            }
        };

        for model in models {
            // The price: the user's override first, else the curated row.
            let row_text = pricing_texts
                .iter()
                .find(|(id, _)| *id == model.model_id)
                .and_then(|(_, t)| t.clone());
            let override_text = match spend.as_mut() {
                Some(conn) => {
                    spend_store::get_price_override_raw_conn(conn, &provider.id, &model.model_id)
                        .await
                        .unwrap_or(None)
                }
                None => None,
            };
            let quote = resolve_price(override_text.as_deref(), row_text.as_deref());

            let readiness = if model.status == ModelStatus::Retired {
                Readiness::Retired
            } else if !key_present {
                Readiness::NoKey
            } else if !consented {
                Readiness::NotConsented
            } else if quote == PriceQuote::Unknown {
                Readiness::PriceUnknown
            } else {
                Readiness::Ready
            };
            let (cost_tier, est_cost_micros) = match &quote {
                PriceQuote::Known(price) if price.is_free() => (CostTier::Free, 0),
                PriceQuote::Known(price) => {
                    let e = worst_case(
                        price,
                        u64::from(input.est_input_tokens),
                        u64::from(input.est_output_tokens),
                    );
                    (
                        CostTier::Metered,
                        u64::try_from(e.worst_case_micros).unwrap_or(u64::MAX),
                    )
                }
                // Not Ready anyway: the tier only orders the options list.
                PriceQuote::Unknown => (CostTier::Metered, u64::MAX),
            };
            out.candidates.push(Candidate {
                subject: CapabilitySubject::Hosted,
                provider_id: provider.id.clone(),
                model_id: model.model_id.clone(),
                readiness,
                context_window_tokens: model.context_window_tokens,
                cost_tier,
                est_cost_micros,
                provider_is_anonymous: provider.is_anonymous,
            });
            out.hosted_models
                .insert((provider.id.clone(), model.model_id.clone()), model);
        }
    }
}
