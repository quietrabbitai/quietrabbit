// src-tauri/src/spend/disclosure.rs
//
// Disclosed attributes, warning codes and the consent attributes hash
// (items.id=710; decisions.id=872 points 2-3, decisions.id=875; spec Part 8d).
//
// Everything here is a pure function of catalog rows. Warnings are returned as
// codes with structured parameters (spend::codes), never as sentences. Nothing
// here blocks anything: a warning informs the user, who may accept it
// (decisions.id=841, 872: warn, then allow). Blocking is the gate's job and
// only ever concerns consent, an unknown price, caps and limits.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::persistence::provider_store::{
    ModelStatus, PrivacyCommitmentBasis, Provider, ProviderModel, VerificationStatus,
};
use crate::spend::codes::{Code, WarningTier};
use crate::spend::price::{parse_pricing, PriceQuote};

/// Prefix of the hashed document. Bump it to force re-consent for everyone.
pub const HASH_VERSION: &str = "qr-provider-attrs-v1";

/// Token-limit keys. They are excluded from the hash: a change of the output
/// limit changes cost exposure, which the caps govern, not privacy.
const LIMIT_KEYS: [&str; 2] = ["max_completion_tokens", "max_tokens"];

// ---------------------------------------------------------------------------
// Cost class
// ---------------------------------------------------------------------------

/// What using a model costs, as far as the catalog knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum CostClass {
    /// The price is known and is exactly zero.
    Free,
    /// The price is known and non-zero.
    Metered,
    /// No readable price. Treated as metered with an unknowable cost.
    Unknown,
}

impl CostClass {
    pub fn as_str(self) -> &'static str {
        match self {
            CostClass::Free => "free",
            CostClass::Metered => "metered",
            CostClass::Unknown => "unknown",
        }
    }
}

/// Cost class from a row's raw `pricing` text.
pub fn cost_class_of(pricing_text: Option<&str>) -> CostClass {
    match pricing_text.and_then(|raw| parse_pricing(raw, None)) {
        Some(p) if p.is_free() => CostClass::Free,
        Some(_) => CostClass::Metered,
        None => CostClass::Unknown,
    }
}

fn pricing_text_for<'a>(texts: &'a [(String, Option<String>)], model_id: &str) -> Option<&'a str> {
    texts
        .iter()
        .find(|(id, _)| id == model_id)
        .and_then(|(_, t)| t.as_deref())
}

/// Whether using this provider can cost money: some non-retired model is
/// metered or has no readable price. A provider whose every live model is
/// known-free needs no spend cap at consent.
pub fn provider_can_cost_money(
    models: &[ProviderModel],
    pricing_texts: &[(String, Option<String>)],
) -> bool {
    let live: Vec<&ProviderModel> = models
        .iter()
        .filter(|m| m.status != ModelStatus::Retired)
        .collect();
    if live.is_empty() {
        // Nothing is curated, so any model used would be uncurated and unpriced.
        return true;
    }
    live.iter()
        .any(|m| cost_class_of(pricing_text_for(pricing_texts, &m.model_id)) != CostClass::Free)
}

// ---------------------------------------------------------------------------
// Effective attributes of a provider + model
// ---------------------------------------------------------------------------

/// The privacy attributes that apply to one model: the model's own override
/// where it has one, else the provider's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveAttributes {
    pub retains_data: bool,
    pub trains_on_data: bool,
    pub retention_days: Option<u32>,
    /// The model overrides the provider's training attribute to true.
    pub model_trains_override: bool,
}

pub fn effective_attributes(
    provider: &Provider,
    model: Option<&ProviderModel>,
) -> EffectiveAttributes {
    let retains_data = model
        .and_then(|m| m.retains_data_override)
        .unwrap_or(provider.retains_data);
    let trains_on_data = model
        .and_then(|m| m.trains_on_data_override)
        .unwrap_or(provider.trains_on_data_by_default);
    let retention_days = model
        .and_then(|m| m.retention_days_override)
        .or(provider.retention_days);
    let model_trains_override = model
        .and_then(|m| m.trains_on_data_override)
        .is_some_and(|v| v && !provider.trains_on_data_by_default);
    EffectiveAttributes {
        retains_data,
        trains_on_data,
        retention_days,
        model_trains_override,
    }
}

// ---------------------------------------------------------------------------
// Warnings
// ---------------------------------------------------------------------------

/// A model's price as the warnings see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceView {
    Known { free: bool, stale: bool },
    Unknown,
}

impl PriceView {
    pub fn from_quote(quote: &PriceQuote, today: chrono::NaiveDate) -> Self {
        match quote {
            PriceQuote::Known(p) => PriceView::Known {
                free: p.is_free(),
                stale: p.is_stale(today),
            },
            PriceQuote::Unknown => PriceView::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
    /// The stable code (spend::codes), for example `warning.unverified_provider`.
    pub code: String,
    pub tier: WarningTier,
    /// Structured values the screen formats. Never pre-formatted text. (The
    /// IPC layer converts this to a typed map; see commands::provider_spend.)
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WarningSet {
    pub warnings: Vec<Warning>,
    /// The loudest tier: the maximum over the codes, the destination-risk
    /// floor included, raised one level for sensitive-Persona content.
    pub overall: WarningTier,
}

/// What is known about the selected model beyond the provider's own
/// attributes (items.id=711). All false for a reviewed, active, listed model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelNotes {
    /// From the user's live list and not reviewed by QR.
    pub uncurated: bool,
    /// The provider's latest list no longer includes it.
    pub may_be_retired: bool,
    /// Marked deprecated, or carries a retirement date.
    pub deprecated: bool,
}

pub struct WarningInput<'a> {
    pub provider: &'a Provider,
    pub model: Option<&'a ProviderModel>,
    pub model_notes: ModelNotes,
    pub price: PriceView,
    pub price_as_of: Option<String>,
    /// Supplied by the caller. The backend has no Persona-level sensitivity
    /// classifier and does not invent one (items.id=710, review change D).
    pub sensitive_persona: bool,
}

fn warning(code: Code, params: Value) -> Warning {
    Warning {
        code: code.as_str().to_owned(),
        tier: code.tier(),
        params,
    }
}

/// Compute the warnings for a provider (and, when known, the selected model).
/// Pure; the table lives in one place so changing a tier is a one-line edit
/// plus the matching test.
pub fn compute_warnings(input: &WarningInput<'_>) -> WarningSet {
    let p = input.provider;
    let eff = effective_attributes(p, input.model);
    let mut out: Vec<Warning> = Vec::new();

    match p.verification_status {
        VerificationStatus::Unverified => {
            out.push(warning(Code::WarnUnverifiedProvider, json!({})))
        }
        VerificationStatus::Stale => out.push(warning(Code::WarnVerificationStale, json!({}))),
        VerificationStatus::Verified => {}
    }

    if eff.model_trains_override {
        out.push(warning(
            Code::WarnModelTrainsOverride,
            json!({"model_id": input.model.map(|m| m.model_id.clone())}),
        ));
    } else if eff.trains_on_data {
        out.push(warning(Code::WarnTrainsOnDataByDefault, json!({})));
    }

    if eff.retains_data {
        out.push(warning(
            Code::WarnRetainsData,
            json!({"retention_days": eff.retention_days}),
        ));
        if eff.retention_days.is_none() {
            out.push(warning(Code::WarnRetentionUnstated, json!({})));
        }
    }

    if !p.is_anonymous {
        out.push(warning(
            Code::WarnNotAnonymous,
            json!({"login_required": p.login_required}),
        ));
    }

    if p.privacy_commitment_basis == Some(PrivacyCommitmentBasis::PolicyOnly) {
        out.push(warning(Code::WarnCommitmentPolicyOnly, json!({})));
    }

    let model_id = || input.model.map(|m| m.model_id.clone());
    if input.model_notes.uncurated {
        out.push(warning(
            Code::WarnModelUncurated,
            json!({"model_id": model_id()}),
        ));
    }
    if input.model_notes.may_be_retired {
        out.push(warning(
            Code::WarnModelMayBeRetired,
            json!({"model_id": model_id()}),
        ));
    }
    if input.model_notes.deprecated {
        out.push(warning(
            Code::WarnModelDeprecated,
            json!({
                "model_id": model_id(),
                "retires_at": input.model.and_then(|m| m.retires_at.clone()),
            }),
        ));
    }

    match input.price {
        PriceView::Unknown => out.push(warning(
            Code::WarnPriceUnknown,
            json!({"model_id": input.model.map(|m| m.model_id.clone())}),
        )),
        PriceView::Known { free, stale } => {
            if !free {
                out.push(warning(Code::WarnCostMetered, json!({})));
            }
            if stale {
                out.push(warning(
                    Code::WarnPriceStale,
                    json!({"as_of": input.price_as_of}),
                ));
            }
        }
    }

    // The curated destination-risk rating is a floor under the overall tier:
    // 1 none, 2 notice, 3 strong (review change A).
    match p.risk_rating {
        0 | 1 => {}
        2 => out.push(warning(
            Code::WarnDestinationRiskMedium,
            json!({"risk_rating": 2}),
        )),
        r => out.push(warning(
            Code::WarnDestinationRiskHigh,
            json!({"risk_rating": r}),
        )),
    }

    let mut overall = out
        .iter()
        .map(|w| w.tier)
        .max()
        .unwrap_or(WarningTier::None);

    if input.sensitive_persona {
        overall = overall.raised();
        out.push(Warning {
            code: Code::WarnSensitivePersonaContent.as_str().to_owned(),
            tier: overall,
            params: json!({}),
        });
    }

    WarningSet {
        warnings: out,
        overall,
    }
}

// ---------------------------------------------------------------------------
// Attributes hash
// ---------------------------------------------------------------------------

fn verification_str(v: VerificationStatus) -> &'static str {
    match v {
        VerificationStatus::Verified => "verified",
        VerificationStatus::Unverified => "unverified",
        VerificationStatus::Stale => "stale",
    }
}

fn basis_str(b: Option<PrivacyCommitmentBasis>) -> Value {
    match b {
        Some(PrivacyCommitmentBasis::Contractual) => json!("contractual"),
        Some(PrivacyCommitmentBasis::PolicyOnly) => json!("policy_only"),
        None => Value::Null,
    }
}

/// A model's request options without the token-limit keys: anything left can
/// change where a request goes or what the provider does with it (for example
/// an upstream data-collection policy), so it is part of what the user
/// consented to. No key is named here: any non-limit option counts.
fn privacy_relevant_options(request_options: Option<&Value>) -> Value {
    match request_options {
        Some(Value::Object(map)) => {
            let rest: Map<String, Value> = map
                .iter()
                .filter(|(k, _)| !LIMIT_KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if rest.is_empty() {
                Value::Null
            } else {
                Value::Object(rest)
            }
        }
        _ => Value::Null,
    }
}

/// The document that is hashed. serde_json's map is ordered by key, so the
/// text is canonical.
pub fn attributes_document(
    provider: &Provider,
    models: &[ProviderModel],
    pricing_texts: &[(String, Option<String>)],
) -> Value {
    let gate = &provider.documentation_gate;
    let mut live: Vec<&ProviderModel> = models
        .iter()
        .filter(|m| m.status != ModelStatus::Retired)
        .collect();
    live.sort_by(|a, b| a.model_id.cmp(&b.model_id));

    let model_docs: Vec<Value> = live
        .iter()
        .map(|m| {
            json!({
                "model_id": m.model_id,
                "retains_data_override": m.retains_data_override,
                "trains_on_data_override": m.trains_on_data_override,
                "retention_days_override": m.retention_days_override,
                "cost_class": cost_class_of(pricing_text_for(pricing_texts, &m.model_id)).as_str(),
                "request_options": privacy_relevant_options(m.request_options.as_ref()),
            })
        })
        .collect();

    json!({
        "version": HASH_VERSION,
        "provider": {
            "id": provider.id,
            "trains_on_data_by_default": provider.trains_on_data_by_default,
            "retains_data": provider.retains_data,
            "retention_days": provider.retention_days,
            "is_anonymous": provider.is_anonymous,
            "login_required": provider.login_required,
            "jurisdiction": provider.jurisdiction,
            "commitment_basis": basis_str(provider.privacy_commitment_basis),
            "verification_status": verification_str(provider.verification_status),
            "risk_rating": provider.risk_rating,
            "documentation_gate": {
                "model_overrides": gate.get("model_overrides").cloned().unwrap_or(Value::Null),
                "disclosures": gate.get("disclosures").cloned().unwrap_or(Value::Null),
                "jurisdiction": gate.get("jurisdiction").cloned().unwrap_or(Value::Null),
            },
        },
        "models": model_docs,
    })
}

/// SHA-256 (lower-case hex) of the canonical attributes document.
///
/// Included: every disclosed provider attribute, the curated destination risk
/// rating, the documentation-gate disclosure fields, and for EVERY non-retired
/// model of the provider its three privacy overrides, its cost class and the
/// non-limit part of its request options. Excluded on purpose: as-of dates,
/// price amounts, token limits and display names, which change without
/// changing what the user consented to.
pub fn attributes_hash(
    provider: &Provider,
    models: &[ProviderModel],
    pricing_texts: &[(String, Option<String>)],
) -> String {
    let doc = attributes_document(provider, models, pricing_texts);
    let text = doc.to_string();
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spend::fixtures::{model, provider};

    fn texts(free: bool) -> Vec<(String, Option<String>)> {
        let t = if free {
            r#"{"input_per_mtok":0,"output_per_mtok":0}"#
        } else {
            r#"{"input_per_mtok":0.5,"output_per_mtok":1.5}"#
        };
        vec![
            ("fixture-model-a".to_owned(), Some(t.to_owned())),
            ("fixture-model-b".to_owned(), Some(t.to_owned())),
        ]
    }

    fn base_hash() -> String {
        attributes_hash(
            &provider(),
            &[model("fixture-model-a"), model("fixture-model-b")],
            &texts(false),
        )
    }

    // -- hash -----------------------------------------------------------------

    #[test]
    fn hash_is_stable_hex_and_ignores_row_order() {
        let h = base_hash();
        assert_eq!(h.len(), 64);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
        let swapped = attributes_hash(
            &provider(),
            &[model("fixture-model-b"), model("fixture-model-a")],
            &texts(false),
        );
        assert_eq!(h, swapped);
    }

    #[test]
    fn every_disclosed_provider_attribute_changes_the_hash() {
        let h = base_hash();
        let models = [model("fixture-model-a"), model("fixture-model-b")];
        let t = texts(false);
        let changed: Vec<(&str, Provider)> = vec![
            (
                "trains",
                Provider {
                    trains_on_data_by_default: true,
                    ..provider()
                },
            ),
            (
                "retains",
                Provider {
                    retains_data: true,
                    ..provider()
                },
            ),
            (
                "retention_days",
                Provider {
                    retention_days: Some(30),
                    ..provider()
                },
            ),
            (
                "anonymous",
                Provider {
                    is_anonymous: false,
                    ..provider()
                },
            ),
            (
                "login",
                Provider {
                    login_required: true,
                    ..provider()
                },
            ),
            (
                "jurisdiction",
                Provider {
                    jurisdiction: Some("Elsewhere".to_owned()),
                    ..provider()
                },
            ),
            (
                "basis",
                Provider {
                    privacy_commitment_basis: Some(PrivacyCommitmentBasis::PolicyOnly),
                    ..provider()
                },
            ),
            (
                "basis_none",
                Provider {
                    privacy_commitment_basis: None,
                    ..provider()
                },
            ),
            (
                "verification",
                Provider {
                    verification_status: VerificationStatus::Stale,
                    ..provider()
                },
            ),
            (
                "verification_unv",
                Provider {
                    verification_status: VerificationStatus::Unverified,
                    ..provider()
                },
            ),
            (
                "risk",
                Provider {
                    risk_rating: 3,
                    ..provider()
                },
            ),
            (
                "gate_overrides",
                Provider {
                    documentation_gate: json!({"model_overrides": ["different"], "disclosures": ["Fixture disclosure."], "jurisdiction": "Fixtureland"}),
                    ..provider()
                },
            ),
            (
                "gate_disclosures",
                Provider {
                    documentation_gate: json!({"model_overrides": ["Batch features retain data."], "disclosures": [], "jurisdiction": "Fixtureland"}),
                    ..provider()
                },
            ),
        ];
        for (name, p) in changed {
            assert_ne!(
                attributes_hash(&p, &models, &t),
                h,
                "{name} must change the hash"
            );
        }
    }

    #[test]
    fn every_model_override_and_option_changes_the_hash() {
        let h = base_hash();
        let t = texts(false);
        let with = |f: &dyn Fn(&mut ProviderModel)| {
            let mut a = model("fixture-model-a");
            f(&mut a);
            attributes_hash(&provider(), &[a, model("fixture-model-b")], &t)
        };
        assert_ne!(with(&|m| m.retains_data_override = Some(true)), h);
        assert_ne!(with(&|m| m.trains_on_data_override = Some(true)), h);
        assert_ne!(with(&|m| m.retention_days_override = Some(7)), h);
        // Upstream routing policy lives in request options.
        assert_ne!(
            with(&|m| m.request_options =
                Some(json!({"max_tokens": 1024, "provider": {"data_collection": "deny"}}))),
            h
        );
        // A model being added is a new posture to consent to.
        let three = attributes_hash(
            &provider(),
            &[
                model("fixture-model-a"),
                model("fixture-model-b"),
                model("fixture-model-c"),
            ],
            &texts(false),
        );
        assert_ne!(three, h);
        // A retired model drops out of the set.
        let mut retired = model("fixture-model-b");
        retired.status = ModelStatus::Retired;
        assert_ne!(
            attributes_hash(
                &provider(),
                &[model("fixture-model-a"), retired],
                &texts(false)
            ),
            h
        );
    }

    #[test]
    fn cost_class_changes_the_hash_but_price_amounts_do_not() {
        let h = base_hash();
        let models = [model("fixture-model-a"), model("fixture-model-b")];
        // Metered -> free is a different disclosed cost class.
        assert_ne!(attributes_hash(&provider(), &models, &texts(true)), h);
        // A different non-zero price is the same class.
        let pricier: Vec<(String, Option<String>)> = vec![
            (
                "fixture-model-a".to_owned(),
                Some(r#"{"input_per_mtok":9,"output_per_mtok":99}"#.to_owned()),
            ),
            (
                "fixture-model-b".to_owned(),
                Some(r#"{"input_per_mtok":9,"output_per_mtok":99}"#.to_owned()),
            ),
        ];
        assert_eq!(attributes_hash(&provider(), &models, &pricier), h);
        // Unknown price is its own class.
        let unknown: Vec<(String, Option<String>)> = vec![
            ("fixture-model-a".to_owned(), None),
            ("fixture-model-b".to_owned(), None),
        ];
        assert_ne!(attributes_hash(&provider(), &models, &unknown), h);
    }

    #[test]
    fn things_that_do_not_change_what_was_consented_to_leave_the_hash_alone() {
        let h = base_hash();
        let t = texts(false);
        // display name, as-of dates in pricing, the token limit, context window.
        let renamed = Provider {
            display_name: "Another Name".to_owned(),
            ..provider()
        };
        assert_eq!(
            attributes_hash(
                &renamed,
                &[model("fixture-model-a"), model("fixture-model-b")],
                &t
            ),
            h
        );
        let mut a = model("fixture-model-a");
        a.request_options = Some(json!({"max_completion_tokens": 9999}));
        a.context_window_tokens = 1;
        a.pricing = Some(json!({"as_of": "2030-01-01"}));
        assert_eq!(
            attributes_hash(&provider(), &[a, model("fixture-model-b")], &t),
            h
        );
        let dated: Vec<(String, Option<String>)> = vec![
            (
                "fixture-model-a".to_owned(),
                Some(
                    r#"{"input_per_mtok":0.5,"output_per_mtok":1.5,"as_of":"2030-01-01"}"#
                        .to_owned(),
                ),
            ),
            (
                "fixture-model-b".to_owned(),
                Some(r#"{"input_per_mtok":0.5,"output_per_mtok":1.5}"#.to_owned()),
            ),
        ];
        assert_eq!(
            attributes_hash(
                &provider(),
                &[model("fixture-model-a"), model("fixture-model-b")],
                &dated
            ),
            h
        );
    }

    // -- cost class -------------------------------------------------------------

    #[test]
    fn provider_cost_exposure() {
        let models = [model("fixture-model-a"), model("fixture-model-b")];
        assert!(!provider_can_cost_money(&models, &texts(true)));
        assert!(provider_can_cost_money(&models, &texts(false)));
        // One paid model among free ones is enough.
        let mixed: Vec<(String, Option<String>)> = vec![
            (
                "fixture-model-a".to_owned(),
                Some(r#"{"input_per_mtok":0,"output_per_mtok":0}"#.to_owned()),
            ),
            (
                "fixture-model-b".to_owned(),
                Some(r#"{"input_per_mtok":1,"output_per_mtok":1}"#.to_owned()),
            ),
        ];
        assert!(provider_can_cost_money(&models, &mixed));
        // A retired paid model does not count.
        let mut retired = model("fixture-model-b");
        retired.status = ModelStatus::Retired;
        assert!(!provider_can_cost_money(
            &[model("fixture-model-a"), retired],
            &mixed
        ));
        // An unpriced model is exposure; so is a provider with no models.
        let unpriced: Vec<(String, Option<String>)> = vec![("fixture-model-a".to_owned(), None)];
        assert!(provider_can_cost_money(
            &[model("fixture-model-a")],
            &unpriced
        ));
        assert!(provider_can_cost_money(&[], &[]));
    }

    // -- warnings ---------------------------------------------------------------

    fn codes_of(set: &WarningSet) -> Vec<&str> {
        set.warnings.iter().map(|w| w.code.as_str()).collect()
    }

    fn input<'a>(
        p: &'a Provider,
        m: Option<&'a ProviderModel>,
        price: PriceView,
    ) -> WarningInput<'a> {
        WarningInput {
            provider: p,
            model: m,
            model_notes: ModelNotes::default(),
            price,
            price_as_of: Some("2026-10-11".to_owned()),
            sensitive_persona: false,
        }
    }

    #[test]
    fn model_notes_add_their_warnings_and_raise_the_overall_tier() {
        let p = provider();
        let m = model("fixture-model-a");
        let free = PriceView::Known {
            free: true,
            stale: false,
        };
        let base = compute_warnings(&input(&p, Some(&m), free));
        assert!(codes_of(&base).is_empty());

        let mut uncurated = input(&p, Some(&m), PriceView::Unknown);
        uncurated.model_notes = ModelNotes {
            uncurated: true,
            ..Default::default()
        };
        let set = compute_warnings(&uncurated);
        assert!(codes_of(&set).contains(&"warning.model_uncurated"));
        assert!(codes_of(&set).contains(&"warning.price_unknown"));
        assert_eq!(set.overall, WarningTier::Strong);

        let mut notes = input(&p, Some(&m), free);
        notes.model_notes = ModelNotes {
            uncurated: false,
            may_be_retired: true,
            deprecated: true,
        };
        let set = compute_warnings(&notes);
        assert_eq!(
            codes_of(&set),
            vec!["warning.model_may_be_retired", "warning.model_deprecated"]
        );
        assert_eq!(set.overall, WarningTier::Notice);
    }

    #[test]
    fn a_clean_free_anonymous_verified_provider_has_no_warnings() {
        let p = provider();
        let set = compute_warnings(&input(
            &p,
            None,
            PriceView::Known {
                free: true,
                stale: false,
            },
        ));
        assert!(set.warnings.is_empty());
        assert_eq!(set.overall, WarningTier::None);
    }

    #[test]
    fn each_condition_yields_its_code_and_tier() {
        let free = PriceView::Known {
            free: true,
            stale: false,
        };
        let cases: Vec<(&str, Provider, PriceView, &str, WarningTier)> = vec![
            (
                "unverified",
                Provider {
                    verification_status: VerificationStatus::Unverified,
                    ..provider()
                },
                free,
                "warning.unverified_provider",
                WarningTier::Strong,
            ),
            (
                "stale",
                Provider {
                    verification_status: VerificationStatus::Stale,
                    ..provider()
                },
                free,
                "warning.verification_stale",
                WarningTier::Notice,
            ),
            (
                "trains",
                Provider {
                    trains_on_data_by_default: true,
                    ..provider()
                },
                free,
                "warning.trains_on_data_by_default",
                WarningTier::Strong,
            ),
            (
                "retains",
                Provider {
                    retains_data: true,
                    retention_days: Some(30),
                    ..provider()
                },
                free,
                "warning.retains_data",
                WarningTier::Notice,
            ),
            (
                "not anonymous",
                Provider {
                    is_anonymous: false,
                    login_required: true,
                    ..provider()
                },
                free,
                "warning.not_anonymous",
                WarningTier::Notice,
            ),
            (
                "policy only",
                Provider {
                    privacy_commitment_basis: Some(PrivacyCommitmentBasis::PolicyOnly),
                    ..provider()
                },
                free,
                "warning.commitment_policy_only",
                WarningTier::Notice,
            ),
            (
                "metered",
                provider(),
                PriceView::Known {
                    free: false,
                    stale: false,
                },
                "warning.cost_metered",
                WarningTier::Notice,
            ),
            (
                "price unknown",
                provider(),
                PriceView::Unknown,
                "warning.price_unknown",
                WarningTier::Strong,
            ),
            (
                "price stale",
                provider(),
                PriceView::Known {
                    free: true,
                    stale: true,
                },
                "warning.price_stale",
                WarningTier::Notice,
            ),
            (
                "risk medium",
                Provider {
                    risk_rating: 2,
                    ..provider()
                },
                free,
                "warning.destination_risk_medium",
                WarningTier::Notice,
            ),
            (
                "risk high",
                Provider {
                    risk_rating: 3,
                    ..provider()
                },
                free,
                "warning.destination_risk_high",
                WarningTier::Strong,
            ),
        ];
        for (name, p, price, code, tier) in cases {
            let set = compute_warnings(&input(&p, None, price));
            assert!(
                codes_of(&set).contains(&code),
                "{name}: expected {code}, got {:?}",
                codes_of(&set)
            );
            assert_eq!(set.overall, tier, "{name}");
        }
    }

    #[test]
    fn retention_unstated_only_matters_when_data_is_retained() {
        let free = PriceView::Known {
            free: true,
            stale: false,
        };
        let retaining = Provider {
            retains_data: true,
            retention_days: None,
            ..provider()
        };
        let set = compute_warnings(&input(&retaining, None, free));
        assert!(codes_of(&set).contains(&"warning.retention_unstated"));
        let quiet = Provider {
            retains_data: false,
            retention_days: None,
            ..provider()
        };
        let set = compute_warnings(&input(&quiet, None, free));
        assert!(!codes_of(&set).contains(&"warning.retention_unstated"));
    }

    #[test]
    fn a_model_override_changes_the_effective_attributes() {
        let free = PriceView::Known {
            free: true,
            stale: false,
        };
        let p = provider();
        let mut m = model("fixture-model-a");
        m.trains_on_data_override = Some(true);
        let set = compute_warnings(&input(&p, Some(&m), free));
        assert!(codes_of(&set).contains(&"warning.model_trains_override"));
        assert_eq!(set.overall, WarningTier::Strong);
        // The same override on a provider that already trains is just the
        // provider's warning.
        let training = Provider {
            trains_on_data_by_default: true,
            ..provider()
        };
        let set = compute_warnings(&input(&training, Some(&m), free));
        assert!(codes_of(&set).contains(&"warning.trains_on_data_by_default"));
        assert!(!codes_of(&set).contains(&"warning.model_trains_override"));
        // An override that turns retention on.
        let mut r = model("fixture-model-b");
        r.retains_data_override = Some(true);
        r.retention_days_override = Some(14);
        let set = compute_warnings(&input(&p, Some(&r), free));
        let w = set
            .warnings
            .iter()
            .find(|w| w.code == "warning.retains_data")
            .unwrap();
        assert_eq!(w.params["retention_days"], 14);
    }

    #[test]
    fn the_risk_rating_is_a_floor_under_the_overall_tier() {
        let free = PriceView::Known {
            free: true,
            stale: false,
        };
        // Nothing else to warn about, but a high rating alone is strong.
        let p = Provider {
            risk_rating: 3,
            ..provider()
        };
        assert_eq!(
            compute_warnings(&input(&p, None, free)).overall,
            WarningTier::Strong
        );
        let p = Provider {
            risk_rating: 2,
            ..provider()
        };
        assert_eq!(
            compute_warnings(&input(&p, None, free)).overall,
            WarningTier::Notice
        );
        // The floor never lowers a louder tier.
        let p = Provider {
            risk_rating: 2,
            trains_on_data_by_default: true,
            ..provider()
        };
        assert_eq!(
            compute_warnings(&input(&p, None, free)).overall,
            WarningTier::Strong
        );
    }

    #[test]
    fn sensitive_persona_content_raises_the_floored_result_one_level() {
        let free = PriceView::Known {
            free: true,
            stale: false,
        };
        let clean = Provider {
            risk_rating: 1,
            ..provider()
        };
        let mut i = input(&clean, None, free);
        i.sensitive_persona = true;
        let set = compute_warnings(&i);
        assert_eq!(set.overall, WarningTier::Notice, "none raised to notice");
        assert!(codes_of(&set).contains(&"warning.sensitive_persona_content"));

        let p = Provider {
            risk_rating: 2,
            ..provider()
        };
        let mut i = input(&p, None, free);
        i.sensitive_persona = true;
        assert_eq!(
            compute_warnings(&i).overall,
            WarningTier::Strong,
            "floor notice raised to strong"
        );
    }

    #[test]
    fn params_are_structured_values_not_prose() {
        let p = Provider {
            retains_data: true,
            retention_days: Some(30),
            is_anonymous: false,
            login_required: true,
            risk_rating: 3,
            ..provider()
        };
        let set = compute_warnings(&input(
            &p,
            None,
            PriceView::Known {
                free: false,
                stale: true,
            },
        ));
        for w in &set.warnings {
            assert!(w.params.is_object(), "{} params must be an object", w.code);
            for v in w.params.as_object().unwrap().values() {
                // No sentence-length strings: only ids, dates and scalars.
                if let Some(s) = v.as_str() {
                    assert!(s.len() < 40 && !s.contains(' '), "{}: {s:?}", w.code);
                }
            }
        }
        // Every code a warning can carry is in the registry.
        let known: std::collections::HashSet<&str> = Code::ALL.iter().map(|c| c.as_str()).collect();
        for w in &set.warnings {
            assert!(
                known.contains(w.code.as_str()),
                "{} not in the registry",
                w.code
            );
        }
    }

    #[test]
    fn price_view_follows_the_quote() {
        use crate::spend::price::resolve_price;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 11).unwrap();
        let q = resolve_price(
            None,
            Some(r#"{"input_per_mtok":0,"output_per_mtok":0,"as_of":"2026-10-11"}"#),
        );
        assert_eq!(
            PriceView::from_quote(&q, today),
            PriceView::Known {
                free: true,
                stale: false
            }
        );
        let q = resolve_price(
            None,
            Some(r#"{"input_per_mtok":1,"output_per_mtok":1,"as_of":"2025-01-01"}"#),
        );
        assert_eq!(
            PriceView::from_quote(&q, today),
            PriceView::Known {
                free: false,
                stale: true
            }
        );
        assert_eq!(
            PriceView::from_quote(&PriceQuote::Unknown, today),
            PriceView::Unknown
        );
    }
}
