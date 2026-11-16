// src-tauri/src/conductor/run_plan.rs
//
// items.id=901 stage 2 (decisions.id=942 architecture point 4 and rulings Q2 and Q3,
// plan label B4): the RUN PLAN. Before a run starts, this module works out what the run
// could do with its model-block steps: which model each one would use, which other
// strategies the user could choose, what each could cost, and what has to be approved.
// It computes. It enforces nothing and approves nothing: the approval store, the
// executor check and the IPC commands are later stages, and a plan is never stored (only
// a user's approval of one is).
//
// SCOPE (ruling R-B). The plan covers steps expanded from a model block, the steps that
// carry a `requirement`. Every plan step has a `kind` (only ModelBlock exists). A Focus
// with no such step has NO plan: has_planned_step() is false and build() returns before
// any read of any kind, so no existing Focus changes. Ordinary external steps and API
// Chat are not plan-gated.
//
// WHAT THE PLAN READS. The Focus definition AFTER the run-start step (so the requirement
// ranges are already narrowed by tighten() and by the mode profile), the run's launch
// resolution (document type and launch values), the Focus-level ceiling, and per step the
// candidates and capability rows the existing gatherer finds. A mode profile can only
// remove destinations, tiers or quality range, so it can only remove offers and approval
// needs; nothing here reads a launch value as an approval.
//
// REACHABLE PATHS (ruling R-C). The plan re-runs the exact enumeration LOAD ran
// (lifecycle::composition_paths, the MAX_PATHS cap and the guard analysis untouched) and
// filters it: a path is reachable when, for every launch name it models, every guard
// clause on that name answers the same for the path's modelled value as for the run's
// real value. A guard on any other name (a runtime output, a session name) is unknown at
// plan time, so BOTH branches stay. A filter cannot add a path, so plan-time paths are at
// most LOAD-time paths, which LOAD already held to MAX_PATHS: the cap cannot be reached
// at plan time unless the shipped data changed after LOAD. It is still handled, closed:
// PlanError::TooManyPaths, no plan, nothing runs.
//
// STRATEGIES. Local, paid API, manual hand-off, each listed or not with a reason id (so a
// screen can say why). Manual hand-off is listed only when EVERY reachable path has a
// cb-15 starter row and no model call. The plan switches no row and adds no bound name:
// for another strategy it lists, as ids, the mode answers under which the Focus would
// take a different one, and the screen sends the user to submit_launch_answers and asks
// for the plan again.
//
// OFFERS. Per step, every candidate the range and ceiling permit, with where it stands
// against the required bar, computed by running the pure resolver once per candidate (so
// this module never re-implements the cell rule). The resolver never picks an unmeasured
// model by itself, and neither does the plan: a hosted model or an uncalibrated local
// model is OFFERED, and only a user's explicit pin ever selects it. quality_floor.max is
// a hint only (an offer already above it is flagged and sorted last, never filtered).
// PlanIncluded is never produced: a hosted model is Free or Metered, never "already
// paid for".
//
// COST. Cost does not depend on quality, so a model with no measured score still gets an
// estimate: the gate's own worst case (spend::estimate::worst_case) over the user's price
// (override first) with the step's input bound and the model's output limit. A priced
// model with no output limit anywhere has no estimate (the call would fail closed) and
// is not selectable. A path costs the sum of its steps, and the plan costs its dearest
// path, because a run takes one path and the union is covered by the dearest.
//
// THE DIGEST (SHA-256, tag rp1) is STRUCTURE ONLY: what the Focus could do, never how
// likely, how much, or what happens to be installed today. It covers the Focus, the
// document profile, per reachable planned step its destinations, highest tier, quality
// range and hosted_possible (the range allows hosted_api and the ceiling reaches
// anonymous_preferred), whether manual hand-off is listed (a fact of the reachable
// paths alone), and any pins. It does NOT depend on readiness, installed models,
// keys, consents, prices, estimates, the resolver's answer, the listing reasons (they
// stay in the plan for display), prompt text, the user's input or settings. A narrower
// mode changes it and re-asks (over-asking, never under-asking).
//
// PRIVACY CONTRACT. A plan holds ids, closed vocabulary words and numbers only: never a
// prompt, a draft, a persona fact, a seed value or a model response. This module names
// none of them, names no model or provider, calls nothing and logs nothing (scan tests
// pin all three).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::conductor::bound_spec::{self, Source};
use crate::conductor::guard::{CompositionError, Path};
use crate::conductor::launch_config::{LaunchConfig, QuestionKind};
use crate::conductor::lifecycle::{composition_paths, FocusDefinition};
use crate::conductor::model_block::config::{Destination, ModelRequirement, QualityBar};
use crate::conductor::model_block::document_type::Registry;
use crate::conductor::model_capability::{AttainedBar, CapabilitySubject};
use crate::conductor::model_resolver::{
    profile_for_document_type, resolve as resolve_model, Candidate, CostTier, NoCandidateReason,
    Resolution, ResolveInput, Standing,
};
use crate::conductor::model_resolver_input::{gather, reach, GatherInput, Gathered, Reach};
use crate::conductor::run_settings::{self, LaunchResolution, StoredAnswers};
use crate::conductor::tokens::{ActionDefinition, ExternalAccess};

/// Version tag of the digest's canonical encoding. Changing the encoding changes the tag,
/// so an older approval never matches a newer plan (it re-asks).
pub const DIGEST_TAG: &str = "rp1";

/// The output tokens a step with no `num_predict` is priced at when it is a local step's
/// output that feeds a later input bound (the executor's own default).
const DEFAULT_STEP_OUTPUT_TOKENS: u32 = 1024;

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// What kind of step a plan line is. Only ModelBlock is built. An ordinary external step
/// would be a second kind on the same list (its envelope: the kind, the action id, the
/// provider the user prefers and the ceiling tier), checked by the same executor test
/// keyed by (action id, kind) and counted by the same ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepKind {
    ModelBlock,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::ModelBlock => "model_block",
        }
    }
}

/// A strategy a Focus run can take as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StrategyKind {
    Local,
    PaidApi,
    ManualHandoff,
}

impl StrategyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StrategyKind::Local => "local",
            StrategyKind::PaidApi => "paid_api",
            StrategyKind::ManualHandoff => "manual_handoff",
        }
    }
}

/// What one offered model is, for the step it is offered to. Local: a local model the
/// resolver would accept. LocalUnmeasured: a local model nobody has measured for this
/// task (only an explicit pin selects it). PaidApi: a hosted model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfferStrategy {
    Local,
    LocalUnmeasured,
    PaidApi,
}

impl OfferStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            OfferStrategy::Local => "local",
            OfferStrategy::LocalUnmeasured => "local_unmeasured",
            OfferStrategy::PaidApi => "paid_api",
        }
    }
}

/// Where an offered model stands against the step's required bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferStanding {
    Qualifies(AttainedBar),
    BelowBar(AttainedBar),
    NotMeasured,
}

impl OfferStanding {
    fn rank(self) -> u8 {
        match self {
            OfferStanding::Qualifies(_) => 0,
            OfferStanding::NotMeasured => 1,
            OfferStanding::BelowBar(_) => 2,
        }
    }
}

/// One explicit choice of a user: use this model for this step instead of letting the
/// resolver choose. Only an unmeasured local model or a paid API model can be pinned.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Pin {
    pub action_id: String,
    pub strategy: OfferStrategy,
    /// Empty for a local model.
    pub provider_id: String,
    pub model_id: String,
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub strategy: OfferStrategy,
    pub subject: CapabilitySubject,
    /// Empty for a local model.
    pub provider_id: String,
    pub model_id: String,
    pub cost_tier: CostTier,
    /// The gate's worst case in micro-USD, or None when there is no estimate.
    pub est_cost_micros: Option<u64>,
    pub standing: OfferStanding,
    /// Qualifies at a bar above quality_floor.max: more than the step is worth paying for.
    pub above_worth_bar: bool,
    pub selectable: bool,
    /// Why it is not selectable: below_bar or no_estimate.
    pub not_selectable_reason: Option<&'static str>,
    /// Selecting it takes the user's explicit approval (a paid model, an unmeasured one).
    pub needs_approval: bool,
}

/// What the resolver answered for the step with no pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolverAnswer {
    Selected {
        subject: CapabilitySubject,
        provider_id: String,
        model_id: String,
        cost_tier: CostTier,
        est_cost_micros: u64,
    },
    TooWeak {
        required: QualityBar,
    },
    NoCandidate(NoCandidateReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub action_id: String,
    pub display_name: String,
    pub kind: StepKind,
    pub task_type: &'static str,
    /// quality_floor.min: the bar the resolver requires.
    pub required_bar: QualityBar,
    /// quality_floor.max: the highest bar worth paying for (a hint).
    pub worth_paying_bar: QualityBar,
    pub destinations: Vec<Destination>,
    pub highest_tier: ExternalAccess,
    /// The range allows hosted_api and the run's ceiling reaches anonymous_preferred.
    pub hosted_possible: bool,
    pub answer: ResolverAnswer,
    pub offers: Vec<Offer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyListing {
    pub kind: StrategyKind,
    pub listed: bool,
    pub reason: &'static str,
}

/// Under another mode answer the Focus would take this strategy (None: could not be
/// worked out, with the reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeAlternative {
    pub mode_option_id: String,
    pub kind: Option<StrategyKind>,
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPlan {
    pub focus_id: String,
    pub profile: Option<String>,
    /// Reachable planned steps, in row order.
    pub steps: Vec<PlanStep>,
    pub strategies: Vec<StrategyListing>,
    pub alternatives: Vec<ModeAlternative>,
    /// Some reachable planned step may go hosted: the run needs an explicit approval.
    pub needs_approval: bool,
    /// Manual hand-off is listed: every reachable path has a cb-15 starter and no model
    /// call. A fact of the paths alone, which is why the digest may carry it (the other
    /// strategy listings depend on readiness and the resolver and are not in the digest).
    pub handoff_listed: bool,
    /// The reachable paths, each as the indices into `steps` it runs (identical sets once).
    pub path_steps: Vec<Vec<usize>>,
    /// Digest of the plan with no pins (what get_run_plan shows).
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("more than the guard path cap of execution paths: no plan is offered")]
    TooManyPaths,
    #[error("the Focus's guards could not be analysed: {0}")]
    Paths(String),
    #[error("no execution path is reachable under the launch answers")]
    NoReachablePath,
    #[error("the plan was given no inputs for step '{0}'")]
    MissingInputs(String),
    #[error("the launch configuration could not be read: {0}")]
    LaunchConfig(String),
}

// ---------------------------------------------------------------------------
// Which steps and paths
// ---------------------------------------------------------------------------

/// Whether the Focus has any step the plan covers. False: no plan, and nothing at all is
/// read for it.
pub fn has_planned_step(steps: &[ActionDefinition]) -> bool {
    steps.iter().any(|s| s.requirement.is_some())
}

/// A step's effective access: its own override tightened by the Focus ceiling, else the
/// ceiling (the same rule lifecycle applies per step).
pub fn effective_access(step: &ActionDefinition, focus_ceiling: ExternalAccess) -> ExternalAccess {
    step.external_access_override
        .map(|o| o.min(focus_ceiling))
        .unwrap_or(focus_ceiling)
}

/// The paths the run can take under its launch answers: the LOAD enumeration filtered by
/// the launch names (see the module comment).
pub fn reachable_paths<'a>(
    steps: &[ActionDefinition],
    paths: &'a [Path],
    launch: &LaunchResolution,
) -> Vec<&'a Path> {
    let clauses: Vec<&crate::conductor::guard::Guard> = steps
        .iter()
        .flat_map(|s| s.guards())
        .filter(|g| bound_spec::LAUNCH_NAMES.contains(&g.var.as_str()))
        .collect();
    paths
        .iter()
        .filter(|p| {
            p.values.iter().all(|(var, modelled)| {
                if !bound_spec::LAUNCH_NAMES.contains(&var.as_str()) {
                    return true;
                }
                let real = launch.values.get(var.as_str());
                let state = |v: Option<&String>| -> HashMap<String, String> {
                    v.map(|v| HashMap::from([(var.clone(), v.clone())]))
                        .unwrap_or_default()
                };
                let (real_state, modelled_state) = (state(real), state(modelled.as_ref()));
                clauses
                    .iter()
                    .filter(|g| g.var == *var)
                    .all(|g| g.evaluate(&real_state) == g.evaluate(&modelled_state))
            })
        })
        .collect()
}

/// A row that makes a model call: a generation step (not a pure block, not a render
/// block, not a store writer). Ordinary and expanded model-block steps are both these.
fn makes_model_call(s: &ActionDefinition) -> bool {
    s.block_stable_id.is_none() && s.store_op.is_none()
}

#[derive(Debug, Clone, Copy, Default)]
struct PathFacts {
    has_render: bool,
    has_model_call: bool,
}

fn path_facts(steps: &[ActionDefinition], path: &Path) -> PathFacts {
    let mut f = PathFacts::default();
    for &i in &path.rows {
        let s = &steps[i];
        f.has_render |= s.is_render_block();
        f.has_model_call |= makes_model_call(s);
    }
    f
}

/// Every reachable path is a hand-off path: it has a cb-15 starter and no model call.
fn handoff_listing(steps: &[ActionDefinition], paths: &[&Path]) -> (bool, &'static str) {
    if paths.is_empty() {
        return (false, "no_handoff_row");
    }
    let facts: Vec<PathFacts> = paths.iter().map(|p| path_facts(steps, p)).collect();
    if facts.iter().all(|f| f.has_render && !f.has_model_call) {
        (true, "every_path_is_handoff")
    } else if facts.iter().any(|f| f.has_model_call) {
        (false, "a_path_calls_a_model")
    } else if facts.iter().any(|f| f.has_render) {
        (false, "a_path_has_no_starter")
    } else {
        (false, "no_handoff_row")
    }
}

/// The planned steps (indices into `steps`) on any of the reachable paths, in row order.
pub fn reachable_planned_rows(steps: &[ActionDefinition], paths: &[&Path]) -> Vec<usize> {
    let on_a_path: HashSet<usize> = paths.iter().flat_map(|p| p.rows.iter().copied()).collect();
    (0..steps.len())
        .filter(|i| on_a_path.contains(i) && steps[*i].requirement.is_some())
        .collect()
}

// ---------------------------------------------------------------------------
// Inputs of one planned step (what the async builder gathers)
// ---------------------------------------------------------------------------

pub struct StepInputs {
    pub gathered: Gathered,
    /// A lower bound on the tokens the call needs (the template's fixed text), as the
    /// executor passes to the resolver.
    pub min_context_tokens: u32,
}

pub struct PlanInput<'a> {
    pub def: &'a FocusDefinition,
    /// The LOAD enumeration over def.steps (lifecycle::composition_paths).
    pub paths: &'a [Path],
    pub launch: &'a LaunchResolution,
    /// The Focus-level ceiling: min(the user's maximum, the Focus's own).
    pub focus_ceiling: ExternalAccess,
    /// The shipped document profile of the run's document type, if any.
    pub profile: Option<&'a str>,
    /// Inputs per reachable planned step, by action id.
    pub inputs: &'a HashMap<String, StepInputs>,
    pub alternatives: Vec<ModeAlternative>,
}

// ---------------------------------------------------------------------------
// compute (pure)
// ---------------------------------------------------------------------------

/// Work out the plan. Pure: no I/O, no clock, no state.
pub fn compute(input: &PlanInput<'_>) -> Result<RunPlan, PlanError> {
    let steps = &input.def.steps;
    let reachable = reachable_paths(steps, input.paths, input.launch);
    if reachable.is_empty() {
        return Err(PlanError::NoReachablePath);
    }
    let rows = reachable_planned_rows(steps, &reachable);

    let mut plan_steps: Vec<PlanStep> = Vec::with_capacity(rows.len());
    let mut row_to_plan: HashMap<usize, usize> = HashMap::new();
    for &row in &rows {
        let step = &steps[row];
        let req = step
            .requirement
            .as_ref()
            .expect("reachable_planned_rows keeps only steps with a requirement");
        let inputs = input
            .inputs
            .get(&step.action_id)
            .ok_or_else(|| PlanError::MissingInputs(step.action_id.clone()))?;
        row_to_plan.insert(row, plan_steps.len());
        plan_steps.push(plan_step(
            step,
            req,
            inputs,
            input.profile,
            input.focus_ceiling,
        ));
    }

    // The reachable paths as sets of plan-step indices, each set once.
    let mut path_steps: Vec<Vec<usize>> = Vec::new();
    for p in &reachable {
        let mut set: Vec<usize> = p
            .rows
            .iter()
            .filter_map(|r| row_to_plan.get(r).copied())
            .collect();
        set.sort_unstable();
        set.dedup();
        if !path_steps.contains(&set) {
            path_steps.push(set);
        }
    }

    let strategies = strategy_listings(steps, &reachable, &plan_steps);
    let handoff_listed = handoff_listing(steps, &reachable).0;
    let needs_approval = plan_steps.iter().any(|s| s.hosted_possible);
    let mut plan = RunPlan {
        focus_id: input.def.focus_id.clone(),
        profile: input.profile.map(str::to_owned),
        steps: plan_steps,
        strategies,
        alternatives: input.alternatives.clone(),
        needs_approval,
        handoff_listed,
        path_steps,
        digest: String::new(),
    };
    plan.digest = plan.digest_with(&[]);
    Ok(plan)
}

fn bar_str(b: QualityBar) -> &'static str {
    match b {
        QualityBar::Basic => "basic",
        QualityBar::Standard => "standard",
        QualityBar::High => "high",
    }
}

fn destination_str(d: Destination) -> &'static str {
    match d {
        Destination::Local => "local",
        Destination::HostedApi => "hosted_api",
        Destination::ManualHandoff => "manual_handoff",
    }
}

fn attained_rank(a: AttainedBar) -> u8 {
    match a {
        AttainedBar::BelowBasic => 0,
        AttainedBar::Basic => 1,
        AttainedBar::Standard => 2,
        AttainedBar::High => 3,
    }
}

fn bar_rank(b: QualityBar) -> u8 {
    match b {
        QualityBar::Basic => 1,
        QualityBar::Standard => 2,
        QualityBar::High => 3,
    }
}

fn plan_step(
    step: &ActionDefinition,
    req: &ModelRequirement,
    inputs: &StepInputs,
    profile: Option<&str>,
    focus_ceiling: ExternalAccess,
) -> PlanStep {
    let effective = effective_access(step, focus_ceiling);
    let Reach { hosted_allowed, .. } = reach(req, effective);
    let resolve_for = |candidates: &[Candidate]| {
        resolve_model(&ResolveInput {
            requirement: req,
            profile,
            ceiling: effective,
            min_context_tokens: inputs.min_context_tokens,
            candidates,
            capabilities: &inputs.gathered.capabilities,
        })
    };

    let answer = match resolve_for(&inputs.gathered.candidates) {
        Resolution::Selected { chosen, .. } => ResolverAnswer::Selected {
            subject: chosen.model.subject,
            provider_id: chosen.model.provider_id,
            model_id: chosen.model.model_id,
            cost_tier: chosen.cost_tier,
            est_cost_micros: chosen.est_cost_micros,
        },
        Resolution::TooWeak { required, .. } => ResolverAnswer::TooWeak { required },
        Resolution::NoCandidate(reason) => ResolverAnswer::NoCandidate(reason),
    };

    // One resolver run per candidate: the resolver's own cell rule says where each one
    // stands, and NoCandidate means it is not permitted or ready, so it is not offered.
    let worth = req.range.quality_floor.max;
    let mut offers: Vec<Offer> = Vec::new();
    for c in &inputs.gathered.candidates {
        let standing = match resolve_for(std::slice::from_ref(c)) {
            Resolution::Selected { chosen, .. } => OfferStanding::Qualifies(chosen.attained),
            Resolution::TooWeak { options, .. } => match options.first().map(|o| o.standing) {
                Some(Standing::BelowBar(a)) => OfferStanding::BelowBar(a),
                Some(Standing::NotMeasured) => OfferStanding::NotMeasured,
                None => continue,
            },
            Resolution::NoCandidate(_) => continue,
        };
        offers.push(offer_for(c, standing, worth));
    }
    offers.sort_by(|a, b| {
        (
            !a.selectable,
            a.standing.rank(),
            a.above_worth_bar,
            a.cost_tier,
            a.est_cost_micros.unwrap_or(u64::MAX),
            &a.provider_id,
            &a.model_id,
        )
            .cmp(&(
                !b.selectable,
                b.standing.rank(),
                b.above_worth_bar,
                b.cost_tier,
                b.est_cost_micros.unwrap_or(u64::MAX),
                &b.provider_id,
                &b.model_id,
            ))
    });

    PlanStep {
        action_id: step.action_id.clone(),
        display_name: step.display_name.clone(),
        kind: StepKind::ModelBlock,
        task_type: req.need.task_type.as_str(),
        required_bar: req.range.quality_floor.min,
        worth_paying_bar: worth,
        destinations: req.range.destinations.clone(),
        highest_tier: req.range.highest_tier,
        hosted_possible: hosted_allowed,
        answer,
        offers,
    }
}

fn offer_for(c: &Candidate, standing: OfferStanding, worth: QualityBar) -> Offer {
    let hosted = c.subject == CapabilitySubject::Hosted;
    let est = if hosted && c.est_cost_micros == u64::MAX {
        None
    } else {
        Some(c.est_cost_micros)
    };
    let (selectable, not_selectable_reason) = match (standing, est) {
        (OfferStanding::BelowBar(_), _) => (false, Some("below_bar")),
        (_, None) => (false, Some("no_estimate")),
        _ => (true, None),
    };
    let strategy = match (hosted, standing) {
        (true, _) => OfferStrategy::PaidApi,
        (false, OfferStanding::NotMeasured) => OfferStrategy::LocalUnmeasured,
        (false, _) => OfferStrategy::Local,
    };
    let above_worth_bar = matches!(standing, OfferStanding::Qualifies(a)
        if attained_rank(a) > bar_rank(worth));
    Offer {
        strategy,
        subject: c.subject,
        provider_id: c.provider_id.clone(),
        model_id: c.model_id.clone(),
        cost_tier: c.cost_tier,
        est_cost_micros: est,
        standing,
        above_worth_bar,
        selectable,
        not_selectable_reason,
        needs_approval: strategy != OfferStrategy::Local,
    }
}

fn strategy_listings(
    steps: &[ActionDefinition],
    reachable: &[&Path],
    plan_steps: &[PlanStep],
) -> Vec<StrategyListing> {
    // Local.
    let local = if plan_steps.is_empty() {
        ("no_model_step", false)
    } else if plan_steps.iter().all(|s| {
        matches!(&s.answer, ResolverAnswer::Selected { subject, .. }
            if *subject == CapabilitySubject::Local)
    }) {
        ("available", true)
    } else if plan_steps
        .iter()
        .any(|s| matches!(s.answer, ResolverAnswer::NoCandidate(_)))
    {
        ("no_candidate", false)
    } else {
        ("too_weak", false)
    };

    // Paid API.
    let paid = if plan_steps.is_empty() {
        ("no_model_step", false)
    } else if plan_steps
        .iter()
        .flat_map(|s| s.offers.iter())
        .any(|o| o.strategy == OfferStrategy::PaidApi && o.selectable)
    {
        ("offered", true)
    } else if !plan_steps.iter().any(|s| s.hosted_possible) {
        if plan_steps
            .iter()
            .any(|s| s.destinations.contains(&Destination::HostedApi))
        {
            ("ceiling_excludes_hosted", false)
        } else {
            ("range_excludes_hosted", false)
        }
    } else if plan_steps
        .iter()
        .flat_map(|s| s.offers.iter())
        .any(|o| o.strategy == OfferStrategy::PaidApi)
    {
        ("none_selectable", false)
    } else {
        ("no_ready_hosted_model", false)
    };

    let (handoff_listed, handoff_reason) = handoff_listing(steps, reachable);
    vec![
        StrategyListing {
            kind: StrategyKind::Local,
            listed: local.1,
            reason: local.0,
        },
        StrategyListing {
            kind: StrategyKind::PaidApi,
            listed: paid.1,
            reason: paid.0,
        },
        StrategyListing {
            kind: StrategyKind::ManualHandoff,
            listed: handoff_listed,
            reason: handoff_reason,
        },
    ]
}

// ---------------------------------------------------------------------------
// Estimates and the digest
// ---------------------------------------------------------------------------

impl RunPlan {
    fn step_cost(&self, step: &PlanStep, pins: &[Pin]) -> u64 {
        if let Some(pin) = pins.iter().find(|p| p.action_id == step.action_id) {
            return step
                .offers
                .iter()
                .find(|o| {
                    o.strategy == pin.strategy
                        && o.provider_id == pin.provider_id
                        && o.model_id == pin.model_id
                })
                .and_then(|o| o.est_cost_micros)
                .unwrap_or(u64::MAX);
        }
        match &step.answer {
            ResolverAnswer::Selected {
                cost_tier,
                est_cost_micros,
                ..
            } if *cost_tier != CostTier::Local => *est_cost_micros,
            _ => 0,
        }
    }

    /// The worst-case cost of the run under these pins: the dearest reachable path, each
    /// path the sum of its steps (a pinned step at its pinned offer's estimate, any other
    /// at the resolver's choice). Saturating; a pin that matches no offer counts as
    /// u64::MAX so it can never look cheap.
    pub fn estimate_micros(&self, pins: &[Pin]) -> u64 {
        self.path_steps
            .iter()
            .map(|set| {
                set.iter()
                    .map(|&i| self.step_cost(&self.steps[i], pins))
                    .fold(0u64, u64::saturating_add)
            })
            .max()
            .unwrap_or(0)
    }

    /// The digest of the plan with these pins (see the module comment for what it covers).
    pub fn digest_with(&self, pins: &[Pin]) -> String {
        let mut lines: Vec<String> = vec![
            DIGEST_TAG.to_owned(),
            format!("focus|{}", self.focus_id),
            format!("profile|{}", self.profile.as_deref().unwrap_or("-")),
        ];
        for s in &self.steps {
            let mut dest: Vec<&str> = s.destinations.iter().map(|d| destination_str(*d)).collect();
            dest.sort_unstable();
            lines.push(format!(
                "step|{}|{}|{}|{}|{}|{}|{}",
                s.action_id,
                s.kind.as_str(),
                dest.join(","),
                s.highest_tier.as_str(),
                bar_str(s.required_bar),
                bar_str(s.worth_paying_bar),
                u8::from(s.hosted_possible),
            ));
        }
        lines.push(format!("handoff|{}", u8::from(self.handoff_listed)));
        let mut sorted: Vec<&Pin> = pins.iter().collect();
        sorted.sort();
        for p in sorted {
            lines.push(format!(
                "pin|{}|{}|{}|{}",
                p.action_id,
                p.strategy.as_str(),
                p.provider_id,
                p.model_id
            ));
        }
        let digest = Sha256::digest(lines.join("\n").as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

// ---------------------------------------------------------------------------
// Mode alternatives (pure)
// ---------------------------------------------------------------------------

/// Under each OTHER mode answer, which strategy would the Focus take? Worked out by
/// resolving the launch answers with that mode (run_settings::resolve), re-expanding a
/// copy of the Focus (run_settings::reexpand, so the mode's profile narrows the ranges
/// exactly as at run start) and reading the structure: every path a hand-off path is
/// manual hand-off, any step whose range reaches hosted is paid API (with approval),
/// else local. Ids only. A Focus with no mode question, or one that is not asked (a
/// preset), has none.
pub fn mode_alternatives(
    def: &FocusDefinition,
    paths: &[Path],
    launch: &LaunchResolution,
    config: &LaunchConfig,
    registry: &Registry,
    focus_ceiling: ExternalAccess,
) -> Vec<ModeAlternative> {
    let Some(q) = config.mode_question().filter(|q| q.asked) else {
        return Vec::new();
    };
    debug_assert_eq!(q.kind, QuestionKind::Mode);
    let mut out = Vec::new();
    for opt in &q.options {
        if launch.mode.as_deref() == Some(opt.id.as_str()) {
            continue;
        }
        let mut stored: StoredAnswers = launch
            .values
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        stored.insert(bound_spec::LAUNCH_MODE.to_owned(), opt.id.clone());
        let computed = run_settings::resolve(Some(config), &stored, None, registry)
            .map_err(|_| "not_computable")
            .and_then(|resolved| {
                let mut copy = def.clone();
                run_settings::reexpand(&mut copy, &resolved).map_err(|_| "not_computable")?;
                Ok((copy, resolved))
            });
        let alt = match computed {
            Err(reason) => ModeAlternative {
                mode_option_id: opt.id.clone(),
                kind: None,
                reason,
            },
            Ok((copy, resolved)) => {
                let reachable = reachable_paths(&copy.steps, paths, &resolved.resolution);
                let (handoff, _) = handoff_listing(&copy.steps, &reachable);
                let hosted = reachable_planned_rows(&copy.steps, &reachable)
                    .into_iter()
                    .any(|i| {
                        let s = &copy.steps[i];
                        let req = s.requirement.as_ref().expect("planned rows have one");
                        reach(req, effective_access(s, focus_ceiling)).hosted_allowed
                    });
                let (kind, reason) = if handoff {
                    (StrategyKind::ManualHandoff, "every_path_is_handoff")
                } else if hosted {
                    (StrategyKind::PaidApi, "range_allows_hosted")
                } else {
                    (StrategyKind::Local, "local_only")
                };
                ModeAlternative {
                    mode_option_id: opt.id.clone(),
                    kind: Some(kind),
                    reason,
                }
            }
        };
        out.push(alt);
    }
    out
}

// ---------------------------------------------------------------------------
// The input bound of a step (for the cost estimate)
// ---------------------------------------------------------------------------

fn num_predict(step: &ActionDefinition) -> Option<u32> {
    step.options_override
        .get("num_predict")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// The most bytes a declared input of `step` can hold, by name: a bound name by the cap
/// its source enforces, a variable an earlier step produced by that step's output limit
/// (4 bytes a token), anything else by the derived-text cap.
fn declared_input_bound_bytes(name: &str, earlier: &[&ActionDefinition]) -> usize {
    if let Some(spec) = bound_spec::spec(name) {
        return match spec.source {
            Source::Session | Source::Launch => {
                crate::persistence::session_store::SESSION_VALUE_MAX_BYTES
            }
            Source::Derived => bound_spec::DERIVED_TEXT_MAX_BYTES,
            Source::Artifact => crate::persistence::pass_artifact_store::ARTIFACT_MAX_BYTES,
        };
    }
    if let Some(producer) = earlier
        .iter()
        .rev()
        .find(|s| s.output_var.as_deref() == Some(name))
    {
        return num_predict(producer)
            .unwrap_or(DEFAULT_STEP_OUTPUT_TOKENS)
            .saturating_mul(4) as usize;
    }
    bound_spec::DERIVED_TEXT_MAX_BYTES
}

/// An upper bound on the tokens a hosted step's prompt can hold: the template's fixed
/// text plus every declared input at its bound. (LOAD refuses a hosted step that names the
/// user's-input or persona-context tokens, so the user's own text is not part of it.)
fn estimate_input_tokens(def: &FocusDefinition, row: usize, fixed_tokens: u32) -> u32 {
    let step = &def.steps[row];
    let earlier: Vec<&ActionDefinition> = def.steps[..row].iter().collect();
    let bytes: usize = step
        .declared_inputs
        .iter()
        .map(|n| declared_input_bound_bytes(n, &earlier))
        .sum();
    fixed_tokens.saturating_add(u32::try_from(bytes / 4).unwrap_or(u32::MAX))
}

// ---------------------------------------------------------------------------
// build (the async wrapper)
// ---------------------------------------------------------------------------

pub struct BuildInput<'a> {
    pub pool: &'a SqlitePool,
    pub user_id: &'a str,
    pub key_hex: &'a str,
    pub def: &'a FocusDefinition,
    pub launch: &'a LaunchResolution,
    /// The Focus-level ceiling: min(the user's maximum, the Focus's own).
    pub focus_ceiling: ExternalAccess,
}

/// Build the plan of a run, or None when the Focus has no planned step. The None case
/// returns BEFORE any read: no database is opened, no table is read, nothing is written.
/// Fails closed (an Err means no plan is offered and the caller must not start a hosted
/// step).
pub async fn build(input: &BuildInput<'_>) -> Result<Option<RunPlan>, PlanError> {
    let def = input.def;
    if !has_planned_step(&def.steps) {
        return Ok(None);
    }
    let paths = composition_paths(&def.steps).map_err(|e| match e {
        CompositionError::TooManyPaths => PlanError::TooManyPaths,
        other => PlanError::Paths(other.to_string()),
    })?;
    let reachable = reachable_paths(&def.steps, &paths, input.launch);
    if reachable.is_empty() {
        return Err(PlanError::NoReachablePath);
    }

    let registry = crate::conductor::model_block::document_type::registry()
        .map_err(PlanError::LaunchConfig)?;
    let profile = input
        .launch
        .document_type
        .as_deref()
        .and_then(|d| profile_for_document_type(d, registry, &[]));

    let mut inputs: HashMap<String, StepInputs> = HashMap::new();
    for row in reachable_planned_rows(&def.steps, &reachable) {
        let step = &def.steps[row];
        let req = step.requirement.as_ref().expect("planned rows have one");
        let r = reach(req, effective_access(step, input.focus_ceiling));
        let fixed = crate::conductor::executor::fixed_tokens_of_step(step, &def.focus_id);
        let gathered = gather(&GatherInput {
            pool: input.pool,
            user_id: input.user_id,
            key_hex: input.key_hex,
            ceiling: r.ceiling,
            local_allowed: r.local_allowed,
            hosted_allowed: r.hosted_allowed,
            est_input_tokens: estimate_input_tokens(def, row, fixed),
            est_output_tokens: num_predict(step),
        })
        .await;
        inputs.insert(
            step.action_id.clone(),
            StepInputs {
                gathered,
                min_context_tokens: fixed,
            },
        );
    }

    let config =
        crate::conductor::launch_config::read_validated(input.pool, &def.focus_id, registry)
            .await
            .map_err(|e| PlanError::LaunchConfig(e.to_string()))?;
    let alternatives = match &config {
        Some(c) => mode_alternatives(def, &paths, input.launch, c, registry, input.focus_ceiling),
        None => Vec::new(),
    };

    compute(&PlanInput {
        def,
        paths: &paths,
        launch: input.launch,
        focus_ceiling: input.focus_ceiling,
        profile: profile.as_deref(),
        inputs: &inputs,
        alternatives,
    })
    .map(Some)
}

/// The per-step estimates of a plan by action id, for callers that show them.
pub fn step_estimates(plan: &RunPlan, pins: &[Pin]) -> BTreeMap<String, u64> {
    plan.steps
        .iter()
        .map(|s| (s.action_id.clone(), plan.step_cost(s, pins)))
        .collect()
}

#[cfg(test)]
#[path = "run_plan_tests.rs"]
mod tests;
