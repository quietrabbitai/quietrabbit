// src-tauri/src/conductor/run_plan_tests.rs
//
// Tests of conductor::run_plan (items.id=901 stage 2). Pure cases use hand-built
// candidates and capability rows (no database); the database cases run the real
// gatherer against a temp shared.db and a loopback mock listener, and prove that
// planning contacts nothing.

use std::collections::{BTreeMap, HashMap};

use super::*;
use crate::conductor::guard::Guard;
use crate::conductor::launch_config::{LaunchConfig, LaunchOption, LaunchQuestion};
use crate::conductor::model_block::config::{
    Difficulty, ModelClass, ModelRange, ModelTaskType, OutputSize, QualityFloor, TaskNeed,
};
use crate::conductor::model_block::document_type::registry;
use crate::conductor::model_capability::{CapabilityRow, Provenance};
use crate::conductor::model_resolver_input::Gathered;
use crate::conductor::tokens::StepType;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

pub(crate) fn requirement(
    min: QualityBar,
    max: QualityBar,
    destinations: &[Destination],
    tier: ExternalAccess,
) -> ModelRequirement {
    ModelRequirement {
        block_stable_id: "fx".to_owned(),
        need: TaskNeed {
            task_type: ModelTaskType::CreativeWriting,
            difficulty: Difficulty::Low,
            output_size: OutputSize::Short,
        },
        range: ModelRange {
            quality_floor: QualityFloor { min, max },
            highest_tier: tier,
            destinations: destinations.to_vec(),
        },
        class_preference: None,
    }
}

pub(crate) fn local_req() -> ModelRequirement {
    requirement(
        QualityBar::Basic,
        QualityBar::High,
        &[Destination::Local],
        ExternalAccess::LocalOnly,
    )
}

pub(crate) fn hosted_req() -> ModelRequirement {
    requirement(
        QualityBar::Basic,
        QualityBar::High,
        &[Destination::Local, Destination::HostedApi],
        ExternalAccess::AnonymousPreferred,
    )
}

pub(crate) fn step(id: &str) -> ActionDefinition {
    ActionDefinition {
        action_id: id.to_owned(),
        display_name: format!("Step {id}"),
        guide_id: "g".to_owned(),
        task_type: "creative_writing".to_owned(),
        routing_tier: ExternalAccess::LocalOnly,
        step_type: StepType::default(),
        output_var: None,
        prompt_template: "Write it.".to_owned(),
        field_requirements: HashMap::new(),
        options_override: HashMap::new(),
        external_access_override: None,
        requires_user_handoff: false,
        revisitable: true,
        block_stable_id: None,
        block_customization: None,
        schedule_trigger: None,
        guard: None,
        declared_inputs: Vec::new(),
        store_op: None,
        and_guards: Vec::new(),
        requirement: None,
    }
}

pub(crate) fn planned(id: &str, req: ModelRequirement) -> ActionDefinition {
    let mut s = step(id);
    let tier = req.range.highest_tier;
    s.routing_tier = tier;
    s.external_access_override = Some(tier);
    s.requirement = Some(req);
    s
}

/// A cb-15 starter row: a render block, no model call.
pub(crate) fn starter(id: &str) -> ActionDefinition {
    let mut s = step(id);
    s.block_stable_id = Some(crate::conductor::handoff_render::STABLE_ID.to_owned());
    s.output_var = Some("starter_text".to_owned());
    s
}

pub(crate) fn guarded(
    mut s: ActionDefinition,
    var: &str,
    op: &str,
    value: &str,
) -> ActionDefinition {
    s.guard = Guard::from_columns(Some(var), Some(op), Some(value)).unwrap();
    s
}

pub(crate) fn with_output(mut s: ActionDefinition, var: &str) -> ActionDefinition {
    s.output_var = Some(var.to_owned());
    s
}

pub(crate) fn def_of(steps: Vec<ActionDefinition>) -> FocusDefinition {
    FocusDefinition {
        focus_id: "fixture-focus".to_owned(),
        display_name: "Fixture".to_owned(),
        description: String::new(),
        version: "1".to_owned(),
        max_routing_tier: ExternalAccess::AnonymousPreferred,
        steps,
        output_type: "text".to_owned(),
        suggest_in_focuses: Vec::new(),
        multi_source_validation: false,
        generic_title_template: String::new(),
        high_priority_trigger: None,
        collection_posture: Default::default(),
        model_origins: Vec::new(),
    }
}

pub(crate) fn launch(mode: Option<&str>) -> LaunchResolution {
    let mut values = BTreeMap::new();
    if let Some(m) = mode {
        values.insert(bound_spec::LAUNCH_MODE, m.to_owned());
    }
    LaunchResolution {
        values,
        mode: mode.map(str::to_owned),
        document_type: None,
        settings: None,
    }
}

pub(crate) fn local_cand(model: &str) -> Candidate {
    Candidate {
        subject: CapabilitySubject::Local,
        provider_id: String::new(),
        model_id: model.to_owned(),
        readiness: crate::conductor::model_resolver::Readiness::Ready,
        context_window_tokens: 8192,
        cost_tier: CostTier::Local,
        est_cost_micros: 0,
        provider_is_anonymous: true,
    }
}

pub(crate) fn hosted_cand(provider: &str, model: &str, est: u64) -> Candidate {
    Candidate {
        subject: CapabilitySubject::Hosted,
        provider_id: provider.to_owned(),
        model_id: model.to_owned(),
        readiness: crate::conductor::model_resolver::Readiness::Ready,
        context_window_tokens: 8192,
        cost_tier: CostTier::Metered,
        est_cost_micros: est,
        provider_is_anonymous: false,
    }
}

pub(crate) fn cell(c: &Candidate, bar: AttainedBar) -> CapabilityRow {
    CapabilityRow {
        subject: c.subject,
        provider_id: c.provider_id.clone(),
        model_id: c.model_id.clone(),
        task_type: ModelTaskType::CreativeWriting,
        profile: String::new(),
        attained_bar: bar,
        model_class: ModelClass::Medium,
        sample_count: 1,
        measured_at: "2026-11-13".to_owned(),
        provenance: Provenance::JasonRead,
        note: None,
    }
}

pub(crate) fn inputs_for(
    ids: &[&str],
    cands: &[Candidate],
    caps: &[CapabilityRow],
) -> HashMap<String, StepInputs> {
    ids.iter()
        .map(|id| {
            (
                (*id).to_owned(),
                StepInputs {
                    gathered: Gathered {
                        candidates: cands.to_vec(),
                        capabilities: caps.to_vec(),
                        hosted_models: HashMap::new(),
                    },
                    min_context_tokens: 0,
                },
            )
        })
        .collect()
}

pub(crate) fn plan_of(
    steps: Vec<ActionDefinition>,
    launch: &LaunchResolution,
    inputs: &HashMap<String, StepInputs>,
) -> Result<RunPlan, PlanError> {
    let def = def_of(steps);
    let paths = composition_paths(&def.steps).expect("the fixture's guards analyse");
    compute(&PlanInput {
        def: &def,
        paths: &paths,
        launch,
        focus_ceiling: ExternalAccess::AnonymousPreferred,
        profile: None,
        inputs,
        alternatives: Vec::new(),
    })
}

fn listing(plan: &RunPlan, kind: StrategyKind) -> (bool, &'static str) {
    let l = plan.strategies.iter().find(|s| s.kind == kind).unwrap();
    (l.listed, l.reason)
}

// ---------------------------------------------------------------------------
// No plan for a Focus without a model block
// ---------------------------------------------------------------------------

/// A pool that can never open a connection: any read through it would fail.
fn dead_pool() -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect_lazy("sqlite:///qr-901-no-such-dir/none.db").unwrap()
}

#[tokio::test]
async fn a_focus_with_no_model_step_has_no_plan_and_reads_nothing() {
    let def = def_of(vec![step("a"), with_output(step("b"), "out")]);
    let pool = dead_pool();
    let launch = launch(None);
    let got = build(&BuildInput {
        pool: &pool,
        user_id: "u",
        key_hex: "",
        def: &def,
        launch: &launch,
        focus_ceiling: ExternalAccess::AnonymousPreferred,
    })
    .await;
    assert_eq!(got, Ok(None));
    assert_eq!(pool.size(), 0, "no connection was even attempted");
}

/// R-B: no shipped Focus names a model block, so none has a plan and every one
/// takes the old path. Reads the real loaded definitions (the six bundled .focus files
/// and every Focus row seeded in shared.db).
#[tokio::test]
async fn no_shipped_focus_has_a_model_block_or_a_plan() {
    with_temp_shared_db(|pool| async move {
        let mut ids: Vec<String> = [
            "writing-assistant",
            "quick-ask",
            "research-and-buy",
            "role-assessment",
            "qr-admin",
            "api-chat",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let rows: Vec<String> = sqlx::query_scalar("SELECT focus_id FROM focuses")
            .fetch_all(&pool)
            .await
            .unwrap();
        for r in rows {
            if !ids.contains(&r) {
                ids.push(r);
            }
        }
        assert!(ids.len() >= 6);
        let dead = dead_pool();
        for id in ids {
            let def = crate::conductor::lifecycle::load_focus_definition(&pool, &id)
                .await
                .unwrap_or_else(|e| panic!("shipped Focus '{id}' must load: {e}"));
            assert!(def.model_origins.is_empty(), "{id} names a model block");
            assert!(
                def.steps.iter().all(|s| s.requirement.is_none()),
                "{id} has a requirement step"
            );
            assert!(!has_planned_step(&def.steps), "{id}");
            // And the plan phase over it reads nothing (a dead pool would fail a read).
            let launch = launch(None);
            let got = build(&BuildInput {
                pool: &dead,
                user_id: "u",
                key_hex: "",
                def: &def,
                launch: &launch,
                focus_ceiling: ExternalAccess::AnonymousPreferred,
            })
            .await;
            assert_eq!(got, Ok(None), "{id}");
        }
        assert_eq!(dead.size(), 0);
    })
    .await;
}

// ---------------------------------------------------------------------------
// Paths and strategies (ruling R-C)
// ---------------------------------------------------------------------------

/// A model path and a hand-off path under different launch modes: hand-off is listed only
/// under the mode whose every path is a hand-off.
#[test]
fn hand_off_is_listed_only_under_the_mode_whose_every_path_is_a_hand_off() {
    let steps = vec![
        guarded(planned("draft", local_req()), "launch_mode", "eq", "write"),
        guarded(starter("starter"), "launch_mode", "ne", "write"),
    ];
    let cands = [local_cand("m1")];
    let caps = [cell(&cands[0], AttainedBar::Basic)];
    let inputs = inputs_for(&["draft"], &cands, &caps);

    let write = plan_of(steps.clone(), &launch(Some("write")), &inputs).unwrap();
    assert_eq!(write.steps.len(), 1);
    assert_eq!(
        listing(&write, StrategyKind::ManualHandoff),
        (false, "a_path_calls_a_model")
    );
    assert_eq!(listing(&write, StrategyKind::Local), (true, "available"));
    assert!(!write.needs_approval);

    // A mode the guards never name is OTHER: the starter row runs, the model row does not.
    let handoff = plan_of(steps, &launch(Some("handoff")), &inputs_for(&[], &[], &[])).unwrap();
    assert!(handoff.steps.is_empty());
    assert_eq!(
        listing(&handoff, StrategyKind::ManualHandoff),
        (true, "every_path_is_handoff")
    );
    assert_eq!(
        listing(&handoff, StrategyKind::Local),
        (false, "no_model_step")
    );
    assert!(!handoff.needs_approval, "a hand-off path needs no approval");
    assert_eq!(handoff.estimate_micros(&[]), 0);
}

/// A path that carries both a starter and a model call is not a hand-off path.
#[test]
fn a_path_with_a_starter_and_a_model_call_is_not_listed_as_hand_off() {
    let steps = vec![planned("draft", local_req()), starter("starter")];
    let cands = [local_cand("m1")];
    let caps = [cell(&cands[0], AttainedBar::Basic)];
    let plan = plan_of(steps, &launch(None), &inputs_for(&["draft"], &cands, &caps)).unwrap();
    assert_eq!(
        listing(&plan, StrategyKind::ManualHandoff),
        (false, "a_path_calls_a_model")
    );
}

/// A guard on a runtime output is unknown at plan time: both branches are counted.
#[test]
fn a_guard_on_a_runtime_output_takes_both_branches() {
    let steps = vec![
        with_output(step("route"), "route"),
        guarded(planned("draft", local_req()), "route", "eq", "a"),
        guarded(starter("starter"), "route", "ne", "a"),
    ];
    let cands = [local_cand("m1")];
    let caps = [cell(&cands[0], AttainedBar::Basic)];
    let plan = plan_of(steps, &launch(None), &inputs_for(&["draft"], &cands, &caps)).unwrap();
    assert_eq!(plan.steps.len(), 1, "the model branch is in the plan");
    assert_eq!(plan.path_steps.len(), 2, "and so is the other branch");
    assert!(plan.path_steps.contains(&vec![0]) && plan.path_steps.contains(&vec![]));
    // The router itself is a model call on every path.
    assert_eq!(
        listing(&plan, StrategyKind::ManualHandoff),
        (false, "a_path_calls_a_model")
    );
}

/// More paths than the cap: no plan, a typed error, and nothing is read.
#[tokio::test]
async fn more_than_the_path_cap_fails_closed_before_any_read() {
    let mut steps = Vec::new();
    for i in 0..9 {
        let var = format!("r{i}");
        steps.push(with_output(step(&format!("router{i}")), &var));
        steps.push(guarded(step(&format!("yes{i}")), &var, "eq", "a"));
        steps.push(guarded(step(&format!("no{i}")), &var, "ne", "a"));
    }
    steps.push(planned("draft", local_req()));
    let def = def_of(steps);
    let launch = launch(None);
    let pool = dead_pool();
    let got = build(&BuildInput {
        pool: &pool,
        user_id: "u",
        key_hex: "",
        def: &def,
        launch: &launch,
        focus_ceiling: ExternalAccess::AnonymousPreferred,
    })
    .await;
    assert_eq!(got, Err(PlanError::TooManyPaths));
    assert_eq!(pool.size(), 0);
}

/// The filter never adds a path: the reachable set is a subset of what LOAD enumerated.
#[test]
fn the_reachable_paths_are_a_subset_of_the_load_paths() {
    let steps = vec![
        guarded(planned("draft", local_req()), "launch_mode", "eq", "write"),
        guarded(starter("starter"), "launch_mode", "ne", "write"),
    ];
    let paths = composition_paths(&steps).unwrap();
    for mode in ["write", "handoff", "anything"] {
        let reachable = reachable_paths(&steps, &paths, &launch(Some(mode)));
        assert!(!reachable.is_empty() && reachable.len() <= paths.len());
    }
}

// ---------------------------------------------------------------------------
// Offers, answers, estimates
// ---------------------------------------------------------------------------

#[test]
fn offers_list_every_permitted_candidate_with_where_it_stands() {
    let l1 = local_cand("l-basic");
    let l2 = local_cand("l-unmeasured");
    let l4 = local_cand("l-high");
    let h1 = hosted_cand("host-a", "h-unmeasured", 500);
    let h2 = hosted_cand("host-a", "h-no-estimate", u64::MAX);
    let h3 = hosted_cand("host-b", "h-below", 100);
    let caps = [
        cell(&l1, AttainedBar::Basic),
        cell(&l4, AttainedBar::High),
        cell(&h3, AttainedBar::BelowBasic),
    ];
    let cands = [l1, l2, l4, h1, h2, h3];
    // min basic, max basic: a High local model is more than the step is worth paying for.
    let req = requirement(
        QualityBar::Basic,
        QualityBar::Basic,
        &[Destination::Local, Destination::HostedApi],
        ExternalAccess::AnonymousPreferred,
    );
    let plan = plan_of(
        vec![planned("draft", req)],
        &launch(None),
        &inputs_for(&["draft"], &cands, &caps),
    )
    .unwrap();
    let step = &plan.steps[0];

    // The resolver would pick the smallest qualifying local model, never a paid one.
    assert!(
        matches!(&step.answer, ResolverAnswer::Selected { subject, model_id, .. }
        if *subject == CapabilitySubject::Local && model_id == "l-basic")
    );
    assert!(plan.needs_approval, "the range reaches a hosted model");

    let names: Vec<&str> = step.offers.iter().map(|o| o.model_id.as_str()).collect();
    assert_eq!(
        names,
        [
            "l-basic",
            "l-high",
            "l-unmeasured",
            "h-unmeasured",
            "h-no-estimate",
            "h-below"
        ],
        "qualifiers first (the more-than-worth one last of them), then unmeasured, then not selectable"
    );
    let by = |m: &str| step.offers.iter().find(|o| o.model_id == m).unwrap();
    assert_eq!(by("l-basic").strategy, OfferStrategy::Local);
    assert!(!by("l-basic").needs_approval && by("l-basic").selectable);
    assert!(by("l-high").above_worth_bar && !by("l-basic").above_worth_bar);
    assert_eq!(by("l-unmeasured").strategy, OfferStrategy::LocalUnmeasured);
    assert!(by("l-unmeasured").needs_approval && by("l-unmeasured").selectable);
    assert_eq!(by("h-unmeasured").strategy, OfferStrategy::PaidApi);
    assert_eq!(by("h-unmeasured").standing, OfferStanding::NotMeasured);
    assert_eq!(by("h-unmeasured").est_cost_micros, Some(500));
    assert!(by("h-unmeasured").needs_approval && by("h-unmeasured").selectable);
    assert_eq!(by("h-no-estimate").est_cost_micros, None);
    assert_eq!(
        by("h-no-estimate").not_selectable_reason,
        Some("no_estimate")
    );
    assert_eq!(by("h-below").not_selectable_reason, Some("below_bar"));
    assert!(!by("h-below").selectable);
    assert_eq!(listing(&plan, StrategyKind::PaidApi), (true, "offered"));
}

#[test]
fn too_weak_and_no_candidate_are_reported_with_their_strategy_reasons() {
    let l1 = local_cand("l-basic");
    let caps = [cell(&l1, AttainedBar::Basic)];
    let high = requirement(
        QualityBar::High,
        QualityBar::High,
        &[Destination::Local],
        ExternalAccess::LocalOnly,
    );
    let plan = plan_of(
        vec![planned("draft", high.clone())],
        &launch(None),
        &inputs_for(&["draft"], &[l1], &caps),
    )
    .unwrap();
    assert!(matches!(
        plan.steps[0].answer,
        ResolverAnswer::TooWeak {
            required: QualityBar::High
        }
    ));
    assert_eq!(listing(&plan, StrategyKind::Local), (false, "too_weak"));
    assert_eq!(
        plan.steps[0].offers[0].not_selectable_reason,
        Some("below_bar")
    );

    let none = plan_of(
        vec![planned("draft", high)],
        &launch(None),
        &inputs_for(&["draft"], &[], &[]),
    )
    .unwrap();
    assert!(matches!(
        none.steps[0].answer,
        ResolverAnswer::NoCandidate(NoCandidateReason::NoneReady)
    ));
    assert_eq!(listing(&none, StrategyKind::Local), (false, "no_candidate"));
    assert!(none.steps[0].offers.is_empty());
}

#[test]
fn paid_api_reasons_name_why_it_is_not_offered() {
    let l1 = local_cand("l-basic");
    let caps = [cell(&l1, AttainedBar::Basic)];
    // The range names no hosted destination.
    let local_only = plan_of(
        vec![planned("d", local_req())],
        &launch(None),
        &inputs_for(&["d"], std::slice::from_ref(&l1), &caps),
    )
    .unwrap();
    assert_eq!(
        listing(&local_only, StrategyKind::PaidApi),
        (false, "range_excludes_hosted")
    );
    // The range allows it but the Focus ceiling does not reach it.
    let def = def_of(vec![planned("d", hosted_req())]);
    let paths = composition_paths(&def.steps).unwrap();
    let inputs = inputs_for(&["d"], std::slice::from_ref(&l1), &caps);
    let low = compute(&PlanInput {
        def: &def,
        paths: &paths,
        launch: &launch(None),
        focus_ceiling: ExternalAccess::LocalOnly,
        profile: None,
        inputs: &inputs,
        alternatives: Vec::new(),
    })
    .unwrap();
    assert_eq!(
        listing(&low, StrategyKind::PaidApi),
        (false, "ceiling_excludes_hosted")
    );
    assert!(
        !low.needs_approval,
        "a hosted step cannot run under this ceiling"
    );
    // The range and ceiling allow it but no hosted model is ready.
    let none_ready = plan_of(
        vec![planned("d", hosted_req())],
        &launch(None),
        &inputs_for(&["d"], std::slice::from_ref(&l1), &caps),
    )
    .unwrap();
    assert_eq!(
        listing(&none_ready, StrategyKind::PaidApi),
        (false, "no_ready_hosted_model")
    );
    assert!(
        none_ready.needs_approval,
        "the range alone makes the plan ask"
    );
    // Ready but nothing selectable (no estimate).
    let h = hosted_cand("host-a", "h", u64::MAX);
    let unusable = plan_of(
        vec![planned("d", hosted_req())],
        &launch(None),
        &inputs_for(&["d"], &[l1, h], &caps),
    )
    .unwrap();
    assert_eq!(
        listing(&unusable, StrategyKind::PaidApi),
        (false, "none_selectable")
    );
}

/// The plan costs its dearest path (a run takes one), each path the sum of its steps.
#[test]
fn the_estimate_is_the_dearest_path_and_a_pin_moves_it() {
    let h1 = hosted_cand("host-a", "h1", 100);
    let h2 = hosted_cand("host-a", "h2", 300);
    let hosted_only = requirement(
        QualityBar::Basic,
        QualityBar::High,
        &[Destination::HostedApi],
        ExternalAccess::AnonymousPreferred,
    );
    let steps = vec![
        with_output(step("route"), "route"),
        guarded(planned("p1", hosted_only.clone()), "route", "eq", "a"),
        guarded(planned("p2", hosted_only.clone()), "route", "ne", "a"),
    ];
    let caps = [cell(&h1, AttainedBar::Basic), cell(&h2, AttainedBar::Basic)];
    let mut inputs = inputs_for(&["p1"], std::slice::from_ref(&h1), &caps);
    inputs.extend(inputs_for(&["p2"], std::slice::from_ref(&h2), &caps));
    let plan = plan_of(steps, &launch(None), &inputs).unwrap();
    assert_eq!(plan.estimate_micros(&[]), 300);
    let pin = |a: &str, m: &str| Pin {
        action_id: a.to_owned(),
        strategy: OfferStrategy::PaidApi,
        provider_id: "host-a".to_owned(),
        model_id: m.to_owned(),
    };
    assert_eq!(plan.estimate_micros(&[pin("p1", "h1")]), 300);
    assert_eq!(plan.estimate_micros(&[pin("p2", "h2")]), 300);
    // A pin that matches no offer can never look cheap.
    assert_eq!(plan.estimate_micros(&[pin("p1", "nope")]), u64::MAX);

    // On ONE path the steps add up.
    let one_path = vec![
        planned("p1", hosted_only.clone()),
        planned("p2", hosted_only),
    ];
    let mut inputs = inputs_for(&["p1"], std::slice::from_ref(&h1), &caps);
    inputs.extend(inputs_for(&["p2"], std::slice::from_ref(&h2), &caps));
    let plan = plan_of(one_path, &launch(None), &inputs).unwrap();
    assert_eq!(plan.estimate_micros(&[]), 400);
    assert_eq!(
        step_estimates(&plan, &[]).into_iter().collect::<Vec<_>>(),
        [("p1".to_owned(), 100), ("p2".to_owned(), 300)]
    );
}

// ---------------------------------------------------------------------------
// The digest
// ---------------------------------------------------------------------------

/// A plan of one hosted-capable step over these candidates and cells.
fn digest_plan(
    req: ModelRequirement,
    cands: &[Candidate],
    caps: &[CapabilityRow],
    focus_ceiling: ExternalAccess,
) -> RunPlan {
    let def = def_of(vec![planned("draft", req)]);
    let paths = composition_paths(&def.steps).unwrap();
    compute(&PlanInput {
        def: &def,
        paths: &paths,
        launch: &launch(None),
        focus_ceiling,
        profile: None,
        inputs: &inputs_for(&["draft"], cands, caps),
        alternatives: Vec::new(),
    })
    .unwrap()
}

/// The digest is STRUCTURE ONLY: it does not move when a local model becomes ready or
/// unready, when a price changes or becomes unknown, or when the resolver's answer changes
/// from Selected to TooWeak.
#[test]
fn the_digest_does_not_depend_on_readiness_prices_or_the_resolvers_answer() {
    use crate::conductor::model_resolver::Readiness;
    let l1 = local_cand("l-basic");
    let h = hosted_cand("host-a", "h", 500);
    let caps = [cell(&l1, AttainedBar::Basic)];
    let ceiling = ExternalAccess::AnonymousPreferred;
    let base = digest_plan(hosted_req(), &[l1.clone(), h.clone()], &caps, ceiling);
    assert_eq!(base.digest.len(), 64);
    assert_eq!(base.digest, base.digest_with(&[]));
    // The plan itself does vary with all of these, so the equalities below mean something.
    assert!(matches!(
        base.steps[0].answer,
        ResolverAnswer::Selected { .. }
    ));

    // A local model becoming unready, then ready again.
    let mut unready = l1.clone();
    unready.readiness = Readiness::NotInstalled;
    let p = digest_plan(hosted_req(), &[unready, h.clone()], &caps, ceiling);
    assert_ne!(p.steps[0].answer, base.steps[0].answer, "the answer moved");
    assert_eq!(p.digest, base.digest);
    let mut disabled = l1.clone();
    disabled.readiness = Readiness::DisabledByUser;
    assert_eq!(
        digest_plan(hosted_req(), &[disabled, h.clone()], &caps, ceiling).digest,
        base.digest
    );

    // A price change, a price that becomes unknown, a missing key, a retired model.
    let mut dearer = h.clone();
    dearer.est_cost_micros = 9_999;
    assert_eq!(
        digest_plan(hosted_req(), &[l1.clone(), dearer], &caps, ceiling).digest,
        base.digest
    );
    for readiness in [
        Readiness::PriceUnknown,
        Readiness::NoKey,
        Readiness::NotConsented,
        Readiness::Retired,
    ] {
        let mut not_ready = h.clone();
        not_ready.readiness = readiness;
        assert_eq!(
            digest_plan(hosted_req(), &[l1.clone(), not_ready], &caps, ceiling).digest,
            base.digest,
            "{readiness:?}"
        );
    }
    let mut unpriced = h.clone();
    unpriced.est_cost_micros = u64::MAX;
    assert_eq!(
        digest_plan(hosted_req(), &[l1.clone(), unpriced], &caps, ceiling).digest,
        base.digest
    );

    // The resolver's answer moves from Selected to TooWeak (the cell goes away) and to
    // NoCandidate (no candidate at all), and the strategy listings with it.
    let weak = digest_plan(hosted_req(), &[l1.clone(), h.clone()], &[], ceiling);
    assert!(matches!(
        weak.steps[0].answer,
        ResolverAnswer::TooWeak { .. }
    ));
    assert_ne!(weak.strategies, base.strategies, "the listings moved");
    assert_eq!(weak.digest, base.digest, "but the digest did not");
    let none = digest_plan(hosted_req(), &[], &[], ceiling);
    assert!(matches!(
        none.steps[0].answer,
        ResolverAnswer::NoCandidate(_)
    ));
    assert_eq!(none.digest, base.digest);
}

/// It does move with the structure: destinations, tier, bars, hosted_possible, the
/// hand-off listing, and a pin.
#[test]
fn the_digest_changes_with_the_structure_and_with_a_pin() {
    let l1 = local_cand("l-basic");
    let h = hosted_cand("host-a", "h", 500);
    let caps = [cell(&l1, AttainedBar::Basic)];
    let cands = [l1, h];
    let ceiling = ExternalAccess::AnonymousPreferred;
    let base = digest_plan(hosted_req(), &cands, &caps, ceiling);
    let digest_of = |req: ModelRequirement| digest_plan(req, &cands, &caps, ceiling).digest;
    let range = |min, max, dest: &[Destination], tier| requirement(min, max, dest, tier);
    let both = [Destination::Local, Destination::HostedApi];
    let anon = ExternalAccess::AnonymousPreferred;

    // Destinations.
    let local_only = range(
        QualityBar::Basic,
        QualityBar::High,
        &[Destination::Local],
        anon,
    );
    assert_ne!(digest_of(local_only), base.digest);
    // Tier (the destinations and bars are unchanged).
    let unrestricted = range(
        QualityBar::Basic,
        QualityBar::High,
        &both,
        ExternalAccess::Unrestricted,
    );
    assert_ne!(digest_of(unrestricted), base.digest);
    // The required bar, and the worth-paying bar.
    assert_ne!(
        digest_of(range(QualityBar::Standard, QualityBar::High, &both, anon)),
        base.digest
    );
    assert_ne!(
        digest_of(range(QualityBar::Basic, QualityBar::Standard, &both, anon)),
        base.digest
    );

    // hosted_possible alone: the same requirement under a ceiling that cannot reach
    // anonymous_preferred. Every other part of the step line is identical.
    let capped = digest_plan(hosted_req(), &cands, &caps, ExternalAccess::LocalOnly);
    assert!(base.steps[0].hosted_possible && !capped.steps[0].hosted_possible);
    assert_eq!(base.steps[0].destinations, capped.steps[0].destinations);
    assert_eq!(base.steps[0].highest_tier, capped.steps[0].highest_tier);
    assert_ne!(capped.digest, base.digest);

    // The hand-off listing alone: flip the one structural fact on a copy.
    assert!(!base.handoff_listed);
    let mut flipped = base.clone();
    flipped.handoff_listed = true;
    assert_ne!(flipped.digest_with(&[]), base.digest);
    // ...and from the paths: a hand-off-only mode and a model mode differ.
    let steps = vec![
        guarded(planned("draft", hosted_req()), "launch_mode", "eq", "write"),
        guarded(starter("starter"), "launch_mode", "ne", "write"),
    ];
    let model_mode = plan_of(
        steps.clone(),
        &launch(Some("write")),
        &inputs_for(&["draft"], &cands, &caps),
    )
    .unwrap();
    let handoff_mode =
        plan_of(steps, &launch(Some("handoff")), &inputs_for(&[], &[], &[])).unwrap();
    assert!(!model_mode.handoff_listed && handoff_mode.handoff_listed);
    assert_ne!(model_mode.digest, handoff_mode.digest);

    // Pins change it, and their order does not.
    let pin = |m: &str| Pin {
        action_id: "draft".to_owned(),
        strategy: OfferStrategy::PaidApi,
        provider_id: "host-a".to_owned(),
        model_id: m.to_owned(),
    };
    let other = Pin {
        action_id: "other".to_owned(),
        ..pin("h")
    };
    assert_ne!(base.digest_with(&[pin("h")]), base.digest);
    assert_ne!(base.digest_with(&[pin("h")]), base.digest_with(&[pin("x")]));
    assert_eq!(
        base.digest_with(&[pin("h"), other.clone()]),
        base.digest_with(&[other, pin("h")])
    );
}

// ---------------------------------------------------------------------------
// Mode alternatives
// ---------------------------------------------------------------------------

fn mode_config(asked: bool) -> LaunchConfig {
    let opt = |id: &str| LaunchOption {
        id: id.to_owned(),
        sort_order: 0,
        label: id.to_owned(),
        hint: String::new(),
    };
    LaunchConfig {
        focus_id: "fixture-focus".to_owned(),
        questions: vec![LaunchQuestion {
            id: "mode".to_owned(),
            slot: "launch_mode".to_owned(),
            kind: crate::conductor::launch_config::QuestionKind::Mode,
            sort_order: 0,
            asked,
            default_option: "write".to_owned(),
            prompt: "Mode?".to_owned(),
            options: vec![opt("write"), opt("handoff")],
        }],
        profiles: Vec::new(),
    }
}

#[test]
fn another_mode_is_listed_with_the_strategy_it_would_take() {
    let reg = registry().unwrap();
    let steps = vec![
        guarded(planned("draft", hosted_req()), "launch_mode", "eq", "write"),
        guarded(starter("starter"), "launch_mode", "ne", "write"),
    ];
    let def = def_of(steps);
    let paths = composition_paths(&def.steps).unwrap();
    let config = mode_config(true);

    // Under write, the other answer (handoff) is a hand-off path.
    let alts = mode_alternatives(
        &def,
        &paths,
        &launch(Some("write")),
        &config,
        reg,
        ExternalAccess::AnonymousPreferred,
    );
    assert_eq!(
        alts,
        [ModeAlternative {
            mode_option_id: "handoff".to_owned(),
            kind: Some(StrategyKind::ManualHandoff),
            reason: "every_path_is_handoff",
        }]
    );
    // Under handoff, the other answer (write) reaches a hosted-capable range.
    let alts = mode_alternatives(
        &def,
        &paths,
        &launch(Some("handoff")),
        &config,
        reg,
        ExternalAccess::AnonymousPreferred,
    );
    assert_eq!(alts[0].mode_option_id, "write");
    assert_eq!(alts[0].kind, Some(StrategyKind::PaidApi));
    assert_eq!(alts[0].reason, "range_allows_hosted");
    // Under a ceiling that cannot reach hosted it is local.
    let alts = mode_alternatives(
        &def,
        &paths,
        &launch(Some("handoff")),
        &config,
        reg,
        ExternalAccess::LocalOnly,
    );
    assert_eq!(alts[0].kind, Some(StrategyKind::Local));
    // A preset mode (not asked) has no alternatives to offer.
    assert!(mode_alternatives(
        &def,
        &paths,
        &launch(Some("write")),
        &mode_config(false),
        reg,
        ExternalAccess::AnonymousPreferred
    )
    .is_empty());
}

// ---------------------------------------------------------------------------
// The real gatherer, a hosted fixture and the mock listener
// ---------------------------------------------------------------------------

use crate::persistence::integration_keys_store::{self, QR_HOSTED_KEY_TYPE};
use crate::persistence::provider_spend_store as spend_store;
use crate::providers::loopback_test_server::{Canned, LoopbackServer};
use crate::spend::disclosure::attributes_hash;
use crate::spend::estimate::worst_case;
use crate::spend::price::{resolve_price, PriceQuote};

pub(crate) const SPEND_USER: &str = "user-plan";
pub(crate) const PRICE_TEXT: &str =
    r#"{"input_per_mtok":0.50,"output_per_mtok":1.50,"as_of":"2026-10-11","source":"curated"}"#;
pub(crate) const HOST: &str = "fixture-host-a";
pub(crate) const MODEL: &str = "fixture-model-a";

pub(crate) fn fixture_key_hex() -> String {
    crate::auth::registry::key_hex(&[0x77; crate::auth::kdf::MASTER_KEY_LEN])
}

pub(crate) async fn with_temp_shared_db<F, Fut>(f: F) -> Fut::Output
where
    F: FnOnce(sqlx::SqlitePool) -> Fut,
    Fut: std::future::Future,
{
    let _lock = crate::test_support::ENV_MUTEX.lock().await;
    let saved_root = std::env::var("QR_DATA_ROOT").ok();
    let tempdir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("QR_DATA_ROOT", tempdir.path());
    crate::persistence::migrations::migrate_shared_db()
        .await
        .expect("shared.db migration");
    let pool =
        sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
            &crate::providers::utils::db_path_shared(),
        ))
        .await
        .expect("shared.db pool");
    let result = f(pool).await;
    if let Some(v) = saved_root {
        std::env::set_var("QR_DATA_ROOT", v);
    } else {
        std::env::remove_var("QR_DATA_ROOT");
    }
    result
}

/// A hosted fixture provider and model with a price, a stored key and a current consent,
/// plus every curated local model marked installed.
pub(crate) async fn hosted_rig(pool: &sqlx::SqlitePool, price: Option<&str>) -> String {
    use crate::persistence::provider_store::{
        create_provider, NewProvider, PrivacyGuardianDefaultLevel, ProviderMode,
    };
    sqlx::query("UPDATE providers SET installed = 1 WHERE provider_type = 'local_model'")
        .execute(pool)
        .await
        .unwrap();
    create_provider(
        pool,
        NewProvider {
            id: HOST,
            display_name: HOST,
            provider_type: "cloud_inference_api",
            mode: ProviderMode::Api,
            launch_url: None,
            login_required: true,
            is_local: false,
            is_anonymous: false,
            retains_data: false,
            trains_on_data_by_default: false,
            qr_internal_eligible: false,
            privacy_guardian_default_level: PrivacyGuardianDefaultLevel::Low,
            risk_rating: 1,
            hardware_requirement: None,
            documentation_gate: &serde_json::json!({}),
            qr_recommended: false,
            privacy_commitment_basis: None,
            performance_profile: None,
            local_model_tag: None,
            focus_eligible: false,
            cloud_chat_visible: false,
        },
    )
    .await
    .expect("fixture provider");
    sqlx::query(
        "INSERT INTO provider_models
            (id, provider_id, model_id, context_window_tokens, is_default, created_at)
         VALUES (?, ?, ?, 4096, 1, '2026-01-01T00:00:00Z')",
    )
    .bind(format!("{HOST}:{MODEL}"))
    .bind(HOST)
    .bind(MODEL)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE providers SET api_dialect = 'openai_chat', base_url_template = 'https://fixture.invalid/v1',
                auth_scheme = 'bearer', extra_headers = '{}', models_path = '/models', user_variables = '[]'
         WHERE id = ?",
    )
    .bind(HOST)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE provider_models SET request_options = '{\"max_completion_tokens\":128}', pricing = ?
         WHERE provider_id = ?",
    )
    .bind(price)
    .bind(HOST)
    .execute(pool)
    .await
    .unwrap();

    let key_hex = fixture_key_hex();
    integration_keys_store::upsert_key(
        SPEND_USER,
        &key_hex,
        HOST,
        QR_HOSTED_KEY_TYPE,
        "stored-key-value",
        None,
        Some("api_key"),
        None,
    )
    .await
    .unwrap();
    let provider = crate::persistence::provider_store::get_provider(pool, HOST)
        .await
        .unwrap()
        .unwrap();
    let models = crate::persistence::provider_store::list_models_for_provider(pool, HOST)
        .await
        .unwrap();
    let texts = crate::persistence::provider_store::list_model_pricing_text(pool, HOST)
        .await
        .unwrap();
    let hash = attributes_hash(&provider, &models, &texts);
    let mut conn = spend_store::open(SPEND_USER, &key_hex).await.unwrap();
    spend_store::insert_consent_conn(&mut conn, HOST, &hash, true, chrono::Utc::now())
        .await
        .unwrap();
    key_hex
}

fn ready_to_plan(def: &FocusDefinition) -> (Vec<Path>, LaunchResolution) {
    (composition_paths(&def.steps).unwrap(), launch(None))
}

/// The plan prices a hosted model the resolver has no score for, from the user's price
/// and the model's own output limit, lists it as an offer, and contacts nothing.
#[tokio::test]
async fn the_real_gatherer_prices_an_unmeasured_hosted_model_and_planning_sends_nothing() {
    with_temp_shared_db(|pool| async move {
        let server = LoopbackServer::start(vec![Canned::json(200, "{}")]);
        let key_hex = hosted_rig(&pool, Some(PRICE_TEXT)).await;
        let step = planned("draft", hosted_req());
        let def = def_of(vec![step.clone()]);
        let (_paths, launch) = ready_to_plan(&def);
        let build_it = |def: &FocusDefinition| {
            let pool = pool.clone();
            let key_hex = key_hex.clone();
            let launch = launch.clone();
            let def = def.clone();
            async move {
                build(&BuildInput {
                    pool: &pool,
                    user_id: SPEND_USER,
                    key_hex: &key_hex,
                    def: &def,
                    launch: &launch,
                    focus_ceiling: ExternalAccess::AnonymousPreferred,
                })
                .await
            }
        };
        let plan = build_it(&def).await.unwrap().expect("a plan");

        // The shipped data has no task-wide cell, so the local models are unmeasured
        // and the hosted one is too: the resolver picks nothing and the plan offers.
        assert!(matches!(
            plan.steps[0].answer,
            ResolverAnswer::TooWeak { .. }
        ));
        let hosted: Vec<&Offer> = plan.steps[0]
            .offers
            .iter()
            .filter(|o| o.subject == CapabilitySubject::Hosted)
            .collect();
        assert_eq!(hosted.len(), 1);
        let offer = hosted[0];
        assert_eq!(offer.strategy, OfferStrategy::PaidApi);
        assert_eq!(offer.standing, OfferStanding::NotMeasured);
        assert!(offer.selectable && offer.needs_approval);

        // The estimate is the gate's worst case at the user's price over the step's fixed
        // text and the model's own 128-token output limit.
        let PriceQuote::Known(price) = resolve_price(None, Some(PRICE_TEXT)) else {
            panic!("fixture price must parse")
        };
        let fixed = crate::conductor::executor::fixed_tokens_of_step(&step, "fixture-focus");
        let expected = worst_case(&price, u64::from(fixed), 128).worst_case_micros;
        assert_eq!(
            offer.est_cost_micros,
            Some(u64::try_from(expected).unwrap())
        );
        assert!(plan.needs_approval);
        assert!(plan.steps[0]
            .offers
            .iter()
            .any(|o| o.strategy == OfferStrategy::LocalUnmeasured));
        assert_eq!(
            plan.estimate_micros(&[]),
            0,
            "no pin: the default spends nothing"
        );
        let pin = Pin {
            action_id: "draft".to_owned(),
            strategy: OfferStrategy::PaidApi,
            provider_id: HOST.to_owned(),
            model_id: MODEL.to_owned(),
        };
        assert_eq!(
            plan.estimate_micros(&[pin]),
            u64::try_from(expected).unwrap()
        );

        // A step that sets num_predict is priced at it.
        let mut limited = step.clone();
        limited
            .options_override
            .insert("num_predict".to_owned(), serde_json::json!(40));
        let plan2 = build_it(&def_of(vec![limited])).await.unwrap().unwrap();
        let est40 = worst_case(&price, u64::from(fixed), 40).worst_case_micros;
        let o2 = plan2.steps[0]
            .offers
            .iter()
            .find(|o| o.subject == CapabilitySubject::Hosted)
            .unwrap();
        assert_eq!(o2.est_cost_micros, Some(u64::try_from(est40).unwrap()));

        // A price override of nothing: unpriced is not offered. No key: not offered.
        sqlx::query("UPDATE provider_models SET pricing = NULL")
            .execute(&pool)
            .await
            .unwrap();
        let unpriced = build_it(&def).await.unwrap().unwrap();
        assert!(unpriced.steps[0]
            .offers
            .iter()
            .all(|o| o.subject == CapabilitySubject::Local));
        assert_eq!(
            listing(&unpriced, StrategyKind::PaidApi),
            (false, "no_ready_hosted_model")
        );
        // Priced, but no output limit anywhere: no estimate, so not selectable.
        sqlx::query("UPDATE provider_models SET pricing = ?, request_options = NULL")
            .bind(PRICE_TEXT)
            .execute(&pool)
            .await
            .unwrap();
        let no_limit = build_it(&def).await.unwrap().unwrap();
        let o3 = no_limit.steps[0]
            .offers
            .iter()
            .find(|o| o.subject == CapabilitySubject::Hosted)
            .unwrap();
        assert_eq!(o3.est_cost_micros, None);
        assert_eq!(o3.not_selectable_reason, Some("no_estimate"));

        // Through all of it, nothing reached the provider.
        assert_eq!(server.connections(), 0);
        assert!(server.captured().is_empty());
    })
    .await;
}

// ---------------------------------------------------------------------------
// Scans of the module (the privacy contract)
// ---------------------------------------------------------------------------

fn code_of_the_module() -> String {
    include_str!("run_plan.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap()
        .to_owned()
}

/// No model or provider id is named in the plan, comments included (decisions.id=872, 875).
#[test]
fn no_model_or_provider_id_is_named_in_the_plan() {
    let code = code_of_the_module().to_lowercase();
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
    assert!(ids.len() > 20);
    for id in ids {
        assert!(
            !code.contains(&id.to_lowercase()),
            "the plan names '{id}': ids arrive in the inputs and are never written here"
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
        assert!(!code.contains(family), "'{family}' in the plan");
    }
}

/// The plan carries ids and numbers only: it names no prompt text, no user input, no
/// persona or personal data, no seed value, no model response, logs nothing and calls
/// nothing. The only I/O it has is the gatherer and the launch config read it is handed
/// a pool for.
#[test]
fn the_plan_holds_no_text_and_calls_nothing() {
    let code = code_of_the_module();
    for banned in [
        "user_input",
        "persona_context",
        "PersonalTrack",
        "personal_track",
        "store_tokens",
        "seeds",
        "voice_profile",
        "GenerateRequest",
        "generate(",
        "hosted_step_call",
        "metered_generate",
        "HostedClient",
        "reqwest",
        "log::",
        "println!",
        "eprintln!",
        "dbg!",
        "tracing::",
        "std::fs",
        "std::env",
        "std::net",
        "tauri::",
        "INSERT",
        "UPDATE",
        "DELETE",
    ] {
        assert!(!code.contains(banned), "'{banned}' in the plan module");
    }
    // The prompt template is read only to size it, never copied into a plan type.
    for line in code.lines().filter(|l| l.contains("prompt_template")) {
        assert!(
            line.contains("render_template")
                || line.trim_start().starts_with("//")
                || line.contains("fixed_tokens_of_step"),
            "{line}"
        );
    }
}

/// The pure core takes everything it decides on as input: compute, the digest and the
/// estimates have no `async`, no clock and no state. (build() is the only async fn.)
#[test]
fn only_build_is_async() {
    let code = code_of_the_module();
    assert_eq!(code.matches("async fn").count(), 1, "build() only");
    for forbidden in [
        "SystemTime",
        "Instant",
        "chrono",
        "static mut",
        "OnceLock",
        "Mutex",
    ] {
        assert!(
            !code.contains(forbidden),
            "'{forbidden}' in the plan module"
        );
    }
}
