// src-tauri/src/commands/provider_spend.rs
//
// Group 14 -- hosted-provider consent, limits and spend (items.id=710,
// decisions.id=875, spec Part 8d-8e). Six commands:
//   get_provider_disclosure, record_provider_consent, set_limit, list_limits,
//   get_usage_summary, estimate_cost.
//
// CONTRACT NOTES (these are the frontend's contract; the handoff repeats them):
//   - Every statement is a stable CODE (spend::codes) plus structured
//     parameters. No user-facing prose is produced here; the screens render
//     codes through i18n (items.id=693, 580, 696).
//   - Amounts that can exceed 32 bits are decimal STRINGS holding an integer:
//     specta-typescript refuses to export i64/u64, and a float would break the
//     integer-money rule. Money is micro-USD (1 USD = "1000000").
//   - Credentials never appear in any response.
//   - The session (user id + key) comes from KeyRegistry; not logged in is an
//     error. Nothing here sends anything to a provider.
//
// serde_json::Value is NOT used in any exported type: specta's TypeScript
// exporter recurses through it without terminating (see commands/mod.rs,
// NotImplementedPlaceholder). Warning parameters are a typed map instead.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use specta::Type;
use sqlx::SqliteConnection;
use tauri::State;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::commands::provider_models::{calibration_dto, CalibrationDto, ContextDto};
use crate::conductor::model_selection::{
    resolve_hosted_model, synthesize_live_model, ModelError, CONTEXT_UNKNOWN,
};
use crate::persistence::provider_models_store::{get_live_model_conn, get_refresh_conn};
use crate::persistence::provider_spend_store::{
    self as store, LimitMetric, LimitPeriod, LimitRow, LimitSpec, ScopeKind, UsageFilter,
};
use crate::persistence::provider_store::{self, Provider, ProviderModel};
use crate::spend::calibration::applied_from;
use crate::spend::codes::{Code, WarningTier};
use crate::spend::disclosure::{
    attributes_hash, compute_warnings, cost_class_of, effective_attributes,
    provider_can_cost_money, CostClass, ModelNotes, PriceView, Warning, WarningInput,
};
use crate::spend::estimate::{
    expected_micros, input_tokens_estimate, worst_case, NEUTRAL_PERMILLE,
};
use crate::spend::money::permille_ceil;
use crate::spend::price::{resolve_price, PriceQuote};

/// Suggested cap at first consent to a metered provider: 5 USD per month.
pub const SUGGESTED_CAP_MICROS: i64 = 5_000_000;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// A warning parameter value. Parameters are structured values the screen
/// formats, never pre-formatted text.
#[derive(Debug, Clone, PartialEq, Serialize, Type)]
#[serde(untagged)]
pub enum ParamValue {
    Flag(bool),
    Count(u32),
    Text(String),
}

pub type ParamMap = BTreeMap<String, Option<ParamValue>>;

fn to_params(v: &Value) -> ParamMap {
    let mut out = ParamMap::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            let p = match val {
                Value::Null => None,
                Value::Bool(b) => Some(ParamValue::Flag(*b)),
                Value::Number(n) => n
                    .as_u64()
                    .map(|u| ParamValue::Count(u32::try_from(u).unwrap_or(u32::MAX))),
                Value::String(s) => Some(ParamValue::Text(s.clone())),
                _ => continue,
            };
            out.insert(k.clone(), p);
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Serialize, Type)]
pub struct WarningDto {
    /// Stable code, for example `warning.unverified_provider`.
    pub code: String,
    pub tier: WarningTier,
    pub params: ParamMap,
}

pub(crate) fn warning_dto(w: &Warning) -> WarningDto {
    WarningDto {
        code: w.code.clone(),
        tier: w.tier,
        params: to_params(&w.params),
    }
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct DisclosedAttributes {
    pub trains_on_data: bool,
    pub retains_data: bool,
    /// None = the provider does not state a retention period.
    pub retention_days: Option<u32>,
    pub is_anonymous: bool,
    pub login_required: bool,
    pub jurisdiction: Option<String>,
    /// `contractual` or `policy_only`; None = not assessed.
    pub commitment_basis: Option<String>,
    /// `verified`, `unverified` or `stale`.
    pub verification_status: String,
    /// Curated destination risk: 1 low, 2 medium, 3 high.
    pub risk_rating: u8,
    /// When the posture was verified, from the catalog (date text).
    pub as_of: Option<String>,
    /// Catalog free-text notes about per-model differences. Data from the
    /// provider row, not text written in code.
    pub model_overrides: Vec<String>,
    pub disclosures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct ProviderDisclosure {
    pub provider_id: String,
    pub display_name: String,
    pub attributes: DisclosedAttributes,
    pub warnings: Vec<WarningDto>,
    pub overall_tier: WarningTier,
    /// What a consent must quote back (record_provider_consent refuses any
    /// other value, so a screen cannot consent to attributes it did not show).
    pub attributes_hash: String,
    /// `consent_state.none | current | stale | revoked`.
    pub consent_state: String,
    pub cost_class: CostClass,
    /// Some live model is metered or unpriced.
    pub can_cost_money: bool,
    /// A spend cap (or an explicit no-cap confirmation) is needed to consent.
    pub cap_required: bool,
    /// Micro-USD, as a decimal string.
    pub suggested_cap_micros: String,
    pub spend_limit_url: Option<String>,
    /// `price_state.known | unknown | stale` for the selected model.
    pub price_state: String,
    pub price_as_of: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapChoice {
    /// Set (or replace) the all-providers monthly cost cap.
    Set { micros: String },
    /// Consent with no cap, explicitly confirmed.
    NoCap { confirmed: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ConsentAction {
    Consent,
    Revoke,
}

#[derive(Debug, Clone, Deserialize, Type)]
pub struct RecordConsentRequest {
    pub provider_id: String,
    pub action: ConsentAction,
    /// Required for `consent`: the `attributes_hash` the screen displayed.
    pub attributes_hash: Option<String>,
    /// Required for `consent` when the provider can cost money and no cap
    /// applies yet.
    pub cap: Option<CapChoice>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct ConsentResult {
    /// `consent_state.current` after a consent, `consent_state.revoked` after
    /// a revoke.
    pub consent_state: String,
    /// The cap limit this consent created or replaced, if any.
    pub cap_limit_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct LimitDto {
    pub id: String,
    pub metric: LimitMetric,
    pub period: LimitPeriod,
    pub scope_kind: ScopeKind,
    pub scope_id: Option<String>,
    /// Micro-USD for the cost metric, a plain count otherwise. Decimal string.
    pub limit_value: String,
    pub warn_at_pct: u8,
    /// The user approves unattended runs spending within this per-run cost
    /// limit. Only a per-run cost limit can carry it.
    pub approves_unattended: bool,
}

fn limit_dto(row: &LimitRow) -> LimitDto {
    LimitDto {
        id: row.id.clone(),
        metric: row.spec.metric,
        period: row.spec.period,
        scope_kind: row.spec.scope_kind,
        scope_id: row.spec.scope_id.clone(),
        limit_value: row.spec.limit_value.to_string(),
        warn_at_pct: row.spec.warn_at_pct,
        approves_unattended: row.spec.approves_unattended,
    }
}

#[derive(Debug, Clone, Deserialize, Type)]
pub struct SetLimitRequest {
    pub metric: LimitMetric,
    pub period: LimitPeriod,
    pub scope_kind: ScopeKind,
    pub scope_id: Option<String>,
    /// Micro-USD for cost, a plain count otherwise. Decimal string, > 0.
    pub limit_value: String,
    pub warn_at_pct: u8,
    pub approves_unattended: bool,
    /// Remove the limit with this (metric, period, scope) instead of setting it.
    pub remove: bool,
}

#[derive(Debug, Clone, Deserialize, Type)]
pub struct UsageSummaryRequest {
    /// `day`, `week` or `month`. (`run` limits apply per run and are not
    /// summarized.)
    pub period: LimitPeriod,
    pub provider_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct ProviderUsageDto {
    pub provider_id: String,
    /// Micro-USD, decimal string.
    pub cost_micros: String,
    pub tokens_in: String,
    pub tokens_out: String,
    pub requests: String,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct LimitStatusDto {
    pub limit: LimitDto,
    /// Used inside the limit's own scope and window (same unit as the limit).
    pub spent: String,
    /// Whole percent of the limit used, capped at 100000.
    pub pct: u32,
    /// `limit_state.ok | warn | reached`.
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct RateSnapshotDto {
    pub observed_at: String,
    pub requests_limit: Option<String>,
    pub requests_remaining: Option<String>,
    pub tokens_limit: Option<String>,
    pub tokens_remaining: Option<String>,
    /// The provider's own reset text, passed through (a duration, a
    /// timestamp or a count).
    pub requests_reset: Option<String>,
    pub tokens_reset: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct FreeTierDto {
    pub provider_id: String,
    pub requests_per_minute: Option<u32>,
    pub requests_per_day: Option<u32>,
    pub tokens_per_minute: Option<u32>,
    pub tokens_per_day: Option<u32>,
    /// The published-limits as-of date from the catalog.
    pub as_of: Option<String>,
    /// The latest rate-limit headers seen from the provider, when any.
    pub observed: Option<RateSnapshotDto>,
}

/// Usage and limit status.
///
/// SETTLEMENT CAN OVERSHOOT A CAP. A call is cleared against its worst-case
/// estimate and charged its actual usage afterwards. Actual usage can exceed
/// the estimate (a provider counting reasoning past the output limit, a
/// dearer prompt-length price tier chosen from the real prompt size, a billed
/// call whose usage was only estimated), and a call that may have reached the
/// provider with no usage returned is charged at its reserved worst case. The
/// overshoot is recorded, not refused, so a figure here can sit above its
/// limit. `settlement_may_exceed_limits` is always true and repeats this in
/// data for consumers.
#[derive(Debug, Clone, Serialize, Type)]
pub struct UsageSummary {
    pub period: LimitPeriod,
    /// Start of the window (UTC, fixed-width text), calendar in local time.
    pub since: String,
    pub settlement_may_exceed_limits: bool,
    /// What the cost figures mean: `cost_basis.list_price_estimate` (list
    /// price from the catalog or a user override, not a billed amount).
    pub cost_basis: String,
    pub totals: Vec<ProviderUsageDto>,
    /// Day, week and month limits only (run limits apply per run).
    pub limits: Vec<LimitStatusDto>,
    pub free_tiers: Vec<FreeTierDto>,
}

#[derive(Debug, Clone, Deserialize, Type)]
pub struct EstimateCostRequest {
    pub provider_id: String,
    /// The model to price. None = the model the next step would use at
    /// `persona_id` and `focus_id` (the stored selection, else the default).
    /// A model only the user's live list names is allowed (items.id=711).
    pub model_id: Option<String>,
    pub persona_id: Option<String>,
    pub focus_id: Option<String>,
    /// Size of the prompt in BYTES (not characters).
    pub input_bytes: u32,
    /// The output limit to price against; the model row's limit when absent.
    pub max_output_tokens: Option<u32>,
    /// The caller's expectation of visible output tokens, for the expected
    /// figure. There is no default.
    pub expected_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct CostEstimate {
    /// `price_state.known | unknown | stale`.
    pub price_state: String,
    /// `provider_api`, `curated` or `user`; None when the price is unknown.
    pub price_source: Option<String>,
    pub price_as_of: Option<String>,
    pub price_age_days: Option<i32>,
    pub cost_basis: String,
    pub input_tokens: String,
    pub output_limit_tokens: String,
    /// Micro-USD, decimal strings. None when the price is unknown.
    pub input_micros: Option<String>,
    pub worst_case_micros: Option<String>,
    pub expected_micros: Option<String>,
    /// The model that was priced.
    pub model_id: String,
    /// `curated` or `uncurated` (items.id=711).
    pub origin: String,
    /// The context window used, with its source and date. `tokens` is None
    /// for an uncurated model whose list reported none.
    pub context: ContextDto,
    /// The calibration factors in force. Output calibration assumes the
    /// provider's completion tokens include its reasoning tokens (verified
    /// for one provider) and feeds only `expected_micros`; the input factor
    /// is applied to the worst case only when it is above 1.0.
    pub calibration: CalibrationDto,
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

pub(crate) async fn session(key_registry: &KeyRegistry) -> Result<(String, String), String> {
    key_registry
        .with_key(|k| (k.user_id.clone(), key_hex(&k.data_key)))
        .await
        .ok_or_else(|| "not logged in".to_owned())
}

pub(crate) fn internal<E: std::fmt::Display>(what: &str, e: E) -> String {
    log::warn!("provider_spend: {what}: {e}");
    format!("internal:{what}")
}

fn parse_amount(text: &str) -> Result<i64, String> {
    let v: i64 = text
        .trim()
        .parse()
        .map_err(|_| "invalid_amount".to_owned())?;
    if v <= 0 {
        return Err("invalid_amount".to_owned());
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Shared lookups
// ---------------------------------------------------------------------------

pub(crate) struct Catalog {
    pub(crate) provider: Provider,
    pub(crate) models: Vec<ProviderModel>,
    pub(crate) pricing_texts: Vec<(String, Option<String>)>,
}

pub(crate) async fn load_catalog(
    pool: &sqlx::SqlitePool,
    provider_id: &str,
) -> Result<Catalog, String> {
    let provider = provider_store::get_provider(pool, provider_id)
        .await
        .map_err(|e| internal("provider lookup", e))?
        .ok_or_else(|| "unknown_provider".to_owned())?;
    let models = provider_store::list_models_for_provider(pool, provider_id)
        .await
        .map_err(|e| internal("model lookup", e))?;
    let pricing_texts = provider_store::list_model_pricing_text(pool, provider_id)
        .await
        .map_err(|e| internal("pricing lookup", e))?;
    Ok(Catalog {
        provider,
        models,
        pricing_texts,
    })
}

pub(crate) fn pricing_text<'a>(
    texts: &'a [(String, Option<String>)],
    model_id: &str,
) -> Option<&'a str> {
    texts
        .iter()
        .find(|(id, _)| id == model_id)
        .and_then(|(_, t)| t.as_deref())
}

pub(crate) fn pick_model<'a>(
    cat: &'a Catalog,
    model_id: Option<&str>,
) -> Option<&'a ProviderModel> {
    match model_id {
        Some(id) => cat.models.iter().find(|m| m.model_id == id),
        None => cat
            .models
            .iter()
            .find(|m| m.is_default)
            .or_else(|| cat.models.first()),
    }
}

pub(crate) fn today_local() -> chrono::NaiveDate {
    chrono::Local::now().date_naive()
}

fn gate_text(provider: &Provider, key: &str) -> Vec<String> {
    provider
        .documentation_gate
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// get_provider_disclosure
// ---------------------------------------------------------------------------

pub(crate) async fn disclosure_impl(
    conn: &mut SqliteConnection,
    cat: &Catalog,
    model_id: Option<&str>,
    sensitive_persona: bool,
) -> Result<ProviderDisclosure, String> {
    let curated = pick_model(cat, model_id);
    // A model only the user's live list names is "available, not reviewed"
    // (items.id=711): it gets the provider's attributes plus its own model_notes.
    let mut model_notes = ModelNotes::default();
    let mut live_model: Option<ProviderModel> = None;
    let refreshed = get_refresh_conn(conn, &cat.provider.id)
        .await
        .map_err(|e| internal("refresh row", e))?
        .is_some();
    if let Some(c) = curated {
        model_notes.deprecated = c.status
            == crate::persistence::provider_store::ModelStatus::Deprecated
            || c.retires_at.is_some();
        if refreshed {
            let row = get_live_model_conn(conn, &cat.provider.id, &c.model_id)
                .await
                .map_err(|e| internal("live model", e))?;
            model_notes.may_be_retired = !row.is_some_and(|r| r.present);
        }
    } else if let Some(id) = model_id {
        if let Some(row) = get_live_model_conn(conn, &cat.provider.id, id)
            .await
            .map_err(|e| internal("live model", e))?
        {
            model_notes.uncurated = true;
            model_notes.may_be_retired = !row.present;
            let default = cat.models.iter().find(|m| m.is_default);
            live_model = Some(synthesize_live_model(default, &row));
        }
    }
    let model = curated.or(live_model.as_ref());
    let hash = attributes_hash(&cat.provider, &cat.models, &cat.pricing_texts);

    let quote = match model {
        Some(m) => {
            let override_raw =
                store::get_price_override_raw_conn(conn, &cat.provider.id, &m.model_id)
                    .await
                    .map_err(|e| internal("price override", e))?;
            resolve_price(
                override_raw.as_deref(),
                pricing_text(&cat.pricing_texts, &m.model_id),
            )
        }
        None => PriceQuote::Unknown,
    };
    let today = today_local();
    let price = PriceView::from_quote(&quote, today);
    let price_as_of = match &quote {
        PriceQuote::Known(p) => p.as_of.map(|d| d.to_string()),
        PriceQuote::Unknown => None,
    };
    let price_state = match price {
        PriceView::Unknown => Code::PriceStateUnknown,
        PriceView::Known { stale: true, .. } => Code::PriceStateStale,
        PriceView::Known { .. } => Code::PriceStateKnown,
    };

    let set = compute_warnings(&WarningInput {
        provider: &cat.provider,
        model,
        model_notes,
        price,
        price_as_of: price_as_of.clone(),
        sensitive_persona,
    });

    let standing = store::consent_standing_conn(conn, &cat.provider.id)
        .await
        .map_err(|e| internal("consent lookup", e))?;
    let consent_state = match &standing.current {
        Some(c) if c.attributes_hash == hash => Code::ConsentStateCurrent,
        Some(_) => Code::ConsentStateStale,
        None if standing.any_row => Code::ConsentStateRevoked,
        None => Code::ConsentStateNone,
    };

    let can_cost = provider_can_cost_money(&cat.models, &cat.pricing_texts);
    let limits = store::applicable_limits_conn(conn, &cat.provider.id, None, None)
        .await
        .map_err(|e| internal("limit lookup", e))?;
    let has_cap = limits.iter().any(|l| {
        l.spec.metric == LimitMetric::CostUsd
            && matches!(
                l.spec.period,
                LimitPeriod::Day | LimitPeriod::Week | LimitPeriod::Month
            )
    });
    let cap_required = can_cost && !has_cap;

    let eff = effective_attributes(&cat.provider, model);
    let p = &cat.provider;
    Ok(ProviderDisclosure {
        provider_id: p.id.clone(),
        display_name: p.display_name.clone(),
        attributes: DisclosedAttributes {
            trains_on_data: eff.trains_on_data,
            retains_data: eff.retains_data,
            retention_days: eff.retention_days,
            is_anonymous: p.is_anonymous,
            login_required: p.login_required,
            jurisdiction: p.jurisdiction.clone(),
            commitment_basis: match p.privacy_commitment_basis {
                Some(crate::persistence::provider_store::PrivacyCommitmentBasis::Contractual) => {
                    Some("contractual".to_owned())
                }
                Some(crate::persistence::provider_store::PrivacyCommitmentBasis::PolicyOnly) => {
                    Some("policy_only".to_owned())
                }
                None => None,
            },
            verification_status: match p.verification_status {
                provider_store::VerificationStatus::Verified => "verified",
                provider_store::VerificationStatus::Unverified => "unverified",
                provider_store::VerificationStatus::Stale => "stale",
            }
            .to_owned(),
            risk_rating: p.risk_rating,
            as_of: p
                .documentation_gate
                .get("verified_on")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| p.last_reviewed_at.clone()),
            model_overrides: gate_text(p, "model_overrides"),
            disclosures: gate_text(p, "disclosures"),
        },
        warnings: set.warnings.iter().map(warning_dto).collect(),
        overall_tier: set.overall,
        attributes_hash: hash,
        consent_state: consent_state.as_str().to_owned(),
        cost_class: match model {
            Some(m) => cost_class_of(pricing_text(&cat.pricing_texts, &m.model_id)),
            None => CostClass::Unknown,
        },
        can_cost_money: can_cost,
        cap_required,
        suggested_cap_micros: SUGGESTED_CAP_MICROS.to_string(),
        spend_limit_url: p.spend_limit_url.clone(),
        price_state: price_state.as_str().to_owned(),
        price_as_of,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn get_provider_disclosure(
    provider_id: String,
    model_id: Option<String>,
    sensitive_persona: bool,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<ProviderDisclosure, String> {
    let (user_id, key_hex_str) = session(&key_registry).await?;
    let cat = load_catalog(&pool, &provider_id).await?;
    let mut conn = store::open(&user_id, &key_hex_str)
        .await
        .map_err(|e| internal("open spend store", e))?;
    disclosure_impl(&mut conn, &cat, model_id.as_deref(), sensitive_persona).await
}

// ---------------------------------------------------------------------------
// record_provider_consent
// ---------------------------------------------------------------------------

pub(crate) async fn consent_impl(
    conn: &mut SqliteConnection,
    cat: &Catalog,
    req: &RecordConsentRequest,
    now: DateTime<Utc>,
) -> Result<ConsentResult, String> {
    match req.action {
        ConsentAction::Revoke => {
            store::revoke_consent_conn(conn, &cat.provider.id, now)
                .await
                .map_err(|e| internal("revoke", e))?;
            Ok(ConsentResult {
                consent_state: Code::ConsentStateRevoked.as_str().to_owned(),
                cap_limit_id: None,
            })
        }
        ConsentAction::Consent => {
            // The consent quotes back what the screen showed. A hash that is
            // not the current one means the attributes changed underneath it.
            let current = attributes_hash(&cat.provider, &cat.models, &cat.pricing_texts);
            let shown = req
                .attributes_hash
                .as_deref()
                .ok_or_else(|| "attributes_hash_required".to_owned())?;
            if shown != current {
                return Err("attributes_changed".to_owned());
            }

            let can_cost = provider_can_cost_money(&cat.models, &cat.pricing_texts);
            let mut no_cap_confirmed = false;
            let mut cap_limit_id = None;

            store::begin_immediate(conn)
                .await
                .map_err(|e| internal("begin", e))?;
            let outcome: Result<(), String> = async {
                if can_cost {
                    let existing =
                        store::applicable_limits_conn(conn, &cat.provider.id, None, None)
                            .await
                            .map_err(|e| internal("limit lookup", e))?;
                    let has_cap = existing.iter().any(|l| {
                        l.spec.metric == LimitMetric::CostUsd
                            && matches!(
                                l.spec.period,
                                LimitPeriod::Day | LimitPeriod::Week | LimitPeriod::Month
                            )
                    });
                    match &req.cap {
                        Some(CapChoice::Set { micros }) => {
                            let value = parse_amount(micros)?;
                            let row = store::upsert_limit_conn(
                                conn,
                                &LimitSpec {
                                    metric: LimitMetric::CostUsd,
                                    period: LimitPeriod::Month,
                                    scope_kind: ScopeKind::All,
                                    scope_id: None,
                                    limit_value: value,
                                    warn_at_pct: 80,
                                    approves_unattended: false,
                                },
                                now,
                            )
                            .await
                            .map_err(|e| format!("invalid_limit:{e}"))?;
                            cap_limit_id = Some(row.id);
                        }
                        Some(CapChoice::NoCap { confirmed: true }) => no_cap_confirmed = true,
                        Some(CapChoice::NoCap { confirmed: false }) | None => {
                            if !has_cap {
                                return Err(Code::BlockCapRequired.as_str().to_owned());
                            }
                        }
                    }
                }
                store::insert_consent_conn(conn, &cat.provider.id, &current, no_cap_confirmed, now)
                    .await
                    .map_err(|e| internal("record consent", e))?;
                Ok(())
            }
            .await;

            match outcome {
                Ok(()) => {
                    store::commit(conn)
                        .await
                        .map_err(|e| internal("commit", e))?;
                    Ok(ConsentResult {
                        consent_state: Code::ConsentStateCurrent.as_str().to_owned(),
                        cap_limit_id,
                    })
                }
                Err(e) => {
                    store::rollback(conn).await;
                    Err(e)
                }
            }
        }
    }
}

#[tauri::command]
#[specta::specta]
pub async fn record_provider_consent(
    request: RecordConsentRequest,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<ConsentResult, String> {
    let (user_id, key_hex_str) = session(&key_registry).await?;
    let cat = load_catalog(&pool, &request.provider_id).await?;
    let mut conn = store::open(&user_id, &key_hex_str)
        .await
        .map_err(|e| internal("open spend store", e))?;
    consent_impl(&mut conn, &cat, &request, Utc::now()).await
}

// ---------------------------------------------------------------------------
// set_limit / list_limits
// ---------------------------------------------------------------------------

pub(crate) async fn set_limit_impl(
    conn: &mut SqliteConnection,
    req: &SetLimitRequest,
    now: DateTime<Utc>,
) -> Result<Option<LimitDto>, String> {
    let scope_id = req.scope_id.as_deref().filter(|s| !s.trim().is_empty());
    if req.remove {
        store::remove_limit_conn(conn, req.metric, req.period, req.scope_kind, scope_id)
            .await
            .map_err(|e| internal("remove limit", e))?;
        return Ok(None);
    }
    let value = parse_amount(&req.limit_value)?;
    let row = store::upsert_limit_conn(
        conn,
        &LimitSpec {
            metric: req.metric,
            period: req.period,
            scope_kind: req.scope_kind,
            scope_id: scope_id.map(str::to_owned),
            limit_value: value,
            warn_at_pct: req.warn_at_pct,
            approves_unattended: req.approves_unattended,
        },
        now,
    )
    .await
    .map_err(|e| format!("invalid_limit:{e}"))?;
    Ok(Some(limit_dto(&row)))
}

#[tauri::command]
#[specta::specta]
pub async fn set_limit(
    request: SetLimitRequest,
    key_registry: State<'_, KeyRegistry>,
) -> Result<Option<LimitDto>, String> {
    let (user_id, key_hex_str) = session(&key_registry).await?;
    let mut conn = store::open(&user_id, &key_hex_str)
        .await
        .map_err(|e| internal("open spend store", e))?;
    set_limit_impl(&mut conn, &request, Utc::now()).await
}

#[tauri::command]
#[specta::specta]
pub async fn list_limits(key_registry: State<'_, KeyRegistry>) -> Result<Vec<LimitDto>, String> {
    let (user_id, key_hex_str) = session(&key_registry).await?;
    let mut conn = store::open(&user_id, &key_hex_str)
        .await
        .map_err(|e| internal("open spend store", e))?;
    let rows = store::list_limits_conn(&mut conn)
        .await
        .map_err(|e| internal("list limits", e))?;
    Ok(rows.iter().map(limit_dto).collect())
}

// ---------------------------------------------------------------------------
// get_usage_summary
// ---------------------------------------------------------------------------

fn free_tier_dto(
    provider: &Provider,
    observed: Option<store::RateSnapshot>,
) -> Option<FreeTierDto> {
    let ft = provider.free_tier.as_ref()?;
    if ft.get("available").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let lim = |k: &str| -> Option<u32> {
        ft.get("limits")
            .and_then(|l| l.get(k))
            .and_then(Value::as_u64)
            .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
    };
    Some(FreeTierDto {
        provider_id: provider.id.clone(),
        requests_per_minute: lim("rpm"),
        requests_per_day: lim("rpd"),
        tokens_per_minute: lim("tpm"),
        tokens_per_day: lim("tpd"),
        as_of: ft.get("as_of").and_then(Value::as_str).map(str::to_owned),
        observed: observed.map(|s| RateSnapshotDto {
            observed_at: s.observed_at,
            requests_limit: s.requests_limit.map(|v| v.to_string()),
            requests_remaining: s.requests_remaining.map(|v| v.to_string()),
            tokens_limit: s.tokens_limit.map(|v| v.to_string()),
            tokens_remaining: s.tokens_remaining.map(|v| v.to_string()),
            requests_reset: s.requests_reset,
            tokens_reset: s.tokens_reset,
        }),
    })
}

pub(crate) async fn usage_summary_impl<Tz: chrono::TimeZone>(
    conn: &mut SqliteConnection,
    providers: &[Provider],
    req: &UsageSummaryRequest,
    tz: &Tz,
    now: DateTime<Utc>,
) -> Result<UsageSummary, String> {
    if req.period == LimitPeriod::Run {
        return Err("invalid_period".to_owned());
    }
    let since = store::period_start(req.period, now, tz)
        .map(store::ledger_timestamp)
        .ok_or_else(|| "invalid_period".to_owned())?;

    let by_provider = store::usage_by_provider_conn(conn, Some(&since))
        .await
        .map_err(|e| internal("usage totals", e))?;
    let totals: Vec<ProviderUsageDto> = by_provider
        .into_iter()
        .filter(|(id, _)| req.provider_id.as_deref().is_none_or(|want| want == id))
        .map(|(id, t)| ProviderUsageDto {
            provider_id: id,
            cost_micros: t.cost_micros.to_string(),
            tokens_in: t.tokens_in.to_string(),
            tokens_out: t.tokens_out.to_string(),
            requests: t.requests.to_string(),
        })
        .collect();

    let mut limits = Vec::new();
    for row in store::list_limits_conn(conn)
        .await
        .map_err(|e| internal("list limits", e))?
    {
        if row.spec.period == LimitPeriod::Run {
            continue;
        }
        if let Some(want) = req.provider_id.as_deref() {
            if row.spec.scope_kind == ScopeKind::Provider
                && row.spec.scope_id.as_deref() != Some(want)
            {
                continue;
            }
        }
        let since_ts = store::period_start(row.spec.period, now, tz).map(store::ledger_timestamp);
        let mut filter = UsageFilter {
            since: since_ts.as_deref(),
            ..Default::default()
        };
        match row.spec.scope_kind {
            ScopeKind::All => {}
            ScopeKind::Provider => filter.provider_id = row.spec.scope_id.as_deref(),
            ScopeKind::Persona => filter.persona_id = row.spec.scope_id.as_deref(),
            ScopeKind::Focus => filter.focus_id = row.spec.scope_id.as_deref(),
        }
        let totals = store::usage_totals_conn(conn, &filter)
            .await
            .map_err(|e| internal("limit usage", e))?;
        let spent = match row.spec.metric {
            LimitMetric::CostUsd => totals.cost_micros,
            LimitMetric::TokensIn => totals.tokens_in,
            LimitMetric::TokensOut => totals.tokens_out,
            LimitMetric::Requests => totals.requests,
        };
        let limit = row.spec.limit_value;
        let spent_u = u128::try_from(spent).unwrap_or(0);
        let limit_u = u128::try_from(limit).unwrap_or(1).max(1);
        let pct = u32::try_from((spent_u * 100 / limit_u).min(100_000)).unwrap_or(100_000);
        let state = if spent >= limit {
            Code::LimitStateReached
        } else if spent_u * 100 >= limit_u * u128::from(row.spec.warn_at_pct) {
            Code::LimitStateWarn
        } else {
            Code::LimitStateOk
        };
        limits.push(LimitStatusDto {
            limit: limit_dto(&row),
            spent: spent.to_string(),
            pct,
            state: state.as_str().to_owned(),
        });
    }

    let mut free_tiers = Vec::new();
    for p in providers {
        if req.provider_id.as_deref().is_some_and(|want| want != p.id) {
            continue;
        }
        if p.api_dialect.is_none() {
            continue;
        }
        let snap = store::get_rate_snapshot_conn(conn, &p.id)
            .await
            .map_err(|e| internal("rate snapshot", e))?;
        if let Some(dto) = free_tier_dto(p, snap) {
            free_tiers.push(dto);
        }
    }

    Ok(UsageSummary {
        period: req.period,
        since,
        settlement_may_exceed_limits: true,
        cost_basis: Code::CostBasisListPriceEstimate.as_str().to_owned(),
        totals,
        limits,
        free_tiers,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn get_usage_summary(
    request: UsageSummaryRequest,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<UsageSummary, String> {
    let (user_id, key_hex_str) = session(&key_registry).await?;
    let providers = provider_store::list_all_providers(&pool)
        .await
        .map_err(|e| internal("provider list", e))?;
    let mut conn = store::open(&user_id, &key_hex_str)
        .await
        .map_err(|e| internal("open spend store", e))?;
    usage_summary_impl(&mut conn, &providers, &request, &chrono::Local, Utc::now()).await
}

// ---------------------------------------------------------------------------
// estimate_cost
// ---------------------------------------------------------------------------

/// The output token limit a model row carries in its request options.
fn row_output_limit(model: &ProviderModel) -> Option<u64> {
    let opts = model.request_options.as_ref()?.as_object()?;
    ["max_completion_tokens", "max_tokens"]
        .iter()
        .find_map(|k| opts.get(*k).and_then(Value::as_u64))
        .filter(|v| *v > 0)
}

pub(crate) async fn estimate_impl(
    conn: &mut SqliteConnection,
    cat: &Catalog,
    req: &EstimateCostRequest,
    today: chrono::NaiveDate,
) -> Result<CostEstimate, String> {
    // A reviewed row, else a model from the user's live list (items.id=711).
    let wanted = req
        .model_id
        .as_deref()
        .or_else(|| {
            cat.models
                .iter()
                .find(|m| m.is_default)
                .map(|m| m.model_id.as_str())
        })
        .ok_or_else(|| "unknown_model".to_owned())?;
    let live_synth: Option<ProviderModel>;
    let (model, curated) = match cat.models.iter().find(|m| m.model_id == wanted) {
        Some(m) => (m, true),
        None => {
            let row = get_live_model_conn(conn, &cat.provider.id, wanted)
                .await
                .map_err(|e| internal("live model", e))?
                .ok_or_else(|| "unknown_model".to_owned())?;
            let default = cat.models.iter().find(|m| m.is_default);
            live_synth = Some(synthesize_live_model(default, &row));
            (live_synth.as_ref().expect("just set"), false)
        }
    };
    let output_limit = req
        .max_output_tokens
        .filter(|n| *n > 0)
        .map(u64::from)
        .or_else(|| row_output_limit(model))
        .ok_or_else(|| "no_output_limit".to_owned())?;

    let override_raw = store::get_price_override_raw_conn(conn, &cat.provider.id, &model.model_id)
        .await
        .map_err(|e| internal("price override", e))?;
    let quote = resolve_price(
        override_raw.as_deref(),
        pricing_text(&cat.pricing_texts, &model.model_id),
    );
    let calibration = store::get_calibration_conn(conn, &cat.provider.id, &model.model_id)
        .await
        .map_err(|e| internal("calibration", e))?;
    let applied = applied_from(calibration.as_ref());
    let token_factor = permille_ceil(model.token_factor).unwrap_or(NEUTRAL_PERMILLE);
    let overhead = permille_ceil(model.output_overhead).unwrap_or(NEUTRAL_PERMILLE);

    // The worst case (and the reported input size) use the cap-safe input
    // factor, never below 1.0; the informational expected figure uses the
    // factor as calibrated.
    let input_bytes = u64::from(req.input_bytes);
    let input_tokens =
        input_tokens_estimate(input_bytes, token_factor, applied.cap_input_permille());
    let expected_input_tokens =
        input_tokens_estimate(input_bytes, token_factor, applied.input_permille);

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
    let origin = if curated { "curated" } else { "uncurated" }.to_owned();
    let calibration_view = calibration_dto(&applied);

    match quote {
        PriceQuote::Unknown => Ok(CostEstimate {
            price_state: Code::PriceStateUnknown.as_str().to_owned(),
            price_source: None,
            price_as_of: None,
            price_age_days: None,
            cost_basis: Code::CostBasisListPriceEstimate.as_str().to_owned(),
            input_tokens: input_tokens.to_string(),
            output_limit_tokens: output_limit.to_string(),
            input_micros: None,
            worst_case_micros: None,
            expected_micros: None,
            model_id: model.model_id.clone(),
            origin,
            context,
            calibration: calibration_view,
        }),
        PriceQuote::Known(price) => {
            let est = worst_case(&price, input_tokens, output_limit);
            let expected = req.expected_output_tokens.map(|out| {
                expected_micros(
                    &price,
                    expected_input_tokens,
                    u64::from(out),
                    overhead,
                    applied.output_permille,
                )
            });
            let stale = price.is_stale(today);
            Ok(CostEstimate {
                price_state: if stale {
                    Code::PriceStateStale
                } else {
                    Code::PriceStateKnown
                }
                .as_str()
                .to_owned(),
                price_source: Some(price.source.as_str().to_owned()),
                price_as_of: price.as_of.map(|d| d.to_string()),
                price_age_days: price.age_days(today).and_then(|d| i32::try_from(d).ok()),
                cost_basis: Code::CostBasisListPriceEstimate.as_str().to_owned(),
                input_tokens: input_tokens.to_string(),
                output_limit_tokens: output_limit.to_string(),
                input_micros: Some(est.input_micros.to_string()),
                worst_case_micros: Some(est.worst_case_micros.to_string()),
                expected_micros: expected.map(|v| v.to_string()),
                model_id: model.model_id.clone(),
                origin,
                context,
                calibration: calibration_view,
            })
        }
    }
}

#[tauri::command]
#[specta::specta]
pub async fn estimate_cost(
    request: EstimateCostRequest,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<CostEstimate, String> {
    let (user_id, key_hex_str) = session(&key_registry).await?;
    let cat = load_catalog(&pool, &request.provider_id).await?;
    let mut request = request;
    if request.model_id.is_none() {
        // The model the next step would use: the stored selection, else the
        // default. A selection that cannot be honored is reported as its code,
        // never replaced by the default.
        let resolved = resolve_hosted_model(
            &pool,
            &user_id,
            &key_hex_str,
            request.persona_id.as_deref(),
            request.focus_id.as_deref(),
            &request.provider_id,
        )
        .await;
        request.model_id = match resolved {
            Ok(r) => Some(r.model.model_id),
            // The limit is the one thing an estimate can supply itself.
            Err(ModelError::OutputLimitRequired { model_id }) => Some(model_id),
            Err(e) => return Err(e.block().0.as_str().to_owned()),
        };
    }
    let mut conn = store::open(&user_id, &key_hex_str)
        .await
        .map_err(|e| internal("open spend store", e))?;
    estimate_impl(&mut conn, &cat, &request, today_local()).await
}

#[cfg(test)]
#[path = "provider_spend_tests.rs"]
mod tests;
