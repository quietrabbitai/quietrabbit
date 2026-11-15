// src-tauri/src/conductor/model_resolver.rs
//
// items.id=896 stage 2 (decisions.id=941 point 4, decisions.id=942 point 9 and
// addendum 2 e): the MODEL RESOLVER for model blocks. A model block states a
// requirement (what the task needs, the quality bar, the destinations and tier it
// may use, an optional class preference). The resolver answers which model meets
// it at the lowest cost to the user.
//
// PURE. This module reads no database, no file, no clock and no network, and holds
// no state. Everything it decides on is passed in: the requirement, the run's
// document profile, the ceiling, a context-size estimate, the candidates a gatherer
// collected (stage 3) and the measured capability rows (model_capability). A source
// scan pins that. The same inputs give the same answer, whatever order the
// candidates arrive in.
//
// FOR MODEL BLOCKS ONLY. Ordinary steps and API Chat keep today's selection
// (decisions.id=870, decisions.id=942 addendum 2 e). A step reaches this module only
// when its expansion carries a requirement.
//
// THE RULES, in order:
//   1. Destinations. A local model is a candidate when the requirement allows the
//      local destination. A hosted model is a candidate only when the requirement
//      allows hosted_api AND the ceiling (the lower of the requirement's highest
//      tier and the run's own) is anonymous_preferred or above. Manual hand-off is
//      never a candidate here: a hand-off is not a model call, it is a pair of
//      passes a Focus row cites directly (items.id=387: the cb-15 render and the
//      handoff_ingest return), and choosing it as a strategy is the run plan's
//      (items.id=901). Cloud Chat panes are
//      user-driven and never an automatic target (decisions.id=680): they are not
//      candidates at all.
//   2. Readiness. Only a candidate its gatherer marked Ready is considered. A hosted
//      model with no stored key, no consent or no readable price is not Ready, so a
//      model of unknown price is never chosen automatically.
//   3. Context. A candidate whose window is below the estimate is dropped. An unknown
//      window passes: the real check runs after the text is rendered.
//   4. Capability. The candidate's measured cell for the task type and the run's
//      document profile (exact profile first, then the task-type-wide cell) is
//      compared with the REQUIRED BAR, which is the requirement's quality floor
//      minimum. A cell that meets it QUALIFIES. A cell below it is BELOW BAR. A
//      candidate with NO cell is NOT MEASURED, hosted or local alike: it is never
//      chosen automatically, only listed in the options (decisions.id=942
//      addendum 2 d: hosted models have no score until items.id=788, and a model
//      nobody has measured is not assumed adequate).
//   5. Ranking among those that qualify: cost tier (local, then free, then
//      plan-included, then metered; decisions.id=872), then at anonymous_preferred
//      an anonymous provider before one that needs an account (decisions.id=870,
//      only a tie-break inside a cost tier), then the model class (the smallest
//      class that qualifies, "lowest resource", or the class nearest the block's
//      preference, which only ranks and never filters), then the estimated cost,
//      then the ids so the order is total. Because local ranks first, a paid model
//      is never chosen while a local one qualifies.
//   6. Nothing qualifies: TooWeak. It never picks a model below the bar. It carries
//      the best candidate below the bar and every other candidate with why it did
//      not qualify (below the bar, or not measured), so the run plan or the user can
//      choose knowingly.
//
// NO MODEL IS NAMED HERE (decisions.id=872 and 875). Model and provider ids are
// opaque strings that arrive in the inputs. A scan test fails on any curated tag or
// catalog id in this file. The resolver also carries no text of any kind into a
// call (a scan pins that too): it chooses a model and nothing else.
//
// THE DOCUMENT PROFILE comes from the run, not from the block. profile_for_
// document_type() maps the document type the run selected to its shipped profile: a
// shipped type is its own profile, a custom type resolves through `based_on` to the
// shipped type it was made from.

use std::collections::HashMap;

use crate::conductor::model_block::config::{
    Destination, ModelClass, ModelRequirement, QualityBar,
};
use crate::conductor::model_block::document_type::{DocumentType, Registry};
use crate::conductor::model_capability::{AttainedBar, CapabilityRow, CapabilitySubject};
use crate::conductor::model_selection::CONTEXT_UNKNOWN;
use crate::conductor::tokens::ExternalAccess;

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// What using a candidate costs the user, in the order the resolver prefers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CostTier {
    /// Runs on the user's own machine.
    Local,
    /// A hosted model whose price is known and zero.
    Free,
    /// A hosted model the user's plan already includes.
    PlanIncluded,
    /// A hosted model with a known non-zero price.
    Metered,
}

/// Whether a candidate may be used at all right now. The gatherer decides; the
/// resolver considers only Ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    /// A local model that is not downloaded.
    NotInstalled,
    /// A local model the user turned off.
    DisabledByUser,
    /// A hosted provider with no stored key.
    NoKey,
    /// A hosted provider the user has not consented to.
    NotConsented,
    /// A hosted model with no readable price (never chosen automatically).
    PriceUnknown,
    /// A model the catalog has retired.
    Retired,
}

/// One model the gatherer found. The ids are opaque.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub subject: CapabilitySubject,
    /// Empty for a local model.
    pub provider_id: String,
    pub model_id: String,
    pub readiness: Readiness,
    /// CONTEXT_UNKNOWN when the window is not known.
    pub context_window_tokens: u32,
    pub cost_tier: CostTier,
    /// Estimated cost of this step in micro-USD. Zero for local and free.
    pub est_cost_micros: u64,
    /// The provider needs no account (hosted only).
    pub provider_is_anonymous: bool,
}

pub struct ResolveInput<'a> {
    pub requirement: &'a ModelRequirement,
    /// The shipped document profile of the run's selected document type, or None
    /// when the run has none (only the task-type-wide cell is then used).
    pub profile: Option<&'a str>,
    /// The run's own ceiling. The requirement's highest tier narrows it further.
    pub ceiling: ExternalAccess,
    /// A lower bound on the tokens the call needs, or 0.
    pub min_context_tokens: u32,
    pub candidates: &'a [Candidate],
    pub capabilities: &'a [CapabilityRow],
}

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub subject: CapabilitySubject,
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub model: ModelRef,
    pub cost_tier: CostTier,
    pub est_cost_micros: u64,
    pub context_window_tokens: u32,
    pub attained: AttainedBar,
    pub model_class: Option<ModelClass>,
}

/// Why a candidate did not qualify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Measured, and below the required bar.
    BelowBar(AttainedBar),
    /// No measurement for this task type and profile.
    NotMeasured,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionEntry {
    pub model: ModelRef,
    pub cost_tier: CostTier,
    pub est_cost_micros: u64,
    pub standing: Standing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoCandidateReason {
    /// The requirement and the ceiling leave no destination.
    NoDestination,
    /// Candidates exist for an allowed destination but none is ready.
    NoneReady,
    /// Ready candidates exist but every window is below the estimate.
    ContextTooSmall,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Selected {
        chosen: Chosen,
        /// The other qualifying LOCAL candidates in rank order, for a context
        /// fallback once the text is rendered. Empty when the choice is hosted.
        fallbacks: Vec<Chosen>,
    },
    TooWeak {
        required: QualityBar,
        /// The best candidate that is below the bar (highest attainment first), if
        /// any candidate was measured at all.
        best_below_bar: Option<OptionEntry>,
        /// Every candidate that did not qualify, with why: measured-below-bar
        /// first, then not measured.
        options: Vec<OptionEntry>,
    },
    NoCandidate(NoCandidateReason),
}

// ---------------------------------------------------------------------------
// The resolver
// ---------------------------------------------------------------------------

type CellKey<'a> = (CapabilitySubject, &'a str, &'a str, &'a str, &'a str);
type ModelKey<'a> = (CapabilitySubject, &'a str, &'a str);

fn class_ord(c: ModelClass) -> u8 {
    match c {
        ModelClass::Small => 0,
        ModelClass::Medium => 1,
        ModelClass::Large => 2,
    }
}

fn attained_ord(a: AttainedBar) -> u8 {
    match a {
        AttainedBar::BelowBasic => 0,
        AttainedBar::Basic => 1,
        AttainedBar::Standard => 2,
        AttainedBar::High => 3,
    }
}

/// The total order of the ranking rule (point 5 of the module comment).
type RankKey = (CostTier, u8, (u8, u8), u64, String, String);

fn rank_key(
    c: &Candidate,
    class: Option<ModelClass>,
    preference: Option<ModelClass>,
    anonymous_first: bool,
) -> RankKey {
    let anon = u8::from(
        anonymous_first && c.subject == CapabilitySubject::Hosted && !c.provider_is_anonymous,
    );
    let class_key = match (class, preference) {
        (None, _) => (u8::MAX, u8::MAX),
        (Some(k), None) => (class_ord(k), 0),
        (Some(k), Some(p)) => (class_ord(k).abs_diff(class_ord(p)), class_ord(k)),
    };
    (
        c.cost_tier,
        anon,
        class_key,
        c.est_cost_micros,
        c.provider_id.clone(),
        c.model_id.clone(),
    )
}

fn model_ref(c: &Candidate) -> ModelRef {
    ModelRef {
        subject: c.subject,
        provider_id: c.provider_id.clone(),
        model_id: c.model_id.clone(),
    }
}

pub fn resolve(input: &ResolveInput<'_>) -> Resolution {
    let req = input.requirement;
    let range = &req.range;
    let ceiling = input.ceiling.min(range.highest_tier);
    let local_allowed = range.destinations.contains(&Destination::Local);
    let hosted_allowed = range.destinations.contains(&Destination::HostedApi)
        && ceiling >= ExternalAccess::AnonymousPreferred;
    if !local_allowed && !hosted_allowed {
        return Resolution::NoCandidate(NoCandidateReason::NoDestination);
    }
    let required = range.quality_floor.min;
    let task = req.need.task_type.as_str();
    let anonymous_first = ceiling == ExternalAccess::AnonymousPreferred;

    // The cells and the class of each model, indexed once.
    let mut cells: HashMap<CellKey<'_>, &CapabilityRow> = HashMap::new();
    let mut classes: HashMap<ModelKey<'_>, ModelClass> = HashMap::new();
    for row in input.capabilities {
        cells.insert(
            (
                row.subject,
                row.provider_id.as_str(),
                row.model_id.as_str(),
                row.task_type.as_str(),
                row.profile.as_str(),
            ),
            row,
        );
        classes.insert(
            (row.subject, row.provider_id.as_str(), row.model_id.as_str()),
            row.model_class,
        );
    }

    // Destination, readiness and context.
    let mut any_dest_ready = false;
    let mut eligible: Vec<&Candidate> = Vec::new();
    for c in input.candidates {
        let dest_ok = match c.subject {
            CapabilitySubject::Local => local_allowed,
            CapabilitySubject::Hosted => hosted_allowed,
        };
        if !dest_ok || c.readiness != Readiness::Ready {
            continue;
        }
        any_dest_ready = true;
        if c.context_window_tokens != CONTEXT_UNKNOWN
            && c.context_window_tokens < input.min_context_tokens
        {
            continue;
        }
        eligible.push(c);
    }
    if eligible.is_empty() {
        return Resolution::NoCandidate(if any_dest_ready {
            NoCandidateReason::ContextTooSmall
        } else {
            NoCandidateReason::NoneReady
        });
    }

    // Capability: qualify, below the bar, or not measured.
    let profile = input.profile.filter(|p| !p.is_empty());
    let mut qualified: Vec<(RankKey, &Candidate, AttainedBar, Option<ModelClass>)> = Vec::new();
    let mut others: Vec<((u8, u8, RankKey), OptionEntry)> = Vec::new();
    for c in eligible {
        let model_key = (c.subject, c.provider_id.as_str(), c.model_id.as_str());
        let class = classes.get(&model_key).copied();
        let cell = |p: &str| {
            cells
                .get(&(
                    c.subject,
                    c.provider_id.as_str(),
                    c.model_id.as_str(),
                    task,
                    p,
                ))
                .copied()
        };
        let row = profile.and_then(cell).or_else(|| cell(""));
        let key = rank_key(c, class, req.class_preference, anonymous_first);
        match row {
            Some(r) if r.attained_bar.meets(required) => {
                qualified.push((key, c, r.attained_bar, class));
            }
            Some(r) => others.push((
                (0, 3 - attained_ord(r.attained_bar), key),
                OptionEntry {
                    model: model_ref(c),
                    cost_tier: c.cost_tier,
                    est_cost_micros: c.est_cost_micros,
                    standing: Standing::BelowBar(r.attained_bar),
                },
            )),
            None => others.push((
                (1, 0, key),
                OptionEntry {
                    model: model_ref(c),
                    cost_tier: c.cost_tier,
                    est_cost_micros: c.est_cost_micros,
                    standing: Standing::NotMeasured,
                },
            )),
        }
    }

    if qualified.is_empty() {
        others.sort_by(|a, b| a.0.cmp(&b.0));
        let options: Vec<OptionEntry> = others.into_iter().map(|(_, o)| o).collect();
        let best_below_bar = options
            .iter()
            .find(|o| matches!(o.standing, Standing::BelowBar(_)))
            .cloned();
        return Resolution::TooWeak {
            required,
            best_below_bar,
            options,
        };
    }

    qualified.sort_by(|a, b| a.0.cmp(&b.0));
    let to_chosen =
        |(_, c, attained, class): &(RankKey, &Candidate, AttainedBar, Option<ModelClass>)| Chosen {
            model: model_ref(c),
            cost_tier: c.cost_tier,
            est_cost_micros: c.est_cost_micros,
            context_window_tokens: c.context_window_tokens,
            attained: *attained,
            model_class: *class,
        };
    let chosen = to_chosen(&qualified[0]);
    let fallbacks = if chosen.model.subject == CapabilitySubject::Local {
        qualified[1..]
            .iter()
            .filter(|q| q.1.subject == CapabilitySubject::Local)
            .map(to_chosen)
            .collect()
    } else {
        Vec::new()
    };
    Resolution::Selected { chosen, fallbacks }
}

// ---------------------------------------------------------------------------
// The document profile
// ---------------------------------------------------------------------------

/// The shipped profile of the document type a run selected. A shipped type is its
/// own profile. A custom type resolves through its `based_on` to the shipped type
/// it was made from, so a custom type never needs a capability row of its own. An
/// unknown id, or a custom type whose base is not a shipped type, has no profile
/// (None): the run then reads only the task-type-wide cell, which for the shipped
/// data means not measured.
pub fn profile_for_document_type(
    selected: &str,
    shipped: &Registry,
    custom: &[DocumentType],
) -> Option<String> {
    if shipped.document_type(selected).is_some() {
        return Some(selected.to_owned());
    }
    let base = custom
        .iter()
        .find(|t| t.id == selected)?
        .based_on
        .as_deref()?;
    shipped.document_type(base).map(|t| t.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::model_block::config::{
        Difficulty, ModelRange, ModelTaskType, OutputSize, QualityFloor, TaskNeed,
    };
    use crate::conductor::model_block::document_type::registry;
    use crate::conductor::model_capability::Provenance;

    const L_SMALL: &str = "fx-small";
    const L_MEDIUM: &str = "fx-medium";
    const L_LARGE: &str = "fx-large";
    const HOST_A: &str = "fx-host-a";
    const HOST_B: &str = "fx-host-b";

    fn req(
        min: QualityBar,
        destinations: &[Destination],
        tier: ExternalAccess,
        preference: Option<ModelClass>,
    ) -> ModelRequirement {
        ModelRequirement {
            block_stable_id: "fx".to_owned(),
            need: TaskNeed {
                task_type: ModelTaskType::CreativeWriting,
                difficulty: Difficulty::Low,
                output_size: OutputSize::Short,
            },
            range: ModelRange {
                quality_floor: QualityFloor {
                    min,
                    max: QualityBar::High,
                },
                highest_tier: tier,
                destinations: destinations.to_vec(),
            },
            class_preference: preference,
        }
    }

    fn local_req(min: QualityBar) -> ModelRequirement {
        req(min, &[Destination::Local], ExternalAccess::LocalOnly, None)
    }

    fn both_req(min: QualityBar) -> ModelRequirement {
        req(
            min,
            &[Destination::Local, Destination::HostedApi],
            ExternalAccess::AnonymousPreferred,
            None,
        )
    }

    fn local(model: &str) -> Candidate {
        Candidate {
            subject: CapabilitySubject::Local,
            provider_id: String::new(),
            model_id: model.to_owned(),
            readiness: Readiness::Ready,
            context_window_tokens: 8192,
            cost_tier: CostTier::Local,
            est_cost_micros: 0,
            provider_is_anonymous: true,
        }
    }

    fn hosted(provider: &str, model: &str, tier: CostTier, cost: u64) -> Candidate {
        Candidate {
            subject: CapabilitySubject::Hosted,
            provider_id: provider.to_owned(),
            model_id: model.to_owned(),
            readiness: Readiness::Ready,
            context_window_tokens: 128_000,
            cost_tier: tier,
            est_cost_micros: cost,
            provider_is_anonymous: false,
        }
    }

    fn row(
        subject: CapabilitySubject,
        provider: &str,
        model: &str,
        profile: &str,
        bar: AttainedBar,
        class: ModelClass,
    ) -> CapabilityRow {
        CapabilityRow {
            subject,
            provider_id: provider.to_owned(),
            model_id: model.to_owned(),
            task_type: ModelTaskType::CreativeWriting,
            profile: profile.to_owned(),
            attained_bar: bar,
            model_class: class,
            sample_count: 4,
            measured_at: "2026-11-13".to_owned(),
            provenance: Provenance::JasonRead,
            note: None,
        }
    }

    fn lrow(model: &str, profile: &str, bar: AttainedBar, class: ModelClass) -> CapabilityRow {
        row(CapabilitySubject::Local, "", model, profile, bar, class)
    }

    fn hrow(provider: &str, model: &str, profile: &str, bar: AttainedBar) -> CapabilityRow {
        row(
            CapabilitySubject::Hosted,
            provider,
            model,
            profile,
            bar,
            ModelClass::Large,
        )
    }

    fn run(
        r: &ModelRequirement,
        profile: Option<&str>,
        ceiling: ExternalAccess,
        candidates: &[Candidate],
        table: &[CapabilityRow],
    ) -> Resolution {
        resolve(&ResolveInput {
            requirement: r,
            profile,
            ceiling,
            min_context_tokens: 0,
            candidates,
            capabilities: table,
        })
    }

    fn chosen_id(r: &Resolution) -> &str {
        match r {
            Resolution::Selected { chosen, .. } => &chosen.model.model_id,
            other => panic!("expected a selection, got {other:?}"),
        }
    }

    fn three_locals() -> (Vec<Candidate>, Vec<CapabilityRow>) {
        (
            vec![local(L_LARGE), local(L_SMALL), local(L_MEDIUM)],
            vec![
                lrow(L_SMALL, "rma", AttainedBar::Basic, ModelClass::Small),
                lrow(L_MEDIUM, "rma", AttainedBar::Basic, ModelClass::Medium),
                lrow(L_LARGE, "rma", AttainedBar::Basic, ModelClass::Large),
            ],
        )
    }

    // -- ranking ---------------------------------------------------------------

    #[test]
    fn the_smallest_qualifying_local_class_wins_and_the_rest_are_fallbacks_in_order() {
        let (c, t) = three_locals();
        let out = run(
            &local_req(QualityBar::Basic),
            Some("rma"),
            ExternalAccess::LocalOnly,
            &c,
            &t,
        );
        let Resolution::Selected { chosen, fallbacks } = &out else {
            panic!("{out:?}")
        };
        assert_eq!(chosen.model.model_id, L_SMALL);
        assert_eq!(chosen.attained, AttainedBar::Basic);
        assert_eq!(chosen.model_class, Some(ModelClass::Small));
        let ids: Vec<&str> = fallbacks
            .iter()
            .map(|f| f.model.model_id.as_str())
            .collect();
        assert_eq!(ids, [L_MEDIUM, L_LARGE]);
    }

    #[test]
    fn a_class_preference_ranks_and_never_filters() {
        let (c, t) = three_locals();
        for (pref, first) in [
            (ModelClass::Small, L_SMALL),
            (ModelClass::Medium, L_MEDIUM),
            (ModelClass::Large, L_LARGE),
        ] {
            let r = req(
                QualityBar::Basic,
                &[Destination::Local],
                ExternalAccess::LocalOnly,
                Some(pref),
            );
            let out = run(&r, Some("rma"), ExternalAccess::LocalOnly, &c, &t);
            assert_eq!(chosen_id(&out), first, "{pref:?}");
        }
        // The preferred class is not measured: the preference does not make it the
        // choice, and the others still qualify.
        let without_medium: Vec<CapabilityRow> = t
            .iter()
            .filter(|r| r.model_id != L_MEDIUM)
            .cloned()
            .collect();
        let r = req(
            QualityBar::Basic,
            &[Destination::Local],
            ExternalAccess::LocalOnly,
            Some(ModelClass::Medium),
        );
        let out = run(
            &r,
            Some("rma"),
            ExternalAccess::LocalOnly,
            &c,
            &without_medium,
        );
        assert_eq!(chosen_id(&out), L_SMALL, "nearest class that is measured");
    }

    #[test]
    fn a_paid_model_is_never_chosen_while_a_local_one_qualifies() {
        let c = vec![hosted(HOST_A, "fx-m1", CostTier::Free, 0), local(L_LARGE)];
        let t = vec![
            lrow(L_LARGE, "rma", AttainedBar::Basic, ModelClass::Large),
            hrow(HOST_A, "fx-m1", "rma", AttainedBar::High),
        ];
        let out = run(
            &both_req(QualityBar::Basic),
            Some("rma"),
            ExternalAccess::AnonymousPreferred,
            &c,
            &t,
        );
        assert_eq!(
            chosen_id(&out),
            L_LARGE,
            "even a free hosted model ranks after local"
        );
    }

    #[test]
    fn hosted_is_chosen_only_when_no_local_qualifies_and_goes_free_then_plan_then_metered() {
        let c = vec![
            local(L_SMALL),
            hosted(HOST_A, "fx-metered", CostTier::Metered, 900),
            hosted(HOST_B, "fx-plan", CostTier::PlanIncluded, 0),
            hosted(HOST_A, "fx-free", CostTier::Free, 0),
        ];
        let t = vec![
            lrow(L_SMALL, "rma", AttainedBar::BelowBasic, ModelClass::Small),
            hrow(HOST_A, "fx-metered", "rma", AttainedBar::Standard),
            hrow(HOST_B, "fx-plan", "rma", AttainedBar::Standard),
            hrow(HOST_A, "fx-free", "rma", AttainedBar::Standard),
        ];
        let r = both_req(QualityBar::Standard);
        let out = run(&r, Some("rma"), ExternalAccess::AnonymousPreferred, &c, &t);
        assert_eq!(chosen_id(&out), "fx-free");
        let c2: Vec<Candidate> = c
            .iter()
            .filter(|x| x.model_id != "fx-free")
            .cloned()
            .collect();
        let out = run(&r, Some("rma"), ExternalAccess::AnonymousPreferred, &c2, &t);
        assert_eq!(chosen_id(&out), "fx-plan");
        let c3: Vec<Candidate> = c2
            .iter()
            .filter(|x| x.model_id != "fx-plan")
            .cloned()
            .collect();
        let out = run(&r, Some("rma"), ExternalAccess::AnonymousPreferred, &c3, &t);
        assert_eq!(chosen_id(&out), "fx-metered");
        // A hosted choice carries no local fallbacks.
        let Resolution::Selected { fallbacks, .. } = &out else {
            panic!()
        };
        assert!(fallbacks.is_empty());
    }

    #[test]
    fn at_anonymous_preferred_an_anonymous_provider_breaks_a_tie_inside_a_cost_tier_only() {
        let mut anon = hosted(HOST_B, "fx-anon", CostTier::Metered, 500);
        anon.provider_is_anonymous = true;
        let acct = hosted(HOST_A, "fx-account", CostTier::Metered, 100);
        let c = vec![acct, anon.clone()];
        let t = vec![
            hrow(HOST_A, "fx-account", "rma", AttainedBar::High),
            hrow(HOST_B, "fx-anon", "rma", AttainedBar::High),
        ];
        let r = req(
            QualityBar::Basic,
            &[Destination::HostedApi],
            ExternalAccess::AnonymousPreferred,
            None,
        );
        // Dearer but anonymous wins at anonymous_preferred...
        let out = run(&r, Some("rma"), ExternalAccess::AnonymousPreferred, &c, &t);
        assert_eq!(chosen_id(&out), "fx-anon");
        // ...and cost decides at unrestricted.
        let r2 = req(
            QualityBar::Basic,
            &[Destination::HostedApi],
            ExternalAccess::Unrestricted,
            None,
        );
        let out = run(&r2, Some("rma"), ExternalAccess::Unrestricted, &c, &t);
        assert_eq!(chosen_id(&out), "fx-account");
        // It never outranks a cheaper cost tier.
        let free = hosted(HOST_A, "fx-free", CostTier::Free, 0);
        let c2 = vec![anon, free];
        let t2 = vec![
            hrow(HOST_B, "fx-anon", "rma", AttainedBar::High),
            hrow(HOST_A, "fx-free", "rma", AttainedBar::High),
        ];
        let out = run(
            &r,
            Some("rma"),
            ExternalAccess::AnonymousPreferred,
            &c2,
            &t2,
        );
        assert_eq!(chosen_id(&out), "fx-free");
    }

    // -- the bar and the unmeasured --------------------------------------------

    #[test]
    fn nothing_below_the_bar_is_ever_selected_and_the_best_below_it_is_reported() {
        let c = vec![local(L_SMALL), local(L_MEDIUM)];
        let t = vec![
            lrow(L_SMALL, "rma", AttainedBar::BelowBasic, ModelClass::Small),
            lrow(L_MEDIUM, "rma", AttainedBar::Basic, ModelClass::Medium),
        ];
        let out = run(
            &local_req(QualityBar::Standard),
            Some("rma"),
            ExternalAccess::LocalOnly,
            &c,
            &t,
        );
        let Resolution::TooWeak {
            required,
            best_below_bar,
            options,
        } = out
        else {
            panic!("{out:?}")
        };
        assert_eq!(required, QualityBar::Standard);
        let best = best_below_bar.expect("two models were measured");
        assert_eq!(best.model.model_id, L_MEDIUM);
        assert_eq!(best.standing, Standing::BelowBar(AttainedBar::Basic));
        let ids: Vec<&str> = options.iter().map(|o| o.model.model_id.as_str()).collect();
        assert_eq!(ids, [L_MEDIUM, L_SMALL], "highest attainment first");
    }

    /// The pin of the approved plan's point 4 (items.id=896, decisions.id=942
    /// addendum 2 d): a model with no measured cell is excluded from automatic
    /// choice, hosted or local, and is listed as not measured among the options.
    #[test]
    fn an_unmeasured_cell_is_never_chosen_and_is_listed_as_not_measured() {
        // (a) Only unmeasured candidates: TooWeak, both listed NotMeasured, and
        // there is no best-below-bar because nothing was measured.
        let c = vec![local(L_SMALL), hosted(HOST_A, "fx-m1", CostTier::Free, 0)];
        let out = run(
            &both_req(QualityBar::Basic),
            Some("rma"),
            ExternalAccess::AnonymousPreferred,
            &c,
            &[],
        );
        let Resolution::TooWeak {
            best_below_bar,
            options,
            ..
        } = &out
        else {
            panic!("an unmeasured candidate must not be selected: {out:?}")
        };
        assert!(best_below_bar.is_none());
        assert_eq!(options.len(), 2);
        assert!(options.iter().all(|o| o.standing == Standing::NotMeasured));
        let subjects: Vec<CapabilitySubject> = options.iter().map(|o| o.model.subject).collect();
        assert!(subjects.contains(&CapabilitySubject::Local));
        assert!(subjects.contains(&CapabilitySubject::Hosted));

        // (b) An unmeasured candidate that would rank FIRST (smaller class, local)
        // loses to a measured one that qualifies, and is not a fallback either.
        let c = vec![local(L_SMALL), local(L_MEDIUM)];
        let t = vec![lrow(
            L_MEDIUM,
            "rma",
            AttainedBar::Basic,
            ModelClass::Medium,
        )];
        let out = run(
            &local_req(QualityBar::Basic),
            Some("rma"),
            ExternalAccess::LocalOnly,
            &c,
            &t,
        );
        assert_eq!(chosen_id(&out), L_MEDIUM);
        let Resolution::Selected { fallbacks, .. } = &out else {
            panic!()
        };
        assert!(fallbacks.is_empty());

        // (c) An unmeasured hosted model that is free never beats a measured hosted
        // one that qualifies.
        let c = vec![
            hosted(HOST_A, "fx-unmeasured", CostTier::Free, 0),
            hosted(HOST_B, "fx-measured", CostTier::Metered, 900),
        ];
        let t = vec![hrow(HOST_B, "fx-measured", "rma", AttainedBar::Standard)];
        let out = run(
            &req(
                QualityBar::Basic,
                &[Destination::HostedApi],
                ExternalAccess::Unrestricted,
                None,
            ),
            Some("rma"),
            ExternalAccess::Unrestricted,
            &c,
            &t,
        );
        assert_eq!(chosen_id(&out), "fx-measured");

        // (d) A model measured for another profile is unmeasured for this one: the
        // cell is keyed by profile, and a cover letter's score says nothing about an
        // RMA letter.
        let c = vec![local(L_SMALL)];
        let t = vec![lrow(
            L_SMALL,
            "other-profile",
            AttainedBar::High,
            ModelClass::Small,
        )];
        let out = run(
            &local_req(QualityBar::Basic),
            Some("rma"),
            ExternalAccess::LocalOnly,
            &c,
            &t,
        );
        let Resolution::TooWeak { options, .. } = &out else {
            panic!("{out:?}")
        };
        assert_eq!(options[0].standing, Standing::NotMeasured);

        // (e) A measured task type other than the block's is not used either.
        let mut other_task = lrow(L_SMALL, "rma", AttainedBar::High, ModelClass::Small);
        other_task.task_type = ModelTaskType::Code;
        let out = run(
            &local_req(QualityBar::Basic),
            Some("rma"),
            ExternalAccess::LocalOnly,
            &c,
            &[other_task],
        );
        assert!(matches!(out, Resolution::TooWeak { .. }));
    }

    #[test]
    fn the_exact_profile_wins_then_the_task_type_wide_cell_and_never_another_profile() {
        let c = vec![local(L_SMALL)];
        let wide = lrow(L_SMALL, "", AttainedBar::Basic, ModelClass::Small);
        let exact_low = lrow(L_SMALL, "rma", AttainedBar::BelowBasic, ModelClass::Small);
        let r = local_req(QualityBar::Basic);
        let at = |profile: Option<&str>, t: &[CapabilityRow]| {
            run(&r, profile, ExternalAccess::LocalOnly, &c, t)
        };
        // The exact cell decides when there is one, even if the wide cell is better.
        assert!(matches!(
            at(Some("rma"), &[wide.clone(), exact_low]),
            Resolution::TooWeak { .. }
        ));
        // No exact cell: the task-type-wide cell stands in.
        assert!(matches!(
            at(Some("rma"), std::slice::from_ref(&wide)),
            Resolution::Selected { .. }
        ));
        // No profile for the run, or an empty one: only the wide cell is read.
        assert!(matches!(
            at(None, std::slice::from_ref(&wide)),
            Resolution::Selected { .. }
        ));
        assert!(matches!(at(Some(""), &[wide]), Resolution::Selected { .. }));
        let profiled = lrow(L_SMALL, "rma", AttainedBar::High, ModelClass::Small);
        assert!(matches!(at(None, &[profiled]), Resolution::TooWeak { .. }));
    }

    // -- destinations, readiness, context --------------------------------------

    #[test]
    fn the_destinations_and_the_ceiling_decide_what_is_a_candidate() {
        let c = vec![hosted(HOST_A, "fx-m1", CostTier::Free, 0)];
        let t = vec![hrow(HOST_A, "fx-m1", "rma", AttainedBar::High)];
        let hosted_only =
            |tier: ExternalAccess| req(QualityBar::Basic, &[Destination::HostedApi], tier, None);
        // Allowed: the requirement and the run both reach hosted.
        assert!(matches!(
            run(
                &hosted_only(ExternalAccess::AnonymousPreferred),
                Some("rma"),
                ExternalAccess::Unrestricted,
                &c,
                &t
            ),
            Resolution::Selected { .. }
        ));
        // The run's ceiling is lower than the requirement's: it wins.
        for low in [ExternalAccess::LocalOnly, ExternalAccess::AnonymousRequired] {
            assert_eq!(
                run(
                    &hosted_only(ExternalAccess::Unrestricted),
                    Some("rma"),
                    low,
                    &c,
                    &t
                ),
                Resolution::NoCandidate(NoCandidateReason::NoDestination),
                "{low:?}"
            );
        }
        // The requirement's own highest tier narrows a higher run ceiling.
        assert_eq!(
            run(
                &hosted_only(ExternalAccess::AnonymousRequired),
                Some("rma"),
                ExternalAccess::Unrestricted,
                &c,
                &t
            ),
            Resolution::NoCandidate(NoCandidateReason::NoDestination)
        );
        // A local-only requirement ignores a hosted candidate entirely.
        let cl = vec![hosted(HOST_A, "fx-m1", CostTier::Free, 0), local(L_SMALL)];
        let tl = vec![
            hrow(HOST_A, "fx-m1", "rma", AttainedBar::High),
            lrow(L_SMALL, "rma", AttainedBar::Basic, ModelClass::Small),
        ];
        let out = run(
            &req(
                QualityBar::Basic,
                &[Destination::Local],
                ExternalAccess::Unrestricted,
                None,
            ),
            Some("rma"),
            ExternalAccess::Unrestricted,
            &cl,
            &tl,
        );
        assert_eq!(chosen_id(&out), L_SMALL);
        // Manual hand-off is not a candidate: a requirement that allows only it has
        // no destination here.
        assert_eq!(
            run(
                &req(
                    QualityBar::Basic,
                    &[Destination::ManualHandoff],
                    ExternalAccess::AnonymousRequired,
                    None
                ),
                Some("rma"),
                ExternalAccess::Unrestricted,
                &cl,
                &tl
            ),
            Resolution::NoCandidate(NoCandidateReason::NoDestination)
        );
    }

    #[test]
    fn only_ready_candidates_count_and_a_small_window_drops_a_candidate() {
        let (mut c, t) = three_locals();
        for readiness in [
            Readiness::NotInstalled,
            Readiness::DisabledByUser,
            Readiness::NoKey,
            Readiness::NotConsented,
            Readiness::PriceUnknown,
            Readiness::Retired,
        ] {
            for x in c.iter_mut() {
                x.readiness = readiness;
            }
            assert_eq!(
                run(
                    &local_req(QualityBar::Basic),
                    Some("rma"),
                    ExternalAccess::LocalOnly,
                    &c,
                    &t
                ),
                Resolution::NoCandidate(NoCandidateReason::NoneReady),
                "{readiness:?}"
            );
        }
        // One ready model among several that are not.
        c[2].readiness = Readiness::Ready;
        let out = run(
            &local_req(QualityBar::Basic),
            Some("rma"),
            ExternalAccess::LocalOnly,
            &c,
            &t,
        );
        assert_eq!(chosen_id(&out), c[2].model_id);

        // Context: the small window is dropped, an unknown window passes.
        let (mut c, t) = three_locals();
        for x in c.iter_mut() {
            x.context_window_tokens = if x.model_id == L_SMALL { 2048 } else { 16_384 };
        }
        let with = |min: u32, c: &[Candidate]| {
            resolve(&ResolveInput {
                requirement: &local_req(QualityBar::Basic),
                profile: Some("rma"),
                ceiling: ExternalAccess::LocalOnly,
                min_context_tokens: min,
                candidates: c,
                capabilities: &t,
            })
        };
        assert_eq!(chosen_id(&with(4096, &c)), L_MEDIUM);
        assert_eq!(
            with(1_000_000, &c),
            Resolution::NoCandidate(NoCandidateReason::ContextTooSmall)
        );
        for x in c.iter_mut() {
            x.context_window_tokens = CONTEXT_UNKNOWN;
        }
        assert_eq!(chosen_id(&with(1_000_000, &c)), L_SMALL);
    }

    #[test]
    fn no_candidates_at_all_is_none_ready() {
        assert_eq!(
            run(
                &local_req(QualityBar::Basic),
                None,
                ExternalAccess::LocalOnly,
                &[],
                &[]
            ),
            Resolution::NoCandidate(NoCandidateReason::NoneReady)
        );
    }

    // -- determinism and the invariants ----------------------------------------

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn shuffle<T>(&mut self, v: &mut [T]) {
            for i in (1..v.len()).rev() {
                let j = self.below(i as u64 + 1) as usize;
                v.swap(i, j);
            }
        }
    }

    fn pick<T: Copy>(rng: &mut Lcg, items: &[T]) -> T {
        items[rng.below(items.len() as u64) as usize]
    }

    /// An independent reading of the cell rule, by linear scan, for the invariants.
    fn standing_of(
        c: &Candidate,
        profile: Option<&str>,
        required: QualityBar,
        table: &[CapabilityRow],
    ) -> Option<bool> {
        let find = |p: &str| {
            table.iter().find(|r| {
                r.subject == c.subject
                    && r.provider_id == c.provider_id
                    && r.model_id == c.model_id
                    && r.task_type == ModelTaskType::CreativeWriting
                    && r.profile == p
            })
        };
        let row = profile
            .filter(|p| !p.is_empty())
            .and_then(find)
            .or_else(|| find(""));
        row.map(|r| r.attained_bar.meets(required))
    }

    #[test]
    fn the_invariants_hold_over_many_random_cases_and_the_order_of_inputs_is_irrelevant() {
        let mut rng = Lcg(0x5EED_0896);
        let bars = [QualityBar::Basic, QualityBar::Standard, QualityBar::High];
        let tiers = [
            ExternalAccess::LocalOnly,
            ExternalAccess::AnonymousRequired,
            ExternalAccess::AnonymousPreferred,
            ExternalAccess::AnonymousPreferred,
            ExternalAccess::Unrestricted,
            ExternalAccess::Unrestricted,
        ];
        let attained = [
            AttainedBar::BelowBasic,
            AttainedBar::Basic,
            AttainedBar::Standard,
            AttainedBar::High,
        ];
        let classes = [ModelClass::Small, ModelClass::Medium, ModelClass::Large];
        let readiness = [
            Readiness::Ready,
            Readiness::Ready,
            Readiness::Ready,
            Readiness::Ready,
            Readiness::Ready,
            Readiness::NoKey,
            Readiness::NotInstalled,
        ];
        let costs = [CostTier::Free, CostTier::PlanIncluded, CostTier::Metered];
        let profiles = [None, Some("rma"), Some("cover"), Some("")];
        let dest_sets: [&[Destination]; 6] = [
            &[Destination::Local],
            &[Destination::HostedApi],
            &[Destination::Local, Destination::HostedApi],
            &[Destination::Local, Destination::HostedApi],
            &[Destination::HostedApi],
            &[Destination::ManualHandoff],
        ];

        // How often each kind of answer came up, so the loop cannot pass by only
        // ever producing one.
        let mut seen = [0usize; 5];
        for case in 0..4000 {
            let min = pick(&mut rng, &bars);
            let range_tier = pick(&mut rng, &tiers);
            let ceiling = pick(&mut rng, &tiers);
            let dests = pick(&mut rng, &dest_sets);
            let preference = if rng.below(2) == 0 {
                None
            } else {
                Some(pick(&mut rng, &classes))
            };
            let r = req(min, dests, range_tier, preference);
            let profile = pick(&mut rng, &profiles);
            let min_ctx = [0u32, 0, 4096, 100_000][rng.below(4) as usize];

            let mut candidates = Vec::new();
            let mut table = Vec::new();
            for i in 0..=rng.below(7) {
                let is_local = rng.below(2) == 0;
                let mut c = if is_local {
                    local(&format!("fx-l{i}"))
                } else {
                    let mut h = hosted(
                        &format!("fx-p{}", rng.below(3)),
                        &format!("fx-h{i}"),
                        pick(&mut rng, &costs),
                        rng.below(2000),
                    );
                    h.provider_is_anonymous = rng.below(2) == 0;
                    h
                };
                c.readiness = pick(&mut rng, &readiness);
                c.context_window_tokens =
                    [2048, 8192, 128_000, CONTEXT_UNKNOWN][rng.below(4) as usize];
                let class = pick(&mut rng, &classes);
                for p in ["", "rma", "cover"] {
                    if rng.below(2) == 0 {
                        table.push(row(
                            c.subject,
                            &c.provider_id,
                            &c.model_id,
                            p,
                            pick(&mut rng, &attained),
                            class,
                        ));
                    }
                }
                candidates.push(c);
            }

            let input = ResolveInput {
                requirement: &r,
                profile,
                ceiling,
                min_context_tokens: min_ctx,
                candidates: &candidates,
                capabilities: &table,
            };
            let out = resolve(&input);

            // Independent facts.
            let eff = ceiling.min(range_tier);
            let local_ok = dests.contains(&Destination::Local);
            let hosted_ok = dests.contains(&Destination::HostedApi)
                && eff >= ExternalAccess::AnonymousPreferred;
            let eligible: Vec<&Candidate> = candidates
                .iter()
                .filter(|c| {
                    (match c.subject {
                        CapabilitySubject::Local => local_ok,
                        CapabilitySubject::Hosted => hosted_ok,
                    }) && c.readiness == Readiness::Ready
                        && (c.context_window_tokens == CONTEXT_UNKNOWN
                            || c.context_window_tokens >= min_ctx)
                })
                .collect();
            let qualifies = |c: &Candidate| standing_of(c, profile, min, &table) == Some(true);
            let find = |m: &ModelRef| {
                candidates
                    .iter()
                    .find(|c| {
                        c.subject == m.subject
                            && c.provider_id == m.provider_id
                            && c.model_id == m.model_id
                    })
                    .unwrap_or_else(|| panic!("case {case}: result names a model not offered"))
            };

            match &out {
                Resolution::NoCandidate(_) => {
                    seen[0] += 1;
                    assert!(eligible.is_empty(), "case {case}: {out:?}");
                }
                Resolution::Selected { chosen, fallbacks } => {
                    let c = find(&chosen.model);
                    seen[if chosen.model.subject == CapabilitySubject::Local {
                        1
                    } else {
                        2
                    }] += 1;
                    seen[4] += usize::from(!fallbacks.is_empty());
                    assert!(eligible.iter().any(|e| std::ptr::eq(*e, c)), "case {case}");
                    assert!(qualifies(c), "case {case}: selected a model not at the bar");
                    assert!(
                        chosen.attained.meets(min),
                        "case {case}: attained below the bar"
                    );
                    if chosen.model.subject == CapabilitySubject::Hosted {
                        assert!(
                            !eligible
                                .iter()
                                .any(|e| e.subject == CapabilitySubject::Local && qualifies(e)),
                            "case {case}: a hosted model chosen while a local one qualifies"
                        );
                        assert!(fallbacks.is_empty());
                    }
                    for f in fallbacks {
                        assert_eq!(f.model.subject, CapabilitySubject::Local);
                        assert!(qualifies(find(&f.model)), "case {case}");
                        assert_ne!(f.model, chosen.model);
                    }
                }
                Resolution::TooWeak {
                    options, required, ..
                } => {
                    seen[3] += 1;
                    assert_eq!(*required, min);
                    assert!(
                        !eligible.iter().any(|e| qualifies(e)),
                        "case {case}: TooWeak while a candidate qualifies"
                    );
                    assert_eq!(options.len(), eligible.len(), "case {case}");
                    for o in options {
                        let c = find(&o.model);
                        match standing_of(c, profile, min, &table) {
                            None => assert_eq!(o.standing, Standing::NotMeasured, "case {case}"),
                            Some(false) => {
                                assert!(matches!(o.standing, Standing::BelowBar(_)), "case {case}")
                            }
                            Some(true) => panic!("case {case}: a qualifier among the options"),
                        }
                    }
                }
            }

            // The order candidates and rows arrive in does not matter.
            let mut c2 = candidates.clone();
            let mut t2 = table.clone();
            rng.shuffle(&mut c2);
            rng.shuffle(&mut t2);
            let again = resolve(&ResolveInput {
                requirement: &r,
                profile,
                ceiling,
                min_context_tokens: min_ctx,
                candidates: &c2,
                capabilities: &t2,
            });
            assert_eq!(out, again, "case {case}: the answer depends on input order");
        }
        assert!(
            seen.iter().all(|n| *n >= 20),
            "every kind of answer must be well covered: {seen:?}"
        );
    }

    // -- the document profile --------------------------------------------------

    #[test]
    fn a_shipped_type_is_its_own_profile_and_a_custom_type_resolves_through_based_on() {
        let shipped = registry().expect("bundled registry");
        let base = shipped
            .document_type("cover_letter")
            .expect("shipped")
            .clone();
        let mut custom = base.clone();
        custom.id = "my-letters".to_owned();
        custom.name = "My letters".to_owned();
        custom.based_on = Some("cover_letter".to_owned());
        let customs = [custom.clone()];

        assert_eq!(
            profile_for_document_type("rma_letter", shipped, &customs).as_deref(),
            Some("rma_letter")
        );
        assert_eq!(
            profile_for_document_type("my-letters", shipped, &customs).as_deref(),
            Some("cover_letter")
        );
        // Unknown, or a custom type whose base is not a shipped type: no profile.
        assert_eq!(profile_for_document_type("nope", shipped, &customs), None);
        let mut orphan = custom.clone();
        orphan.id = "orphan".to_owned();
        orphan.based_on = Some("my-letters".to_owned());
        assert_eq!(
            profile_for_document_type("orphan", shipped, &[custom.clone(), orphan]),
            None,
            "a chain through another custom type does not resolve"
        );
        let mut no_base = custom;
        no_base.id = "free".to_owned();
        no_base.based_on = None;
        assert_eq!(profile_for_document_type("free", shipped, &[no_base]), None);
        // The shipped type wins over a custom type that reuses its id.
        let mut shadow = base;
        shadow.based_on = Some("essay_section".to_owned());
        assert_eq!(
            profile_for_document_type("cover_letter", shipped, &[shadow]).as_deref(),
            Some("cover_letter")
        );
    }

    #[test]
    fn every_profile_in_the_shipped_capability_data_is_a_shipped_document_type() {
        let shipped = registry().unwrap();
        let doc = crate::provider_catalog::format::parse_catalog(
            crate::provider_catalog::BASELINE_CATALOG_JSON.as_bytes(),
        )
        .unwrap();
        assert!(!doc.capabilities.is_empty());
        for c in &doc.capabilities {
            if let Some(p) = &c.profile {
                assert_eq!(
                    profile_for_document_type(p, shipped, &[]).as_deref(),
                    Some(p.as_str())
                );
            }
        }
    }

    // -- scans of this file ----------------------------------------------------

    fn code_of_this_file() -> String {
        let src = include_str!("model_resolver.rs");
        src.split("#[cfg(test)]").next().unwrap().to_owned()
    }

    /// The pin of the approved plan's point 3 and decisions.id=872 and 875: no
    /// model or provider is named in the resolver, comments included.
    #[test]
    fn no_model_or_provider_id_is_named_in_the_resolver() {
        let code = code_of_this_file().to_lowercase();
        let mut ids: Vec<String> = crate::providers::curated_models::CURATED_LOCAL_MODEL_TAGS
            .iter()
            .map(|t| (*t).to_owned())
            .collect();
        ids.extend(
            crate::providers::evaluation::RELEASE_1_MODELS
                .iter()
                .map(|t| (*t).to_owned()),
        );
        let doc: serde_json::Value =
            serde_json::from_str(crate::provider_catalog::BASELINE_CATALOG_JSON).unwrap();
        for section in ["providers", "models", "capabilities"] {
            for entry in doc[section].as_array().unwrap() {
                for key in ["id", "provider_id", "model_id"] {
                    if let Some(v) = entry.get(key).and_then(|v| v.as_str()) {
                        ids.push(v.to_owned());
                    }
                }
            }
        }
        assert!(ids.len() > 20, "the scan must have something to look for");
        for id in ids {
            assert!(
                !code.contains(&id.to_lowercase()),
                "the resolver names '{id}': ids arrive in the inputs and are never written here"
            );
        }
        for family in [
            "llama",
            "qwen",
            "gemma",
            "gpt",
            "claude",
            "gemini",
            "mistral",
            "deepseek",
            "groq",
            "ollama",
            "openrouter",
        ] {
            assert!(!code.contains(family), "'{family}' in the resolver");
        }
    }

    #[test]
    fn the_resolver_is_pure_and_carries_no_text_into_a_call() {
        let code = code_of_this_file();
        for forbidden in [
            "sqlx",
            "tokio",
            "std::fs",
            "std::env",
            "std::net",
            "std::thread",
            "reqwest",
            "log::",
            "Instant",
            "SystemTime",
            "chrono",
            "static mut",
            "Mutex",
            "OnceLock",
            "async fn",
            ".await",
        ] {
            assert!(
                !code.contains(forbidden),
                "'{forbidden}' in the resolver: it must stay pure"
            );
        }
        // Nothing here builds text for a model: no prompt, no system text.
        let lower = code.to_lowercase();
        for word in ["prompt", "system", "format!("] {
            assert!(!lower.contains(word), "'{word}' in the resolver");
        }
    }
}
