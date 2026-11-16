// src-tauri/src/commands/run_plan.rs
//
// items.id=901 stage 8 (decisions.id=942 Q2 and Q3): the RUN PLAN COMMANDS, IPC Group 39 of
// HANDOFF_IPC_SURFACE.md (Group 36 in ipc.rs's own list), for the plan screen (items.id=902 and
// 904). Five commands, no screen:
//
//   get_run_plan(persona_id, context_key?, focus_id)
//       READ ONLY (it writes nothing and consumes nothing) except that a call with NO chat
//       registers the run-scoped offer in the memory-only home and returns its opaque
//       `plan_token`. The plan a run would act under: per planned step its bars, destinations,
//       the resolver's answer and the offers (models the user could pick), the strategies with
//       a reason id each, the mode alternatives, the cost estimate and the structure digest.
//       METADATA ONLY: ids, closed words and numbers. Never a prompt, a draft, a persona fact, a
//       seed value or a model response, and no display name (the screen maps an action id to its
//       label from the Focus's own data). Micro-USD amounts are decimal STRINGS.
//   approve_run_plan(request: ApproveRunPlanRequest { persona_id, context_key?, plan_token?,
//                    focus_id, digest, ceiling_micros, choices })
//       (a request struct: the generated bindings take at most ten arguments)
//       takes the PIN-FREE digest the user was shown, a per-run ceiling and the user's pins. The
//       server recomputes the plan and compares: a changed plan is `plan_changed` and approves
//       nothing. A chat's approval is persisted in the persona's encrypted outputs.db when the
//       chat can write, and held in memory only when the chat is latched, its latch cannot be
//       read or its database cannot be opened (conductor::plan_approval::save_chat_approval,
//       the one writer). A run with no chat is approved by the plan_token its offer returned.
//   get_run_plan_approval(persona_id, context_key, focus_id)
//       READ ONLY: whether a chat holds an approval for the Focus, in which home, its digest,
//       ceiling and pins, and whether the chat is a one-time chat.
//   revoke_run_plan_approval(persona_id, context_key?, plan_token?, focus_id)
//       removes the approval from BOTH homes (or the offer a token names). Not latch-gated: it
//       only removes a permission.
//   get_run_park(persona_id, run_id)
//       READ ONLY: why a run is parked (registered code, step id, filtered parameters), from the
//       memory-only home first and then run_parks. None when the run is not parked.
//
// EXACTLY ONE of context_key and plan_token names the target of approve and revoke: both or
// neither is `invalid_context_key`.
//
// WHAT THIS NEVER DOES: widen a gate. An approval only lets the executor go beyond what the
// resolver would pick on its own, and the executor, the spend gate and the outbound review
// still run (conductor::plan_gate, executor, spend::gate). Nothing here reads a launch value as
// an approval, and the server never uses a digest the client sends except to compare it with the
// plan it just recomputed.
//
// The same shape as the launch, consent and device commands: writer_gate::command first (a key
// rotation waits for it), the user and key from the unlocked session (no user_id argument to
// spoof), the Persona id a canonical UUID that belongs to that user (persona_not_found for a
// malformed and a foreign id alike), the Focus id a slug, and the work in a *_for_session body
// with no Tauri state, tested directly. The plan is computed on a probe run built by the same
// constructor as the send preflight (execution::plan_probe_run), so the digest a screen is shown
// is the digest the run recomputes.
//
// ERROR CODES (ERROR_CODES; each has a placeholder key plan.error.<code> in the frontend's
// en.json and a test requires it): persona_not_found, invalid_focus_id, invalid_context_key,
// invalid_plan_token, invalid_run_id, no_plan, plan_changed, invalid_choice, invalid_ceiling,
// plan_too_many_paths, plan_unavailable. plan_unavailable is any other failure (the Focus or its
// launch answers cannot be loaded, a store cannot be read) and carries no detail. No code or log
// line holds an id from the request or a value from a plan.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::execution::plan_probe_run;
use crate::commands::persona::session_user_and_key;
use crate::conductor::concurrency::ConductorScheduler;
use crate::conductor::lifecycle::{LifecycleError, PlanOfferError};
use crate::conductor::model_capability::AttainedBar;
use crate::conductor::model_resolver::CostTier;
use crate::conductor::plan_approval::{
    self, read_chat_approval_with_home, revoke_chat_approval, save_chat_approval, Approval,
    ApprovalError, Home, HomeKind, PLAN_CEILING_MAX_MICROS,
};
use crate::conductor::plan_memory::{ChatScope, OfferError, OfferOwner, PlanMemory};
use crate::conductor::run_plan::{
    bar_str, destination_str, Offer, OfferStanding, OfferStrategy, Pin, PlanStep, ResolverAnswer,
    RunPlan,
};
use crate::persistence::{output_store, persona_store, run_park_store, session_store};

/// Every error string this module returns.
pub const ERROR_CODES: [&str; 11] = [
    "persona_not_found",
    "invalid_focus_id",
    "invalid_context_key",
    "invalid_plan_token",
    "invalid_run_id",
    "no_plan",
    "plan_changed",
    "invalid_choice",
    "invalid_ceiling",
    "plan_too_many_paths",
    "plan_unavailable",
];

/// The most choices (pins) an approval may carry, a bound on the request.
const MAX_CHOICES: usize = 64;
/// A plan token is 16 random bytes as lowercase hex.
const TOKEN_LEN: usize = 32;

// ---------------------------------------------------------------------------
// DTOs. Ids, closed words, booleans and decimal strings only (a test serializes a real plan
// and checks every string value, and a scan pins that no text source is named here).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ResolverAnswerView {
    /// selected, too_weak or no_candidate.
    pub kind: String,
    // selected:
    pub subject: Option<String>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub cost_tier: Option<String>,
    /// micro-USD, a decimal string.
    pub est_cost_micros: Option<String>,
    // too_weak:
    pub required_bar: Option<String>,
    // no_candidate:
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct OfferView {
    /// local, local_unmeasured or paid_api.
    pub strategy: String,
    /// local or hosted.
    pub subject: String,
    /// Empty for a local model.
    pub provider_id: String,
    pub model_id: String,
    /// local, free, plan_included or metered.
    pub cost_tier: String,
    /// The gate's worst case in micro-USD (a decimal string), or None when there is none.
    pub est_cost_micros: Option<String>,
    /// qualifies, below_bar or not_measured.
    pub standing: String,
    /// below_basic, basic, standard or high, when measured.
    pub attained_bar: Option<String>,
    pub above_worth_bar: bool,
    pub selectable: bool,
    /// below_bar or no_estimate, when not selectable.
    pub not_selectable_reason: Option<String>,
    pub needs_approval: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct PlanStepView {
    pub action_id: String,
    /// model_block.
    pub kind: String,
    pub task_type: String,
    pub required_bar: String,
    pub worth_paying_bar: String,
    /// local, hosted_api, manual_handoff.
    pub destinations: Vec<String>,
    /// local_only, anonymous_required, anonymous_preferred or unrestricted.
    pub highest_tier: String,
    pub hosted_possible: bool,
    pub answer: ResolverAnswerView,
    pub offers: Vec<OfferView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct StrategyView {
    /// local, paid_api or manual_handoff.
    pub kind: String,
    pub listed: bool,
    /// A reason id (a closed word).
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ModeAlternativeView {
    pub mode_option_id: String,
    /// The strategy the Focus would take under that mode, or None with the reason.
    pub strategy: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct RunPlanView {
    pub focus_id: String,
    /// The pin-free digest: what approve_run_plan must be given back.
    pub digest: String,
    pub needs_approval: bool,
    /// The chat already holds an approval that matches this plan (a run would proceed).
    /// Always false for a run with no chat.
    pub approved: bool,
    pub handoff_listed: bool,
    pub document_profile: Option<String>,
    /// The dearest reachable path with no pins, in micro-USD (a decimal string).
    pub estimate_micros: String,
    /// The most a ceiling may be, in micro-USD (a decimal string).
    pub ceiling_max_micros: String,
    /// A run with no chat: the opaque single-use token of its offer. None for a chat.
    pub plan_token: Option<String>,
    pub steps: Vec<PlanStepView>,
    pub strategies: Vec<StrategyView>,
    pub alternatives: Vec<ModeAlternativeView>,
}

/// One explicit choice of the user, as sent and as stored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Type)]
pub struct PinInput {
    pub action_id: String,
    /// local_unmeasured or paid_api.
    pub strategy: String,
    /// Empty for a local model.
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct PinView {
    pub action_id: String,
    pub strategy: String,
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct RunPlanApprovalView {
    pub focus_id: String,
    /// An approval is held.
    pub approved: bool,
    /// persisted or memory, when approved.
    pub home: Option<String>,
    pub digest: Option<String>,
    /// micro-USD, a decimal string.
    pub ceiling_micros: Option<String>,
    pub choices: Vec<PinView>,
    /// The chat cannot save (latched, or its latch could not be read): its approval lives in
    /// memory only and is asked again after a restart.
    pub one_time_chat: bool,
    /// Echoed for a run with no chat.
    pub plan_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct RunPlanRevocationView {
    /// An approval or offer was removed from the memory-only home.
    pub memory: bool,
    /// An approval row was removed from the persisted home.
    pub persisted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct RunParkView {
    pub run_id: String,
    pub action_id: String,
    /// A registered code (spend::codes::Code).
    pub code: String,
    /// A JSON object of ids, counts and reason words, at most 4096 bytes.
    pub params_json: String,
}

// ---------------------------------------------------------------------------
// Shared checks
// ---------------------------------------------------------------------------

/// What the *_for_session bodies need: no Tauri state.
pub(crate) struct PlanEnv<'a> {
    pub pool: &'a sqlx::SqlitePool,
    pub scheduler: Arc<ConductorScheduler>,
    pub memory: &'a PlanMemory,
    pub user_id: &'a str,
    pub key_hex: &'a str,
}

fn unavailable(what: &str) -> String {
    // A fixed string: no id, no value and no error text.
    log::warn!("run_plan: {what}");
    "plan_unavailable".to_owned()
}

/// The Persona id must be a canonical UUID AND belong to the signed-in user. The two
/// failures answer alike. Private on purpose, like the other commands' copies, so
/// items.id=638 can lift it into one shared check.
async fn require_persona(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: &str,
) -> Result<(), String> {
    let canonical = uuid::Uuid::parse_str(persona_id)
        .map(|u| u.hyphenated().to_string() == persona_id)
        .unwrap_or(false);
    if !canonical {
        return Err("persona_not_found".to_owned());
    }
    match persona_store::is_user_in_persona(pool, user_id, persona_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err("persona_not_found".to_owned()),
        Err(_) => Err(unavailable("persona lookup")),
    }
}

/// A Focus slug: lowercase letters, digits, '-' and '_', 1 to 64 characters.
fn require_focus_id(focus_id: &str) -> Result<(), String> {
    let ok = !focus_id.is_empty()
        && focus_id.len() <= 64
        && focus_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err("invalid_focus_id".to_owned())
    }
}

fn require_context_key(context_key: &str) -> Result<(), String> {
    session_store::validate_context_key(context_key).map_err(|_| "invalid_context_key".to_owned())
}

fn require_token(token: &str) -> Result<(), String> {
    if token.len() == TOKEN_LEN
        && token
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        Ok(())
    } else {
        Err("invalid_plan_token".to_owned())
    }
}

/// A run id: letters, digits, '-' and '_', 1 to 128 characters (run ids are UUIDs).
fn require_run_id(run_id: &str) -> Result<(), String> {
    let ok = !run_id.is_empty()
        && run_id.len() <= 128
        && run_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err("invalid_run_id".to_owned())
    }
}

/// What an approval or revocation names: a chat, or the offer of a run with no chat.
enum Target {
    Chat(String),
    Token(String),
}

/// Exactly one of the two; both or neither is `invalid_context_key`.
fn target_of(context_key: Option<&str>, plan_token: Option<&str>) -> Result<Target, String> {
    match (context_key, plan_token) {
        (Some(chat), None) => {
            require_context_key(chat)?;
            Ok(Target::Chat(chat.to_owned()))
        }
        (None, Some(token)) => {
            require_token(token)?;
            Ok(Target::Token(token.to_owned()))
        }
        _ => Err("invalid_context_key".to_owned()),
    }
}

fn scope_of(env: &PlanEnv<'_>, persona_id: &str, chat: &str, focus_id: &str) -> ChatScope {
    ChatScope {
        user_id: env.user_id.to_owned(),
        persona_id: persona_id.to_owned(),
        context_key: chat.to_owned(),
        focus_id: focus_id.to_owned(),
    }
}

fn owner_of(env: &PlanEnv<'_>, persona_id: &str, focus_id: &str) -> OfferOwner {
    OfferOwner {
        user_id: env.user_id.to_owned(),
        persona_id: persona_id.to_owned(),
        focus_id: focus_id.to_owned(),
    }
}

/// The plan of a run for this Focus and chat (None: no chat), on a probe run built like the
/// send preflight's, and whether the chat already holds a matching approval.
async fn offer_for(
    env: &PlanEnv<'_>,
    persona_id: &str,
    focus_id: &str,
    context_key: Option<&str>,
) -> Result<(RunPlan, bool), String> {
    let mut run = plan_probe_run(
        Arc::clone(&env.scheduler),
        env.pool.clone(),
        env.memory.clone(),
        env.key_hex.to_owned(),
        env.user_id.to_owned(),
        persona_id.to_owned(),
        focus_id.to_owned(),
        context_key.map(str::to_owned),
    );
    match run.load().await {
        Ok(()) => {}
        Err(LifecycleError::FocusNotFound(_)) => return Err("invalid_focus_id".to_owned()),
        Err(_) => return Err(unavailable("the Focus could not be loaded")),
    }
    run.authorize_checks()
        .await
        .map_err(|_| unavailable("the run could not be authorized"))?;
    match run.plan_offer().await {
        Ok(Some(found)) => Ok(found),
        Ok(None) => Err("no_plan".to_owned()),
        Err(PlanOfferError::TooManyPaths) => Err("plan_too_many_paths".to_owned()),
        Err(PlanOfferError::Unavailable) => Err(unavailable("the plan could not be built")),
    }
}

// ---------------------------------------------------------------------------
// Views of a plan
// ---------------------------------------------------------------------------

fn cost_tier_str(t: CostTier) -> &'static str {
    match t {
        CostTier::Local => "local",
        CostTier::Free => "free",
        CostTier::PlanIncluded => "plan_included",
        CostTier::Metered => "metered",
    }
}

fn micros(v: u64) -> String {
    v.to_string()
}

fn answer_view(a: &ResolverAnswer) -> ResolverAnswerView {
    let mut v = ResolverAnswerView {
        kind: String::new(),
        subject: None,
        provider_id: None,
        model_id: None,
        cost_tier: None,
        est_cost_micros: None,
        required_bar: None,
        reason: None,
    };
    match a {
        ResolverAnswer::Selected {
            subject,
            provider_id,
            model_id,
            cost_tier,
            est_cost_micros,
        } => {
            v.kind = "selected".to_owned();
            v.subject = Some(subject.as_str().to_owned());
            v.provider_id = Some(provider_id.clone());
            v.model_id = Some(model_id.clone());
            v.cost_tier = Some(cost_tier_str(*cost_tier).to_owned());
            v.est_cost_micros = Some(micros(*est_cost_micros));
        }
        ResolverAnswer::TooWeak { required } => {
            v.kind = "too_weak".to_owned();
            v.required_bar = Some(bar_str(*required).to_owned());
        }
        ResolverAnswer::NoCandidate(reason) => {
            use crate::conductor::model_resolver::NoCandidateReason as R;
            v.kind = "no_candidate".to_owned();
            v.reason = Some(
                match reason {
                    R::NoDestination => "no_destination",
                    R::NoneReady => "none_ready",
                    R::ContextTooSmall => "context_too_small",
                }
                .to_owned(),
            );
        }
    }
    v
}

fn offer_view(o: &Offer) -> OfferView {
    let (standing, attained): (&str, Option<AttainedBar>) = match o.standing {
        OfferStanding::Qualifies(a) => ("qualifies", Some(a)),
        OfferStanding::BelowBar(a) => ("below_bar", Some(a)),
        OfferStanding::NotMeasured => ("not_measured", None),
    };
    OfferView {
        strategy: o.strategy.as_str().to_owned(),
        subject: o.subject.as_str().to_owned(),
        provider_id: o.provider_id.clone(),
        model_id: o.model_id.clone(),
        cost_tier: cost_tier_str(o.cost_tier).to_owned(),
        est_cost_micros: o.est_cost_micros.map(micros),
        standing: standing.to_owned(),
        attained_bar: attained.map(|a| a.as_str().to_owned()),
        above_worth_bar: o.above_worth_bar,
        selectable: o.selectable,
        not_selectable_reason: o.not_selectable_reason.map(str::to_owned),
        needs_approval: o.needs_approval,
    }
}

fn step_view(s: &PlanStep) -> PlanStepView {
    PlanStepView {
        action_id: s.action_id.clone(),
        kind: s.kind.as_str().to_owned(),
        task_type: s.task_type.to_owned(),
        required_bar: bar_str(s.required_bar).to_owned(),
        worth_paying_bar: bar_str(s.worth_paying_bar).to_owned(),
        destinations: s
            .destinations
            .iter()
            .map(|d| destination_str(*d).to_owned())
            .collect(),
        highest_tier: s.highest_tier.as_str().to_owned(),
        hosted_possible: s.hosted_possible,
        answer: answer_view(&s.answer),
        offers: s.offers.iter().map(offer_view).collect(),
    }
}

fn plan_view(plan: &RunPlan, approved: bool, plan_token: Option<String>) -> RunPlanView {
    RunPlanView {
        focus_id: plan.focus_id.clone(),
        digest: plan.digest.clone(),
        needs_approval: plan.needs_approval,
        approved,
        handoff_listed: plan.handoff_listed,
        document_profile: plan.profile.clone(),
        estimate_micros: micros(plan.estimate_micros(&[])),
        ceiling_max_micros: micros(PLAN_CEILING_MAX_MICROS),
        plan_token,
        steps: plan.steps.iter().map(step_view).collect(),
        strategies: plan
            .strategies
            .iter()
            .map(|s| StrategyView {
                kind: s.kind.as_str().to_owned(),
                listed: s.listed,
                reason: s.reason.to_owned(),
            })
            .collect(),
        alternatives: plan
            .alternatives
            .iter()
            .map(|a| ModeAlternativeView {
                mode_option_id: a.mode_option_id.clone(),
                strategy: a.kind.map(|k| k.as_str().to_owned()),
                reason: a.reason.to_owned(),
            })
            .collect(),
    }
}

fn pin_view(p: &Pin) -> PinView {
    PinView {
        action_id: p.action_id.clone(),
        strategy: p.strategy.as_str().to_owned(),
        provider_id: p.provider_id.clone(),
        model_id: p.model_id.clone(),
    }
}

fn approval_view(
    focus_id: &str,
    approval: Option<(&Approval, &str)>,
    one_time_chat: bool,
    plan_token: Option<String>,
) -> RunPlanApprovalView {
    RunPlanApprovalView {
        focus_id: focus_id.to_owned(),
        approved: approval.is_some(),
        home: approval.map(|(_, h)| h.to_owned()),
        digest: approval.map(|(a, _)| a.digest.clone()),
        ceiling_micros: approval.map(|(a, _)| micros(a.ceiling_micros)),
        choices: approval
            .map(|(a, _)| a.pins.iter().map(pin_view).collect())
            .unwrap_or_default(),
        one_time_chat,
        plan_token,
    }
}

fn home_str(h: HomeKind) -> &'static str {
    match h {
        HomeKind::Persisted => "persisted",
        HomeKind::Memory => "memory",
    }
}

/// Whether the chat is a one-time chat. A latch that cannot be read counts as one: the
/// failure direction is "nothing is saved", as everywhere the latch is read.
async fn latched(conn: Option<&mut sqlx::SqliteConnection>, chat: &str) -> bool {
    match conn {
        None => true,
        Some(c) => !matches!(
            session_store::write_mode(c, chat).await,
            Ok(session_store::WriteMode::Open)
        ),
    }
}

async fn open_conn(env: &PlanEnv<'_>, persona_id: &str) -> Option<sqlx::SqliteConnection> {
    output_store::open_outputs_db_retrying(env.user_id, persona_id, env.key_hex)
        .await
        .ok()
}

// ---------------------------------------------------------------------------
// get_run_plan
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn get_run_plan(
    persona_id: String,
    context_key: Option<String>,
    focus_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    scheduler: State<'_, Arc<ConductorScheduler>>,
    plan_memory: State<'_, PlanMemory>,
) -> Result<RunPlanView, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    let env = PlanEnv {
        pool: &pool,
        scheduler: Arc::clone(&scheduler),
        memory: &plan_memory,
        user_id: &user_id,
        key_hex: &key_hex,
    };
    get_run_plan_for_session(&env, &persona_id, context_key.as_deref(), &focus_id).await
}

pub(crate) async fn get_run_plan_for_session(
    env: &PlanEnv<'_>,
    persona_id: &str,
    context_key: Option<&str>,
    focus_id: &str,
) -> Result<RunPlanView, String> {
    require_persona(env.pool, env.user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    if let Some(chat) = context_key {
        require_context_key(chat)?;
    }
    let (plan, approved) = offer_for(env, persona_id, focus_id, context_key).await?;
    // A run with no chat is approved through the offer registered here.
    let token = match context_key {
        Some(_) => None,
        None => Some(
            env.memory
                .register_offer(&owner_of(env, persona_id, focus_id), &plan.digest)
                .map_err(|_| unavailable("the offer could not be registered"))?,
        ),
    };
    Ok(plan_view(&plan, approved, token))
}

// ---------------------------------------------------------------------------
// approve_run_plan
// ---------------------------------------------------------------------------

/// What approve_run_plan takes (a request struct: the generated bindings take at most ten
/// arguments, and with the Tauri state this command has more).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Type)]
pub struct ApproveRunPlanRequest {
    pub persona_id: String,
    /// The chat. Exactly one of context_key and plan_token.
    pub context_key: Option<String>,
    /// The token get_run_plan returned for a run with no chat.
    pub plan_token: Option<String>,
    pub focus_id: String,
    /// The PIN-FREE digest get_run_plan showed.
    pub digest: String,
    /// micro-USD, a decimal string.
    pub ceiling_micros: String,
    pub choices: Vec<PinInput>,
}

#[tauri::command]
#[specta::specta]
pub async fn approve_run_plan(
    request: ApproveRunPlanRequest,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    scheduler: State<'_, Arc<ConductorScheduler>>,
    plan_memory: State<'_, PlanMemory>,
) -> Result<RunPlanApprovalView, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    let env = PlanEnv {
        pool: &pool,
        scheduler: Arc::clone(&scheduler),
        memory: &plan_memory,
        user_id: &user_id,
        key_hex: &key_hex,
    };
    approve_run_plan_for_session(&env, &request).await
}

/// A ceiling as sent: decimal digits only, no sign, no space.
fn parse_ceiling(s: &str) -> Result<u64, String> {
    if s.is_empty() || s.len() > 20 || !s.chars().all(|c| c.is_ascii_digit()) {
        return Err("invalid_ceiling".to_owned());
    }
    s.parse::<u64>().map_err(|_| "invalid_ceiling".to_owned())
}

fn parse_pins(choices: &[PinInput]) -> Result<Vec<Pin>, String> {
    if choices.len() > MAX_CHOICES {
        return Err("invalid_choice".to_owned());
    }
    choices
        .iter()
        .map(|c| {
            // Only an unmeasured local model or a paid model can be pinned; anything else,
            // `local` included, is not a choice.
            let strategy = match c.strategy.as_str() {
                "local_unmeasured" => OfferStrategy::LocalUnmeasured,
                "paid_api" => OfferStrategy::PaidApi,
                _ => return Err("invalid_choice".to_owned()),
            };
            Ok(Pin {
                action_id: c.action_id.clone(),
                strategy,
                provider_id: c.provider_id.clone(),
                model_id: c.model_id.clone(),
            })
        })
        .collect()
}

fn approval_error(e: ApprovalError) -> String {
    match e {
        ApprovalError::PlanChanged => "plan_changed".to_owned(),
        ApprovalError::UnknownStep
        | ApprovalError::DuplicatePin
        | ApprovalError::PinNotAllowed
        | ApprovalError::PinNotOffered
        | ApprovalError::NotSelectable => "invalid_choice".to_owned(),
        ApprovalError::CeilingBelowEstimate | ApprovalError::CeilingTooHigh => {
            "invalid_ceiling".to_owned()
        }
        ApprovalError::BadChoices | ApprovalError::Unreadable | ApprovalError::Unavailable => {
            unavailable("the approval could not be made")
        }
    }
}

pub(crate) async fn approve_run_plan_for_session(
    env: &PlanEnv<'_>,
    request: &ApproveRunPlanRequest,
) -> Result<RunPlanApprovalView, String> {
    let persona_id = request.persona_id.as_str();
    let focus_id = request.focus_id.as_str();
    let digest = request.digest.as_str();
    let (context_key, plan_token) = (
        request.context_key.as_deref(),
        request.plan_token.as_deref(),
    );
    let (ceiling_micros, choices) = (request.ceiling_micros.as_str(), request.choices.as_slice());
    require_persona(env.pool, env.user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    let target = target_of(context_key, plan_token)?;
    let ceiling = parse_ceiling(ceiling_micros)?;
    let pins = parse_pins(choices)?;

    // The plan is recomputed for the same chat (or none): the client's digest is only compared.
    let chat = match &target {
        Target::Chat(c) => Some(c.as_str()),
        Target::Token(_) => None,
    };
    let (plan, _) = offer_for(env, persona_id, focus_id, chat).await?;
    let approval = plan_approval::approve(&plan, digest, ceiling, pins).map_err(approval_error)?;

    match target {
        Target::Chat(chat) => {
            let scope = scope_of(env, persona_id, &chat, focus_id);
            let mut conn = open_conn(env, persona_id).await;
            let home = save_chat_approval(env.memory, conn.as_mut(), &scope, &approval)
                .await
                .map_err(approval_error)?;
            let home = match home {
                Home::Persisted { .. } => HomeKind::Persisted,
                Home::Memory => HomeKind::Memory,
            };
            let one_time = latched(conn.as_mut(), &chat).await;
            Ok(approval_view(
                focus_id,
                Some((&approval, home_str(home))),
                one_time,
                None,
            ))
        }
        Target::Token(token) => {
            env.memory
                .approve_offer(
                    &token,
                    &owner_of(env, persona_id, focus_id),
                    digest,
                    approval.clone(),
                )
                .map_err(|e| match e {
                    OfferError::NotFound => "invalid_plan_token".to_owned(),
                    OfferError::PlanChanged => "plan_changed".to_owned(),
                    OfferError::NoRandomness => unavailable("the offer could not be approved"),
                })?;
            Ok(approval_view(
                focus_id,
                Some((&approval, "memory")),
                false,
                Some(token),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// get_run_plan_approval
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn get_run_plan_approval(
    persona_id: String,
    context_key: String,
    focus_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    scheduler: State<'_, Arc<ConductorScheduler>>,
    plan_memory: State<'_, PlanMemory>,
) -> Result<RunPlanApprovalView, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    let env = PlanEnv {
        pool: &pool,
        scheduler: Arc::clone(&scheduler),
        memory: &plan_memory,
        user_id: &user_id,
        key_hex: &key_hex,
    };
    get_run_plan_approval_for_session(&env, &persona_id, &context_key, &focus_id).await
}

pub(crate) async fn get_run_plan_approval_for_session(
    env: &PlanEnv<'_>,
    persona_id: &str,
    context_key: &str,
    focus_id: &str,
) -> Result<RunPlanApprovalView, String> {
    require_persona(env.pool, env.user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    require_context_key(context_key)?;
    let scope = scope_of(env, persona_id, context_key, focus_id);
    let mut conn = open_conn(env, persona_id).await;
    // An approval that cannot be read is an error here, never "none": the screen must not
    // say "ask again" about a chat whose state it does not know.
    let held = read_chat_approval_with_home(env.memory, conn.as_mut(), &scope)
        .await
        .map_err(|_| unavailable("the approval could not be read"))?;
    let one_time = latched(conn.as_mut(), context_key).await;
    Ok(approval_view(
        focus_id,
        held.as_ref().map(|(a, h)| (a, home_str(*h))),
        one_time,
        None,
    ))
}

// ---------------------------------------------------------------------------
// revoke_run_plan_approval
// ---------------------------------------------------------------------------

// 8 arguments: the command's four plus the four Tauri states every plan command takes.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
#[specta::specta]
pub async fn revoke_run_plan_approval(
    persona_id: String,
    context_key: Option<String>,
    plan_token: Option<String>,
    focus_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    scheduler: State<'_, Arc<ConductorScheduler>>,
    plan_memory: State<'_, PlanMemory>,
) -> Result<RunPlanRevocationView, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    let env = PlanEnv {
        pool: &pool,
        scheduler: Arc::clone(&scheduler),
        memory: &plan_memory,
        user_id: &user_id,
        key_hex: &key_hex,
    };
    revoke_run_plan_approval_for_session(
        &env,
        &persona_id,
        context_key.as_deref(),
        plan_token.as_deref(),
        &focus_id,
    )
    .await
}

pub(crate) async fn revoke_run_plan_approval_for_session(
    env: &PlanEnv<'_>,
    persona_id: &str,
    context_key: Option<&str>,
    plan_token: Option<&str>,
    focus_id: &str,
) -> Result<RunPlanRevocationView, String> {
    require_persona(env.pool, env.user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    match target_of(context_key, plan_token)? {
        Target::Chat(chat) => {
            let scope = scope_of(env, persona_id, &chat, focus_id);
            let mut conn = open_conn(env, persona_id).await;
            // Not latch-gated: a revocation only removes a permission. Both homes.
            let revoked = revoke_chat_approval(env.memory, conn.as_mut(), &scope)
                .await
                .map_err(|_| unavailable("the approval could not be revoked"))?;
            Ok(RunPlanRevocationView {
                memory: revoked.memory,
                persisted: revoked.persisted,
            })
        }
        Target::Token(token) => {
            let removed = env
                .memory
                .revoke_offer(&token, &owner_of(env, persona_id, focus_id));
            Ok(RunPlanRevocationView {
                memory: removed,
                persisted: false,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// get_run_park
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn get_run_park(
    persona_id: String,
    run_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    scheduler: State<'_, Arc<ConductorScheduler>>,
    plan_memory: State<'_, PlanMemory>,
) -> Result<Option<RunParkView>, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    let env = PlanEnv {
        pool: &pool,
        scheduler: Arc::clone(&scheduler),
        memory: &plan_memory,
        user_id: &user_id,
        key_hex: &key_hex,
    };
    get_run_park_for_session(&env, &persona_id, &run_id).await
}

pub(crate) async fn get_run_park_for_session(
    env: &PlanEnv<'_>,
    persona_id: &str,
    run_id: &str,
) -> Result<Option<RunParkView>, String> {
    require_persona(env.pool, env.user_id, persona_id).await?;
    require_run_id(run_id)?;
    // Memory first: a park held there is only ever the newer one (conductor::lifecycle).
    if let Some(held) = env.memory.get_run_park(env.user_id, persona_id, run_id) {
        return Ok(Some(RunParkView {
            run_id: run_id.to_owned(),
            action_id: held.action_id,
            code: held.code.as_str().to_owned(),
            params_json: held.params_json,
        }));
    }
    let mut conn = open_conn(env, persona_id)
        .await
        .ok_or_else(|| unavailable("outputs.db could not be opened"))?;
    let stored = run_park_store::get_park(&mut conn, run_id)
        .await
        .map_err(|_| unavailable("the park could not be read"))?;
    Ok(stored.map(|p| RunParkView {
        run_id: p.run_id,
        action_id: p.action_id,
        code: p.code,
        params_json: p.params_json,
    }))
}

#[cfg(test)]
#[path = "run_plan_commands_tests.rs"]
mod tests;
