// src-tauri/src/recommendation.rs
//
// items.id=607 (decisions.id=852, decisions.id=854) and items.id=712
// (decisions.id=872, 873, 875, 878; PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md
// Part 8h): onboarding Step 1 recommendation engine. `recommend(profile,
// catalog)` is pure -- no DB, no IPC, no I/O -- so every hardware class is
// unit-testable on constructed profiles. commands::system::
// get_provider_recommendation loads rows, presence-only key facts and plan
// keys, reduces them with `hosted_candidate`, and wraps `recommend`.
//
// NO PROVIDER OR MODEL IDS in this module: the candidate set, cost class,
// posture and preferred order come from providers-table and provider_models
// fields only. Tests use neutral fixture ids for the same reason.
//
// NO USER COPY: the result carries ids, enums and stable warning codes
// (spend::codes, items.id=710 -- no second code set is defined here); the
// frontend maps them to i18n keys.
//
// A RECOMMENDATION IS NOT ROUTING (decisions.id=873, decisions.id=666): this
// engine never selects a provider for a run and implies no consent. It only
// proposes what onboarding shows.
//
// RULES (decisions.id=852 B/C, kept):
//   * Local candidate on a usable GPU whose VRAM covers min_vram: fit is
//     Comfortable (VRAM >= 1.5x the requirement) or Tight. Nothing else is
//     ever Comfortable or Tight for a local model.
//   * Every other local case is CPU (no GPU, Unusable, Unknown, integrated
//     with no dedicated VRAM, or the model does not fit the GPU): fit is
//     Slow, and the model is "fitting" only if ram_class >= min_ram_class.
//   * Hosted candidates are not hardware-bound: fit is always Comfortable
//     (a label, not a hardware-fit statement).
//   * weak_hardware = no usable GPU AND RamClass::Low. It lists hosted
//     before local; strong hardware lists local before hosted.
//
// HOSTED RULES (items.id=712, decisions.id=872 points 4-5, 875, 878):
//   * ELIGIBLE SET: an active, non-local cloud_inference_api row that is
//     callable (api_dialect and base_url_template set) with a non-retired
//     default model. `hosted_candidate` is the one place that decides it.
//   * RANKING is per user. Sort key, in order:
//       1. unverified last, overall (after every metered entry too)
//       2. cost class: Free, PlanIncluded, Metered (low to high by list
//          price), Unknown
//       3. posture: verified-clean before verified-not-clean (a soft
//          tie-break inside a cost class)
//       4. providers.qr_recommendation_rank (NULL last)
//       5. key already configured (presence only)
//       6. provider id (determinism only)
//     Rank is compared only within a kind. Fit is a label, not a sort key.
//   * RANKING COST CLASS IS NOT CAP ACCOUNTING. For ranking, Free = the
//     provider's free tier is available AND the default model is not
//     requires_paid; PlanIncluded = the user's plan at that provider is in
//     plan_inclusion; Metered = a readable price that is not free, ordered by
//     list price; Unknown = no readable price. Caps and spend accounting
//     (spend::disclosure::cost_class_of, items.id=710) count LIST PRICE:
//     Free there means a known price of exactly zero. So a model on a free
//     tier with a non-zero list price ranks Free here and counts Metered
//     there, and a zero-priced model with no free tier counts Free there and
//     ranks Metered (price 0, first among the metered) here. Ranking uses the
//     curated price, not a per-user price override, and the provider's
//     default model, not the user's selected model (items.id=711).
//   * RECOMMENDABLE = verified-clean posture AND providers.qr_recommended
//     (decisions.id=878: only clean free providers carry the flag). Clean =
//     verified, the effective (model-aware) training attribute false, and
//     risk_rating at most 1. Retention is disclosed as a warning, it does not
//     disqualify. Only a recommendable hosted entry can be `recommended` or an
//     alternative. Every other hosted entry, and any recommendable one past
//     the alternatives cap, is listed under `other_providers` with its
//     warning codes. Nothing is blocked.
//   * `tied` marks entries that share a rank: the same kind, cost class
//     (price too, for metered), posture group and qr_recommendation_rank.
//     Key-configured and id only order entries, they never create a rank
//     (decisions.id=853: not a ranking), so the UI shows no badge on a tied
//     entry.
//
// NOT HANDLED: an integrated GPU Usable by Ollama (unified memory) reports
// no vram_mb, so it takes the CPU path here until the macOS/Windows probe
// items (608/609) define how its shared memory should be sized.

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use serde::Serialize;
use serde_json::Value;
use specta::Type;

use crate::commands::provider_spend::{warning_dto, WarningDto};
use crate::hardware_probe::{HardwareProfile, OllamaUsability, RamClass};
use crate::persistence::provider_store::{
    ActivationStatus, ModelStatus, Provider, ProviderModel, VerificationStatus,
};
use crate::spend::disclosure::{
    compute_warnings, effective_attributes, ModelNotes, PriceView, WarningInput,
};
use crate::spend::price::{resolve_price, PriceQuote};

/// VRAM at or above this multiple of a model's requirement is Comfortable,
/// below it (but still sufficient) is Tight. Expressed as a fraction to stay
/// in integer arithmetic.
const COMFORT_NUM: u64 = 3;
const COMFORT_DEN: u64 = 2;

/// Maximum alternatives returned alongside the primary pick.
const MAX_ALTERNATIVES: usize = 4;
/// Minimum alternatives to reach by pulling in non-fitting local candidates.
const MIN_ALTERNATIVES: usize = 2;

/// The providers.provider_type of a hosted API row (the per-type slot
/// recorded in shared_016.sql).
pub const HOSTED_PROVIDER_TYPE: &str = "cloud_inference_api";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    LocalModel,
    HostedApi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    Comfortable,
    Tight,
    Slow,
}

/// The cost class used to RANK hosted candidates. Deliberately not the
/// spend-accounting `spend::disclosure::CostClass`: see the module header for
/// how the two differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RankCostClass {
    /// Free tier available and the default model is not requires_paid.
    Free,
    /// The user's plan at this provider includes API use.
    PlanIncluded,
    /// A readable price that is not free.
    Metered,
    /// No readable price.
    Unknown,
}

impl RankCostClass {
    fn order(self) -> u8 {
        match self {
            RankCostClass::Free => 0,
            RankCostClass::PlanIncluded => 1,
            RankCostClass::Metered => 2,
            RankCostClass::Unknown => 3,
        }
    }
}

/// A hosted provider's curated privacy posture, for ranking and for the
/// recommendable test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posture {
    /// Verified, does not train, risk rating 1 or lower.
    Clean,
    /// Verified but not clean, or verification stale.
    NotClean,
    /// Never verified by curation. Ranks last overall.
    Unverified,
}

/// What the engine needs to rank and label one hosted candidate, reduced from
/// provider and model rows by `hosted_candidate`.
#[derive(Debug, Clone, PartialEq)]
pub struct HostedFacts {
    pub cost_class: RankCostClass,
    /// List price (input plus output, micro-USD per million tokens, highest
    /// tier) for a Metered class; 0 otherwise. Orders metered entries low to
    /// high. Not a cap figure.
    pub metered_price: u64,
    pub posture: Posture,
    /// Clean posture AND the curator's qr_recommended flag.
    pub recommendable: bool,
    /// items.id=710 warning codes for the provider and its default model.
    pub warnings: Vec<WarningDto>,
    /// Names of the per-user variables the base URL needs (for example an
    /// account id), so the UI can prompt for them.
    pub user_variables: Vec<String>,
}

/// One catalog entry, already reduced from a providers row.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub provider_id: String,
    pub kind: CandidateKind,
    /// providers.qr_recommendation_rank: lower = more preferred, None = no
    /// stated order (sorts last within its kind).
    pub rank: Option<u32>,
    /// Local candidates only.
    pub min_ram_class: Option<RamClass>,
    /// Local candidates only, decimal megabytes (curated min_vram_gb * 1000).
    pub min_vram_mb: Option<u32>,
    /// Local: weights already pulled. Hosted: always false.
    pub installed: bool,
    /// Hosted: an active API key already exists (presence only). Local:
    /// ignored.
    pub has_key: bool,
    /// Hosted candidates only. A hosted candidate with no facts ranks as
    /// unverified, unknown cost and not recommendable.
    pub hosted: Option<HostedFacts>,
}

/// Per-user facts the IPC layer loads for ranking. Presence and plan names
/// only, never key material.
#[derive(Debug, Clone, Default)]
pub struct UserContext {
    /// Provider ids that have an active API key.
    pub configured_provider_ids: BTreeSet<String>,
    /// provider id -> the user's plan_key at that provider
    /// (user_provider_preference.plan_key, account-wide row).
    pub plan_keys: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Type)]
pub struct Recommendation {
    pub provider_id: String,
    pub kind: CandidateKind,
    pub fit: Fit,
    pub needs_install: bool,
    pub needs_api_key: bool,
    pub installed: bool,
    /// Shares a rank with another entry of the same kind: the UI shows no
    /// badge on it (decisions.id=853).
    pub tied: bool,
    /// Hosted entries only; None for a local model.
    pub cost_class: Option<RankCostClass>,
    /// items.id=710 warning codes (hosted entries; empty for a local model).
    pub warnings: Vec<WarningDto>,
    /// Hosted entries only: per-user URL variables the provider needs.
    pub user_variables: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Type)]
pub struct ProviderRecommendation {
    /// None only when nothing is recommendable: an empty catalog, or only
    /// hosted entries that are not recommendable.
    pub recommended: Option<Recommendation>,
    /// 0 to 4 recommendable entries; fewer than 2 only when the catalog has
    /// fewer than 3 candidates. Never an error.
    pub alternatives: Vec<Recommendation>,
    /// Every hosted entry that is not the recommendation or an alternative:
    /// all non-recommendable ones plus any recommendable one past the
    /// alternatives cap, in rank order, each with its warning codes.
    pub other_providers: Vec<Recommendation>,
    pub weak_hardware: bool,
}

fn ram_order(c: RamClass) -> u8 {
    match c {
        RamClass::Low => 0,
        RamClass::Medium => 1,
        RamClass::High => 2,
        RamClass::VeryHigh => 3,
    }
}

fn has_usable_gpu(profile: &HardwareProfile) -> bool {
    profile
        .primary_gpu
        .as_ref()
        .is_some_and(|g| g.usable_by_ollama == OllamaUsability::Usable)
}

/// Fit of a local candidate, or None when it fits neither the GPU nor RAM.
fn local_fit(profile: &HardwareProfile, c: &Candidate) -> Option<Fit> {
    let gpu_vram = profile
        .primary_gpu
        .as_ref()
        .filter(|g| g.usable_by_ollama == OllamaUsability::Usable)
        .and_then(|g| g.vram_mb);
    if let (Some(vram), Some(need)) = (gpu_vram, c.min_vram_mb) {
        if vram >= need {
            let comfortable = u64::from(vram) * COMFORT_DEN >= u64::from(need) * COMFORT_NUM;
            return Some(if comfortable {
                Fit::Comfortable
            } else {
                Fit::Tight
            });
        }
    }
    match c.min_ram_class {
        Some(min) if ram_order(profile.ram_class) >= ram_order(min) => Some(Fit::Slow),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Rows -> hosted candidate
// ---------------------------------------------------------------------------

fn free_tier_available(provider: &Provider) -> bool {
    provider
        .free_tier
        .as_ref()
        .and_then(|v| v.get("available"))
        .and_then(Value::as_bool)
        == Some(true)
}

fn plan_included(provider: &Provider, plan_key: Option<&str>) -> bool {
    let Some(key) = plan_key else {
        return false;
    };
    provider
        .plan_inclusion
        .as_ref()
        .and_then(Value::as_array)
        .is_some_and(|plans| plans.iter().any(|p| p.as_str() == Some(key)))
}

/// Ranking cost class and, for Metered, the list-price figure that orders it.
fn rank_cost_class(
    provider: &Provider,
    model: &ProviderModel,
    quote: &PriceQuote,
    plan_key: Option<&str>,
) -> (RankCostClass, u64) {
    if free_tier_available(provider) && !model.requires_paid {
        return (RankCostClass::Free, 0);
    }
    if plan_included(provider, plan_key) {
        return (RankCostClass::PlanIncluded, 0);
    }
    match quote {
        PriceQuote::Known(price) => {
            let (input, output) = price.max_rates();
            (RankCostClass::Metered, input.saturating_add(output))
        }
        PriceQuote::Unknown => (RankCostClass::Unknown, 0),
    }
}

fn posture_of(provider: &Provider, model: &ProviderModel) -> Posture {
    match provider.verification_status {
        VerificationStatus::Unverified => Posture::Unverified,
        VerificationStatus::Stale => Posture::NotClean,
        VerificationStatus::Verified => {
            let eff = effective_attributes(provider, Some(model));
            if !eff.trains_on_data && provider.risk_rating <= 1 {
                Posture::Clean
            } else {
                Posture::NotClean
            }
        }
    }
}

fn user_variable_names(provider: &Provider) -> Vec<String> {
    let Some(Value::Array(items)) = provider.user_variables.as_ref() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| match v.as_str() {
            Some(name) => Some(name.to_owned()),
            None => {
                log::warn!(
                    "provider '{}' user_variables has a non-string entry, dropped",
                    provider.id
                );
                None
            }
        })
        .collect()
}

/// Reduce a hosted providers row (with its default model and that model's raw
/// `pricing` text) to an engine candidate, or None when the row is not in the
/// eligible set. Pure: `ctx` and `today` are supplied by the caller.
pub fn hosted_candidate(
    provider: &Provider,
    default_model: Option<&ProviderModel>,
    pricing_text: Option<&str>,
    ctx: &UserContext,
    today: NaiveDate,
) -> Option<Candidate> {
    if provider.is_local
        || provider.provider_type != HOSTED_PROVIDER_TYPE
        || provider.activation_status != ActivationStatus::Active
        || provider.api_dialect.is_none()
        || provider.base_url_template.is_none()
    {
        return None;
    }
    let model = default_model.filter(|m| m.status != ModelStatus::Retired)?;

    let quote = resolve_price(None, pricing_text);
    let plan_key = ctx.plan_keys.get(&provider.id).map(String::as_str);
    let (cost_class, metered_price) = rank_cost_class(provider, model, &quote, plan_key);
    let posture = posture_of(provider, model);

    let price_as_of = match &quote {
        PriceQuote::Known(p) => p.as_of.map(|d| d.to_string()),
        PriceQuote::Unknown => None,
    };
    let set = compute_warnings(&WarningInput {
        provider,
        model: Some(model),
        model_notes: ModelNotes::default(),
        price: PriceView::from_quote(&quote, today),
        price_as_of,
        // A recommendation is shown before any content exists.
        sensitive_persona: false,
    });

    Some(Candidate {
        provider_id: provider.id.clone(),
        kind: CandidateKind::HostedApi,
        rank: provider.qr_recommendation_rank,
        min_ram_class: None,
        min_vram_mb: None,
        installed: false,
        has_key: ctx.configured_provider_ids.contains(&provider.id),
        hosted: Some(HostedFacts {
            cost_class,
            metered_price,
            posture,
            recommendable: posture == Posture::Clean && provider.qr_recommended,
            warnings: set.warnings.iter().map(warning_dto).collect(),
            user_variables: user_variable_names(provider),
        }),
    })
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

/// What makes two hosted entries share a rank (everything above key-configured
/// and id in the module header's order).
type HostedRankKey = (bool, u8, u64, u8, bool, u32);
/// What makes two local entries share a rank.
type LocalRankKey = (bool, u32);

fn hosted_rank_key(c: &Candidate) -> HostedRankKey {
    let (unverified, class, price, clean_group) = match &c.hosted {
        Some(h) => (
            h.posture == Posture::Unverified,
            h.cost_class.order(),
            h.metered_price,
            u8::from(h.posture != Posture::Clean),
        ),
        None => (true, RankCostClass::Unknown.order(), 0, 1),
    };
    (
        unverified,
        class,
        price,
        clean_group,
        c.rank.is_none(),
        c.rank.unwrap_or(0),
    )
}

fn local_rank_key(c: &Candidate) -> LocalRankKey {
    (c.rank.is_none(), c.rank.unwrap_or(0))
}

fn cmp_hosted(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    // !has_key: key-already-configured first. provider_id is determinism only.
    (hosted_rank_key(a), !a.has_key, &a.provider_id).cmp(&(
        hosted_rank_key(b),
        !b.has_key,
        &b.provider_id,
    ))
}

fn cmp_local(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    (local_rank_key(a), &a.provider_id).cmp(&(local_rank_key(b), &b.provider_id))
}

/// Ids of the entries that share a rank with another entry in `group`.
fn tied_ids<K: Ord>(group: &[&Candidate], key: impl Fn(&Candidate) -> K) -> BTreeSet<String> {
    let mut counts: BTreeMap<K, usize> = BTreeMap::new();
    for c in group {
        *counts.entry(key(c)).or_default() += 1;
    }
    group
        .iter()
        .filter(|c| counts.get(&key(c)).copied().unwrap_or(0) > 1)
        .map(|c| c.provider_id.clone())
        .collect()
}

fn to_recommendation(c: &Candidate, fit: Fit, tied: &BTreeSet<String>) -> Recommendation {
    let local = c.kind == CandidateKind::LocalModel;
    Recommendation {
        provider_id: c.provider_id.clone(),
        kind: c.kind,
        fit,
        needs_install: local && !c.installed,
        needs_api_key: !local && !c.has_key,
        installed: local && c.installed,
        tied: tied.contains(&c.provider_id),
        cost_class: c.hosted.as_ref().map(|h| h.cost_class),
        warnings: c
            .hosted
            .as_ref()
            .map(|h| h.warnings.clone())
            .unwrap_or_default(),
        user_variables: c
            .hosted
            .as_ref()
            .map(|h| h.user_variables.clone())
            .unwrap_or_default(),
    }
}

fn is_recommendable_hosted(c: &Candidate) -> bool {
    c.hosted.as_ref().is_some_and(|h| h.recommendable)
}

pub fn recommend(profile: &HardwareProfile, catalog: &[Candidate]) -> ProviderRecommendation {
    let weak_hardware = !has_usable_gpu(profile) && profile.ram_class == RamClass::Low;

    let mut local_fit_list: Vec<(&Candidate, Fit)> = Vec::new();
    let mut local_unfit: Vec<&Candidate> = Vec::new();
    let mut hosted: Vec<&Candidate> = Vec::new();
    for c in catalog {
        match c.kind {
            CandidateKind::LocalModel => match local_fit(profile, c) {
                Some(fit) => local_fit_list.push((c, fit)),
                None => local_unfit.push(c),
            },
            CandidateKind::HostedApi => hosted.push(c),
        }
    }
    local_fit_list.sort_by(|(a, _), (b, _)| cmp_local(a, b));
    local_unfit.sort_by(|a, b| cmp_local(a, b));
    hosted.sort_by(|a, b| cmp_hosted(a, b));

    let all_locals: Vec<&Candidate> = catalog
        .iter()
        .filter(|c| c.kind == CandidateKind::LocalModel)
        .collect();
    let mut tied = tied_ids(&all_locals, local_rank_key);
    tied.extend(tied_ids(&hosted, hosted_rank_key));

    // Only a recommendable hosted entry can lead or be an alternative; the
    // rest are other providers (decisions.id=872 point 4).
    let (recommendable, mut others): (Vec<&Candidate>, Vec<&Candidate>) = hosted
        .iter()
        .copied()
        .partition(|c| is_recommendable_hosted(c));

    // Cross-kind order: strong hardware lists local before hosted, weak
    // hardware lists hosted before local.
    let hosted_entries = recommendable.iter().map(|c| (*c, Fit::Comfortable));
    let mut ordered: Vec<(&Candidate, Fit)> = if weak_hardware {
        hosted_entries.chain(local_fit_list).collect()
    } else {
        local_fit_list.into_iter().chain(hosted_entries).collect()
    };

    // Non-fitting local candidates are used only to reach the minimum, and
    // as a last-resort primary; they are flagged Slow.
    let mut spill = local_unfit.iter().map(|c| (*c, Fit::Slow));

    if ordered.is_empty() {
        if let Some(first) = spill.next() {
            ordered.push(first);
        }
    }
    let other_providers = |others: &mut Vec<&Candidate>| -> Vec<Recommendation> {
        others.sort_by(|a, b| cmp_hosted(a, b));
        others
            .iter()
            .map(|c| to_recommendation(c, Fit::Comfortable, &tied))
            .collect()
    };
    if ordered.is_empty() {
        return ProviderRecommendation {
            recommended: None,
            alternatives: Vec::new(),
            other_providers: other_providers(&mut others),
            weak_hardware,
        };
    }
    let (first, first_fit) = ordered.remove(0);
    let mut rest = ordered;
    while rest.len() < MIN_ALTERNATIVES {
        match spill.next() {
            Some(r) => rest.push(r),
            None => break,
        }
    }
    // A recommendable hosted entry cut by the cap is still an other provider.
    if rest.len() > MAX_ALTERNATIVES {
        others.extend(
            rest.split_off(MAX_ALTERNATIVES)
                .into_iter()
                .map(|(c, _)| c)
                .filter(|c| c.kind == CandidateKind::HostedApi),
        );
    }

    ProviderRecommendation {
        recommended: Some(to_recommendation(first, first_fit, &tied)),
        alternatives: rest
            .iter()
            .map(|(c, fit)| to_recommendation(c, *fit, &tied))
            .collect(),
        other_providers: other_providers(&mut others),
        weak_hardware,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware_probe::{GpuInfo, GpuVendor, PROFILE_VERSION};
    use crate::spend::codes::Code;
    use crate::spend::fixtures;
    use serde_json::json;

    fn gpu(usable: OllamaUsability, vram_mb: Option<u32>, integrated: bool) -> GpuInfo {
        GpuInfo {
            vendor: GpuVendor::Other,
            name: None,
            architecture: None,
            is_integrated: integrated,
            vram_mb,
            usable_by_ollama: usable,
        }
    }

    fn profile(ram_mb: u32, gpu: Option<GpuInfo>) -> HardwareProfile {
        HardwareProfile {
            profile_version: PROFILE_VERSION,
            ram_mb,
            ram_class: RamClass::from_mb(ram_mb),
            cpu_cores: 8,
            gpus: gpu.iter().cloned().collect(),
            primary_gpu: gpu,
        }
    }

    fn local(id: &str, rank: Option<u32>, ram: RamClass, vram_mb: u32) -> Candidate {
        Candidate {
            provider_id: id.into(),
            kind: CandidateKind::LocalModel,
            rank,
            min_ram_class: Some(ram),
            min_vram_mb: Some(vram_mb),
            installed: false,
            has_key: false,
            hosted: None,
        }
    }

    /// A free, clean, recommendable hosted candidate.
    fn hosted(id: &str, rank: Option<u32>, has_key: bool) -> Candidate {
        Candidate {
            provider_id: id.into(),
            kind: CandidateKind::HostedApi,
            rank,
            min_ram_class: None,
            min_vram_mb: None,
            installed: false,
            has_key,
            hosted: Some(HostedFacts {
                cost_class: RankCostClass::Free,
                metered_price: 0,
                posture: Posture::Clean,
                recommendable: true,
                warnings: Vec::new(),
                user_variables: Vec::new(),
            }),
        }
    }

    fn tweak(mut c: Candidate, f: impl FnOnce(&mut HostedFacts)) -> Candidate {
        f(c.hosted.as_mut().expect("hosted candidate"));
        c
    }

    fn metered(c: Candidate, price: u64) -> Candidate {
        tweak(c, |h| {
            h.cost_class = RankCostClass::Metered;
            h.metered_price = price;
        })
    }

    fn not_clean(c: Candidate) -> Candidate {
        tweak(c, |h| {
            h.posture = Posture::NotClean;
            h.recommendable = false;
        })
    }

    fn unverified(c: Candidate) -> Candidate {
        tweak(c, |h| {
            h.posture = Posture::Unverified;
            h.recommendable = false;
        })
    }

    /// Three local candidates (large, mid, small) plus two hosted, neutral ids.
    fn catalog() -> Vec<Candidate> {
        vec![
            local("local-a", Some(1), RamClass::Medium, 6_000),
            local("local-b", Some(2), RamClass::Medium, 6_000),
            local("local-c", Some(3), RamClass::Low, 3_000),
            hosted("hosted-a", Some(1), false),
            hosted("hosted-b", Some(2), false),
        ]
    }

    fn ids(r: &ProviderRecommendation) -> Vec<&str> {
        r.recommended
            .iter()
            .chain(r.alternatives.iter())
            .map(|x| x.provider_id.as_str())
            .collect()
    }

    fn other_ids(r: &ProviderRecommendation) -> Vec<&str> {
        r.other_providers
            .iter()
            .map(|x| x.provider_id.as_str())
            .collect()
    }

    // -- hardware behaviour kept from items.id=607 (decisions.id=852) --------

    #[test]
    fn cpu_only_30gib_is_local_slow() {
        // This dev machine: MemTotal 31,988,204 kB = ~32,756 decimal MB
        // (VeryHigh), no GPU Ollama can use.
        let p = profile(
            32_756,
            Some(gpu(OllamaUsability::Unusable, Some(8_573), false)),
        );
        let r = recommend(&p, &catalog());
        assert!(!r.weak_hardware);
        let rec = r.recommended.unwrap();
        assert_eq!(rec.provider_id, "local-a");
        assert_eq!(rec.fit, Fit::Slow);
        assert!(rec.needs_install);
        assert!(!rec.needs_api_key);
        assert_eq!(r.alternatives.len(), 4);
        assert!(r.alternatives.iter().all(|a| a.fit != Fit::Tight));
    }

    #[test]
    fn low_ram_no_gpu_is_weak_and_hosted_primary() {
        let p = profile(7_900, None);
        let r = recommend(&p, &catalog());
        assert!(r.weak_hardware);
        let rec = r.recommended.clone().unwrap();
        assert_eq!(rec.kind, CandidateKind::HostedApi);
        assert!(rec.needs_api_key);
        // Hosted entries are labeled Comfortable (decisions.id=852).
        assert_eq!(rec.fit, Fit::Comfortable);
        // The small local model still fits low RAM and stays an alternative,
        // flagged Slow; the larger ones do not fit and are not listed.
        let local_alts: Vec<_> = r
            .alternatives
            .iter()
            .filter(|a| a.kind == CandidateKind::LocalModel)
            .collect();
        assert_eq!(local_alts.len(), 1);
        assert_eq!(local_alts[0].provider_id, "local-c");
        assert_eq!(local_alts[0].fit, Fit::Slow);
        // Weak hardware lists hosted before local.
        assert_eq!(ids(&r), vec!["hosted-a", "hosted-b", "local-c"]);
    }

    #[test]
    fn weak_hardware_without_hosted_uses_best_local_slow() {
        let p = profile(7_900, None);
        let cat: Vec<_> = catalog()
            .into_iter()
            .filter(|c| c.kind == CandidateKind::LocalModel)
            .collect();
        let r = recommend(&p, &cat);
        assert!(r.weak_hardware);
        let rec = r.recommended.unwrap();
        assert_eq!(rec.provider_id, "local-c");
        assert_eq!(rec.fit, Fit::Slow);
        // Non-fitting locals are pulled in to reach two alternatives.
        assert_eq!(r.alternatives.len(), 2);
        assert!(r.alternatives.iter().all(|a| a.fit == Fit::Slow));
    }

    #[test]
    fn low_ram_with_usable_gpu_is_not_weak() {
        let p = profile(
            7_900,
            Some(gpu(OllamaUsability::Usable, Some(12_000), false)),
        );
        let r = recommend(&p, &catalog());
        assert!(!r.weak_hardware);
        let rec = r.recommended.unwrap();
        assert_eq!(rec.provider_id, "local-a");
        assert_eq!(rec.fit, Fit::Comfortable);
    }

    #[test]
    fn usable_gpu_comfortable_vs_tight_split() {
        // 1.5x of 6,000 = 9,000.
        let comfy = profile(
            16_000,
            Some(gpu(OllamaUsability::Usable, Some(9_000), false)),
        );
        let tight = profile(
            16_000,
            Some(gpu(OllamaUsability::Usable, Some(8_999), false)),
        );
        let cat = vec![local("local-a", Some(1), RamClass::Medium, 6_000)];
        assert_eq!(
            recommend(&comfy, &cat).recommended.unwrap().fit,
            Fit::Comfortable
        );
        assert_eq!(recommend(&tight, &cat).recommended.unwrap().fit, Fit::Tight);
    }

    #[test]
    fn gpu_too_small_falls_back_to_ram_and_is_slow() {
        let p = profile(
            16_000,
            Some(gpu(OllamaUsability::Usable, Some(4_000), false)),
        );
        let cat = vec![local("local-a", Some(1), RamClass::Medium, 6_000)];
        let rec = recommend(&p, &cat).recommended.unwrap();
        assert_eq!(rec.fit, Fit::Slow);
    }

    #[test]
    fn unknown_and_unusable_gpus_are_cpu() {
        for usable in [OllamaUsability::Unknown, OllamaUsability::Unusable] {
            let p = profile(16_000, Some(gpu(usable, Some(24_000), false)));
            let rec = recommend(&p, &catalog()).recommended.unwrap();
            assert_eq!(rec.fit, Fit::Slow, "{usable:?}");
        }
    }

    #[test]
    fn usable_gpu_without_vram_figure_is_cpu() {
        let p = profile(16_000, Some(gpu(OllamaUsability::Usable, None, true)));
        let rec = recommend(&p, &catalog()).recommended.unwrap();
        assert_eq!(rec.fit, Fit::Slow);
    }

    #[test]
    fn order_is_rank_only_not_fit() {
        // local-x outranks local-y but is only Slow (needs more VRAM than
        // the GPU has), while local-y would be Comfortable. Rank wins.
        let p = profile(
            16_000,
            Some(gpu(OllamaUsability::Usable, Some(4_000), false)),
        );
        let cat = vec![
            local("local-y", Some(2), RamClass::Low, 2_000),
            local("local-x", Some(1), RamClass::Medium, 6_000),
        ];
        let r = recommend(&p, &cat);
        assert_eq!(r.recommended.as_ref().unwrap().provider_id, "local-x");
        assert_eq!(r.alternatives[0].provider_id, "local-y");
        assert_eq!(r.alternatives[0].fit, Fit::Comfortable);
    }

    #[test]
    fn null_rank_sorts_last_and_id_breaks_ties() {
        let p = profile(32_756, None);
        let cat = vec![
            local("local-z", None, RamClass::Low, 1_000),
            local("local-b", Some(5), RamClass::Low, 1_000),
            local("local-a", Some(5), RamClass::Low, 1_000),
        ];
        assert_eq!(
            ids(&recommend(&p, &cat)),
            vec!["local-a", "local-b", "local-z"]
        );
    }

    #[test]
    fn strong_hardware_lists_local_before_hosted() {
        let p = profile(32_756, None);
        let r = recommend(&p, &catalog());
        assert_eq!(
            ids(&r),
            vec!["local-a", "local-b", "local-c", "hosted-a", "hosted-b"]
        );
    }

    #[test]
    fn installed_local_reports_state() {
        let p = profile(32_756, None);
        let mut c = local("local-a", Some(1), RamClass::Low, 1_000);
        c.installed = true;
        let rec = recommend(&p, &[c]).recommended.unwrap();
        assert!(rec.installed);
        assert!(!rec.needs_install);
        assert!(rec.warnings.is_empty());
        assert!(rec.user_variables.is_empty());
        assert_eq!(rec.cost_class, None);
    }

    #[test]
    fn small_catalog_never_errors() {
        let p = profile(32_756, None);
        let empty = recommend(&p, &[]);
        assert!(empty.recommended.is_none());
        assert!(empty.alternatives.is_empty());
        assert!(empty.other_providers.is_empty());

        let one = recommend(&p, &[hosted("hosted-a", Some(1), false)]);
        assert_eq!(one.recommended.unwrap().provider_id, "hosted-a");
        assert!(one.alternatives.is_empty());

        let two = recommend(
            &p,
            &[
                hosted("hosted-a", Some(1), false),
                hosted("hosted-b", Some(2), false),
            ],
        );
        assert_eq!(two.alternatives.len(), 1);
    }

    #[test]
    fn nothing_fits_still_returns_a_primary() {
        let p = profile(7_900, None);
        let cat = vec![
            local("local-a", Some(1), RamClass::High, 9_000),
            local("local-b", Some(2), RamClass::High, 9_000),
            local("local-c", Some(3), RamClass::High, 9_000),
        ];
        let r = recommend(&p, &cat);
        assert_eq!(r.recommended.unwrap().fit, Fit::Slow);
        assert_eq!(r.alternatives.len(), 2);
    }

    // -- hosted ranking (items.id=712) ---------------------------------------

    #[test]
    fn hosted_order_is_cost_class_then_price_low_to_high() {
        let p = profile(7_900, None);
        let cat = vec![
            tweak(hosted("hosted-unknown", Some(1), false), |h| {
                h.cost_class = RankCostClass::Unknown;
            }),
            metered(hosted("hosted-dear", Some(1), false), 9_000_000),
            metered(hosted("hosted-cheap", Some(9), false), 1_000_000),
            tweak(hosted("hosted-plan", Some(9), false), |h| {
                h.cost_class = RankCostClass::PlanIncluded;
            }),
            hosted("hosted-free", Some(9), false),
        ];
        let r = recommend(&p, &cat);
        // Rank 9 on the free and plan entries does not matter: class first.
        assert_eq!(
            ids(&r),
            vec![
                "hosted-free",
                "hosted-plan",
                "hosted-cheap",
                "hosted-dear",
                "hosted-unknown"
            ]
        );
        assert_eq!(r.recommended.unwrap().cost_class, Some(RankCostClass::Free));
    }

    #[test]
    fn posture_is_a_soft_tie_break_inside_a_cost_class_only() {
        let p = profile(7_900, None);
        // Not-clean free sits ahead of clean metered (class beats posture),
        // and behind clean free (posture breaks the tie inside the class).
        // The not-clean entry is an other provider, so look at that list.
        let cat = vec![
            not_clean(hosted("hosted-notclean-free", Some(1), false)),
            metered(hosted("hosted-clean-metered", Some(1), false), 1_000),
            hosted("hosted-clean-free", Some(5), false),
        ];
        let r = recommend(&p, &cat);
        assert_eq!(ids(&r), vec!["hosted-clean-free", "hosted-clean-metered"]);
        assert_eq!(other_ids(&r), vec!["hosted-notclean-free"]);

        // Same class, same rank: clean before not-clean in the full order.
        let mut cat2 = vec![
            not_clean(hosted("hosted-a", Some(1), true)),
            hosted("hosted-b", Some(1), false),
        ];
        cat2.push(not_clean(hosted("hosted-c", Some(1), false)));
        let r2 = recommend(&p, &cat2);
        assert_eq!(ids(&r2), vec!["hosted-b"]);
        assert_eq!(other_ids(&r2), vec!["hosted-a", "hosted-c"]);
    }

    #[test]
    fn unverified_sorts_last_overall_after_metered_and_unknown() {
        let p = profile(7_900, None);
        let cat = vec![
            unverified(hosted("hosted-unverified-free", Some(1), true)),
            tweak(hosted("hosted-unknown", Some(1), false), |h| {
                h.cost_class = RankCostClass::Unknown;
            }),
            metered(not_clean(hosted("hosted-metered", Some(1), false)), 5_000),
        ];
        let r = recommend(&p, &cat);
        assert!(r.recommended.is_some());
        assert_eq!(
            other_ids(&r),
            vec!["hosted-metered", "hosted-unverified-free"]
        );
        assert_eq!(r.recommended.unwrap().provider_id, "hosted-unknown");
    }

    #[test]
    fn rank_then_key_then_id_order_within_one_class_and_posture() {
        let p = profile(32_756, None);
        let cat = vec![
            hosted("hosted-a", Some(1), false),
            hosted("hosted-b", Some(1), true),
            hosted("hosted-c", Some(0), false),
            hosted("hosted-d", None, true),
        ];
        let r = recommend(&p, &cat);
        assert_eq!(
            ids(&r),
            vec!["hosted-c", "hosted-b", "hosted-a", "hosted-d"]
        );
        // Key-already-configured is reported through needs_api_key.
        assert!(!r.alternatives[0].needs_api_key);
        assert!(r.alternatives[1].needs_api_key);
    }

    #[test]
    fn only_a_recommendable_hosted_entry_can_lead_or_be_an_alternative() {
        let p = profile(7_900, None);
        // The best-ranked hosted entry is not recommendable (clean but the
        // curator did not flag it, or not clean): it must not be primary.
        let unflagged = tweak(hosted("hosted-unflagged", Some(1), true), |h| {
            h.recommendable = false;
        });
        let cat = vec![
            unflagged,
            not_clean(hosted("hosted-notclean", Some(1), true)),
            hosted("hosted-ok", Some(7), false),
            local("local-c", Some(3), RamClass::Low, 3_000),
        ];
        let r = recommend(&p, &cat);
        assert!(r.weak_hardware);
        assert_eq!(r.recommended.as_ref().unwrap().provider_id, "hosted-ok");
        assert_eq!(
            r.alternatives
                .iter()
                .map(|a| a.provider_id.as_str())
                .collect::<Vec<_>>(),
            vec!["local-c"]
        );
        // Clean-but-unflagged sorts ahead of not-clean inside the same class.
        assert_eq!(other_ids(&r), vec!["hosted-unflagged", "hosted-notclean"]);
    }

    #[test]
    fn weak_hardware_with_only_non_recommendable_hosted_recommends_local() {
        let p = profile(7_900, None);
        let cat = vec![
            not_clean(hosted("hosted-a", Some(1), false)),
            unverified(hosted("hosted-b", Some(2), false)),
            local("local-c", Some(3), RamClass::Low, 3_000),
        ];
        let r = recommend(&p, &cat);
        let rec = r.recommended.as_ref().unwrap();
        assert_eq!(rec.provider_id, "local-c");
        assert_eq!(rec.kind, CandidateKind::LocalModel);
        assert_eq!(other_ids(&r), vec!["hosted-a", "hosted-b"]);
    }

    #[test]
    fn only_non_recommendable_hosted_means_no_recommendation_not_an_error() {
        let p = profile(32_756, None);
        let cat = vec![
            not_clean(hosted("hosted-a", Some(1), false)),
            unverified(hosted("hosted-b", Some(2), false)),
        ];
        let r = recommend(&p, &cat);
        assert!(r.recommended.is_none());
        assert!(r.alternatives.is_empty());
        assert_eq!(other_ids(&r), vec!["hosted-a", "hosted-b"]);
    }

    #[test]
    fn alternatives_capped_at_four_and_overflow_is_an_other_provider() {
        let p = profile(32_756, None);
        let mut cat = catalog();
        cat.push(hosted("hosted-c", Some(3), false));
        let r = recommend(&p, &cat);
        // local-a primary; then local-b, local-c, hosted-a, hosted-b.
        assert_eq!(r.alternatives.len(), 4);
        assert_eq!(other_ids(&r), vec!["hosted-c"]);
        assert_eq!(r.other_providers[0].fit, Fit::Comfortable);
    }

    #[test]
    fn warnings_and_user_variables_ride_on_each_entry() {
        let p = profile(7_900, None);
        let warning = WarningDto {
            code: Code::WarnRetainsData.as_str().to_owned(),
            tier: Code::WarnRetainsData.tier(),
            params: Default::default(),
        };
        let cat = vec![
            tweak(hosted("hosted-a", Some(1), false), |h| {
                h.warnings = vec![warning.clone()];
                h.user_variables = vec!["fixture_var".to_owned()];
            }),
            not_clean(hosted("hosted-b", Some(2), false)),
        ];
        let r = recommend(&p, &cat);
        let rec = r.recommended.unwrap();
        assert_eq!(rec.warnings, vec![warning]);
        assert_eq!(rec.user_variables, vec!["fixture_var".to_owned()]);
        assert!(r.other_providers[0].warnings.is_empty());
    }

    // -- tied ------------------------------------------------------------------

    #[test]
    fn entries_sharing_a_rank_are_tied_and_a_distinct_rank_is_not() {
        let p = profile(7_900, None);
        let cat = vec![
            hosted("hosted-a", Some(1), false),
            hosted("hosted-b", Some(1), true),
            hosted("hosted-c", Some(2), false),
        ];
        let r = recommend(&p, &cat);
        let by_id = |id: &str| {
            r.recommended
                .iter()
                .chain(r.alternatives.iter())
                .chain(r.other_providers.iter())
                .find(|x| x.provider_id == id)
                .unwrap()
                .tied
        };
        assert!(by_id("hosted-a"));
        assert!(by_id("hosted-b"));
        assert!(!by_id("hosted-c"));
    }

    #[test]
    fn key_configured_and_id_never_create_a_tie_and_none_ranks_tie() {
        let p = profile(7_900, None);
        // Different rank: key-first ordering applies but nothing is tied.
        let r = recommend(
            &p,
            &[
                hosted("hosted-a", Some(1), false),
                hosted("hosted-b", Some(2), true),
            ],
        );
        assert!(!r.recommended.as_ref().unwrap().tied);
        assert!(r.alternatives.iter().all(|a| !a.tied));
        // Two entries with no stated rank share "no rank".
        let r = recommend(
            &p,
            &[
                hosted("hosted-a", None, false),
                hosted("hosted-b", None, true),
            ],
        );
        assert!(r.recommended.as_ref().unwrap().tied);
        assert!(r.alternatives[0].tied);
    }

    #[test]
    fn same_rank_in_a_different_class_or_posture_or_kind_is_not_tied() {
        let p = profile(7_900, None);
        let cat = vec![
            hosted("hosted-free", Some(1), false),
            metered(hosted("hosted-metered", Some(1), false), 100),
            metered(hosted("hosted-metered-dear", Some(1), false), 200),
            not_clean(hosted("hosted-notclean", Some(1), false)),
            local("local-a", Some(1), RamClass::Low, 1_000),
        ];
        let r = recommend(&p, &cat);
        assert!(r
            .recommended
            .iter()
            .chain(r.alternatives.iter())
            .chain(r.other_providers.iter())
            .all(|x| !x.tied));
    }

    #[test]
    fn equal_priced_metered_entries_with_equal_rank_are_tied() {
        let p = profile(7_900, None);
        let cat = vec![
            metered(hosted("hosted-a", Some(1), false), 700),
            metered(hosted("hosted-b", Some(1), false), 700),
        ];
        let r = recommend(&p, &cat);
        assert!(r.recommended.as_ref().unwrap().tied);
        assert!(r.alternatives[0].tied);
    }

    // -- rows -> candidate (hosted_candidate) ---------------------------------

    const PRICE_FREE: &str = r#"{"input_per_mtok":0,"output_per_mtok":0,"currency":"USD","as_of":"2026-10-11","source":"curated"}"#;
    const PRICE_CHEAP: &str = r#"{"input_per_mtok":0.5,"output_per_mtok":1.5,"currency":"USD","as_of":"2026-10-11","source":"curated"}"#;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 12).unwrap()
    }

    fn row(id: &str) -> Provider {
        let mut p = fixtures::provider();
        p.id = id.to_owned();
        p.base_url_template = Some("https://fixture.test/v1".to_owned());
        p.qr_recommended = true;
        p.qr_recommendation_rank = Some(3);
        p
    }

    fn default_model(provider_id: &str) -> ProviderModel {
        let mut m = fixtures::model("fixture-model-a");
        m.provider_id = provider_id.to_owned();
        m
    }

    fn reduce(p: &Provider, m: Option<&ProviderModel>, price: Option<&str>) -> Option<Candidate> {
        hosted_candidate(p, m, price, &UserContext::default(), today())
    }

    fn facts(c: &Candidate) -> &HostedFacts {
        c.hosted.as_ref().unwrap()
    }

    #[test]
    fn eligible_curated_row_reduces_to_a_candidate() {
        let p = row("fixture-host-a");
        let m = default_model(&p.id);
        let c = reduce(&p, Some(&m), Some(PRICE_CHEAP)).unwrap();
        assert_eq!(c.provider_id, "fixture-host-a");
        assert_eq!(c.kind, CandidateKind::HostedApi);
        assert_eq!(c.rank, Some(3));
        assert!(!c.has_key);
        assert_eq!(facts(&c).cost_class, RankCostClass::Metered);
        assert_eq!(facts(&c).posture, Posture::Clean);
        assert!(facts(&c).recommendable);
    }

    #[test]
    fn rows_outside_the_eligible_set_are_excluded() {
        let m = |p: &Provider| default_model(&p.id);

        let mut local_row = row("fixture-host-a");
        local_row.is_local = true;
        assert!(reduce(&local_row, Some(&m(&local_row)), None).is_none());

        let mut other_type = row("fixture-host-a");
        other_type.provider_type = "split_screen_web".to_owned();
        assert!(reduce(&other_type, Some(&m(&other_type)), None).is_none());

        let mut inactive = row("fixture-host-a");
        inactive.activation_status = ActivationStatus::Deprecated;
        assert!(reduce(&inactive, Some(&m(&inactive)), None).is_none());

        let mut no_dialect = row("fixture-host-a");
        no_dialect.api_dialect = None;
        assert!(reduce(&no_dialect, Some(&m(&no_dialect)), None).is_none());

        let mut no_url = row("fixture-host-a");
        no_url.base_url_template = None;
        assert!(reduce(&no_url, Some(&m(&no_url)), None).is_none());

        let p = row("fixture-host-a");
        assert!(reduce(&p, None, None).is_none(), "no default model");
        let mut retired = m(&p);
        retired.status = ModelStatus::Retired;
        assert!(reduce(&p, Some(&retired), None).is_none());
        let mut deprecated = m(&p);
        deprecated.status = ModelStatus::Deprecated;
        assert!(reduce(&p, Some(&deprecated), None).is_some());
    }

    #[test]
    fn the_qr_recommended_flag_and_a_clean_posture_are_both_required() {
        let m = |p: &Provider| default_model(&p.id);

        let mut unflagged = row("fixture-host-a");
        unflagged.qr_recommended = false;
        let c = reduce(&unflagged, Some(&m(&unflagged)), None).unwrap();
        assert_eq!(facts(&c).posture, Posture::Clean);
        assert!(!facts(&c).recommendable);

        let mut trains = row("fixture-host-a");
        trains.trains_on_data_by_default = true;
        let c = reduce(&trains, Some(&m(&trains)), None).unwrap();
        assert_eq!(facts(&c).posture, Posture::NotClean);
        assert!(!facts(&c).recommendable);

        let mut risky = row("fixture-host-a");
        risky.risk_rating = 2;
        let c = reduce(&risky, Some(&m(&risky)), None).unwrap();
        assert_eq!(facts(&c).posture, Posture::NotClean);

        let mut stale = row("fixture-host-a");
        stale.verification_status = VerificationStatus::Stale;
        let c = reduce(&stale, Some(&m(&stale)), None).unwrap();
        assert_eq!(facts(&c).posture, Posture::NotClean);
        assert!(!facts(&c).recommendable);

        let mut unv = row("fixture-host-a");
        unv.verification_status = VerificationStatus::Unverified;
        let c = reduce(&unv, Some(&m(&unv)), None).unwrap();
        assert_eq!(facts(&c).posture, Posture::Unverified);
        assert!(!facts(&c).recommendable);
    }

    #[test]
    fn a_model_that_trains_makes_the_posture_not_clean() {
        let p = row("fixture-host-a");
        let mut m = default_model(&p.id);
        m.trains_on_data_override = Some(true);
        let c = reduce(&p, Some(&m), None).unwrap();
        assert_eq!(facts(&c).posture, Posture::NotClean);
    }

    #[test]
    fn retention_is_a_warning_not_a_disqualifier() {
        let mut p = row("fixture-host-a");
        p.retains_data = true;
        p.retention_days = Some(30);
        let m = default_model(&p.id);
        let c = reduce(&p, Some(&m), Some(PRICE_FREE)).unwrap();
        assert!(facts(&c).recommendable);
        assert!(facts(&c)
            .warnings
            .iter()
            .any(|w| w.code == Code::WarnRetainsData.as_str()));
    }

    #[test]
    fn free_needs_a_free_tier_and_a_model_that_is_not_requires_paid() {
        let mut p = row("fixture-host-a");
        p.free_tier = Some(json!({"available": true}));
        let m = default_model(&p.id);
        // A free tier ranks Free even when the list price is non-zero. For
        // caps the same model counts as metered (module header).
        let c = reduce(&p, Some(&m), Some(PRICE_CHEAP)).unwrap();
        assert_eq!(facts(&c).cost_class, RankCostClass::Free);
        assert_eq!(
            crate::spend::disclosure::cost_class_of(Some(PRICE_CHEAP)),
            crate::spend::disclosure::CostClass::Metered
        );

        let mut paid = default_model(&p.id);
        paid.requires_paid = true;
        let c = reduce(&p, Some(&paid), Some(PRICE_CHEAP)).unwrap();
        assert_eq!(facts(&c).cost_class, RankCostClass::Metered);

        let mut no_tier = row("fixture-host-a");
        no_tier.free_tier = Some(json!({"available": false}));
        let c = reduce(&no_tier, Some(&m), Some(PRICE_CHEAP)).unwrap();
        assert_eq!(facts(&c).cost_class, RankCostClass::Metered);

        // A zero price with no free tier counts free for caps and ranks
        // Metered at price 0 here.
        let none = row("fixture-host-a");
        let c = reduce(&none, Some(&m), Some(PRICE_FREE)).unwrap();
        assert_eq!(facts(&c).cost_class, RankCostClass::Metered);
        assert_eq!(facts(&c).metered_price, 0);
        assert_eq!(
            crate::spend::disclosure::cost_class_of(Some(PRICE_FREE)),
            crate::spend::disclosure::CostClass::Free
        );
    }

    #[test]
    fn plan_included_uses_the_users_plan_at_that_provider() {
        let mut p = row("fixture-host-a");
        p.plan_inclusion = Some(json!(["fixture-plan"]));
        let m = default_model(&p.id);

        let mut ctx = UserContext::default();
        ctx.plan_keys
            .insert(p.id.clone(), "fixture-plan".to_owned());
        let c = hosted_candidate(&p, Some(&m), Some(PRICE_CHEAP), &ctx, today()).unwrap();
        assert_eq!(facts(&c).cost_class, RankCostClass::PlanIncluded);

        // Another plan, or a plan at a different provider, does not count.
        let mut other = UserContext::default();
        other
            .plan_keys
            .insert(p.id.clone(), "fixture-other-plan".to_owned());
        other
            .plan_keys
            .insert("fixture-host-z".to_owned(), "fixture-plan".to_owned());
        let c = hosted_candidate(&p, Some(&m), Some(PRICE_CHEAP), &other, today()).unwrap();
        assert_eq!(facts(&c).cost_class, RankCostClass::Metered);

        // No plan recorded: not plan-included.
        let c = reduce(&p, Some(&m), Some(PRICE_CHEAP)).unwrap();
        assert_eq!(facts(&c).cost_class, RankCostClass::Metered);
    }

    #[test]
    fn metered_price_orders_by_list_price_with_the_dearest_tier() {
        let p = row("fixture-host-a");
        let m = default_model(&p.id);
        let cheap = reduce(&p, Some(&m), Some(PRICE_CHEAP)).unwrap();
        let dear_price = r#"{"input_per_mtok":3,"output_per_mtok":15,"currency":"USD","as_of":"2026-10-11","source":"curated"}"#;
        let dear = reduce(&p, Some(&m), Some(dear_price)).unwrap();
        assert!(facts(&cheap).metered_price < facts(&dear).metered_price);
        assert_eq!(facts(&cheap).metered_price, 2_000_000);
    }

    #[test]
    fn no_readable_price_is_unknown_cost_with_the_price_warning() {
        let p = row("fixture-host-a");
        let m = default_model(&p.id);
        let c = reduce(&p, Some(&m), None).unwrap();
        assert_eq!(facts(&c).cost_class, RankCostClass::Unknown);
        assert!(facts(&c)
            .warnings
            .iter()
            .any(|w| w.code == Code::WarnPriceUnknown.as_str()));
    }

    #[test]
    fn warnings_reuse_the_710_codes_and_tiers() {
        let mut p = row("fixture-host-a");
        p.verification_status = VerificationStatus::Unverified;
        let m = default_model(&p.id);
        let c = reduce(&p, Some(&m), Some(PRICE_CHEAP)).unwrap();
        let codes: Vec<&str> = facts(&c).warnings.iter().map(|w| w.code.as_str()).collect();
        assert!(codes.contains(&Code::WarnUnverifiedProvider.as_str()));
        assert!(codes.contains(&Code::WarnCostMetered.as_str()));
        for w in &facts(&c).warnings {
            assert!(
                Code::ALL.iter().any(|k| k.as_str() == w.code),
                "{} is not a 710 code",
                w.code
            );
        }
    }

    #[test]
    fn user_variables_are_exposed_by_name_and_junk_is_dropped() {
        let mut p = row("fixture-host-a");
        p.user_variables = Some(json!(["fixture_var", 7, "other_var"]));
        let m = default_model(&p.id);
        let c = reduce(&p, Some(&m), None).unwrap();
        assert_eq!(
            facts(&c).user_variables,
            vec!["fixture_var".to_owned(), "other_var".to_owned()]
        );
        let none = row("fixture-host-b");
        let m2 = default_model(&none.id);
        assert!(facts(&reduce(&none, Some(&m2), None).unwrap())
            .user_variables
            .is_empty());
    }

    #[test]
    fn key_presence_comes_from_the_context_and_never_from_key_material() {
        let p = row("fixture-host-a");
        let m = default_model(&p.id);
        let mut ctx = UserContext::default();
        ctx.configured_provider_ids.insert(p.id.clone());
        let c = hosted_candidate(&p, Some(&m), None, &ctx, today()).unwrap();
        assert!(c.has_key);
        let rec = to_recommendation(&c, Fit::Comfortable, &BTreeSet::new());
        assert!(!rec.needs_api_key);
    }

    #[test]
    fn the_engine_is_deterministic_for_any_input_order() {
        let p = profile(7_900, None);
        let cat = vec![
            hosted("hosted-a", Some(1), false),
            hosted("hosted-b", Some(1), false),
            metered(hosted("hosted-c", Some(2), false), 10),
            not_clean(hosted("hosted-d", Some(3), true)),
            unverified(hosted("hosted-e", None, false)),
            local("local-c", Some(3), RamClass::Low, 3_000),
        ];
        let forward = recommend(&p, &cat);
        let mut reversed = cat.clone();
        reversed.reverse();
        assert_eq!(forward, recommend(&p, &reversed));
    }
}
