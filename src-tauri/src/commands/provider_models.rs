// src-tauri/src/commands/provider_models.rs
//
// Group 26 -- hosted-provider models, the live model list, price overrides
// (items.id=711, decisions.id=875 point 5-6, spec Part 8e-8f). Four commands:
//   list_provider_models, refresh_available_models, set_model_selection,
//   set_price_override.
// (`estimate_cost`, items.id=710, is extended in commands::provider_spend.)
//
// CONTRACT NOTES (the frontend's contract; the handoff repeats them):
//   - Every statement is a stable CODE (spend::codes) or a short error string
//     (`unknown_provider`, `refresh_unavailable`, ...). No user-facing prose
//     is produced here.
//   - Amounts that can exceed 32 bits are decimal STRINGS holding an integer
//     (specta-typescript refuses i64/u64): prices are micro-USD per million
//     tokens, "70000" = 0.07 USD.
//   - No credential ever appears in a response.
//   - ONLY `refresh_available_models` reaches a provider, and only when the
//     user clicks it, with the user's own saved key. Nothing refreshes by
//     itself, on a timer or at startup (a test scans for any other caller of
//     the list request).
//
// THREE KINDS OF MODEL (spec 8f): a CURATED row (reviewed, global); a
// model only the provider's live list names, "available, not reviewed" (price
// unknown, privacy unverified, caps still apply); and the provider-reported
// FACTS about either (context window, retirement date), which carry the source
// `provider_api` and the date they were read. `origin` is `curated` or
// `uncurated`; `listed` says whether the latest list names the model.
//
// serde_json::Value is NOT used in any exported type (specta's TypeScript
// exporter does not terminate on it; see commands::provider_spend).

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use specta::Type;
use sqlx::SqliteConnection;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::provider_spend::{
    internal, load_catalog, pricing_text, session, today_local, Catalog,
};
use crate::conductor::executor::hosted_client;
use crate::conductor::model_selection::{synthesize_live_model, CONTEXT_UNKNOWN};
use crate::persistence::integration_keys_store::{self, IntegrationKey, QR_HOSTED_KEY_TYPE};
use crate::persistence::provider_models_store::{
    get_live_model_conn, get_refresh_conn, list_live_models_conn, record_refresh_conn,
    set_user_output_limit_conn, LiveModelRow,
};
use crate::persistence::provider_spend_store::{
    self as store, delete_price_override_conn, list_calibrations_conn, list_price_overrides_conn,
    upsert_price_override_conn, CalibrationText,
};
use crate::persistence::provider_store::{ModelStatus, Provider, ProviderModel};
use crate::persistence::user_provider_preference_store::{
    self as prefs, ResolvedModelSelection, SelectionScope, SelectionSource,
};
use crate::providers::errors::ProviderError;
use crate::providers::hosted_call::{CallContext, HostedRoute};
use crate::providers::hosted_client::HostedClient;
use crate::spend::calibration::{applied_from, Applied, MIN_SAMPLES};
use crate::spend::codes::Code;
use crate::spend::money::{format_decimal_micros, parse_decimal_micros};
use crate::spend::price::{parse_pricing, resolve_price, PriceQuote, PriceSource};

/// The most a typed-in price may be, per million tokens: 1,000 USD. A guard
/// against unit mistakes, not a price list.
pub const MAX_OVERRIDE_MICROS_PER_MTOK: u64 = 1_000_000_000;
/// The most prompt-length tiers an override may carry.
pub const MAX_OVERRIDE_TIERS: usize = 4;
/// The largest output limit a user may choose for a model whose context
/// window is not reported.
pub const MAX_OUTPUT_LIMIT_UNKNOWN_CONTEXT: u32 = 1_000_000;

type BuildFn = fn(&HostedRoute, &str, &Value) -> Result<CallContext, ProviderError>;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Type)]
pub struct PriceTierDto {
    /// Micro-USD tier bounds as token counts, decimal strings. Exactly one set.
    pub prompt_tokens_up_to: Option<String>,
    pub prompt_tokens_over: Option<String>,
    pub input_micros_per_mtok: String,
    pub output_micros_per_mtok: String,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct ModelPriceDto {
    /// `price_state.known | unknown | stale`. `stale` is the "estimate may be
    /// stale" note: a curated or provider-reported price older than 90 days.
    pub state: String,
    /// `provider_api`, `curated` or `user`; None when unknown.
    pub source: Option<String>,
    pub as_of: Option<String>,
    pub age_days: Option<i32>,
    /// Micro-USD per million tokens, decimal strings. None when unknown.
    pub input_micros_per_mtok: Option<String>,
    pub output_micros_per_mtok: Option<String>,
    pub tiers: Vec<PriceTierDto>,
    pub is_free: bool,
    /// The user's own price is in force.
    pub overridden: bool,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct ContextDto {
    /// Tokens as a decimal string; None when not known.
    pub tokens: Option<String>,
    /// `provider_api`, `curated` or `user`.
    pub source: Option<String>,
    pub as_of: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct CalibrationDto {
    /// 1000 = 1.0. The factor in force (neutral until enough samples).
    pub input_permille: u32,
    pub output_permille: u32,
    pub input_samples: u32,
    pub output_samples: u32,
    pub input_applied: bool,
    pub output_applied: bool,
    /// Samples a factor needs before it applies.
    pub min_samples: u32,
}

pub(crate) fn calibration_dto(a: &Applied) -> CalibrationDto {
    let n = |v: i64| u32::try_from(v.max(0)).unwrap_or(u32::MAX);
    CalibrationDto {
        input_permille: u32::try_from(a.input_permille).unwrap_or(u32::MAX),
        output_permille: u32::try_from(a.output_permille).unwrap_or(u32::MAX),
        input_samples: n(a.input_samples),
        output_samples: n(a.output_samples),
        input_applied: a.input_applied,
        output_applied: a.output_applied,
        min_samples: n(MIN_SAMPLES),
    }
}

pub(crate) fn price_dto(quote: &PriceQuote, overridden: bool, today: NaiveDate) -> ModelPriceDto {
    match quote {
        PriceQuote::Unknown => ModelPriceDto {
            state: Code::PriceStateUnknown.as_str().to_owned(),
            source: None,
            as_of: None,
            age_days: None,
            input_micros_per_mtok: None,
            output_micros_per_mtok: None,
            tiers: Vec::new(),
            is_free: false,
            overridden,
        },
        PriceQuote::Known(p) => ModelPriceDto {
            state: if p.is_stale(today) {
                Code::PriceStateStale
            } else {
                Code::PriceStateKnown
            }
            .as_str()
            .to_owned(),
            source: Some(p.source.as_str().to_owned()),
            as_of: p.as_of.map(|d| d.to_string()),
            age_days: p.age_days(today).and_then(|d| i32::try_from(d).ok()),
            input_micros_per_mtok: Some(p.input_micro_per_mtok.to_string()),
            output_micros_per_mtok: Some(p.output_micro_per_mtok.to_string()),
            tiers: p
                .tiers
                .iter()
                .map(|t| PriceTierDto {
                    prompt_tokens_up_to: t.up_to.map(|n| n.to_string()),
                    prompt_tokens_over: t.over.map(|n| n.to_string()),
                    input_micros_per_mtok: t.input_micro_per_mtok.to_string(),
                    output_micros_per_mtok: t.output_micro_per_mtok.to_string(),
                })
                .collect(),
            is_free: p.is_free(),
            overridden,
        },
    }
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct ProviderModelDto {
    pub model_id: String,
    /// `curated` (reviewed by QR) or `uncurated` (available, not reviewed).
    pub origin: String,
    /// `active`, `deprecated` or `retired`.
    pub status: String,
    pub is_default: bool,
    /// The model the next step will use at the asked Persona and Focus.
    pub selected: bool,
    /// Named by the latest refresh. None until the user has refreshed.
    pub listed: Option<bool>,
    /// A refresh happened and no longer names this model.
    pub may_be_retired: bool,
    pub retires_at: Option<String>,
    /// `curated` or `provider_api`; None when there is no date.
    pub retires_at_source: Option<String>,
    /// The window QR uses. For an uncurated model whose list reported none,
    /// `tokens` is None and QR makes no pre-call context check.
    pub context: ContextDto,
    /// What the latest list reported, when it reported one.
    pub provider_context: Option<ContextDto>,
    pub price: ModelPriceDto,
    /// Tokenizer and hidden-reasoning multipliers of the curated row (1.0 for
    /// an uncurated model).
    pub token_factor: f64,
    pub output_overhead: f64,
    pub requires_paid: bool,
    pub qr_recommended: bool,
    pub qr_recommendation_rank: Option<u32>,
    /// The output limit the user chose for an uncurated model.
    pub user_output_limit: Option<u32>,
    pub calibration: Option<CalibrationDto>,
    /// Stable warning codes that apply to this model on their own.
    pub notices: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct SelectionInfoDto {
    /// The stored selection at this Persona and Focus, None = the default.
    pub stored_model_id: Option<String>,
    pub stored_from: Option<SelectionSource>,
    /// The model the next step would use.
    pub effective_model_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct ProviderModelsView {
    pub provider_id: String,
    /// The provider publishes a model list QR can read (`models_path`).
    pub refresh_available: bool,
    pub refreshed_at: Option<String>,
    pub listed_count: Option<u32>,
    pub selection: SelectionInfoDto,
    pub models: Vec<ProviderModelDto>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct RefreshResultDto {
    pub listed: u32,
    pub new: u32,
    /// Previously present, absent from this list: flagged may be retired.
    pub newly_missing: u32,
    /// Flagged before, listed again.
    pub returned: u32,
    pub refreshed_at: String,
}

#[derive(Debug, Clone, Deserialize, Type)]
pub struct SetModelSelectionRequest {
    pub provider_id: String,
    pub scope: SelectionScope,
    /// The Persona a Focus belongs to (used only to copy the right preference
    /// when a Focus scope gets its first row).
    pub persona_id: Option<String>,
    /// None clears the selection at this scope.
    pub model_id: Option<String>,
    /// Required the first time an UNCURATED model is selected: QR sends
    /// nothing without an output token limit. Ignored for a curated model.
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Type)]
pub struct PriceTierInput {
    pub prompt_tokens_up_to: Option<u32>,
    pub prompt_tokens_over: Option<u32>,
    /// USD per million tokens as exact decimal text, for example `0.07`.
    pub input_per_mtok: String,
    pub output_per_mtok: String,
}

#[derive(Debug, Clone, Deserialize, Type)]
pub struct PriceInput {
    /// USD per million tokens as exact decimal text (at most six decimals).
    pub input_per_mtok: String,
    pub output_per_mtok: String,
    pub tiers: Vec<PriceTierInput>,
}

#[derive(Debug, Clone, Deserialize, Type)]
pub struct SetPriceOverrideRequest {
    pub provider_id: String,
    pub model_id: String,
    /// None removes the override.
    pub price: Option<PriceInput>,
    /// A price of zero makes every cap rule skip the model, so it needs this
    /// explicit confirmation.
    pub confirmed_free: bool,
}

// ---------------------------------------------------------------------------
// list_provider_models
// ---------------------------------------------------------------------------

/// A curated model the provider's latest list no longer names. Only knowable
/// after a refresh.
fn absent_from_latest_list(listed: Option<bool>) -> bool {
    listed == Some(false)
}

#[allow(clippy::too_many_arguments)]
fn model_dto(
    model: &ProviderModel,
    curated: bool,
    live: Option<&LiveModelRow>,
    refreshed: bool,
    override_raw: Option<&str>,
    row_pricing: Option<&str>,
    calibration: Option<&CalibrationText>,
    effective_id: Option<&str>,
    today: NaiveDate,
) -> ProviderModelDto {
    let quote = resolve_price(override_raw, row_pricing);
    let listed = refreshed.then(|| live.is_some_and(|l| l.present));
    let may_be_retired = absent_from_latest_list(listed);
    let row_retires = model.retires_at.clone();
    let (retires_at, retires_at_source) =
        match (&row_retires, live.and_then(|l| l.retires_at.clone())) {
            (Some(r), _) => (Some(r.clone()), Some("curated".to_owned())),
            (None, Some(r)) => (Some(r), Some("provider_api".to_owned())),
            (None, None) => (None, None),
        };
    let deprecated = model.status == ModelStatus::Deprecated || retires_at.is_some();

    let context = if curated {
        ContextDto {
            tokens: Some(model.context_window_tokens.to_string()),
            source: model.context_window_source.clone(),
            as_of: model.context_window_as_of.clone(),
        }
    } else {
        ContextDto {
            tokens: (model.context_window_tokens != CONTEXT_UNKNOWN)
                .then(|| model.context_window_tokens.to_string()),
            source: model.context_window_source.clone(),
            as_of: model.context_window_as_of.clone(),
        }
    };
    let provider_context = live.and_then(|l| {
        l.context_tokens.map(|t| ContextDto {
            tokens: Some(t.to_string()),
            source: Some("provider_api".to_owned()),
            as_of: l.context_as_of.clone(),
        })
    });

    let mut notices = Vec::new();
    if !curated {
        notices.push(Code::WarnModelUncurated.as_str().to_owned());
    }
    if may_be_retired {
        notices.push(Code::WarnModelMayBeRetired.as_str().to_owned());
    }
    if deprecated {
        notices.push(Code::WarnModelDeprecated.as_str().to_owned());
    }
    match &quote {
        PriceQuote::Unknown => notices.push(Code::WarnPriceUnknown.as_str().to_owned()),
        PriceQuote::Known(p) if p.is_stale(today) => {
            notices.push(Code::WarnPriceStale.as_str().to_owned())
        }
        PriceQuote::Known(_) => {}
    }

    ProviderModelDto {
        model_id: model.model_id.clone(),
        origin: if curated { "curated" } else { "uncurated" }.to_owned(),
        status: match model.status {
            ModelStatus::Active => "active",
            ModelStatus::Deprecated => "deprecated",
            ModelStatus::Retired => "retired",
        }
        .to_owned(),
        is_default: model.is_default,
        selected: effective_id == Some(model.model_id.as_str()),
        listed,
        may_be_retired,
        retires_at,
        retires_at_source,
        context,
        provider_context,
        price: price_dto(&quote, override_raw.is_some(), today),
        token_factor: model.token_factor,
        output_overhead: model.output_overhead,
        requires_paid: model.requires_paid,
        qr_recommended: model.qr_recommended,
        qr_recommendation_rank: model.qr_recommendation_rank,
        user_output_limit: live.and_then(|l| l.user_output_limit),
        calibration: calibration.map(|c| calibration_dto(&applied_from(Some(c)))),
        notices,
    }
}

pub(crate) async fn list_impl(
    conn: &mut SqliteConnection,
    cat: &Catalog,
    stored: Option<&ResolvedModelSelection>,
    today: NaiveDate,
) -> Result<ProviderModelsView, String> {
    let pid = cat.provider.id.as_str();
    let live = list_live_models_conn(conn, pid)
        .await
        .map_err(|e| internal("live models", e))?;
    let refresh = get_refresh_conn(conn, pid)
        .await
        .map_err(|e| internal("refresh row", e))?;
    let overrides: BTreeMap<String, String> = list_price_overrides_conn(conn, pid)
        .await
        .map_err(|e| internal("price overrides", e))?
        .into_iter()
        .collect();
    let calibrations: BTreeMap<String, CalibrationText> = list_calibrations_conn(conn, pid)
        .await
        .map_err(|e| internal("calibration", e))?
        .into_iter()
        .collect();

    let default_model = cat.models.iter().find(|m| m.is_default);
    let effective_id = stored
        .map(|s| s.model_id.as_str())
        .or_else(|| default_model.map(|m| m.model_id.as_str()));
    let refreshed = refresh.is_some();

    let mut models = Vec::new();
    for m in &cat.models {
        let live_row = live.iter().find(|l| l.model_id == m.model_id);
        models.push(model_dto(
            m,
            true,
            live_row,
            refreshed,
            overrides.get(&m.model_id).map(String::as_str),
            pricing_text(&cat.pricing_texts, &m.model_id),
            calibrations.get(&m.model_id),
            effective_id,
            today,
        ));
    }
    for l in &live {
        if cat.models.iter().any(|m| m.model_id == l.model_id) {
            continue;
        }
        let synth = synthesize_live_model(default_model, l);
        models.push(model_dto(
            &synth,
            false,
            Some(l),
            refreshed,
            overrides.get(&l.model_id).map(String::as_str),
            None,
            calibrations.get(&l.model_id),
            effective_id,
            today,
        ));
    }

    Ok(ProviderModelsView {
        provider_id: pid.to_owned(),
        refresh_available: is_hosted(&cat.provider) && cat.provider.models_path.is_some(),
        refreshed_at: refresh.as_ref().map(|r| r.refreshed_at.clone()),
        listed_count: refresh.as_ref().map(|r| r.listed_count),
        selection: SelectionInfoDto {
            stored_model_id: stored.map(|s| s.model_id.clone()),
            stored_from: stored.map(|s| s.source),
            effective_model_id: effective_id.map(str::to_owned),
        },
        models,
    })
}

fn is_hosted(p: &Provider) -> bool {
    p.provider_type == "cloud_inference_api" && p.api_dialect.is_some()
}

#[tauri::command]
#[specta::specta]
pub async fn list_provider_models(
    provider_id: String,
    persona_id: Option<String>,
    focus_id: Option<String>,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<ProviderModelsView, String> {
    let (user_id, key_hex_str) = session(&key_registry).await?;
    let cat = load_catalog(&pool, &provider_id).await?;
    let stored = prefs::resolve_model_selection(
        &pool,
        &user_id,
        persona_id.as_deref(),
        focus_id.as_deref(),
        &provider_id,
    )
    .await
    .map_err(|e| internal("selection lookup", e))?;
    let mut conn = store::open(&user_id, &key_hex_str)
        .await
        .map_err(|e| internal("open spend store", e))?;
    list_impl(&mut conn, &cat, stored.as_ref(), today_local()).await
}

// ---------------------------------------------------------------------------
// refresh_available_models
// ---------------------------------------------------------------------------

fn refresh_error(e: &ProviderError) -> String {
    match e {
        ProviderError::MissingApiKey { .. } => "missing_api_key",
        ProviderError::InvalidApiKey { .. } => "invalid_api_key",
        ProviderError::RateLimit { .. } => "rate_limited",
        ProviderError::Timeout { .. } => "timeout",
        ProviderError::Unavailable { .. } | ProviderError::ServerError { .. } => {
            "provider_unavailable"
        }
        ProviderError::MalformedResponse { .. } => "malformed_response",
        ProviderError::Misconfigured { .. } => "misconfigured",
        _ => "refresh_failed",
    }
    .to_owned()
}

/// The refresh, with the client and the call-context builder as parameters so
/// it can be exercised against a loopback server (`build` is
/// `CallContext::build` in production, which refuses anything but HTTPS).
pub(crate) async fn refresh_impl(
    client: &HostedClient,
    conn: &mut SqliteConnection,
    cat: &Catalog,
    key: Option<IntegrationKey>,
    build: BuildFn,
    now: DateTime<Utc>,
) -> Result<RefreshResultDto, String> {
    if !is_hosted(&cat.provider) {
        return Err("unknown_provider".to_owned());
    }
    // No models_path means the provider publishes no list QR can read.
    if cat.provider.models_path.is_none() {
        return Err("refresh_unavailable".to_owned());
    }
    // A route needs a model row to build from; every hosted provider has one.
    let route_model = cat
        .models
        .iter()
        .find(|m| m.is_default)
        .or_else(|| cat.models.first())
        .ok_or_else(|| "refresh_unavailable".to_owned())?;
    let key = key.ok_or_else(|| "missing_api_key".to_owned())?;

    let route = HostedRoute::from_rows(&cat.provider, route_model);
    let ctx = build(&route, &key.credential, &key.extra_metadata).map_err(|e| refresh_error(&e))?;
    let listed = client
        .list_models(&ctx)
        .await
        .map_err(|e| refresh_error(&e))?;
    // Absence is how "may be retired" is inferred, so an empty list (a glitch
    // or a wrong endpoint) must not flag every model.
    if listed.is_empty() {
        return Err("empty_model_list".to_owned());
    }
    let summary = record_refresh_conn(conn, &cat.provider.id, &listed, now)
        .await
        .map_err(|e| internal("record refresh", e))?;
    Ok(RefreshResultDto {
        listed: summary.listed,
        new: summary.new,
        newly_missing: summary.newly_missing,
        returned: summary.returned,
        refreshed_at: crate::persistence::provider_spend_store::ledger_timestamp(now),
    })
}

/// Refresh the provider's model list. USER-CLICKED ONLY: the one place QR asks
/// a provider for its list, with the user's own saved key.
#[tauri::command]
#[specta::specta]
pub async fn refresh_available_models(
    provider_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<RefreshResultDto, String> {
    let (user_id, key_hex_str) = session(&key_registry).await?;
    let cat = load_catalog(&pool, &provider_id).await?;
    let key = integration_keys_store::get_active_key(
        &user_id,
        &key_hex_str,
        &provider_id,
        QR_HOSTED_KEY_TYPE,
        None,
    )
    .await
    .map_err(|e| internal("read key", e))?;
    let mut conn = store::open(&user_id, &key_hex_str)
        .await
        .map_err(|e| internal("open spend store", e))?;
    refresh_impl(
        hosted_client(),
        &mut conn,
        &cat,
        key,
        CallContext::build,
        Utc::now(),
    )
    .await
}

// ---------------------------------------------------------------------------
// set_model_selection
// ---------------------------------------------------------------------------

pub(crate) async fn set_selection_impl(
    pool: &sqlx::SqlitePool,
    conn: &mut SqliteConnection,
    user_id: &str,
    cat: &Catalog,
    req: &SetModelSelectionRequest,
) -> Result<(), String> {
    if !is_hosted(&cat.provider) {
        return Err("unknown_provider".to_owned());
    }
    let scope_store_err = |e: prefs::UserProviderPreferenceStoreError| match e {
        prefs::UserProviderPreferenceStoreError::Validation(_) => "invalid_scope".to_owned(),
        other => internal("save selection", other),
    };

    let Some(model_id) = req.model_id.as_deref() else {
        return prefs::set_model_selection(
            pool,
            user_id,
            &req.scope,
            req.persona_id.as_deref(),
            &cat.provider.id,
            None,
        )
        .await
        .map_err(scope_store_err);
    };

    let curated = cat.models.iter().find(|m| m.model_id == model_id);
    if let Some(m) = curated {
        if m.status == ModelStatus::Retired {
            return Err(Code::BlockModelRetired.as_str().to_owned());
        }
    } else {
        // Not a reviewed model: it must be one the user's own refresh named,
        // and it needs an output limit the user chose.
        let live = get_live_model_conn(conn, &cat.provider.id, model_id)
            .await
            .map_err(|e| internal("live model", e))?
            .ok_or_else(|| "unknown_model".to_owned())?;
        let limit = req
            .max_output_tokens
            .or(live.user_output_limit)
            .ok_or_else(|| Code::BlockModelOutputLimitRequired.as_str().to_owned())?;
        if limit == 0 {
            return Err("invalid_output_limit".to_owned());
        }
        let ceiling = live
            .context_tokens
            .unwrap_or(MAX_OUTPUT_LIMIT_UNKNOWN_CONTEXT);
        if limit > ceiling {
            return Err("invalid_output_limit".to_owned());
        }
        set_user_output_limit_conn(conn, &cat.provider.id, model_id, Some(limit))
            .await
            .map_err(|e| internal("save output limit", e))?;
    }

    prefs::set_model_selection(
        pool,
        user_id,
        &req.scope,
        req.persona_id.as_deref(),
        &cat.provider.id,
        Some(model_id),
    )
    .await
    .map_err(scope_store_err)
}

#[tauri::command]
#[specta::specta]
pub async fn set_model_selection(
    request: SetModelSelectionRequest,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<(), String> {
    let (user_id, key_hex_str) = session(&key_registry).await?;
    let cat = load_catalog(&pool, &request.provider_id).await?;
    let mut conn = store::open(&user_id, &key_hex_str)
        .await
        .map_err(|e| internal("open spend store", e))?;
    set_selection_impl(&pool, &mut conn, &user_id, &cat, &request).await
}

// ---------------------------------------------------------------------------
// set_price_override
// ---------------------------------------------------------------------------

struct ValidTier {
    up_to: Option<u32>,
    over: Option<u32>,
    input: u64,
    output: u64,
}

fn rate(text: &str) -> Result<u64, String> {
    let micros = parse_decimal_micros(text.trim()).ok_or_else(|| "invalid_price".to_owned())?;
    if micros > MAX_OVERRIDE_MICROS_PER_MTOK {
        return Err("price_too_high".to_owned());
    }
    Ok(micros)
}

/// The `pricing` JSON of a user override, built from the validated micros and
/// written with the exact-decimal text `spend::price` reads back.
fn pricing_json(input: u64, output: u64, tiers: &[ValidTier], today: NaiveDate) -> String {
    let tiers_json = if tiers.is_empty() {
        "null".to_owned()
    } else {
        let parts: Vec<String> = tiers
            .iter()
            .map(|t| {
                let bound = match (t.up_to, t.over) {
                    (Some(n), _) => format!("\"prompt_tokens_up_to\":{n}"),
                    (_, Some(n)) => format!("\"prompt_tokens_over\":{n}"),
                    _ => String::new(),
                };
                format!(
                    "{{{bound},\"input_per_mtok\":{},\"output_per_mtok\":{}}}",
                    format_decimal_micros(t.input),
                    format_decimal_micros(t.output)
                )
            })
            .collect();
        format!("[{}]", parts.join(","))
    };
    format!(
        "{{\"input_per_mtok\":{},\"output_per_mtok\":{},\"currency\":\"USD\",\"tiers\":{tiers_json},\"as_of\":\"{today}\",\"source\":\"user\"}}",
        format_decimal_micros(input),
        format_decimal_micros(output),
    )
}

pub(crate) async fn set_price_override_impl(
    conn: &mut SqliteConnection,
    cat: &Catalog,
    req: &SetPriceOverrideRequest,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> Result<ModelPriceDto, String> {
    if !is_hosted(&cat.provider) {
        return Err("unknown_provider".to_owned());
    }
    let pid = cat.provider.id.as_str();
    let mid = req.model_id.as_str();
    let curated = cat.models.iter().any(|m| m.model_id == mid);
    if !curated
        && get_live_model_conn(conn, pid, mid)
            .await
            .map_err(|e| internal("live model", e))?
            .is_none()
    {
        return Err("unknown_model".to_owned());
    }
    let row_pricing = pricing_text(&cat.pricing_texts, mid);

    let Some(price) = &req.price else {
        delete_price_override_conn(conn, pid, mid)
            .await
            .map_err(|e| internal("remove price override", e))?;
        return Ok(price_dto(&resolve_price(None, row_pricing), false, today));
    };

    let input = rate(&price.input_per_mtok)?;
    let output = rate(&price.output_per_mtok)?;
    if price.tiers.len() > MAX_OVERRIDE_TIERS {
        return Err("too_many_tiers".to_owned());
    }
    let mut tiers = Vec::new();
    for t in &price.tiers {
        // Exactly one bound, and a real one.
        let bound_ok = match (t.prompt_tokens_up_to, t.prompt_tokens_over) {
            (Some(n), None) | (None, Some(n)) => n > 0,
            _ => false,
        };
        if !bound_ok {
            return Err("invalid_tier".to_owned());
        }
        tiers.push(ValidTier {
            up_to: t.prompt_tokens_up_to,
            over: t.prompt_tokens_over,
            input: rate(&t.input_per_mtok)?,
            output: rate(&t.output_per_mtok)?,
        });
    }

    let json = pricing_json(input, output, &tiers, today);
    // The override must read back as exactly what was typed: the same parser
    // the spend gate uses is the validator.
    let parsed = parse_pricing(&json, Some(PriceSource::User))
        .filter(|p| {
            p.input_micro_per_mtok == input
                && p.output_micro_per_mtok == output
                && p.tiers.len() == tiers.len()
        })
        .ok_or_else(|| "invalid_price".to_owned())?;
    if parsed.is_free() && !req.confirmed_free {
        return Err("free_price_needs_confirmation".to_owned());
    }

    upsert_price_override_conn(conn, pid, mid, &json, now)
        .await
        .map_err(|e| internal("save price override", e))?;
    Ok(price_dto(
        &resolve_price(Some(&json), row_pricing),
        true,
        today,
    ))
}

#[tauri::command]
#[specta::specta]
pub async fn set_price_override(
    request: SetPriceOverrideRequest,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<ModelPriceDto, String> {
    let (user_id, key_hex_str) = session(&key_registry).await?;
    let cat = load_catalog(&pool, &request.provider_id).await?;
    let mut conn = store::open(&user_id, &key_hex_str)
        .await
        .map_err(|e| internal("open spend store", e))?;
    set_price_override_impl(&mut conn, &cat, &request, today_local(), Utc::now()).await
}

#[cfg(test)]
#[path = "provider_models_tests.rs"]
mod tests;
