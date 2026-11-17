// src-tauri/src/conductor/executor.rs
//
// StepExecutor — 15-step execution sequence for a single Conductor step.
// StepContext — fully-resolved metadata for one step (metadata only).
//
// Python oracle: conductor/executor.py — StepExecutor class + StepContext dataclass.
//
// Step sequence (Architecture Section 6.3):
//   Floor invariants  — explicit Err(ConductorError), not assert! (D6-348)
//   Step 3  — tier ceiling gate
//   Step 4  — cloud_frontier handled by lifecycle; never reaches executor
//   Steps 6-7 — PG_GATE_1 (field approval + abstraction)
//   Field projection — step-scope boundary
//   Floor Consent Gate — await_floor_consent or floor_consent_auto event
//   Step 8  — prompt assembly (render_template; with or without disclosure)
//   Step 5  — context window check (after prompt assembly, matching oracle order)
//   Step 10 — acquire inference slot; generate; release slot (always released)
//   Step 11 — PG_GATE_2 (inbound contamination scan)
//   Step 12 — TaskTrack.add_step(); step_sensitivity computation
//   Step 13 — PG_GATE_3 (cross-tier content promotion, conditional)
//
// Rust deviations from Python oracle:
//   - StepContext holds metadata only, not tracks or gateway (D6-342 actor model).
//     PersonalTrack, TaskTrack, SharedStateTrack, FailureHandler,
//     PrivacyGateway, and ConductorScheduler are separate parameters.
//   - HostedClient and OllamaClient are module-level OnceLock singletons (D6-349).
//     HostedClient (items.id=709) is stateless: provider row, model row and the
//     user's stored key are resolved per call into a CallContext.
//   - No regex crate available; TOKEN_PATTERN scanning and PII detection use manual
//     char-level implementations with semantically identical behaviour to the Python
//     oracle's re.compile() patterns.
//   - scan_voice_profile() is async and returns Result<HashMap, ConductorError>
//     rather than raising. qr_local: Ok(cleaned), contaminated attrs stripped.
//     Tier 2+: Err(ConductorError::VoiceProfileContamination) on first hit.
//   - VP contamination audit uses privacy_gateway.logger.write() directly;
//     PrivacyGateway has no record_voice_profile_contamination() method.
//   - to_gate_track() bridges conductor::types::PersonalTrack (sqlx-friendly
//     String fields) -> privacy::types::PersonalTrack (typed enum fields) for gates.
//   - output_vars collected from task_track.steps() at call time (no public
//     output_vars() accessor on TaskTrack; avoids modifying types.rs).
//   - render methods are sync (scan_voice_profile + output_var collection happen
//     before render, results passed in); this avoids async closure complexity.
//   - floor invariant violations return Err(ConductorError::TierBoundaryViolation)
//     (D6-348 — debug_assert! stripped in release builds).
//
// PII detection notes:
//   word-boundary: str::find + ASCII char boundary checks (semantically equiv to
//     re.search(r"\b" + re.escape(pv) + r"\b", ..., re.IGNORECASE) for the values
//     personal fields contain in practice).
//   email: contains('@') with both sides non-empty and at least one '.' after '@'.
//   digit-dense: 7+ consecutive ASCII digits via char scan (equiv to r"\d{7,}").
//
// Rename chain (CLAUDE.md):
//   path_id -> focus_id | path_run_id -> focus_run_id
//   persona_context replaces space_context / life_context (D6-323)
//   space_max_permitted_tier retained (correct semantic)

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use indexmap::IndexMap;
use serde_json;

use crate::conductor::concurrency::{ConductorScheduler, PathPriority};
use crate::conductor::failure::{
    ConductorError, FailureAction, FailureHandler, FailureResult, FailureSeverity, MAX_RETRIES,
    RETRY_AFTER_METADATA_KEY,
};
use crate::conductor::model_selection::{resolve_hosted_model, ModelError};
use crate::conductor::privacy::logger::{DisclosureLogEntry, DisclosureLogger};
use crate::conductor::privacy::types::{
    AbstractionPolicy, PersonalField as GateField, PersonalTrack as GateTrack, Sensitivity,
};
use crate::conductor::privacy::PrivacyGateway;
use crate::conductor::step_review::{self, ReviewedPrompt};
use crate::conductor::step_treatment::{
    self, floor_invariant_violation, Basis, CeilingValues, Chosen, StepTreatment,
};
use crate::conductor::tokens::{ActionDefinition, ExternalAccess};
use crate::conductor::types::{PersonalTrack, SharedStateTrack, TaskStep, TaskTrack};
use crate::final_failure;
use crate::persistence::integration_keys_store::{self, QR_HOSTED_KEY_TYPE};
use crate::persistence::provider_store::{self, Provider, ProviderModel};
use crate::providers::errors::ProviderError;
use crate::providers::hosted_call::{CallContext, HostedRoute};
use crate::providers::hosted_client::{HostedClient, HostedRequest};
use crate::providers::hosted_stream::{NullSink, StreamEnd, StreamOutcome, StreamSink};
use crate::providers::ollama_client::{check_context_window, OllamaClient};
use crate::providers::types::{
    CompletionStatus, ContextWindowStatusKind, GenerateOptions, GenerateRequest, GenerateResponse,
};
use crate::retry_wait::{self, RetryWaitIdentity, RetryWaitNotice, RetryWaitReason};
use crate::spend::codes::Code;
use crate::spend::gate::{
    block_metadata, metered_generate, metered_generate_streamed, RunAttendance, SpendBlock,
    SpendCall, SpendCallError,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The longest Retry-After the retry loop will sit through (items.id=710).
/// A provider asking for more than this parks the run for the user instead of
/// holding it asleep; the wait before any one retry is therefore bounded, and
/// the number of retries by MAX_RETRIES.
const RETRY_AFTER_CAP_SECS: u64 = 60;

/// Backoff before a hosted retry that carries no Retry-After, or a shorter
/// one (items.id=727): full jitter, a random wait in [0, min(cap, base x
/// 2^attempt)], attempt counted from 0. With MAX_RETRIES = 3 the ceilings are
/// 1 s, 2 s and 4 s, so the cap is not reached today; it keeps the bound
/// stated if MAX_RETRIES is ever raised.
const BACKOFF_BASE_MS: u64 = 1_000;

/// The longest computed backoff before one hosted retry (items.id=727). A
/// Retry-After is the floor of the wait (the wait is the larger of the two,
/// never their sum), so one retry waits at most the larger of this and
/// RETRY_AFTER_CAP_SECS, and a run of retries at most MAX_RETRIES times that.
const BACKOFF_CAP_SECS: u64 = 30;

/// The failure_mode of the hosted-provider class (failure.rs F10). A Retry
/// result with this mode is a hosted call that failed; F1 (local Ollama) and
/// F2 (quality) retries are not, and wait for nothing.
const HOSTED_FAILURE_MODE: &str = "F10";

/// Minimum personal field value length considered for word-boundary PII matching.
/// Avoids false positives from short values like "OR", "CA", "I", "a".
/// Python oracle: _VP_MIN_FIELD_LENGTH = 8
const VP_MIN_FIELD_LENGTH: usize = 8;

/// floor_consent_preference value indicating pre-authorized consent to proceed
/// with floor-clamped fields in this run. Set in personas.extra_metadata.
/// Python oracle: ctx.floor_consent_preference == "modified" check in _execute_once().
const FLOOR_CONSENT_MODIFIED: &str = "modified";

/// Personal voice attribute allowlist. Only these keys are injected into prompts.
/// Unknown keys are excluded and logged. Sorted output enforces prompt reproducibility.
/// Python oracle: ALLOWED_VOICE_ATTRIBUTES frozenset
const ALLOWED_VOICE_ATTRIBUTES: &[&str] = &[
    "directness",
    "formality",
    "length_preference",
    "pacing",
    "tone",
];

// ---------------------------------------------------------------------------
// Module-level client singletons (D6-349)
// ---------------------------------------------------------------------------
// HostedClient and OllamaClient hold reqwest::Client instances (Send+Sync).
// Constructed once via OnceLock -- safe across threads and focus runs.
//
// HostedClient is stateless (items.id=709): it carries no key and no
// provider. There is no registry of providers here any more; the dispatch
// below resolves the provider row, model row and the user's stored key into
// a CallContext on every call (decisions.id=875).
//
// Python oracle: module-level provider instances.
// D6-349: client instantiation at call sites in executor.rs, not in FocusRun.

static HOSTED_CLIENT: OnceLock<HostedClient> = OnceLock::new();
static OLLAMA_CLIENT: OnceLock<OllamaClient> = OnceLock::new();

pub(crate) fn hosted_client() -> &'static HostedClient {
    HOSTED_CLIENT.get_or_init(HostedClient::new)
}

fn ollama_client() -> &'static OllamaClient {
    OLLAMA_CLIENT.get_or_init(OllamaClient::new)
}

// ---------------------------------------------------------------------------
// StepContext
// ---------------------------------------------------------------------------

/// Fully-resolved metadata for a single step execution attempt.
///
/// Rust deviation: Python StepContext holds ALL state including PersonalTrack,
/// TaskTrack, SharedStateTrack, FailureHandler, PrivacyGateway, and Scheduler.
/// In Rust, these are passed as separate parameters to StepExecutor::execute()
/// to preserve D6-342 actor ownership without Arc<Mutex<>> wrappers.
/// StepContext carries step metadata only.
///
/// All tier values are pre-computed by lifecycle::execute_step().
/// Executor is a pure consumer — no tier computation happens here.
///
/// floor_consent_preference: read from persona extra_metadata by lifecycle.
///   "modified" -> skip Floor Consent Gate this run; write floor_consent_auto event.
///   None       -> normal Floor Consent Gate evaluation.
///   One-run scope: applies for this context only; not persisted.
///
/// qr_hosted_provider_preference: resolved by lifecycle (items.id=251, repointed
///   items.id=432, ceiling-aware since items.id=692) via
///   conductor::provider_selection::resolve_external_provider(), which narrows
///   the qr_hosted candidate set (providers.provider_type=
///   'cloud_inference_api', items.id=430) to the providers the step's
///   effective_access permits (hard anonymity filter at anonymous_required,
///   anonymous-first ranking at anonymous_preferred) and then applies
///   user_provider_preference_store::resolve_preference()'s
///   Focus -> Persona -> account precedence --
///   not the legacy users.tier2_provider_preference column (dropped by
///   items.id=433 -- a real, historical column name, not this codebase's
///   current vocabulary). Only populated when effective_access != LocalOnly.
///   Some(provider_id) -> dispatch to that provider through the generic hosted
///   client (items.id=709): the provider's row in the providers table drives
///   the call, so there is no closed set of providers in code.
///   None -> no provider chosen (or the resolved preference was ambiguous
///   across candidates, or the user's provider is excluded by the ceiling --
///   see qr_hosted_ceiling_excluded); StepExecutor raises F10
///   MissingQrHostedConfig / ProviderExcludedByCeiling rather than guessing
///   (architecture: "no prescribed default"). Always None when
///   effective_access == LocalOnly.
pub struct StepContext {
    pub step: ActionDefinition,
    pub focus_id: String,
    pub focus_run_id: String,
    pub user_input: String,
    /// items.id=883 (decisions.id=937 point 3): the text an EXTERNAL render inserts for
    /// {user_input}: the user's own turns only, never a model turn. `user_input` (the
    /// full blended window) is read by the local render alone. See FocusRun.external_input.
    pub external_input: String,
    pub persona_context: String, // rendered MemoryBroker output (Phase 3)
    /// items.id=406: identity fields PG_GATE_3 needs to open outputs.db/
    /// personal.db for the fact-identity persistence cascade (prior-decision
    /// query, pf_fact_mentions, pf_standing_preferences). Already present on
    /// FocusRun (lifecycle.rs) — StepContext previously carried "step
    /// metadata only" and left identity to be borrowed separately alongside
    /// it, but gate3 itself now needs these directly, not just the logger
    /// built from them. key_hex is "" when FocusRun.key_hex is None, matching
    /// the empty-string-when-absent convention SqliteDisclosureLogger's own
    /// callers already use.
    pub user_id: String,
    pub persona_id: String,
    pub key_hex: String,
    /// items.id=710: whether a user is present. The spend gate lets an
    /// unattended step spend only within an approved per-run budget.
    pub attendance: RunAttendance,
    /// items.id=715: Some when this run's owner asked its hosted steps to
    /// stream (API Chat). None (every other run) = whole-body calls, exactly
    /// as before. Ignored by a local (Ollama) step.
    pub stream: Option<crate::chat_stream::ChatStream>,
    /// items.id=778: tripped when the user chose 'Stop and save' for a key
    /// rotation. A whole-body model call (local, or hosted without streaming)
    /// is raced against it and ends the step; a streamed call stops through
    /// `stream`, whose token is a child of this one. None = never stopped
    /// (tests, runs without a lease).
    pub stop: Option<tokio_util::sync::CancellationToken>,
    /// items.id=716: Some when this run belongs to an API conversation bound to
    /// one provider and model. The run then uses exactly that destination (the
    /// user's preference is not consulted) through the same consent, limits,
    /// reservation and gate path, and it never falls through to the local
    /// model. None (every other run) = unchanged.
    pub bound: Option<crate::api_chat::BoundDestination>,
    /// items.id=439 (Part 6): the Focus-level external_access ceiling,
    /// excluding this step's own external_access_override (see
    /// FocusRun::_focus_external_access) — used by the Step 3 tighten-only
    /// re-check and gate3's Check 1, both below. Renamed/retyped from the
    /// former space_max_permitted_tier: u8, which had exactly these two use
    /// sites.
    ///
    /// Do NOT reuse this field for abstraction-axis decisions (the
    /// ADR-012 floor clamp): it is deliberately Focus-level-only and does
    /// not see a step's own override, which is exactly the input the floor
    /// clamp needs. See lifecycle.rs::execute_step()'s locally-computed
    /// effective_access and PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Part 7
    /// (items.id=444) for the regression this would otherwise cause.
    pub focus_external_access: ExternalAccess,
    /// items.id=528 Phase 2: this step's own effective external-access
    /// ceiling -- focus_external_access folded with the step's own
    /// external_access_override (ActionDefinition::external_access_override)
    /// via ExternalAccess::min(). Previously computed by lifecycle.rs and
    /// used only locally there (the ADR-012 floor clamp below), then
    /// discarded; promoted onto StepContext so the "does this step require
    /// external access at all" checks throughout this file can test the
    /// typed value directly (`!= ExternalAccess::LocalOnly`) instead of
    /// re-deriving the same answer from execution_tier's numeric bridge
    /// (execution_tier_ordinal() collapses LocalOnly -> 1 and everything
    /// else -> 2/3, so `execution_tier >= 2` and `!= LocalOnly` were always
    /// the same question, just one of them asked through a lossy proxy).
    ///
    /// Deliberately NOT used by the floor invariant below (Step 1): that
    /// check cross-verifies execution_tier against abstraction_tier as two
    /// independently-computed axes (abstraction_tier itself mixes in the
    /// user's own privacy_tier preference, lifecycle.rs's
    /// _focus_privacy_tier.min(execution_tier)) — keying it on this field,
    /// an input to only one side of that comparison, would make the check
    /// test itself instead of catching real divergence between the two axes.
    pub effective_access: ExternalAccess,
    pub execution_tier: u8,
    pub abstraction_tier: u8,
    pub raw_abstraction: u8,
    pub floor_consent_preference: Option<String>, // "modified" | "local" | None
    pub qr_hosted_provider_preference: Option<String>, // providers.id of a hosted provider row, or None
    /// items.id=692: Some(display name) iff the user HAS a hosted provider
    /// set up but this step's ceiling excludes it, so
    /// `qr_hosted_provider_preference` is None for that reason rather than
    /// because nothing is configured. Always None when effective_access ==
    /// LocalOnly. Lets Step 4.5 tell the two apart.
    pub qr_hosted_ceiling_excluded: Option<String>,
    pub next_execution_tier: Option<u8>,
    pub retry_count: u32,
    /// Display name of the Focus, passed to gate3 for the consent modal header.
    pub focus_name: String,
    /// items.id=886: this pass's seeds of the bound names (session and case
    /// store content) and the set of names a step sees only when its own
    /// template names them. Applied in render_prompt* beside the system tokens;
    /// see conductor::taint. Empty for a Focus that uses no bound name.
    pub store_tokens: crate::conductor::taint::StoreTokens,
    /// items.id=901 (the run plan): the approval this run acts under, set by the plan gate
    /// at the start of the run only when a stored approval was read and verified against
    /// the plan the run recomputed. None: no approval, so a resolved step may run only on
    /// a local model the resolver itself picks. Read ONLY by plan_pin() and
    /// check_plan_envelope() below (a source scan pins that).
    pub plan: Option<Arc<crate::conductor::plan_approval::Approval>>,
}

// ---------------------------------------------------------------------------
// StepExecutor
// ---------------------------------------------------------------------------

/// Executes one step through the complete 15-step sequence.
///
/// Stateless — safe to construct per-step. Provider singletons handle HTTP reuse.
/// Python oracle: StepExecutor class in conductor/executor.py.
pub struct StepExecutor;

impl Default for StepExecutor {
    fn default() -> Self {
        Self
    }
}

impl StepExecutor {
    pub fn new() -> Self {
        Self
    }

    // =========================================================================
    // Public entry point
    // =========================================================================

    /// Execute a step with retry loop.
    ///
    /// DisclosureLogWriteError and VoiceProfileContamination propagate as
    /// Err(ConductorError) from execute_once() and are caught here, mapped to
    /// FailureResult via failure_handler.handle().
    /// await_floor_consent exits immediately — never retried.
    /// The retry policy (stop check, hosted backoff, Retry-After) lives in
    /// `retry_loop`.
    ///
    /// Python oracle: StepExecutor.execute()
    #[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
    pub async fn execute<L: DisclosureLogger>(
        &self,
        ctx: StepContext,
        personal_track: &PersonalTrack,
        task_track: &mut TaskTrack,
        shared_state: &mut SharedStateTrack,
        failure_handler: &FailureHandler,
        privacy_gateway: &PrivacyGateway<L>,
        scheduler: &Arc<ConductorScheduler>,
        app_handle: Option<&tauri::AppHandle<tauri::Wry>>,
        pool: &sqlx::SqlitePool,
    ) -> Option<FailureResult> {
        let mut attempt = RealAttempt {
            executor: self,
            ctx: &ctx,
            personal_track,
            task_track,
            shared_state,
            failure_handler,
            privacy_gateway,
            scheduler,
            app_handle,
            pool,
            wait_notice: app_handle.map(|h| {
                Arc::new(retry_wait::TauriRetryWaitNotice::new(h.clone()))
                    as Arc<dyn RetryWaitNotice>
            }),
        };
        retry_loop(&mut attempt, &ctx, failure_handler, random_ms).await
    }

    // =========================================================================
    // Inner execution — full 15-step sequence
    // =========================================================================

    /// One attempt at the full step sequence.
    ///
    /// Returns:
    ///   Ok(None)                  — success
    ///   Ok(Some(FailureResult))   — classified, recoverable failure
    ///   Err(ConductorError)       — DisclosureLogWrite (F_SYSTEM) or
    ///                               VoiceProfileContamination (F4 subtype)
    ///
    /// Python oracle: StepExecutor._execute_once()
    #[allow(clippy::too_many_arguments)]
    async fn execute_once<L: DisclosureLogger>(
        &self,
        ctx: &StepContext,
        retry_count: u32,
        personal_track: &PersonalTrack,
        task_track: &mut TaskTrack,
        shared_state: &mut SharedStateTrack,
        failure_handler: &FailureHandler,
        privacy_gateway: &PrivacyGateway<L>,
        scheduler: &Arc<ConductorScheduler>,
        app_handle: Option<&tauri::AppHandle<tauri::Wry>>,
        pool: &sqlx::SqlitePool,
    ) -> Result<Option<FailureResult>, ConductorError> {
        // -- Floor invariants (ADR-012 Amendment 3, D6-348) --
        // Explicit Err() returns — never debug_assert!, which is stripped in release builds.
        // These are load-bearing privacy checks, not programmer-error guards.
        // items.id=921: checked here on the CEILING values lifecycle computed (the
        // same text as before, now from step_treatment), and again inside
        // step_treatment::derive on the values the step is actually gated with.
        if let Some(plain_language) = floor_invariant_violation(
            ctx.execution_tier,
            ctx.abstraction_tier,
            ctx.raw_abstraction,
        ) {
            return Err(ConductorError::TierBoundaryViolation { plain_language });
        }

        // -- Step 3 — external_access ceiling gate (items.id=439, Part 6e) --
        // Defense-in-depth re-check of authorize()'s own tighten-only
        // validation, same shape, corrected vocabulary.
        if let Some(override_) = ctx.step.external_access_override {
            if override_ > ctx.focus_external_access {
                return Ok(Some(failure_handler.handle(
                    &ConductorError::TierBoundaryViolation {
                        plain_language: format!(
                            "Step '{}' requires external access '{}' but this life only \
                             permits '{}'. [Get help]",
                            ctx.step.action_id,
                            override_.as_str(),
                            ctx.focus_external_access.as_str()
                        ),
                    },
                    Some(&ctx.step.action_id),
                    Some(&ctx.focus_id),
                    retry_count,
                )));
            }
        }

        // -- Step 4 — cloud_frontier boundary handled by lifecycle; executor never reached --

        // -- Step 4.4 — a bound API Chat run uses exactly its bound provider (items.id=716) --
        // Before 4.5 on purpose: missing_provider_error() treats a LocalOnly
        // step as needing no provider, which for a bound run would let it fall
        // through to the local model below. It never does.
        if let Some(bound) = &ctx.bound {
            if let Some(code) = crate::api_chat::bound_run_stop(
                bound,
                ctx.effective_access,
                ctx.qr_hosted_provider_preference.as_deref(),
            ) {
                return Ok(Some(failure_handler.handle(
                    &spend_block_error(
                        &SpendBlock::new(code).with("provider_id", bound.provider_id.clone()),
                    ),
                    Some(&ctx.step.action_id),
                    Some(&ctx.focus_id),
                    retry_count,
                )));
            }
        }

        // items.id=896: a step expanded from a model block carries a requirement
        // and is resolved by conductor::model_resolver instead of Step 4.5 and
        // select_model(). A bound API Chat run NEVER takes this path: it keeps its
        // bound destination whatever the step carries (Step 4.4 above), and a step
        // with no requirement (every ordinary step) keeps Step 4.5 and
        // select_model() exactly as they were.
        let resolver_req = if is_resolved(ctx) {
            ctx.step.requirement.as_ref()
        } else {
            None
        };

        // -- Step 4.5 — Tier 1.5 (qr_hosted) provider preference gate (F10) --
        // No prescribed default (architecture: "User choice at install. No
        // prescribed default."). An unset preference is a real gap, not
        // silently resolved to any default — short-circuits before any provider
        // is touched. Only checked when this step requires external access
        // at all; qr_local never needs a provider. A provider the user HAS
        // set up but this step's ceiling excludes (items.id=692) fails
        // separately, naming the setting to change.
        if resolver_req.is_none() {
            if let Some(error) = missing_provider_error(
                ctx.effective_access,
                ctx.qr_hosted_provider_preference.as_deref(),
                ctx.qr_hosted_ceiling_excluded.as_deref(),
            ) {
                return Ok(Some(failure_handler.handle(
                    &error,
                    Some(&ctx.step.action_id),
                    Some(&ctx.focus_id),
                    retry_count,
                )));
            }
        }

        // The local models the context check may fall back to (items.id=896).
        // Empty for every step but a resolved one.
        let mut local_fallbacks: Vec<LocalFallback> = Vec::new();
        let mut selected_model = match (&ctx.bound, resolver_req) {
            (Some(bound), _) => select_bound_model(pool, ctx, bound).await?,
            // items.id=901: under the run's approval. A pinned choice replaces the resolver;
            // any other pick must be a local model (plan_exceeded otherwise). Either way the
            // refusal is made here, BEFORE derive_treatment and every gate below.
            (None, Some(req)) => match select_under_plan(pool, ctx, req).await {
                Ok(sel) => {
                    local_fallbacks = sel.fallbacks;
                    sel.selected
                }
                // The resolver stopped the step before any model was called: it
                // parks for the user (F10), like a spend block.
                Err(e) => {
                    return Ok(Some(failure_handler.handle(
                        &e,
                        Some(&ctx.step.action_id),
                        Some(&ctx.focus_id),
                        retry_count,
                    )));
                }
            },
            (None, None) => {
                select_model(
                    pool,
                    &ctx.step.task_type,
                    ctx.effective_access,
                    ctx.qr_hosted_provider_preference.as_deref(),
                    Some(SelectionLookup {
                        user_id: &ctx.user_id,
                        key_hex: &ctx.key_hex,
                        persona_id: &ctx.persona_id,
                        focus_id: &ctx.focus_id,
                    }),
                )
                .await?
            }
        };

        // items.id=921: from here to the end of the step, the privacy treatment keys
        // off the DESTINATION the step actually goes to, not the ceiling it may
        // reach. `treatment` is derived once per attempt, from this attempt's own
        // selection (a retry or a resumed run selects and derives again), and is
        // never stored. An ordinary step and a bound API Chat run get exactly the
        // ceiling values lifecycle computed; a resolved step with a local pick gets
        // the local treatment; a resolved step with a hosted pick keeps the
        // external treatment at its reach. A disagreement (a hosted pick under a
        // local-only ceiling or outside the step's range, a half-hosted selection)
        // stops the step before any gate.
        let treatment = match derive_treatment(ctx, resolver_req, &selected_model) {
            Ok(t) => t,
            Err(e) => {
                return Ok(Some(failure_handler.handle(
                    &ConductorError::TierBoundaryViolation {
                        plain_language: e.plain_language(&ctx.step.action_id),
                    },
                    Some(&ctx.step.action_id),
                    Some(&ctx.focus_id),
                    retry_count,
                )));
            }
        };

        // Build gate track for Gate1/Gate2 calls.
        // privacy::types::PersonalTrack (typed enum fields) differs from
        // conductor::types::PersonalTrack (sqlx-friendly String fields).
        // to_gate_track() converts between them. See module comment.
        let gate_track = to_gate_track(personal_track);

        // -- Steps 6-7 — PG_GATE_1: field approval + abstraction --
        let g1 = privacy_gateway
            .gate1(
                &ctx.step.action_id,
                &ctx.focus_run_id,
                &gate_track,
                treatment.abstraction_tier,
                treatment.raw_abstraction,
                treatment.execution_tier,
                treatment.access,
                selected_model.provider_id.clone(),
            )
            .await
            .map_err(|e| ConductorError::DisclosureLogWrite {
                plain_language: e.plain_language.clone(),
            })?;

        // -- Field projection — step-scope boundary --
        // Post-Layer 6 fix: Gate1 evaluates full PersonalTrack; projection scopes
        // approved_fields to the step's declared field_requirements.
        let projected_fields: HashMap<String, String> = if !ctx.step.field_requirements.is_empty() {
            g1.approved_fields
                .iter()
                .filter(|(name, _)| ctx.step.field_requirements.contains_key(*name))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        } else {
            HashMap::new()
        };

        // -- Floor Consent Gate (ADR-012 Amendment 3) --
        if !g1.floor_clamped_fields.is_empty() {
            if ctx.floor_consent_preference.as_deref() == Some(FLOOR_CONSENT_MODIFIED) {
                // Pre-authorized — write audit record (non-fatal if logger fails).
                let _ = privacy_gateway
                    .logger
                    .write(DisclosureLogEntry {
                        action_id: ctx.step.action_id.clone(),
                        focus_run_id: ctx.focus_run_id.clone(),
                        execution_tier: treatment.execution_tier,
                        abstraction_tier: Some(treatment.abstraction_tier),
                        provider: selected_model.provider_id.clone(),
                        fields_shared: projected_fields.keys().cloned().collect(),
                        fields_abstracted: IndexMap::new(),
                        fields_withheld: g1.withheld_fields.clone(),
                        override_declined: false,
                        event_type: "floor_consent_auto".to_owned(),
                        category: None,
                        fact_key: None,
                    })
                    .await;
            } else {
                // Halt and ask user — AwaitFloorConsent exits the retry loop.
                let mut meta = HashMap::new();
                meta.insert(
                    "floor_clamped_fields".to_owned(),
                    serde_json::to_value(&g1.floor_clamped_fields).unwrap_or_else(|e| {
                        log::warn!("executor: floor_clamped_fields serialize failed: {e}");
                        serde_json::Value::Array(vec![])
                    }),
                );
                meta.insert(
                    "approved_fields".to_owned(),
                    serde_json::to_value(&projected_fields).unwrap_or_else(|e| {
                        log::warn!("executor: projected_fields serialize failed: {e}");
                        serde_json::Value::Object(serde_json::Map::new())
                    }),
                );
                meta.insert(
                    "action_id".to_owned(),
                    serde_json::Value::String(ctx.step.action_id.clone()),
                );
                meta.insert(
                    "execution_tier".to_owned(),
                    serde_json::Value::Number(treatment.execution_tier.into()),
                );
                meta.insert(
                    "abstraction_tier".to_owned(),
                    serde_json::Value::Number(treatment.abstraction_tier.into()),
                );
                return Ok(Some(FailureResult {
                    action: FailureAction::AwaitFloorConsent,
                    failure_mode: None,
                    plain_language: "Quiet Rabbit modified some of your fields to maintain \
                         privacy for external use. Please review and choose."
                        .to_owned(),
                    is_recoverable: true,
                    severity: FailureSeverity::Pause,
                    action_id: Some(ctx.step.action_id.clone()),
                    focus_id: Some(ctx.focus_id.clone()),
                    metadata: Some(meta),
                }));
            }
        }

        // -- Write projected fields to disclosure buffer (pre-Step 8) --
        shared_state.write_disclosure_buffer(&ctx.step.action_id, projected_fields.clone());

        // -- Step 8 — assemble final prompt --
        // Collect output_vars and previous_output before render (read-only task_track access).
        // Step 12's add_step() hasn't run yet; borrow is released before the mutable call.
        let output_vars = collect_output_vars(task_track.steps());
        let previous_output = task_track.last_output().unwrap_or("").to_owned();

        // scan_voice_profile is async (writes disclosure audit on contamination).
        // Called once; result passed to both render paths.
        let cleaned_voice_profile =
            scan_voice_profile(ctx, &treatment, personal_track, privacy_gateway).await?;
        let voice_profile_str = format_voice_profile(&cleaned_voice_profile);

        // items.id=921: keyed on the destination (treatment.hosted), which for an
        // ordinary step is the ceiling test it always was.
        let prompt = if treatment.hosted {
            let disclosure = shared_state.read_disclosure_buffer(&ctx.step.action_id);
            render_prompt_with_disclosure(
                ctx,
                &output_vars,
                &previous_output,
                &disclosure,
                &voice_profile_str,
            )
        } else {
            render_prompt(ctx, &output_vars, &previous_output, &voice_profile_str)
        };

        // -- items.id=884 — outbound review of the rendered prompt --
        // Fires iff the selected model is hosted (`treatment.hosted`, the one
        // predicate that also picks the render and the network branch,
        // decisions.id=947). A local pick is not reviewed. A bound API Chat run
        // takes its prompt as reviewed already (send_api_chat_message). After the
        // render and before the context-window check, the inference slot and the
        // spend gate, so what is measured and sent is the released text. A park
        // (the user must answer, the filter is unavailable, anything fail-closed)
        // is returned as the step's result and never retried.
        let reviewed: Option<ReviewedPrompt> = if treatment.hosted {
            match step_review::review_hosted_step(
                ctx,
                &treatment,
                selected_model.provider_id.as_deref(),
                &prompt,
                privacy_gateway,
                app_handle,
            )
            .await
            {
                Ok(p) => Some(p),
                Err(parked) => return Ok(Some(parked)),
            }
        } else {
            None
        };
        let prompt = match &reviewed {
            Some(p) => p.as_str().to_owned(),
            None => prompt,
        };

        // -- Step 5 — context window check (after prompt assembly, matching oracle) --
        let options_raw: HashMap<String, serde_json::Value> = {
            let mut m = HashMap::new();
            m.insert("temperature".to_owned(), serde_json::json!(0.5));
            m.insert("top_p".to_owned(), serde_json::json!(0.90));
            m.insert("num_predict".to_owned(), serde_json::json!(1024));
            for (k, v) in &ctx.step.options_override {
                m.insert(k.clone(), v.clone());
            }
            m
        };

        let explicit_num_ctx = options_raw.get("num_ctx").and_then(|v| v.as_u64());
        let mut effective_ctx = match explicit_num_ctx {
            Some(v) => v as u32,
            // A hosted step's window is the resolved row's own: a model from
            // the user's live list has no curated row to look up again.
            None => match &selected_model.hosted_model {
                Some(model) => model.context_window_tokens,
                None => {
                    get_context_window(
                        pool,
                        selected_model.provider_id.as_deref(),
                        &selected_model.model_id,
                    )
                    .await
                }
            },
        };

        let mut ctx_status = check_context_window(&prompt, &ctx.step.task_type, effective_ctx);

        // items.id=896: a resolved step that does not fit its model's window may
        // move to the next qualifying LOCAL candidate the resolver ranked. The
        // prompt is NOT re-rendered and Gate 1 is NOT re-run: every fallback is a
        // local model, as is the one it replaces, so the destination class, the
        // tiers and the provider recorded by Gate 1 (none) are the same, and the
        // text is the one already assembled. Only the model changes. The disclosure
        // log is not touched (it records a provider and fields, and nothing leaves
        // the device); the change is logged below with the two local model tags.
        // A step that fixes its own num_ctx has one window whatever the model, so
        // it never falls back.
        // items.id=921: `treatment` is local here and stays so. The fallback changes
        // the model only, never the treatment.
        if resolver_req.is_some()
            && explicit_num_ctx.is_none()
            && !treatment.hosted
            && selected_model.hosted_model.is_none()
            && ctx_status.status == ContextWindowStatusKind::Exceeded
        {
            for fb in std::mem::take(&mut local_fallbacks) {
                let status =
                    check_context_window(&prompt, &ctx.step.task_type, fb.context_window_tokens);
                if status.status != ContextWindowStatusKind::Exceeded {
                    log::info!(
                        "executor: step {} does not fit {} ({} tokens): using local fallback {} \
                         ({} tokens), same prompt, Gate 1 not re-run",
                        ctx.step.action_id,
                        selected_model.model_id,
                        effective_ctx,
                        fb.model_id,
                        fb.context_window_tokens
                    );
                    selected_model = SelectedModel::local(fb.model_id);
                    effective_ctx = fb.context_window_tokens;
                    ctx_status = status;
                    break;
                }
            }
        }
        if ctx_status.status == ContextWindowStatusKind::Exceeded {
            return Ok(Some(failure_handler.handle(
                &ConductorError::ContextWindowExceeded {
                    plain_language: ctx_status.plain_language.unwrap_or_else(|| {
                        "This is too long for local processing. \
                         [Use an external service] [Shorten the document]"
                            .to_owned()
                    }),
                },
                Some(&ctx.step.action_id),
                Some(&ctx.focus_id),
                retry_count,
            )));
        }

        let options = build_options(&options_raw, effective_ctx);

        let request = GenerateRequest {
            provider_id: selected_model.provider_id.clone(),
            model_id: selected_model.model_id.clone(),
            prompt,
            images: None,
            task_type: ctx.step.task_type.clone(),
            stream: Some(false), // always false in Release 1 — resolved by StepExecutor
            options: Some(options),
        };

        // -- Step 10 — acquire slot; generate; release slot (always, matching finally) --
        let inference_acquired = scheduler
            .acquire_inference_slot(&ctx.focus_run_id, PathPriority::Interactive)
            .await;

        if !inference_acquired {
            return Ok(Some(
                failure_handler.handle(
                    &ConductorError::OllamaUnavailable {
                        plain_language: "Quiet Rabbit is busy with another task. \
                        [Try again] [Get help]"
                            .to_owned(),
                    },
                    Some(&ctx.step.action_id),
                    Some(&ctx.focus_id),
                    retry_count,
                ),
            ));
        }

        // items.id=715: a hosted step streams only when the run's owner asked
        // for it (API Chat). A local step never does.
        // Does this call go to a hosted provider? For an ordinary step the answer
        // has always been the step's own ceiling. For a RESOLVED step the ceiling
        // only says what is allowed, and the resolver may well have chosen a local
        // model under it, so the answer is the model that was chosen. Both come out
        // of the one derivation (items.id=921), which refuses a hosted choice under
        // a LocalOnly ceiling before any gate, and the same flag keys the render,
        // the stream sink and (items.id=884) the outbound review.
        let hosted_step = treatment.hosted;
        let stream_sink: Option<Box<dyn StreamSink>> = match (&ctx.stream, step_streams(ctx)) {
            (Some(_), true) => Some(match app_handle {
                Some(h) => Box::new(crate::chat_stream::TauriDeltaSink::new(
                    h.clone(),
                    ctx.focus_run_id.clone(),
                    ctx.step.action_id.clone(),
                )),
                None => Box::new(NullSink),
            }),
            _ => None,
        };
        let mut stream_tail = StreamTail::default();

        // items.id=778: a whole-body call has no partial text to keep, so a
        // stop simply ends the step. A streamed call is stopped through its
        // own token (a child of ctx.stop) and keeps what was shown.
        let race_stop = ctx.stop.clone().filter(|_| !step_streams(ctx));
        let generate_future = async {
            if hosted_step {
                // Step 4.5 above already guarantees qr_hosted_provider_preference is
                // Some(...) at this point -- no silent fallback to any default
                // provider for a None preference (matches select_model()'s
                // policy). The provider row, model row and the user's stored
                // key become one CallContext (items.id=709); a provider id with
                // no row is a real ConductorError, not a panic. The spend gate
                // (items.id=710) runs between building that context and the
                // network call.
                // For a resolved step the provider is the one the resolver chose
                // (its row's own provider_id, set together with the model row by
                // SelectedModel::hosted), not the user's preference, which may be
                // unset because the resolver does not consult it.
                let provider_for_call = match resolver_req {
                    Some(_) => selected_model.provider_id.as_deref(),
                    None => ctx.qr_hosted_provider_preference.as_deref(),
                };
                match (
                    provider_for_call,
                    selected_model.hosted_model.as_ref(),
                    reviewed.as_ref(),
                ) {
                    (Some(provider_id), Some(model), Some(reviewed_prompt)) => {
                        let parts = match (&ctx.stream, stream_sink.as_deref()) {
                            (Some(handle), Some(sink)) => Some(StreamParts {
                                sink,
                                cancel: handle.token(),
                                tail: &mut stream_tail,
                            }),
                            _ => None,
                        };
                        hosted_step_call(
                            hosted_client(),
                            pool,
                            ctx,
                            provider_id,
                            model,
                            &request,
                            reviewed_prompt,
                            &chrono::Local,
                            HostedInputs::build_call,
                            parts,
                        )
                        .await
                    }
                    _ => unreachable!(
                        "a hosted step with no provider, no resolved model or no reviewed \
                     prompt -- the Step 4.5 guard, select_model() and the outbound review \
                     (items.id=884) must already have short-circuited"
                    ),
                }
            } else {
                ollama_client().generate(&request).await
            }
        };
        let Some(generate_result) = until_stopped(race_stop.as_ref(), generate_future).await else {
            // Release the inference slot, then end the step. The run's owner
            // (lifecycle) sees the stop and cancels the run; this failure is
            // never shown or retried.
            scheduler.release_inference_slot(&ctx.focus_run_id);
            return Ok(Some(stopped_failure(ctx)));
        };
        let generate_result: Result<_, ConductorError> = generate_result;

        // Python oracle: finally block — always release regardless of generate result.
        scheduler.release_inference_slot(&ctx.focus_run_id);

        let response = match generate_result {
            Ok(r) => r,
            Err(e) => {
                // items.id=715: a streamed reply that ended badly or was
                // cancelled was still SHOWN, so Gate 2 flags what the user
                // saw (decisions.id=883) and the front end is told how it
                // ended. A failure before any text was shown is reported
                // through the ordinary failure path only, because it may be
                // retried.
                if let Some(sink) = stream_sink.as_deref() {
                    if stream_tail.cancelled || !stream_tail.received_text.is_empty() {
                        // items.id=716 (decisions.id=883): what the user saw
                        // is kept, marked cancelled or cut, never retracted.
                        keep_partial_reply(ctx, &stream_tail, app_handle).await;
                        let g2 = reply_gate2(
                            ctx,
                            &treatment,
                            privacy_gateway,
                            &gate_track,
                            &stream_tail.received_text,
                            selected_model.provider_id.clone(),
                            &g1.fields_shared,
                            app_handle,
                        )
                        .await;
                        sink.end(&StreamEnd {
                            outcome: if stream_tail.cancelled {
                                StreamOutcome::Cancelled
                            } else {
                                StreamOutcome::Failed
                            },
                            personal_flagged: g2.flagged,
                            audit_unavailable: g2.audit_unavailable,
                        });
                    }
                }
                // items.id=817: a hosted retry carries the context its wait
                // row needs (the retry loop has neither this error nor the
                // selected model).
                let handled = failure_handler.handle(
                    &e,
                    Some(&ctx.step.action_id),
                    Some(&ctx.focus_id),
                    retry_count,
                );
                let handled = with_wait_context(
                    handled,
                    &e,
                    selected_model.provider_id.as_deref(),
                    &selected_model.model_id,
                );
                // items.id=831: the last retry failed too. The result becomes
                // the final-failure message, which names the provider from its
                // row and carries the ids the chat surface needs.
                return Ok(Some(
                    with_final_failure(
                        handled,
                        &e,
                        retry_count,
                        ctx,
                        selected_model.provider_id.as_deref(),
                        &selected_model.model_id,
                        pool,
                    )
                    .await,
                ));
            }
        };

        // -- Step 11 — PG_GATE_2: inbound contamination scan --
        // items.id=735 (decisions.id=883): for any external reply Gate 2
        // flags and notifies; it never holds, discards, edits or retracts it,
        // and the run continues to steps 12 and 13 whether or not it hit. A
        // streamed reply and a whole-body reply take this one path.
        let g2 = reply_gate2(
            ctx,
            &treatment,
            privacy_gateway,
            &gate_track,
            &response.content,
            selected_model.provider_id.clone(),
            &g1.fields_shared,
            app_handle,
        )
        .await;
        if let Some(sink) = stream_sink.as_deref() {
            sink.end(&StreamEnd {
                outcome: StreamOutcome::Complete,
                personal_flagged: g2.flagged,
                audit_unavailable: g2.audit_unavailable,
            });
        }
        record_completed_reply(ctx, &response, app_handle).await;

        // -- Step 12 — update TaskTrack --
        // D4-040: content = model output ONLY — never prompt-expanded input.
        // step_sensitivity: from projected fields (Tier 2) or template token scan (qr_local),
        // raised to the highest matched Gate 2 field (completed_step).
        let step_sensitivity =
            compute_step_sensitivity(ctx, &treatment, personal_track, &projected_fields);

        let step = completed_step(
            ctx,
            &response.content,
            step_sensitivity,
            treatment.execution_tier,
            &g2,
        );
        let step_sensitivity = step.sensitivity_severity;
        task_track.add_step(step);

        // -- Step 13 — PG_GATE_3: cross-tier content promotion (conditional) --
        // Only when next step has higher execution_tier.
        if let Some(next_tier) = ctx.next_execution_tier {
            // items.id=921: the tier this step really ran at. A local pick under an
            // external ceiling is tier 1, so Gate 3 fires when the next step may
            // go out (next_execution_tier is the ceiling look-ahead and cannot
            // know that step's own pick, so it may fire when that one stays local).
            if next_tier > treatment.execution_tier {
                let content_key = ctx
                    .step
                    .output_var
                    .as_deref()
                    .unwrap_or(&ctx.step.action_id);

                let g3 = privacy_gateway
                    .gate3(
                        &ctx.step.action_id,
                        &ctx.focus_run_id,
                        &ctx.focus_name,
                        content_key,
                        &response.content,
                        step_sensitivity as u8,
                        next_tier,
                        ctx.focus_external_access,
                        treatment.execution_tier,
                        app_handle,
                        // items.id=406: no specific-provider selection concept
                        // at this generic Focus-step-promotion layer -- falls
                        // back to target_tier itself, unchanged behavior.
                        None,
                        // items.id=458: step_sensitivity above is a real
                        // per-field severity from compute_step_sensitivity,
                        // not a placeholder -- keep the full
                        // destination_risk-forcing behavior on zero PF spans.
                        true,
                        &ctx.user_id,
                        &ctx.persona_id,
                        &ctx.key_hex,
                    )
                    .await
                    .map_err(|e| ConductorError::DisclosureLogWrite {
                        plain_language: e.plain_language.clone(),
                    })?;

                if g3.blocked {
                    return Ok(Some(failure_handler.handle(
                        &ConductorError::ContentPromotionBlocked {
                            plain_language: g3.plain_language.unwrap_or_else(|| {
                                "This content can't be shared with a higher-tier service. \
                                 [Use local only]"
                                    .to_owned()
                            }),
                        },
                        Some(&ctx.step.action_id),
                        Some(&ctx.focus_id),
                        retry_count,
                    )));
                }

                if g3.pending_consent {
                    return Ok(Some(FailureResult {
                        action: FailureAction::AwaitConsent,
                        failure_mode: None,
                        plain_language: "Quiet Rabbit identified personal information in this \
                                         content. Please review before it leaves your device."
                            .to_owned(),
                        is_recoverable: true,
                        severity: FailureSeverity::Pause,
                        action_id: Some(ctx.step.action_id.clone()),
                        focus_id: Some(ctx.focus_id.clone()),
                        metadata: None,
                    }));
                }

                if g3.approved {
                    shared_state.promote_content(
                        &ctx.step.action_id,
                        content_key,
                        &response.content,
                    );
                }
            }
        }

        Ok(None) // step completed successfully
    }
}

// ---------------------------------------------------------------------------
// Prompt rendering (free functions — no self required)
// ---------------------------------------------------------------------------

/// Single token resolution engine for all prompt rendering.
/// Merge order invariant: output_vars -> (disclosure) -> SYSTEM_TOKENS.
/// Unresolved tokens are stripped (logged at DEBUG). Matches Python oracle.
///
/// Python oracle: StepExecutor._render_template()
pub(crate) fn render_template(
    template: &str,
    tokens: &HashMap<String, String>,
    action_id: &str,
    focus_id: &str,
) -> String {
    let mut result = template.to_owned();
    for (token, value) in tokens {
        result = result.replace(&format!("{{{token}}}"), value);
    }
    let unresolved = find_tokens(&result);
    if !unresolved.is_empty() {
        log::debug!(
            "executor: unresolved tokens stripped: {:?} step={action_id} focus={focus_id}",
            unresolved
        );
        result = strip_tokens(&result);
    }
    result.trim().to_owned()
}

/// qr_local prompt render. Never reads disclosure buffer.
/// Token merge order: output_vars -> SYSTEM_TOKENS.
/// Python oracle: StepExecutor._render_prompt()
pub(crate) fn render_prompt(
    ctx: &StepContext,
    output_vars: &HashMap<String, String>,
    previous_output: &str,
    voice_profile_str: &str,
) -> String {
    let mut tokens: HashMap<String, String> = output_vars
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // SYSTEM_TOKENS (later wins — override output_vars with same key)
    tokens.insert("user_input".to_owned(), ctx.user_input.clone());
    tokens.insert("previous_output".to_owned(), previous_output.to_owned());
    tokens.insert("focus_context".to_owned(), ctx.focus_id.clone());
    tokens.insert("persona_context".to_owned(), ctx.persona_context.clone());
    tokens.insert("voice_profile".to_owned(), voice_profile_str.to_owned());
    // items.id=886: bound-name seeds, beside the system tokens. A seed reaches
    // only a step whose own template names it.
    ctx.store_tokens
        .apply(&ctx.step.prompt_template, &mut tokens);
    render_template(
        &ctx.step.prompt_template,
        &tokens,
        &ctx.step.action_id,
        &ctx.focus_id,
    )
}

/// Tier 2+ prompt render. NEVER reads PersonalTrack directly — only disclosure buffer.
/// Token merge order: output_vars -> disclosure -> SYSTEM_TOKENS.
/// Python oracle: StepExecutor._render_prompt_with_disclosure()
fn render_prompt_with_disclosure(
    ctx: &StepContext,
    output_vars: &HashMap<String, String>,
    previous_output: &str,
    disclosure: &HashMap<String, String>,
    voice_profile_str: &str,
) -> String {
    let mut tokens: HashMap<String, String> = output_vars
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // Disclosure fields override output_vars (same key -> disclosure wins)
    for (k, v) in disclosure {
        tokens.insert(k.clone(), v.clone());
    }
    // SYSTEM_TOKENS override everything
    // items.id=883 (decisions.id=937 point 3): the user's own turns only. The full
    // window (ctx.user_input, which holds model turns) is never read on this path, and a
    // scan test pins that.
    tokens.insert("user_input".to_owned(), ctx.external_input.clone());
    tokens.insert("previous_output".to_owned(), previous_output.to_owned());
    tokens.insert("focus_context".to_owned(), ctx.focus_id.clone());
    // items.id=882 (decisions.id=937 point 2): {persona_context} renders EMPTY on this
    // path, the one every step that goes to an external destination takes
    // (`treatment.hosted`). The assembled slice is never read here: a standing summary
    // is labelled tier2_permitted at every ceiling and the eligibility check is not
    // complete (items.id=536), so the slice is not safe to send. Personalisation of an
    // external step goes through its declared field_requirements, which Gate 1 sees and
    // which arrive through `disclosure` above. render_prompt (local) still inserts the
    // slice. A test pins that this function never names ctx.persona_context.
    tokens.insert("persona_context".to_owned(), String::new());
    tokens.insert("voice_profile".to_owned(), voice_profile_str.to_owned());
    // items.id=886: as in render_prompt. (Since items.id=884 flipped
    // STORE_NAMES_EXTERNAL_ALLOWED an external step may name a bound name it declares:
    // its seed is put in here, and the text then goes through the outbound review
    // (step_review). The device names are never in an external template: the lint
    // refuses them, DEVICE_NAMES_NEVER_EXTERNAL.)
    ctx.store_tokens
        .apply(&ctx.step.prompt_template, &mut tokens);
    render_template(
        &ctx.step.prompt_template,
        &tokens,
        &ctx.step.action_id,
        &ctx.focus_id,
    )
}

// ---------------------------------------------------------------------------
// Voice profile scan
// ---------------------------------------------------------------------------

/// Scan voice profile values for PII before prompt assembly.
///
/// Three detection signals (Python oracle: _scan_voice_profile):
///   1. personal_field_match — value contains a PersonalTrack field value as a
///      whole word (case-insensitive). Only field values >= VP_MIN_FIELD_LENGTH tested.
///   2. email_pattern — '@' present with non-empty parts and '.' after '@'.
///   3. digit_dense — 7+ consecutive ASCII digits.
///
/// qr_local: contaminated attrs stripped and logged; execution continues. Ok(cleaned).
/// Tier 2+: Err(ConductorError::VoiceProfileContamination) on first contamination.
///   Audit event written before returning Err.
///
/// Returns Ok(cleaned_map) — only ALLOWED_VOICE_ATTRIBUTES present.
///
/// Python oracle: StepExecutor._scan_voice_profile()
async fn scan_voice_profile<L: DisclosureLogger>(
    ctx: &StepContext,
    treatment: &StepTreatment,
    personal_track: &PersonalTrack,
    privacy_gateway: &PrivacyGateway<L>,
) -> Result<HashMap<String, String>, ConductorError> {
    // Collect field values long enough to test (avoids short-word false positives).
    let personal_values: Vec<String> = personal_track
        .fields()
        .values()
        .filter(|pf| pf.field_value.len() >= VP_MIN_FIELD_LENGTH)
        .map(|pf| pf.field_value.clone())
        .collect();

    let mut cleaned: HashMap<String, String> = HashMap::new();

    for attr in ALLOWED_VOICE_ATTRIBUTES {
        let value = match personal_track.voice_profile().get(*attr) {
            Some(v) => v.clone(),
            None => continue,
        };

        let contamination_type: Option<&'static str> = 'detection: {
            for pv in &personal_values {
                if has_word_boundary_match_ci(&value, pv) {
                    break 'detection Some("personal_field_match");
                }
            }
            if is_email_pattern(&value) {
                break 'detection Some("email_pattern");
            }
            if has_digit_dense(&value) {
                break 'detection Some("digit_dense");
            }
            None
        };

        if let Some(ct) = contamination_type {
            log::warn!(
                "executor: voice profile contamination — attr={attr} type={ct} \
                 tier={} focus={} step={}",
                treatment.execution_tier,
                ctx.focus_id,
                ctx.step.action_id,
            );

            // Audit: log the contamination event via the disclosure logger.
            // This replaces Python's ctx.privacy_gateway.record_voice_profile_contamination().
            let _ = privacy_gateway
                .logger
                .write(DisclosureLogEntry {
                    action_id: ctx.step.action_id.clone(),
                    focus_run_id: ctx.focus_run_id.clone(),
                    execution_tier: treatment.execution_tier,
                    abstraction_tier: Some(treatment.abstraction_tier),
                    provider: None,
                    fields_shared: vec![],
                    fields_abstracted: IndexMap::new(),
                    fields_withheld: vec![attr.to_string()],
                    override_declined: false,
                    event_type: "voice_profile_contamination".to_owned(),
                    category: None,
                    fact_key: None,
                })
                .await;

            // items.id=921: keyed on the step's destination. A local pick under an
            // external ceiling strips the attribute and continues, as any local
            // step does; a hosted call still halts.
            if treatment.access != ExternalAccess::LocalOnly {
                // External access live: halt — contaminated data must not leave the device.
                return Err(ConductorError::VoiceProfileContamination {
                    plain_language: "One of your communication style settings appears to contain \
                         personal information. Quiet Rabbit stopped this request \
                         before sending it outside your device. Review your voice \
                         profile settings and remove personal details before trying again."
                        .to_owned(),
                });
            }
            // qr_local: strip attr, continue scanning remaining attrs.
        } else {
            cleaned.insert(attr.to_string(), value);
        }
    }

    Ok(cleaned)
}

// ---------------------------------------------------------------------------
// Model selection and options
// ---------------------------------------------------------------------------

/// Resolved model selection, carrying `provider_store::ProviderModel`'s own
/// `provider_id`/`model_id` columns directly (decisions.id=813 — an id is an
/// opaque key, never parsed to recover data the table already holds).
/// `provider_id` is `None` exactly at qr_local (Ollama has no `provider_models`
/// row to resolve against yet); `Some(id)` at Tier 1.5 (qr_hosted), from the row's own
/// `provider_id` column. Not `specta::Type` — internal to the executor, never
/// crosses IPC.
#[derive(Debug)]
struct SelectedModel {
    provider_id: Option<String>,
    model_id: String,
    /// The catalog row select_model() resolved at Tier 1.5 (qr_hosted), so the
    /// hosted call needs no second lookup (items.id=709). `None` at qr_local.
    hosted_model: Option<ProviderModel>,
}

impl SelectedModel {
    /// A local model. `provider_id` and `hosted_model` are both None.
    fn local(model_id: String) -> Self {
        Self {
            provider_id: None,
            model_id,
            hosted_model: None,
        }
    }

    /// A hosted model. `provider_id`, `model_id` and `hosted_model` all come from
    /// the one catalog row, so a hosted selection can never carry a model row
    /// without its provider (the generation branch's `unreachable!` depends on
    /// that, items.id=896).
    fn hosted(model: ProviderModel) -> Self {
        Self {
            provider_id: Some(model.provider_id.clone()),
            model_id: model.model_id.clone(),
            hosted_model: Some(model),
        }
    }
}

/// Step 4.5's provider gate (F10), pulled out so the two failure shapes are
/// testable without a full execute_once harness. None when the step needs no
/// hosted provider or has one selected. Otherwise: the hosted provider is
/// excluded by the ceiling (items.id=692) -> ProviderExcludedByCeiling, or
/// nothing is set up -> MissingQrHostedConfig (text unchanged).
fn missing_provider_error(
    effective_access: ExternalAccess,
    selected_provider: Option<&str>,
    ceiling_excluded_provider: Option<&str>,
) -> Option<ConductorError> {
    if effective_access == ExternalAccess::LocalOnly || selected_provider.is_some() {
        return None;
    }
    Some(match ceiling_excluded_provider {
        Some(provider_name) => ConductorError::ProviderExcludedByCeiling {
            plain_language: format!(
                "This Focus is set to use only providers that don't require an account, \
                 so it can't use your hosted provider ({provider_name}). To use it, change \
                 this Focus's 'Maximum permitted tier' setting to allow providers that \
                 require an account. [Open Settings] [Get help]"
            ),
        },
        None => ConductorError::MissingQrHostedConfig {
            plain_language: "No Tier 1.5 AI provider is set up yet. \
                        Choose a provider in Settings to continue. \
                        [Open Settings] [Get help]"
                .to_owned(),
        },
    })
}

/// Select a model based on task_type, the step's effective external-access
/// ceiling, and (when that ceiling requires external access) the user's
/// Tier 1.5 (qr_hosted) provider preference.
/// Python oracle: StepExecutor._select_model()
///
/// items.id=465 (closes B3): the Tier 1.5 (qr_hosted) branch now queries
/// provider_store::get_default_model() (provider_models WHERE
/// provider_id = ? AND is_default = 1) instead of matching on the literal
/// provider string -- adding a third qr_hosted provider is now a catalog row,
/// not a new match arm here.
///
/// qr_hosted_provider must be Some(...) whenever effective_access != LocalOnly
/// — the caller (execute_once's Step 4.5 guard) short-circuits to the F10
/// MissingQrHostedConfig failure before ever reaching this function with
/// effective_access != LocalOnly and None; that invariant is untouched by
/// this fix, so the None arm below still panics rather than silently
/// misrouting. A provider id that IS present but has no default model
/// curated in the catalog (unknown provider, or a known one not yet given a
/// provider_models row) is a real ConductorError, not a panic.
async fn select_model(
    pool: &sqlx::SqlitePool,
    task_type: &str,
    effective_access: ExternalAccess,
    qr_hosted_provider: Option<&str>,
    selection: Option<SelectionLookup<'_>>,
) -> Result<SelectedModel, ConductorError> {
    if effective_access == ExternalAccess::LocalOnly {
        let model_id = match task_type {
            "code" => "qwen2.5:7b",
            "quick_response" | "summarization" => "llama3.2:3b",
            _ => "llama3.1:8b",
        }
        .to_owned();
        return Ok(SelectedModel {
            provider_id: None,
            model_id,
            hosted_model: None,
        });
    }

    let provider_id = match qr_hosted_provider {
        Some(id) => id,
        None => unreachable!(
            "select_model called with effective_access != LocalOnly and no \
             qr_hosted_provider -- caller must guard on \
             ctx.qr_hosted_provider_preference.is_none() before calling \
             select_model (see the Step 4.5 MissingQrHostedConfig check in \
             execute_once)"
        ),
    };

    // items.id=711: the user's stored selection (Focus, then Persona, then
    // account) wins over the provider's default model. A selection that
    // cannot be honored is an error, never a quiet fall back to the default.
    if let Some(lookup) = selection {
        fn non_empty(s: &str) -> Option<&str> {
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        return match resolve_hosted_model(
            pool,
            lookup.user_id,
            lookup.key_hex,
            non_empty(lookup.persona_id),
            non_empty(lookup.focus_id),
            provider_id,
        )
        .await
        {
            Ok(resolved) => {
                if resolved.may_be_retired || resolved.deprecated {
                    log::info!(
                        "selected model for provider '{provider_id}' is {}",
                        if resolved.deprecated {
                            "deprecated"
                        } else {
                            "missing from the latest model list"
                        }
                    );
                }
                Ok(SelectedModel {
                    provider_id: Some(resolved.model.provider_id.clone()),
                    model_id: resolved.model.model_id.clone(),
                    hosted_model: Some(resolved.model),
                })
            }
            Err(ModelError::NoModel) => Err(ConductorError::UnknownProvider {
                plain_language: format!(
                    "Tier 1.5 provider '{provider_id}' has no default model configured \
                     in the provider catalog."
                ),
            }),
            Err(e) => Err(model_error(&e)),
        };
    }

    match crate::persistence::provider_store::get_default_model(pool, provider_id).await {
        Ok(Some(model)) => Ok(SelectedModel {
            provider_id: Some(model.provider_id.clone()),
            model_id: model.model_id.clone(),
            hosted_model: Some(model),
        }),
        Ok(None) => Err(ConductorError::UnknownProvider {
            plain_language: format!(
                "Tier 1.5 provider '{provider_id}' has no default model configured \
                 in the provider catalog."
            ),
        }),
        Err(e) => Err(ConductorError::UnknownProvider {
            plain_language: format!(
                "could not look up the default model for Tier 1.5 provider \
                 '{provider_id}': {e}"
            ),
        }),
    }
}

/// The model of a bound API Chat run (items.id=716): the model the conversation
/// was bound to, resolved by id. A retired or missing model stops the step with
/// its code; the provider's default is never substituted.
async fn select_bound_model(
    pool: &sqlx::SqlitePool,
    ctx: &StepContext,
    bound: &crate::api_chat::BoundDestination,
) -> Result<SelectedModel, ConductorError> {
    match crate::conductor::model_selection::resolve_bound_model(
        pool,
        &ctx.user_id,
        &ctx.key_hex,
        &bound.provider_id,
        &bound.model_id,
    )
    .await
    {
        Ok(resolved) => {
            if resolved.may_be_retired || resolved.deprecated {
                log::info!(
                    "bound model for provider '{}' is {}",
                    bound.provider_id,
                    if resolved.deprecated {
                        "deprecated"
                    } else {
                        "missing from the latest model list"
                    }
                );
            }
            Ok(SelectedModel {
                provider_id: Some(resolved.model.provider_id.clone()),
                model_id: resolved.model.model_id.clone(),
                hosted_model: Some(resolved.model),
            })
        }
        Err(e) => Err(model_error(&e)),
    }
}

/// A local model the context check may switch to (items.id=896).
#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalFallback {
    model_id: String,
    context_window_tokens: u32,
}

/// What the resolver chose for a model-block step.
struct BlockSelection {
    selected: SelectedModel,
    /// The other qualifying local models in rank order (empty when the choice is
    /// hosted).
    fallbacks: Vec<LocalFallback>,
}

/// A lower bound on the tokens the step's prompt will take: the template's fixed
/// text with every {token} stripped, which is what the render keeps whatever the
/// tokens hold. The post-render context check stays the authority.
fn fixed_prompt_tokens(ctx: &StepContext) -> u32 {
    fixed_tokens_of_step(&ctx.step, &ctx.focus_id)
}

/// fixed_prompt_tokens() for a step and a Focus id, so the run plan (items.id=901) and the
/// executor read the same lower bound.
pub(crate) fn fixed_tokens_of_step(step: &ActionDefinition, focus_id: &str) -> u32 {
    let fixed = render_template(
        &step.prompt_template,
        &HashMap::new(),
        &step.action_id,
        focus_id,
    );
    crate::providers::ollama_client::estimate_token_count(&fixed, &step.task_type)
}

/// The run's shipped document profile: the selected document type, read ONLY from
/// the pass's seeds under the bound name, mapped to its shipped profile. Absent,
/// unknown or unmappable means no profile (items.id=896 ruling: the block's own
/// configuration is never consulted).
fn run_profile(ctx: &StepContext) -> Option<String> {
    let selected = ctx
        .store_tokens
        .seeds
        .get(crate::conductor::model_resolver_input::DOCUMENT_TYPE_NAME)?;
    let shipped = crate::conductor::model_block::document_type::registry().ok()?;
    crate::conductor::model_resolver::profile_for_document_type(selected, shipped, &[])
}

/// The two stops the resolver can return, as the error the failure handler parks.
fn resolver_stop(outcome: &crate::conductor::model_resolver::Resolution) -> Option<ConductorError> {
    use crate::conductor::model_resolver::{NoCandidateReason, OptionEntry, Resolution, Standing};
    fn entry(o: &OptionEntry) -> serde_json::Value {
        let (standing, attained) = match o.standing {
            Standing::BelowBar(a) => ("below_bar", Some(a.as_str())),
            Standing::NotMeasured => ("not_measured", None),
        };
        let mut m = serde_json::Map::new();
        m.insert(
            "subject".into(),
            serde_json::json!(o.model.subject.as_str()),
        );
        if !o.model.provider_id.is_empty() {
            m.insert("provider_id".into(), serde_json::json!(o.model.provider_id));
        }
        m.insert("model_id".into(), serde_json::json!(o.model.model_id));
        m.insert("standing".into(), serde_json::json!(standing));
        if let Some(a) = attained {
            m.insert("attained".into(), serde_json::json!(a));
        }
        serde_json::Value::Object(m)
    }
    match outcome {
        Resolution::Selected { .. } => None,
        Resolution::TooWeak {
            required,
            best_below_bar,
            options,
        } => {
            // Bounded: the parameters ride in the failure's metadata.
            let shown: Vec<serde_json::Value> = options.iter().take(16).map(entry).collect();
            Some(spend_block_error(
                &SpendBlock::new(Code::ResolverTooWeak)
                    .with(
                        "required",
                        serde_json::to_value(required).unwrap_or(serde_json::Value::Null),
                    )
                    .with(
                        "best_below_bar",
                        best_below_bar
                            .as_ref()
                            .map(entry)
                            .unwrap_or(serde_json::Value::Null),
                    )
                    .with("options", serde_json::Value::Array(shown))
                    // The count before the cap above, so a stored park can say "and N more"
                    // (conductor::run_park).
                    .with("options_total", options.len()),
            ))
        }
        Resolution::NoCandidate(reason) => Some(spend_block_error(
            &SpendBlock::new(Code::ResolverNoCandidate).with(
                "reason",
                match reason {
                    NoCandidateReason::NoDestination => "no_destination",
                    NoCandidateReason::NoneReady => "none_ready",
                    NoCandidateReason::ContextTooSmall => "context_too_small",
                },
            ),
        )),
    }
}

/// Resolve the model of a model-block step (items.id=896). Gathers the candidates
/// and the measured capability rows, runs the pure resolver, and turns its answer
/// into a SelectedModel. Never consults the user's provider preference: the
/// resolver chooses inside the ceiling and the requirement's own range.
///
/// A hosted choice under a LocalOnly ceiling is refused before any gate: the
/// resolver already filters by ceiling, and since items.id=921 the refusal is made
/// by step_treatment::derive in execute_once (HostedUnderLocalOnly, same wording),
/// which keeps the network predicate safe even if that filter were ever wrong.
async fn select_model_for_block(
    pool: &sqlx::SqlitePool,
    ctx: &StepContext,
    req: &crate::conductor::model_block::config::ModelRequirement,
) -> Result<BlockSelection, ConductorError> {
    use crate::conductor::model_capability::CapabilitySubject;
    use crate::conductor::model_resolver::{resolve, Resolution, ResolveInput};
    use crate::conductor::model_resolver_input::{gather, GatherInput};

    let crate::conductor::model_resolver_input::Reach {
        ceiling,
        local_allowed,
        hosted_allowed,
    } = crate::conductor::model_resolver_input::reach(req, ctx.effective_access);
    let fixed_tokens = fixed_prompt_tokens(ctx);
    let est_output = ctx
        .step
        .options_override
        .get("num_predict")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(1024);
    let gathered = gather(&GatherInput {
        pool,
        user_id: &ctx.user_id,
        key_hex: &ctx.key_hex,
        ceiling,
        local_allowed,
        hosted_allowed,
        est_input_tokens: fixed_tokens,
        est_output_tokens: Some(est_output),
    })
    .await;
    let profile = run_profile(ctx);
    let outcome = resolve(&ResolveInput {
        requirement: req,
        profile: profile.as_deref(),
        ceiling: ctx.effective_access,
        min_context_tokens: fixed_tokens,
        candidates: &gathered.candidates,
        capabilities: &gathered.capabilities,
    });
    let (chosen, fallbacks) = match outcome {
        Resolution::Selected { chosen, fallbacks } => (chosen, fallbacks),
        // TooWeak and NoCandidate each have their stop. resolver_stop answers None
        // only for a selection, so the fallback is never taken, and it fails closed.
        other => {
            return Err(resolver_stop(&other).unwrap_or_else(|| {
                spend_block_error(&SpendBlock::new(Code::BlockModelUnavailable))
            }))
        }
    };
    match chosen.model.subject {
        CapabilitySubject::Local => Ok(BlockSelection {
            selected: SelectedModel::local(chosen.model.model_id),
            fallbacks: fallbacks
                .into_iter()
                .map(|f| LocalFallback {
                    model_id: f.model.model_id,
                    context_window_tokens: f.context_window_tokens,
                })
                .collect(),
        }),
        CapabilitySubject::Hosted => {
            let key = (chosen.model.provider_id, chosen.model.model_id);
            match gathered.hosted_models.get(&key) {
                Some(row) => Ok(BlockSelection {
                    selected: SelectedModel::hosted(row.clone()),
                    fallbacks: Vec::new(),
                }),
                None => Err(spend_block_error(&SpendBlock::new(
                    Code::BlockModelUnavailable,
                ))),
            }
        }
    }
}

/// The approved choice (a pin) of this step, if the run's approval has one. The first of
/// the three sites that read ctx.plan (the others are check_plan_envelope and
/// plan_cap_micros); a scan pins that.
fn plan_pin(ctx: &StepContext) -> Option<&crate::conductor::run_plan::Pin> {
    ctx.plan
        .as_deref()?
        .pins
        .iter()
        .find(|p| p.action_id == ctx.step.action_id)
}

/// The run plan's cost ceiling for THIS call (items.id=901), for the spend gate: the ceiling
/// of the approval the run acts under, in micro-USD, for a call of a RESOLVED (model-block)
/// step; None for every other call (an ordinary external step, a bound API Chat run), which
/// the gate then decides exactly as before. A resolved step that somehow reaches a hosted
/// call with no approval gets a ceiling of ZERO, so the gate refuses anything that can cost
/// money (a pick like that is already stopped by check_plan_envelope; this is the second
/// line, the stricter direction). The third site that reads ctx.plan.
fn plan_cap_micros(ctx: &StepContext) -> Option<i64> {
    if !is_resolved(ctx) {
        return None;
    }
    Some(
        ctx.plan
            .as_deref()
            .map_or(0, |a| i64::try_from(a.ceiling_micros).unwrap_or(i64::MAX)),
    )
}

/// A plan stop as the error the failure handler parks (F10, like a resolver stop): the
/// code plan.exceeded with the action id and a reason (no_approval, model_not_planned,
/// pin_unavailable). Nothing was sent and no other model was used in its place.
fn plan_exceeded(ctx: &StepContext, reason: &str) -> ConductorError {
    spend_block_error(
        &SpendBlock::new(Code::PlanExceeded)
            .with("action_id", ctx.step.action_id.clone())
            .with("reason", reason),
    )
}

/// The envelope check for a pick the resolver made (no pin for this step): only a LOCAL
/// model may be picked without an approved choice. A hosted pick is beyond the envelope:
/// no_approval when the run has no approval at all (a rehydrated run, a run that needed none)
/// and model_not_planned when it has one that does not name this model for this step (a
/// zero-pin approval permits resolver local picks only). The second of the three
/// sites that read ctx.plan.
fn check_plan_envelope(ctx: &StepContext, selected: &SelectedModel) -> Result<(), ConductorError> {
    if selected.provider_id.is_none() {
        return Ok(());
    }
    Err(plan_exceeded(
        ctx,
        if ctx.plan.is_none() {
            "no_approval"
        } else {
            "model_not_planned"
        },
    ))
}

/// select_model_for_block() under the run's approval (items.id=901): the approved pin if
/// the step has one, else the resolver's choice checked against the envelope.
async fn select_under_plan(
    pool: &sqlx::SqlitePool,
    ctx: &StepContext,
    req: &crate::conductor::model_block::config::ModelRequirement,
) -> Result<BlockSelection, ConductorError> {
    match plan_pin(ctx) {
        Some(pin) => select_pinned_model(pool, ctx, req, pin).await,
        None => {
            let selection = select_model_for_block(pool, ctx, req).await?;
            check_plan_envelope(ctx, &selection.selected)?;
            Ok(selection)
        }
    }
}

/// The model of an approved pin. The resolver is NOT asked to choose: the user chose, and
/// the only question left is whether the pinned model may still be used. It must be a
/// candidate the gatherer finds now, of the pinned kind (a paid model hosted, an unmeasured
/// local model local), and eligible exactly as the resolver judges it (destination under the
/// step's range and ceiling, ready, window large enough). Its standing is not re-decided
/// except in one direction: a model that has since been measured BELOW the required bar is
/// not used (nothing below the bar is ever selected). Anything else is pin_unavailable: no
/// silent switch to another model or strategy.
async fn select_pinned_model(
    pool: &sqlx::SqlitePool,
    ctx: &StepContext,
    req: &crate::conductor::model_block::config::ModelRequirement,
    pin: &crate::conductor::run_plan::Pin,
) -> Result<BlockSelection, ConductorError> {
    use crate::conductor::model_capability::CapabilitySubject;
    use crate::conductor::model_resolver::{resolve, Resolution, ResolveInput, Standing};
    use crate::conductor::model_resolver_input::{gather, GatherInput, Reach};
    use crate::conductor::run_plan::OfferStrategy;

    let unavailable = || plan_exceeded(ctx, "pin_unavailable");
    let subject = match pin.strategy {
        OfferStrategy::PaidApi => CapabilitySubject::Hosted,
        OfferStrategy::LocalUnmeasured => CapabilitySubject::Local,
        // A model the resolver picks anyway is never pinned.
        OfferStrategy::Local => return Err(unavailable()),
    };
    let Reach {
        ceiling,
        local_allowed,
        hosted_allowed,
    } = crate::conductor::model_resolver_input::reach(req, ctx.effective_access);
    let fixed_tokens = fixed_prompt_tokens(ctx);
    let gathered = gather(&GatherInput {
        pool,
        user_id: &ctx.user_id,
        key_hex: &ctx.key_hex,
        ceiling,
        local_allowed,
        hosted_allowed,
        est_input_tokens: fixed_tokens,
        est_output_tokens: None,
    })
    .await;
    let candidate = gathered
        .candidates
        .iter()
        .find(|c| {
            c.subject == subject && c.provider_id == pin.provider_id && c.model_id == pin.model_id
        })
        .ok_or_else(unavailable)?;
    let profile = run_profile(ctx);
    let judged = resolve(&ResolveInput {
        requirement: req,
        profile: profile.as_deref(),
        ceiling: ctx.effective_access,
        min_context_tokens: fixed_tokens,
        candidates: std::slice::from_ref(candidate),
        capabilities: &gathered.capabilities,
    });
    match judged {
        Resolution::NoCandidate(_) => return Err(unavailable()),
        Resolution::TooWeak { options, .. }
            if options
                .first()
                .is_some_and(|o| matches!(o.standing, Standing::BelowBar(_))) =>
        {
            return Err(unavailable())
        }
        _ => {}
    }
    match subject {
        CapabilitySubject::Local => Ok(BlockSelection {
            selected: SelectedModel::local(pin.model_id.clone()),
            fallbacks: Vec::new(),
        }),
        CapabilitySubject::Hosted => {
            let key = (pin.provider_id.clone(), pin.model_id.clone());
            match gathered.hosted_models.get(&key) {
                Some(row) => Ok(BlockSelection {
                    selected: SelectedModel::hosted(row.clone()),
                    fallbacks: Vec::new(),
                }),
                None => Err(unavailable()),
            }
        }
    }
}

/// Who is asking, for the stored model selection (items.id=711).
struct SelectionLookup<'a> {
    user_id: &'a str,
    key_hex: &'a str,
    persona_id: &'a str,
    focus_id: &'a str,
}

/// A model-selection stop as the error the failure handler understands: the
/// same F10 parking as a spend block, with its own code.
fn model_error(e: &ModelError) -> ConductorError {
    let (code, params) = e.block();
    let mut block = SpendBlock::new(code);
    if let Some(o) = params.as_object() {
        for (k, v) in o {
            block = block.with(k, v.clone());
        }
    }
    spend_block_error(&block)
}

/// Map the executor's request onto the hosted client's request (items.id=709).
///
/// The output token budget is forwarded ONLY when the step set `num_predict`
/// explicitly: the executor's own default (1024) is a local-model default and
/// is not forwarded, so a hosted model's limit comes from its catalog row
/// (`request_options`) and the client fails closed when there is none.
fn hosted_request_from(
    options_override: &HashMap<String, serde_json::Value>,
    request: &GenerateRequest,
    reviewed: &ReviewedPrompt,
) -> HostedRequest {
    HostedRequest {
        // items.id=884: never any text of its own. Any system text added here later
        // goes through step_review as well.
        system: None,
        // The text the outbound review released, not the request's own field.
        prompt: reviewed.as_str().to_owned(),
        temperature: request.options.as_ref().map(|o| o.temperature),
        max_output_tokens: options_override
            .get("num_predict")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
    }
}

/// The Retry-After a Retry result carries, in seconds.
fn retry_after_of(result: &FailureResult) -> Option<u64> {
    result
        .metadata
        .as_ref()
        .and_then(|m| m.get(RETRY_AFTER_METADATA_KEY))
        .and_then(|v| v.as_u64())
}

/// Sleep for `d` unless `stop` is tripped first (items.id=727). `true` = the
/// whole wait passed; `false` = the run was stopped, before or during it. The
/// select is biased so a token that is already tripped wins even for a zero
/// delay. With no token it simply sleeps.
async fn sleep_or_stop(
    d: std::time::Duration,
    stop: Option<&tokio_util::sync::CancellationToken>,
) -> bool {
    match stop {
        Some(stop) => {
            tokio::select! {
                biased;
                () = stop.cancelled() => false,
                () = tokio::time::sleep(d) => true,
            }
        }
        None => {
            if !d.is_zero() {
                tokio::time::sleep(d).await;
            }
            true
        }
    }
}

/// The ceiling of the jittered backoff before retry number `retry_count`
/// (0-based): min(BACKOFF_CAP_SECS, BACKOFF_BASE_MS x 2^retry_count).
fn backoff_ceiling(retry_count: u32) -> std::time::Duration {
    let cap_ms = BACKOFF_CAP_SECS * 1_000;
    let factor = 1u64.checked_shl(retry_count).unwrap_or(u64::MAX);
    std::time::Duration::from_millis(BACKOFF_BASE_MS.saturating_mul(factor).min(cap_ms))
}

/// Full jitter: a wait in [0, ceiling]. `sample(max_ms)` returns a value in
/// [0, max_ms] and is the only source of randomness, so tests can pin it; the
/// result is clamped to the ceiling, so no sampler can exceed the cap.
fn jittered(ceiling: std::time::Duration, sample: impl Fn(u64) -> u64) -> std::time::Duration {
    let max_ms = u64::try_from(ceiling.as_millis()).unwrap_or(u64::MAX);
    std::time::Duration::from_millis(sample(max_ms).min(max_ms))
}

/// The production jitter sampler: uniform in [0, max_ms] from the OS random
/// source (the modulo bias is negligible at the <= 30 000 this is called
/// with). If the source fails it returns `max_ms`, the deterministic ceiling,
/// so a broken RNG delays a retry at most the cap and never skips or fails it.
fn random_ms(max_ms: u64) -> u64 {
    let mut bytes = [0u8; 8];
    if let Err(e) = getrandom::fill(&mut bytes) {
        log::warn!("executor: retry jitter source failed, using the ceiling: {e}");
        return max_ms;
    }
    u64::from_le_bytes(bytes) % max_ms.saturating_add(1)
}

/// Whether a result is a hosted-provider call that failed and may be retried
/// (the F10 class). Decided by the failure class, not by the step's external
/// access ceiling: a step allowed to go external can still have run, and
/// failed, on local Ollama.
fn is_hosted_retry(result: &FailureResult) -> bool {
    result.action == FailureAction::Retry
        && result.failure_mode.as_deref() == Some(HOSTED_FAILURE_MODE)
}

/// Add the wait row's context (reason, provider id, model id) to a hosted
/// Retry result (items.id=817). Anything that is not a hosted retry of a
/// provider-error class, or has no provider chosen, is returned unchanged.
fn with_wait_context(
    mut result: FailureResult,
    error: &ConductorError,
    provider_id: Option<&str>,
    model_id: &str,
) -> FailureResult {
    if let (true, Some(reason), Some(provider_id)) = (
        is_hosted_retry(&result),
        RetryWaitReason::of_error(error),
        provider_id,
    ) {
        retry_wait::attach_context(&mut result, reason, provider_id, model_id);
    }
    result
}

/// items.id=831 (decisions.id=925): the exhausted result of a hosted call
/// becomes the final-failure message. Anything that is not the exhaustion of a
/// provider-error class, or has no provider chosen, is returned unchanged.
/// The provider is named from its row (data, never code); if the row cannot be
/// read the message names nothing rather than guess.
async fn with_final_failure(
    mut result: FailureResult,
    error: &ConductorError,
    retry_count: u32,
    ctx: &StepContext,
    provider_id: Option<&str>,
    model_id: &str,
    pool: &sqlx::SqlitePool,
) -> FailureResult {
    let (Some(reason), Some(provider_id)) = (RetryWaitReason::of_error(error), provider_id) else {
        return result;
    };
    if !final_failure::is_hosted_exhaustion(&result, retry_count) {
        return result;
    }
    let display_name = match provider_store::get_provider(pool, provider_id).await {
        Ok(Some(p)) => Some(p.display_name),
        _ => None,
    };
    let who = match display_name.as_deref() {
        Some(provider) => final_failure::Who::Named {
            provider,
            model: model_id,
        },
        None => final_failure::Who::Unknown,
    };
    // API Chat shows the sentence alone; QR Chat and a Focus run put the step
    // and Focus first, as their wait row does.
    let scope = if ctx.bound.is_some() {
        final_failure::Scope::Bare
    } else {
        final_failure::Scope::Focus {
            focus: &ctx.focus_name,
            step: &ctx.step.display_name,
        }
    };
    final_failure::apply(&mut result, reason, provider_id, model_id, who, scope);
    result
}

/// The wait before retry number `retry_count` (0-based) of a failed step
/// (items.id=727). Returns `Some(result)` - do not retry, return this - when
/// the provider's Retry-After parks the run or the run was stopped during the
/// wait; `None` = the wait is over, retry.
///
/// Only a hosted (F10) retry waits: the larger of a provider's Retry-After
/// (the floor, itself bounded by RETRY_AFTER_CAP_SECS; one over the cap parks
/// the run and nothing is slept) and a jittered exponential backoff (bounded
/// by BACKOFF_CAP_SECS). A local (F1/F2) retry waits for nothing, exactly as
/// before. The caller holds no inference slot: execute_once released it before
/// returning the failure.
///
/// items.id=817: the TOTAL is computed first, announced through `notify`
/// BEFORE the sleep, and the end of the wait is announced when the guard
/// drops, whichever way the wait ends (it ran out, the run was stopped or
/// cancelled, or this future was dropped). The sleep is `sleep_or_stop`, so a
/// stop still wins at once, even for a zero wait.
///
/// Spend: the retry that follows re-runs the whole step, including the spend
/// pre-flight, and counts as one more request (items.id=710). Nothing here
/// changes that.
async fn wait_before_retry(
    result: &FailureResult,
    retry_count: u32,
    stop: Option<&tokio_util::sync::CancellationToken>,
    sample: impl Fn(u64) -> u64,
    notify: Option<&RetryWaitIdentity<'_>>,
) -> Option<FailureResult> {
    if !is_hosted_retry(result) {
        return None;
    }
    let retry_after = retry_after_of(result);
    if let Some(secs) = retry_after.filter(|s| *s > RETRY_AFTER_CAP_SECS) {
        return Some(long_wait_result(result.clone(), secs));
    }
    let floor = std::time::Duration::from_secs(retry_after.unwrap_or(0));
    let backoff = jittered(backoff_ceiling(retry_count), sample);
    let total = floor.max(backoff);
    let _announced = notify.and_then(|n| n.begin(result, total, retry_count));
    if !sleep_or_stop(total, stop).await {
        return Some(result.clone());
    }
    None
}

/// One attempt at a step, as the retry loop sees it. The real implementation
/// runs `StepExecutor::execute_once`; a test supplies a scripted one so the
/// loop's rules can be exercised without the 15-step sequence (items.id=727).
/// The returned future must be Send: `StepExecutor::execute` is spawned.
trait StepAttempt {
    fn once(
        &mut self,
        retry_count: u32,
    ) -> impl std::future::Future<Output = Result<Option<FailureResult>, ConductorError>> + Send;

    /// Where a hosted retry wait is announced (items.id=817). None = nowhere
    /// (the default): the wait is silent, as it was before the row existed.
    fn retry_wait_notice(&self) -> Option<Arc<dyn RetryWaitNotice>> {
        None
    }
}

/// The real attempt: the borrowed arguments of `StepExecutor::execute`.
struct RealAttempt<'a, L: DisclosureLogger> {
    executor: &'a StepExecutor,
    ctx: &'a StepContext,
    personal_track: &'a PersonalTrack,
    task_track: &'a mut TaskTrack,
    shared_state: &'a mut SharedStateTrack,
    failure_handler: &'a FailureHandler,
    privacy_gateway: &'a PrivacyGateway<L>,
    scheduler: &'a Arc<ConductorScheduler>,
    app_handle: Option<&'a tauri::AppHandle<tauri::Wry>>,
    pool: &'a sqlx::SqlitePool,
    /// items.id=817: where the retry loop announces a hosted wait. None when
    /// there is no app handle (tests, headless runs).
    wait_notice: Option<Arc<dyn RetryWaitNotice>>,
}

impl<L: DisclosureLogger> StepAttempt for RealAttempt<'_, L> {
    fn retry_wait_notice(&self) -> Option<Arc<dyn RetryWaitNotice>> {
        self.wait_notice.clone()
    }

    fn once(
        &mut self,
        retry_count: u32,
    ) -> impl std::future::Future<Output = Result<Option<FailureResult>, ConductorError>> + Send
    {
        self.executor.execute_once(
            self.ctx,
            retry_count,
            self.personal_track,
            self.task_track,
            self.shared_state,
            self.failure_handler,
            self.privacy_gateway,
            self.scheduler,
            self.app_handle,
            self.pool,
        )
    }
}

/// The retry loop of `StepExecutor::execute`: run an attempt, and retry a
/// Retry result up to MAX_RETRIES times.
///
/// Stop (items.id=727): if `ctx.stop` is tripped, a Retry result is returned
/// as it is instead of calling the attempt again, for every failure class, and
/// a hosted wait that the stop interrupts does the same. The run's owner
/// (lifecycle) checks the token before it looks at any failure and cancels the
/// run, so the model is never called after a cancel.
async fn retry_loop<A: StepAttempt>(
    attempt: &mut A,
    ctx: &StepContext,
    failure_handler: &FailureHandler,
    sample: impl Fn(u64) -> u64,
) -> Option<FailureResult> {
    let mut retry_count = 0u32;
    // items.id=817: taken once, owned, so it does not borrow `attempt`.
    let notice = attempt.retry_wait_notice();
    let announce = notice.as_deref().map(|sink| RetryWaitIdentity {
        sink,
        focus_run_id: &ctx.focus_run_id,
        focus_name: &ctx.focus_name,
        step_display_name: &ctx.step.display_name,
    });

    loop {
        match attempt.once(retry_count).await {
            // DisclosureLogWrite (F_SYSTEM) and VoiceProfileContamination (F4 subtype)
            // propagate out of execute_once as Err(). Convert to FailureResult.
            // Python oracle: except (DisclosureLogWriteError, VoiceProfileContaminationError)
            Err(e) => {
                return Some(failure_handler.handle(
                    &e,
                    Some(&ctx.step.action_id),
                    Some(&ctx.focus_id),
                    retry_count,
                ));
            }

            Ok(None) => return None, // step succeeded

            Ok(Some(result)) => {
                if result.action == FailureAction::AwaitFloorConsent {
                    return Some(result); // never retried
                }
                if result.action == FailureAction::Retry && retry_count < MAX_RETRIES {
                    // items.id=727: a stopped run is not retried. items.id=816:
                    // a user's Stop of a streamed reply counts too.
                    let stop = effective_stop(ctx);
                    if stop.is_some_and(|s| s.is_cancelled()) {
                        return Some(on_stop(ctx, result));
                    }
                    // items.id=710/727: a hosted retry waits (Retry-After and
                    // jittered backoff); a Retry-After over the cap parks the
                    // run, and a stop during the wait ends it. The inference
                    // slot was released before this point. Each retry re-runs
                    // the spend pre-flight and counts as a request (items.id=710).
                    if let Some(done) =
                        wait_before_retry(&result, retry_count, stop, &sample, announce.as_ref())
                            .await
                    {
                        // A wait that a stop interrupted ends as a stop; one
                        // that ended for another reason (a Retry-After over the
                        // cap parks the run) is returned as it is.
                        if stop.is_some_and(|s| s.is_cancelled()) {
                            return Some(on_stop(ctx, done));
                        }
                        return Some(done);
                    }
                    retry_count += 1;
                    continue;
                }
                return Some(result);
            }
        }
    }
}

/// A rate limit asked for a wait longer than the cap: stop retrying and park
/// the run for the user (items.id=710). The wait travels as data.
fn long_wait_result(mut result: FailureResult, secs: u64) -> FailureResult {
    let mut meta = HashMap::new();
    meta.insert(
        "code".to_owned(),
        serde_json::json!(Code::BlockRateLimitedLongWait.as_str()),
    );
    meta.insert(RETRY_AFTER_METADATA_KEY.to_owned(), serde_json::json!(secs));
    meta.insert(
        "wait_cap_secs".to_owned(),
        serde_json::json!(RETRY_AFTER_CAP_SECS),
    );
    result.action = FailureAction::AwaitUser;
    result.severity = FailureSeverity::Require;
    result.metadata = Some(meta);
    result
}

/// A spend block as the error the failure handler understands.
fn spend_block_error(block: &SpendBlock) -> ConductorError {
    ConductorError::SpendBlock {
        plain_language: block.code_str().to_owned(),
        params: block_metadata(block),
    }
}

/// One hosted step call, end to end (items.id=709 dispatch, items.id=710 spend
/// gate): resolve the rows and the stored key, build the call context, run the
/// consent / price / cap / limit / unattended pre-flight, make the call,
/// settle the ledger. Nothing reaches the network unless the pre-flight
/// clears. The client and the time zone are parameters so the whole path can be
/// exercised against a loopback server (`build` is `HostedInputs::build_call`
/// in production, which refuses anything but HTTPS).
#[allow(clippy::too_many_arguments)]
async fn hosted_step_call<Tz: chrono::TimeZone>(
    client: &HostedClient,
    pool: &sqlx::SqlitePool,
    ctx: &StepContext,
    provider_id: &str,
    model: &ProviderModel,
    request: &GenerateRequest,
    reviewed: &ReviewedPrompt,
    tz: &Tz,
    build: fn(&HostedInputs) -> Result<CallContext, ProviderError>,
    stream: Option<StreamParts<'_>>,
) -> Result<GenerateResponse, ConductorError> {
    // items.id=884: the one place a hosted step's text reaches a request. The
    // request's own prompt was set to the released text before the context-window
    // check, so the two are the same string. A difference means someone built the
    // request from other text: nothing is sent.
    if request.prompt != reviewed.as_str() {
        return Err(ConductorError::TierBoundaryViolation {
            plain_language: format!(
                "Step '{}' was about to send text other than the text that was reviewed. \
                 Nothing was sent. [Get help]",
                ctx.step.action_id
            ),
        });
    }
    let hosted_request = hosted_request_from(&ctx.step.options_override, request, reviewed);
    let inputs = resolve_hosted_inputs(pool, &ctx.user_id, &ctx.key_hex, provider_id, model)
        .await
        .map_err(ConductorError::from)?;
    let call = build(&inputs).map_err(ConductorError::from)?;

    // The hash of the disclosed attributes spans every model of the provider,
    // and exact price parsing needs the raw pricing text.
    let all_models = provider_store::list_models_for_provider(pool, provider_id)
        .await
        .map_err(|_| spend_block_error(&SpendBlock::new(Code::BlockLedgerUnavailable)))?;
    let pricing_texts = provider_store::list_model_pricing_text(pool, provider_id)
        .await
        .map_err(|_| spend_block_error(&SpendBlock::new(Code::BlockLedgerUnavailable)))?;

    let non_empty = |s: &'_ str| {
        if s.is_empty() {
            None
        } else {
            Some(s.to_owned())
        }
    };
    let persona_id = non_empty(&ctx.persona_id);
    let focus_id = non_empty(&ctx.focus_id);
    let run_id = non_empty(&ctx.focus_run_id);
    let spend = SpendCall {
        user_id: &ctx.user_id,
        key_hex: &ctx.key_hex,
        provider: &inputs.provider,
        model,
        all_models: &all_models,
        pricing_texts: &pricing_texts,
        persona_id: persona_id.as_deref(),
        focus_id: focus_id.as_deref(),
        run_id: run_id.as_deref(),
        attendance: ctx.attendance,
        plan_cap_micros: plan_cap_micros(ctx),
    };

    let result = match stream {
        None => metered_generate(client, &call, &hosted_request, &spend, tz).await,
        Some(parts) => {
            // items.id=715: the same gate, with the call streamed. The text
            // the user was shown comes back in the tail even when the call
            // did not complete, so Gate 2 can scan it.
            let m = metered_generate_streamed(
                client,
                &call,
                &hosted_request,
                &spend,
                tz,
                parts.sink,
                parts.cancel,
            )
            .await;
            parts.tail.received_text = m.received_text;
            parts.tail.cancelled = m.cancelled;
            m.result
        }
    };
    match result {
        Ok(metered) => Ok(metered.outcome.response),
        Err(SpendCallError::Block(block)) => Err(spend_block_error(&block)),
        Err(SpendCallError::Provider(e)) => Err(ConductorError::from(e)),
    }
}

/// Whether this step streams its reply (items.id=715): its run's owner asked
/// for it AND the step goes to a hosted provider. A local step never streams.
///
/// items.id=921: the retry loop asks this without a selection (it has neither the
/// error nor the selected model), so it cannot key on the destination a resolver
/// picked. A RESOLVED step therefore never streams (R1 streams only API Chat, whose
/// bound run is never resolved); asking the same question from here and from
/// execute_once keeps the two in step. An ordinary step and a bound run answer as
/// before.
fn step_streams(ctx: &StepContext) -> bool {
    ctx.stream.is_some() && ctx.effective_access != ExternalAccess::LocalOnly && !is_resolved(ctx)
}

/// Whether the model of this step is chosen by the resolver (items.id=896): it
/// carries a requirement and is not a bound API Chat run, which keeps its bound
/// destination whatever the step carries. One definition for execute_once's
/// resolver gate and the stream predicate.
fn is_resolved(ctx: &StepContext) -> bool {
    ctx.bound.is_none() && ctx.step.requirement.is_some()
}

/// The treatment of this attempt (items.id=921): the ceiling values lifecycle
/// computed, run through step_treatment::derive with this attempt's own selection.
fn derive_treatment(
    ctx: &StepContext,
    resolver_req: Option<&crate::conductor::model_block::config::ModelRequirement>,
    selected: &SelectedModel,
) -> Result<StepTreatment, step_treatment::TreatmentError> {
    let basis = match resolver_req {
        None => Basis::Ordinary,
        Some(req) => Basis::Resolved {
            highest_tier: req.range.highest_tier,
            chosen: Chosen::from_parts(
                selected.provider_id.is_some(),
                selected.hosted_model.is_some(),
            )?,
        },
    };
    step_treatment::derive(
        CeilingValues {
            access: ctx.effective_access,
            execution_tier: ctx.execution_tier,
            abstraction_tier: ctx.abstraction_tier,
            raw_abstraction: ctx.raw_abstraction,
        },
        basis,
    )
}

/// The one token the retry machinery watches (items.id=816). When the step
/// streams it is the stream's token, a child of `ctx.stop` for API Chat, so it
/// is tripped both by the user's Stop (`cancel_chat_stream` trips only the
/// child) and by a parent trip (a rotation's 'Stop and save', `cancel_run`).
/// Otherwise it is `ctx.stop`. A run with no stream sees exactly `ctx.stop`.
fn effective_stop(ctx: &StepContext) -> Option<&tokio_util::sync::CancellationToken> {
    if step_streams(ctx) {
        ctx.stream.as_ref().map(|s| s.token())
    } else {
        ctx.stop.as_ref()
    }
}

/// What the retry loop returns once the effective stop is tripped, in place of
/// `last` (the result it would have retried). A parent trip (`ctx.stop`) leaves
/// `last` as it is: the lifecycle sees the lease token and cancels the run. A
/// trip of the stream's token alone is a user's Stop, which the lifecycle can
/// only learn from the failure, so it becomes `stopped_failure`.
fn on_stop(ctx: &StepContext, last: FailureResult) -> FailureResult {
    if ctx.stop.as_ref().is_some_and(|s| s.is_cancelled()) {
        last
    } else {
        stopped_failure(ctx)
    }
}

/// What a streamed step needs from its caller (items.id=715): where the
/// display-only text goes, the cancel token, and a place to hand back what
/// was received.
pub(crate) struct StreamParts<'a> {
    pub sink: &'a dyn StreamSink,
    pub cancel: &'a tokio_util::sync::CancellationToken,
    pub tail: &'a mut StreamTail,
}

/// What a streamed call leaves behind, for Gate 2 on a call that did not
/// complete. Never printed: it holds reply text.
#[derive(Default)]
pub(crate) struct StreamTail {
    pub received_text: String,
    pub cancelled: bool,
}

/// What Gate 2 found in an external reply (streamed or not).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReplyGate2 {
    pub flagged: bool,
    pub audit_unavailable: bool,
    /// Highest sensitivity_severity among the matched fields; 0 when nothing
    /// matched. Severity only, never a value.
    pub matched_severity: i32,
}

impl ReplyGate2 {
    /// The step's sensitivity once Gate 2 has spoken (items.id=735,
    /// decisions.id=883): never lower than the severity computed from the
    /// fields projected into the prompt, and raised to the highest matched
    /// field, because a withheld value coming back in the reply is understated
    /// by the projected-field figure. A no-hit reply changes nothing.
    pub fn apply_to(&self, computed_sensitivity: i32) -> i32 {
        computed_sensitivity.max(self.matched_severity)
    }
}

/// Gate 2 for an external reply, streamed or not (items.id=715 for the
/// streamed path, items.id=735 for the rest; decisions.id=883): scan the text,
/// write gate2's disclosure-log entry on a hit, mark the chat reply
/// personal-flagged, and tell the caller to raise the notice and the step's
/// sensitivity. It never fails the step and never alters, holds, discards or
/// retracts the reply. This is the one implementation of the rule; both
/// branches of step 11 call it.
///
/// If the disclosure-log write fails (gate2 returns an error only when it
/// matched AND external access is live), the reply is kept and still flagged,
/// the notice says the audit entry is unavailable, and a content-free error is
/// logged. Failing the run would drop or retract a reply whose provider cost
/// is already spent. Chat-PM accepted this 2026-10-12 for streams and the
/// same rule applies here.
///
/// qr_local is inert: Gate 1 at Raw abstraction shares every field and gate2
/// skips shared fields, so this never flags a local step.
#[allow(clippy::too_many_arguments)] // items.id=921: the treatment joined an already explicit call.
async fn reply_gate2<L: DisclosureLogger>(
    ctx: &StepContext,
    treatment: &StepTreatment,
    privacy_gateway: &PrivacyGateway<L>,
    gate_track: &GateTrack,
    text: &str,
    provider: Option<String>,
    fields_shared: &[String],
    app_handle: Option<&tauri::AppHandle<tauri::Wry>>,
) -> ReplyGate2 {
    if text.is_empty() {
        return ReplyGate2::default();
    }
    let found = privacy_gateway
        .gate2(
            &ctx.step.action_id,
            &ctx.focus_run_id,
            text,
            gate_track,
            treatment.execution_tier,
            treatment.access,
            provider,
            Some(fields_shared),
        )
        .await;
    let outcome = match found {
        Ok(g2) if !g2.flagged => return ReplyGate2::default(),
        Ok(g2) => ReplyGate2 {
            flagged: true,
            audit_unavailable: false,
            matched_severity: i32::from(g2.matched_max_severity),
        },
        Err(_) => {
            log::error!(
                "executor: Gate 2 matched a reply but its disclosure-log entry \
                 could not be written (run {}, step {}); the reply is kept and flagged",
                ctx.focus_run_id,
                ctx.step.action_id
            );
            // The error carries no result, so recover the severity from the
            // same pure scan.
            let severity = crate::conductor::privacy::gate2::scan_matches(
                text,
                gate_track,
                Some(fields_shared),
            )
            .into_iter()
            .map(|(_, severity)| i32::from(severity))
            .max()
            .unwrap_or(0);
            ReplyGate2 {
                flagged: true,
                audit_unavailable: true,
                matched_severity: severity,
            }
        }
    };
    if ctx.key_hex.is_empty() {
        return outcome;
    }
    // The personal-data flag is privacy-relevant: if it cannot be saved the
    // user is told (items.id=778) and shown what to do, not just logged.
    let marked = crate::write_failure::retry_transient(|| async {
        crate::persistence::message_store::mark_reply_personal_flagged(
            &ctx.user_id,
            &ctx.persona_id,
            &ctx.key_hex,
            &ctx.focus_run_id,
        )
        .await
        .map_err(|e| e.to_string())
    })
    .await;
    if let Err(e) = marked {
        crate::write_failure::report(
            app_handle,
            crate::write_failure::Lost::PersonalFlag,
            &format!(
                "executor: could not mark the reply of run {} personal-flagged: {e}",
                ctx.focus_run_id
            ),
        );
    }
    outcome
}

/// Run `fut` unless `stop` is tripped first: `None` means it was stopped (the
/// future is dropped, which abandons the HTTP request; nothing it had not
/// already returned is kept). With no token it simply awaits (items.id=778).
async fn until_stopped<T>(
    stop: Option<&tokio_util::sync::CancellationToken>,
    fut: impl std::future::Future<Output = T>,
) -> Option<T> {
    match stop {
        Some(stop) => {
            tokio::select! {
                result = fut => Some(result),
                () = stop.cancelled() => None,
            }
        }
        None => Some(fut.await),
    }
}

/// The failure mode of a step that was stopped (items.id=778, 816). Built only
/// by `stopped_failure`; the lifecycle ends a run as cancelled on it (a
/// source-scan test in auth/writer_gate.rs pins the single producer).
pub(crate) const USER_STOP_MODE: &str = "USER_STOP";

/// Whether `failure` is the stop `stopped_failure` builds.
pub(crate) fn is_user_stop(failure: &FailureResult) -> bool {
    failure.action == FailureAction::Stop && failure.failure_mode.as_deref() == Some(USER_STOP_MODE)
}

/// The failure a step returns when it was stopped: by 'Stop and save' for a key
/// rotation (items.id=778), by a user's Cancel (items.id=816), or by an API Chat
/// Stop during a retry wait (items.id=816). It is never shown, retried or
/// parked: the run's lifecycle ends the run as cancelled.
fn stopped_failure(ctx: &StepContext) -> FailureResult {
    FailureResult {
        action: FailureAction::Stop,
        failure_mode: Some(USER_STOP_MODE.to_owned()),
        plain_language: "This was stopped.".to_owned(),
        is_recoverable: false,
        severity: FailureSeverity::Stop,
        action_id: Some(ctx.step.action_id.clone()),
        focus_id: Some(ctx.focus_id.clone()),
        metadata: None,
    }
}

/// A streamed reply that was cancelled or cut keeps the text the user was
/// shown, on the chat reply row of this run, marked `cancelled` or `cut`
/// (items.id=716; decisions.id=883: displayed text is never retracted). The
/// run still parks through the ordinary failure path, and finalize_chat_reply
/// leaves a kept partial reply alone. A run that owns no chat reply row (not a
/// chat send) is a no-op. A failed write is logged and never changes the run.
async fn keep_partial_reply(
    ctx: &StepContext,
    tail: &StreamTail,
    app_handle: Option<&tauri::AppHandle<tauri::Wry>>,
) {
    if tail.received_text.is_empty() || ctx.key_hex.is_empty() {
        return;
    }
    let status = if tail.cancelled {
        crate::persistence::message_store::ReplyStatus::Cancelled
    } else {
        crate::persistence::message_store::ReplyStatus::Cut
    };
    let kept = crate::write_failure::retry_transient(|| async {
        crate::persistence::message_store::set_reply_outcome(
            &ctx.user_id,
            &ctx.persona_id,
            &ctx.key_hex,
            &ctx.focus_run_id,
            status,
            Some(&tail.received_text),
        )
        .await
        .map_err(|e| e.to_string())
    })
    .await;
    // The text the user was shown is user content: if it cannot be kept, say
    // so (items.id=778) instead of logging and moving on.
    if let Err(e) = kept {
        crate::write_failure::report(
            app_handle,
            crate::write_failure::Lost::Reply,
            &format!(
                "executor: could not keep the partial reply of run {}: {e}",
                ctx.focus_run_id
            ),
        );
    }
}

/// A reply that completed: mark it `truncated` when the provider stopped at
/// the output limit (items.id=716, the ruling of items.id=709 that a truncated
/// hosted answer must be surfaced), and for a streamed or bound run reset the
/// status to `complete` otherwise, so an earlier attempt that was cut and then
/// retried does not leave its mark on the finished reply. Not a chat send: a
/// no-op.
async fn record_completed_reply(
    ctx: &StepContext,
    response: &GenerateResponse,
    app_handle: Option<&tauri::AppHandle<tauri::Wry>>,
) {
    if ctx.key_hex.is_empty() {
        return;
    }
    let truncated = response.completion_status == CompletionStatus::Truncated;
    if !truncated && ctx.stream.is_none() && ctx.bound.is_none() {
        return;
    }
    let status = if truncated {
        crate::persistence::message_store::ReplyStatus::Truncated
    } else {
        crate::persistence::message_store::ReplyStatus::Complete
    };
    let recorded = crate::write_failure::retry_transient(|| async {
        crate::persistence::message_store::set_reply_outcome(
            &ctx.user_id,
            &ctx.persona_id,
            &ctx.key_hex,
            &ctx.focus_run_id,
            status,
            None,
        )
        .await
        .map_err(|e| e.to_string())
    })
    .await;
    // The marker says whether a reply was cut off at the output limit
    // (items.id=716): if it cannot be saved the user is told (items.id=778).
    if let Err(e) = recorded {
        crate::write_failure::report(
            app_handle,
            crate::write_failure::Lost::ReplyStatus,
            &format!(
                "executor: could not record the outcome of run {}'s reply: {e}",
                ctx.focus_run_id
            ),
        );
    }
}

/// Step 12: the TaskStep a completed step records. `content` is the model
/// output only (D4-040) and is never altered; Gate 2's outcome raises the
/// sensitivity and carries the flags with the content so they survive
/// checkpoint and resume and reach the step-completion status.
fn completed_step(
    ctx: &StepContext,
    content: &str,
    computed_sensitivity: i32,
    execution_tier: u8,
    g2: &ReplyGate2,
) -> TaskStep {
    let output_var = ctx
        .step
        .output_var
        .clone()
        .or_else(|| Some(ctx.step.action_id.clone()))
        .filter(|v| !v.is_empty());
    TaskStep {
        action_id: ctx.step.action_id.clone(),
        output_var,
        content: content.to_owned(),
        sensitivity_severity: g2.apply_to(computed_sensitivity),
        routing_tier_used: execution_tier as i32,
        personal_flagged: g2.flagged,
        audit_unavailable: g2.audit_unavailable,
    }
}

/// What a hosted call is built from: the two catalog rows (as a plain-data
/// route) and the user's stored key record.
struct HostedInputs {
    route: HostedRoute,
    /// The provider's catalog row (items.id=710: the spend gate reads its
    /// disclosed attributes).
    provider: Provider,
    key: integration_keys_store::IntegrationKey,
}

impl HostedInputs {
    /// Validate and assemble the CallContext (HTTPS-only; see hosted_call.rs).
    fn build_call(&self) -> Result<CallContext, ProviderError> {
        CallContext::build(&self.route, &self.key.credential, &self.key.extra_metadata)
    }
}

/// Resolve the provider row, the model row and the user's stored key
/// (items.id=709, decisions.id=875). The key comes ONLY from integration_keys
/// -- no environment variable is consulted. A missing key, or a store that
/// can't be opened (empty `key_hex`: no unlocked session), is MissingApiKey,
/// with copy that points to Settings.
async fn resolve_hosted_inputs(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    provider_id: &str,
    model: &ProviderModel,
) -> Result<HostedInputs, ProviderError> {
    let provider = match provider_store::get_provider(pool, provider_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(ProviderError::UnknownProvider {
                provider: provider_id.to_owned(),
                plain_language: "The selected AI provider is not in this build's provider \
                    catalog. [Open Settings] [Get help]"
                    .to_owned(),
            })
        }
        Err(_) => {
            return Err(ProviderError::UnknownProvider {
                provider: provider_id.to_owned(),
                plain_language: "Quiet Rabbit couldn't look up the selected AI provider. \
                    [Try again] [Get help]"
                    .to_owned(),
            })
        }
    };

    let route = HostedRoute::from_rows(&provider, model);
    let missing = || ProviderError::MissingApiKey {
        provider: provider.id.clone(),
        plain_language: format!(
            "No {} API key is saved. Add one in Settings to use it. \
             [Open Settings] [Use local AI instead]",
            provider.display_name
        ),
    };
    if key_hex.is_empty() {
        return Err(missing());
    }
    let key = integration_keys_store::get_active_key(
        user_id,
        key_hex,
        provider_id,
        QR_HOSTED_KEY_TYPE,
        None,
    )
    .await
    .map_err(|_| ProviderError::Unavailable {
        provider: provider.id.clone(),
        plain_language: "Quiet Rabbit couldn't read your saved API key. \
            [Try again] [Get help]"
            .to_owned(),
    })?
    .ok_or_else(missing)?;

    Ok(HostedInputs {
        route,
        provider,
        key,
    })
}

/// Context window size for a resolved model selection.
/// Python oracle: StepExecutor._get_context_window()
///
/// items.id=465/items.id=504 (PARTIAL close of B4 -- confirmed acceptable
/// scope, chat_session_handoffs.id=316): at Tier 2 (`provider_id: Some(_)`),
/// checks provider_models by its real `provider_id`/`model_id` columns
/// (never a reconstructed composite id -- decisions.id=813) before falling
/// back to the 3-entry hardcoded map. At qr_local (`provider_id: None`) the
/// catalog is skipped entirely -- Ollama ids have no providers row to hang a
/// catalog entry off yet (Ollama-as-qr_local wiring is a separate, larger,
/// out-of-scope initiative). Not a full close of B4.
pub(crate) async fn get_context_window(
    pool: &sqlx::SqlitePool,
    provider_id: Option<&str>,
    model_id: &str,
) -> u32 {
    if let Some(provider_id) = provider_id {
        let from_catalog = crate::persistence::provider_store::get_model_by_provider_and_model_id(
            pool,
            provider_id,
            model_id,
        )
        .await
        .ok()
        .flatten();
        if let Some(model) = from_catalog {
            return model.context_window_tokens;
        }
    }

    match model_id {
        "llama3.2:3b" => 4096,
        "llama3.1:8b" => 8192,
        "qwen2.5:7b" => 8192,
        _ => 2048,
    }
}

/// Build GenerateOptions from options_raw dict and effective context window.
/// Python oracle: StepExecutor._build_options()
/// GenerateOptions.temperature and top_p are f64 (providers/types.rs) — no cast needed.
fn build_options(
    options_raw: &HashMap<String, serde_json::Value>,
    effective_ctx: u32,
) -> GenerateOptions {
    GenerateOptions {
        temperature: options_raw
            .get("temperature")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.5),
        top_p: options_raw
            .get("top_p")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.90),
        num_ctx: effective_ctx,
        num_predict: options_raw
            .get("num_predict")
            .and_then(|v| v.as_u64())
            .unwrap_or(1024) as u32,
    }
}

// ---------------------------------------------------------------------------
// Step sensitivity computation
// ---------------------------------------------------------------------------

/// Derive step sensitivity severity from projected fields (external access
/// live) or template token scan (qr_local).
///
/// External access live: max sensitivity_severity across fields actually
///   referenced in projected_fields that are present in personal_track.
/// qr_local: max sensitivity_severity across personal_track fields whose
///   field_name appears as a {token} in the prompt template.
/// Default: 1 (general) when no personal fields involved.
///
/// Python oracle: Step 12 sensitivity logic in StepExecutor._execute_once()
///
/// items.id=528 Phase 2: ctx.execution_tier's redundant `execution_tier: u8`
/// parameter dropped -- ctx is already passed, so this reads
/// ctx.effective_access directly instead of a second copy of the same
/// magnitude question threaded in separately.
fn compute_step_sensitivity(
    ctx: &StepContext,
    treatment: &StepTreatment,
    personal_track: &PersonalTrack,
    projected_fields: &HashMap<String, String>,
) -> i32 {
    // items.id=921: keyed on the step's destination, not its ceiling.
    if treatment.access != ExternalAccess::LocalOnly && !projected_fields.is_empty() {
        let severities: Vec<i32> = projected_fields
            .keys()
            .filter_map(|name| personal_track.fields().get(name))
            .map(|pf| pf.sensitivity_severity)
            .collect();
        return severities.into_iter().max().unwrap_or(1);
    }

    if treatment.access == ExternalAccess::LocalOnly {
        let used_fields: Vec<i32> = find_tokens(&ctx.step.prompt_template)
            .into_iter()
            .filter_map(|token| personal_track.fields().get(&token))
            .map(|pf| pf.sensitivity_severity)
            .collect();
        return used_fields.into_iter().max().unwrap_or(1);
    }

    1
}

// ---------------------------------------------------------------------------
// PersonalTrack conversion
// ---------------------------------------------------------------------------

/// Convert conductor::types::PersonalTrack (String-typed fields, voice_profile)
/// to privacy::types::PersonalTrack (typed enum fields for gate API).
///
/// Required because PrivacyGateway::gate1/gate2 take privacy::types::PersonalTrack.
/// The conversion maps String sensitivity/abstraction policy fields -> typed enums.
/// Fail-closed on unrecognised values (Unknown(s) and General/Pass respectively).
///
/// Python oracle: N/A — single PersonalTrack class in Python.
fn to_gate_track(ct: &PersonalTrack) -> GateTrack {
    let mut gt = GateTrack::new();
    for (_, field) in ct.fields() {
        let sensitivity = match field.sensitivity.as_str() {
            "personal" => Sensitivity::Personal,
            "medical" => Sensitivity::Medical,
            "financial" => Sensitivity::Financial,
            _ => Sensitivity::General, // "general" + unknown -> General (fail-safe)
        };
        let _ = gt.add_field(GateField {
            field_name: field.field_name.clone(),
            field_value: field.field_value.clone(),
            sensitivity,
            sensitivity_severity: field.sensitivity_severity as u8,
            source_id: field.source_id.clone(),
            abstraction_tier2: AbstractionPolicy::from_str(&field.abstraction_tier2),
            abstraction_tier3: AbstractionPolicy::from_str(&field.abstraction_tier3),
        });
    }
    gt.seal();
    gt
}

// ---------------------------------------------------------------------------
// TaskTrack output_vars helper
// ---------------------------------------------------------------------------

/// Collect output_var->content pairs from all completed steps.
/// Matches TaskTrack's internal output_vars HashMap without adding a public
/// accessor (avoids modifying types.rs in this commit).
/// Python oracle: ctx.task_track.output_vars.items()
pub(crate) fn collect_output_vars(steps: &[TaskStep]) -> HashMap<String, String> {
    steps
        .iter()
        .filter_map(|s| {
            s.output_var
                .as_ref()
                .filter(|v| !v.is_empty())
                .map(|v| (v.clone(), s.content.clone()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// PII detection helpers (regex-free, semantically equivalent)
// ---------------------------------------------------------------------------

/// Find all {snake_case_token} patterns in a template string.
/// Matches [a-z_][a-z0-9_]* — identical to Python oracle's _TOKEN_PATTERN.
/// Returns deduplicated list in discovery order.
pub(crate) fn find_tokens(template: &str) -> Vec<String> {
    let chars: Vec<char> = template.chars().collect();
    let n = chars.len();
    let mut tokens: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut i = 0;

    while i < n {
        if chars[i] != '{' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        // Scan to closing brace; abort on nested '{' or end of string.
        while j < n && chars[j] != '}' && chars[j] != '{' {
            j += 1;
        }
        if j >= n || chars[j] != '}' {
            i += 1;
            continue;
        }
        let token: String = chars[start..j].iter().collect();
        // Validate pattern: [a-z_][a-z0-9_]*
        let mut tc = token.chars();
        let first_ok = tc
            .next()
            .map(|c| c == '_' || c.is_ascii_lowercase())
            .unwrap_or(false);
        let rest_ok = tc.all(|c| c == '_' || c.is_ascii_digit() || c.is_ascii_lowercase());
        if first_ok && rest_ok && !seen.contains(&token) {
            seen.insert(token.clone());
            tokens.push(token);
        }
        i = j + 1;
    }
    tokens
}

/// Strip all {token} patterns from a template string.
/// Matches only valid snake_case tokens (same constraint as find_tokens).
/// Python oracle: _TOKEN_PATTERN.sub("", template)
fn strip_tokens(template: &str) -> String {
    let mut result = template.to_owned();
    for token in find_tokens(template) {
        result = result.replace(&format!("{{{token}}}"), "");
    }
    result
}

/// Case-insensitive whole-word substring match.
/// Equivalent to re.search(r"\b" + re.escape(needle) + r"\b", haystack, re.IGNORECASE).
/// Word boundary: chars on both sides of the match are non-alphanumeric (or absent).
fn has_word_boundary_match_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let h = haystack.to_lowercase();
    let n = needle.to_lowercase();
    let n_len = n.len();
    let mut start = 0;

    while let Some(pos) = h[start..].find(&n) {
        let abs_pos = start + pos;
        let before_ok = if abs_pos == 0 {
            true
        } else {
            h[..abs_pos]
                .chars()
                .next_back()
                .map(|c| !c.is_alphanumeric())
                .unwrap_or(true)
        };
        let end = abs_pos + n_len;
        let after_ok = if end >= h.len() {
            true
        } else {
            h[end..]
                .chars()
                .next()
                .map(|c| !c.is_alphanumeric())
                .unwrap_or(true)
        };
        if before_ok && after_ok {
            return true;
        }
        start = abs_pos + 1;
        if start >= h.len() {
            break;
        }
    }
    false
}

/// Simple email pattern detection: non-empty local@domain with dot in domain.
/// Equivalent to re.search(r"\S+@\S+\.\S+", value) for voice profile values.
fn is_email_pattern(value: &str) -> bool {
    if let Some(at_pos) = value.find('@') {
        let local = &value[..at_pos];
        let domain = &value[at_pos + 1..];
        !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
    } else {
        false
    }
}

/// Detect 7+ consecutive ASCII digit characters.
/// Equivalent to re.search(r"\d{7,}", value).
fn has_digit_dense(value: &str) -> bool {
    let mut count = 0usize;
    for c in value.chars() {
        if c.is_ascii_digit() {
            count += 1;
            if count >= 7 {
                return true;
            }
        } else {
            count = 0;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Voice profile formatting
// ---------------------------------------------------------------------------

/// Inject approved voice attributes into a prompt-safe string.
/// Excludes keys not in ALLOWED_VOICE_ATTRIBUTES. Logs excluded keys.
/// Sorted output for prompt reproducibility.
/// Python oracle: _format_voice_profile()
fn format_voice_profile(voice_profile: &HashMap<String, String>) -> String {
    if voice_profile.is_empty() {
        return String::new();
    }
    let mut approved: Vec<(&str, &str)> = ALLOWED_VOICE_ATTRIBUTES
        .iter()
        .filter_map(|attr| voice_profile.get(*attr).map(|v| (*attr, v.as_str())))
        .collect();

    let unknown: Vec<&str> = voice_profile
        .keys()
        .filter(|k| !ALLOWED_VOICE_ATTRIBUTES.contains(&k.as_str()))
        .map(|k| k.as_str())
        .collect();

    if !unknown.is_empty() {
        log::warn!(
            "executor: voice profile attrs excluded (not in allowlist): {:?}",
            unknown
        );
    }

    if approved.is_empty() {
        return String::new();
    }

    approved.sort_by_key(|(k, _)| *k);
    approved
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The treatment of an ORDINARY step (items.id=921): the context's own ceiling
    /// values, copied as they are (no invariant check, so a test may set them freely).
    fn ordinary(ctx: &StepContext) -> StepTreatment {
        StepTreatment {
            access: ctx.effective_access,
            execution_tier: ctx.execution_tier,
            abstraction_tier: ctx.abstraction_tier,
            raw_abstraction: ctx.raw_abstraction,
            hosted: ctx.effective_access != ExternalAccess::LocalOnly,
        }
    }

    #[test]
    fn find_tokens_basic() {
        let t = "Hello {user_input}, your {city} is ready.";
        let tokens = find_tokens(t);
        assert_eq!(tokens, vec!["user_input", "city"]);
    }

    #[test]
    fn find_tokens_snake_case_only() {
        let t = "{User} {1invalid} {valid_one}";
        let tokens = find_tokens(t);
        assert_eq!(tokens, vec!["valid_one"]);
    }

    #[test]
    fn find_tokens_deduplicates() {
        let t = "{city} and {city} again";
        let tokens = find_tokens(t);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0], "city");
    }

    #[test]
    fn find_tokens_empty_template() {
        assert!(find_tokens("no tokens here").is_empty());
    }

    #[test]
    fn find_tokens_unclosed_brace_skipped() {
        assert!(find_tokens("hello {unclosed world").is_empty());
    }

    #[test]
    fn strip_tokens_removes_valid() {
        let stripped = strip_tokens("Hello {user_input} world");
        assert!(!stripped.contains("{user_input}"));
    }

    #[test]
    fn strip_tokens_leaves_invalid() {
        let stripped = strip_tokens("value: {1bad}");
        assert!(stripped.contains("{1bad}"));
    }

    #[test]
    fn digit_dense_detects_7_digits() {
        assert!(has_digit_dense("1234567"));
    }

    #[test]
    fn digit_dense_detects_embedded_digits() {
        assert!(has_digit_dense("call 1234567 now"));
    }

    #[test]
    fn digit_dense_6_digits_no_match() {
        assert!(!has_digit_dense("123456"));
    }

    #[test]
    fn digit_dense_interrupted_digits() {
        assert!(!has_digit_dense("123-456"));
    }

    #[test]
    fn digit_dense_empty() {
        assert!(!has_digit_dense(""));
    }

    #[test]
    fn email_detects_basic_address() {
        assert!(is_email_pattern("user@example.com"));
    }

    #[test]
    fn email_no_at_sign() {
        assert!(!is_email_pattern("notanemail"));
    }

    #[test]
    fn email_no_dot_after_at() {
        assert!(!is_email_pattern("user@nodot"));
    }

    #[test]
    fn email_empty_local() {
        assert!(!is_email_pattern("@example.com"));
    }

    #[test]
    fn word_boundary_whole_word_match() {
        assert!(has_word_boundary_match_ci(
            "I live in Portland today",
            "Portland"
        ));
    }

    #[test]
    fn word_boundary_case_insensitive() {
        assert!(has_word_boundary_match_ci(
            "Tone like portland vibes",
            "PORTLAND"
        ));
    }

    #[test]
    fn word_boundary_substring_not_matched() {
        // "port" does not boundary-match within "Portland" — 'l' follows, alphanumeric
        assert!(!has_word_boundary_match_ci("Portland is great", "port"));
    }

    #[test]
    fn word_boundary_at_start_of_string() {
        assert!(has_word_boundary_match_ci("Alice is here", "Alice"));
    }

    #[test]
    fn word_boundary_at_end_of_string() {
        assert!(has_word_boundary_match_ci("my name is Alice", "Alice"));
    }

    #[test]
    fn word_boundary_empty_needle() {
        assert!(!has_word_boundary_match_ci("anything", ""));
    }

    #[test]
    fn word_boundary_no_match() {
        assert!(!has_word_boundary_match_ci(
            "tone: conversational",
            "Portland"
        ));
    }

    #[test]
    fn format_voice_profile_known_keys_only() {
        let mut vp = HashMap::new();
        vp.insert("tone".to_owned(), "warm".to_owned());
        vp.insert("unknown_key".to_owned(), "ignored".to_owned());
        let formatted = format_voice_profile(&vp);
        assert!(formatted.contains("tone=warm"));
        assert!(!formatted.contains("unknown_key"));
    }

    #[test]
    fn format_voice_profile_sorted_output() {
        let mut vp = HashMap::new();
        vp.insert("tone".to_owned(), "warm".to_owned());
        vp.insert("formality".to_owned(), "casual".to_owned());
        let formatted = format_voice_profile(&vp);
        let formality_pos = formatted.find("formality=").unwrap();
        let tone_pos = formatted.find("tone=").unwrap();
        assert!(formality_pos < tone_pos);
    }

    #[test]
    fn format_voice_profile_empty_input() {
        assert_eq!(format_voice_profile(&HashMap::new()), "");
    }

    #[test]
    fn format_voice_profile_all_unknown() {
        let mut vp = HashMap::new();
        vp.insert("bad_key".to_owned(), "value".to_owned());
        assert_eq!(format_voice_profile(&vp), "");
    }

    /// items.id=465: select_model()'s Tier 1.5 (qr_hosted) branch and get_context_window()
    /// now read provider_models (shared.db) instead of matching on literal
    /// provider/model strings -- these tests need a real, migrated shared.db
    /// (shared_016.sql's own curation pass seeds the hosted default
    /// models), not just in-process logic. Mirrors provider_store.rs's own
    /// list_active_providers_and_max_risk_rating_via_public_api pattern:
    /// ENV_MUTEX-serialized, tempdir-backed QR_DATA_ROOT.
    async fn with_temp_shared_db<F, Fut>(f: F) -> Fut::Output
    where
        F: FnOnce(sqlx::SqlitePool) -> Fut,
        Fut: std::future::Future,
    {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration must succeed in test setup");

        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool must connect");

        let result = f(pool).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        result
    }

    /// qr_local never touches provider_store -- an in-memory, unmigrated pool
    /// is fine here (never acquired against).
    fn dummy_pool() -> sqlx::SqlitePool {
        sqlx::SqlitePool::connect_lazy_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(":memory:"),
        )
    }

    #[tokio::test]
    async fn select_model_tier1_code() {
        // qr_local never touches provider_store -- no DB setup needed.
        let selected = select_model(&dummy_pool(), "code", ExternalAccess::LocalOnly, None, None)
            .await
            .unwrap();
        assert_eq!(selected.provider_id, None);
        assert_eq!(selected.model_id, "qwen2.5:7b");
    }

    #[tokio::test]
    async fn select_model_tier1_quick_response() {
        let selected = select_model(
            &dummy_pool(),
            "quick_response",
            ExternalAccess::LocalOnly,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(selected.provider_id, None);
        assert_eq!(selected.model_id, "llama3.2:3b");
    }

    #[tokio::test]
    async fn select_model_tier1_summarization() {
        let selected = select_model(
            &dummy_pool(),
            "summarization",
            ExternalAccess::LocalOnly,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(selected.provider_id, None);
        assert_eq!(selected.model_id, "llama3.2:3b");
    }

    #[tokio::test]
    async fn select_model_tier1_general() {
        let selected = select_model(
            &dummy_pool(),
            "general",
            ExternalAccess::LocalOnly,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(selected.provider_id, None);
        assert_eq!(selected.model_id, "llama3.1:8b");
    }

    /// Inserts a neutral fixture hosted provider with one default model into
    /// the temp shared.db, and returns (provider_id, model_id, context window).
    /// Hosted tests read rows through fixtures rather than asserting seeded
    /// provider or model ids, so curation changes never break them
    /// (decisions.id=854 and 872, items.id=708).
    async fn seed_fixture_hosted(
        pool: &sqlx::SqlitePool,
        provider_id: &str,
        model_id: &str,
        context_window_tokens: u32,
    ) {
        crate::persistence::provider_store::create_provider(
            pool,
            crate::persistence::provider_store::NewProvider {
                id: provider_id,
                display_name: provider_id,
                provider_type: "cloud_inference_api",
                mode: crate::persistence::provider_store::ProviderMode::Api,
                launch_url: None,
                login_required: true,
                is_local: false,
                is_anonymous: false,
                retains_data: false,
                trains_on_data_by_default: false,
                qr_internal_eligible: false,
                privacy_guardian_default_level:
                    crate::persistence::provider_store::PrivacyGuardianDefaultLevel::Low,
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
        .expect("fixture provider must insert");
        sqlx::query(
            "INSERT INTO provider_models
                (id, provider_id, model_id, context_window_tokens, is_default, created_at)
             VALUES (?, ?, ?, ?, 1, '2026-01-01T00:00:00Z')",
        )
        .bind(format!("{provider_id}:{model_id}"))
        .bind(provider_id)
        .bind(model_id)
        .bind(context_window_tokens as i64)
        .execute(pool)
        .await
        .expect("fixture model must insert");
    }

    #[tokio::test]
    async fn select_model_qr_hosted_any_type() {
        with_temp_shared_db(|pool| async move {
            seed_fixture_hosted(&pool, "fixture-host-a", "fixture-model-a", 4096).await;

            for task_type in ["code", "general"] {
                let selected = select_model(
                    &pool,
                    task_type,
                    ExternalAccess::AnonymousRequired,
                    Some("fixture-host-a"),
                    None,
                )
                .await
                .unwrap();
                assert_eq!(selected.provider_id.as_deref(), Some("fixture-host-a"));
                assert_eq!(selected.model_id, "fixture-model-a");
            }
        })
        .await;
    }

    /// Every seeded hosted provider that can be selected must resolve to its
    /// own default model row. Field-based: names no provider or model
    /// (items.id=708 / items.id=703).
    #[tokio::test]
    async fn select_model_resolves_every_seeded_hosted_default() {
        with_temp_shared_db(|pool| async move {
            let hosted: Vec<(String, String)> = sqlx::query_as(
                "SELECT p.id, m.model_id FROM providers p
                 JOIN provider_models m ON m.provider_id = p.id AND m.is_default = 1
                 WHERE p.provider_type = 'cloud_inference_api' AND p.api_dialect IS NOT NULL",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert!(
                !hosted.is_empty(),
                "seeded hosted rows must have default models"
            );

            for (provider_id, model_id) in hosted {
                let selected = select_model(
                    &pool,
                    "general",
                    ExternalAccess::AnonymousRequired,
                    Some(&provider_id),
                    None,
                )
                .await
                .unwrap();
                assert_eq!(selected.provider_id.as_deref(), Some(provider_id.as_str()));
                assert_eq!(selected.model_id, model_id);
            }
        })
        .await;
    }

    /// items.id=251 — the test proving provider selection actually
    /// switches behavior, not just that a preference can be stored.
    /// Same task_type and tier; the only thing that differs is
    /// qr_hosted_provider, and the two calls must produce different models.
    #[tokio::test]
    async fn select_model_switches_on_qr_hosted_provider_preference() {
        with_temp_shared_db(|pool| async move {
            seed_fixture_hosted(&pool, "fixture-host-a", "fixture-model-a", 4096).await;
            seed_fixture_hosted(&pool, "fixture-host-b", "fixture-model-b", 8192).await;

            let a = select_model(
                &pool,
                "general",
                ExternalAccess::AnonymousRequired,
                Some("fixture-host-a"),
                None,
            )
            .await
            .unwrap();
            let b = select_model(
                &pool,
                "general",
                ExternalAccess::AnonymousRequired,
                Some("fixture-host-b"),
                None,
            )
            .await
            .unwrap();
            assert_ne!(a.model_id, b.model_id);
            assert_eq!(a.provider_id.as_deref(), Some("fixture-host-a"));
            assert_eq!(a.model_id, "fixture-model-a");
            assert_eq!(b.provider_id.as_deref(), Some("fixture-host-b"));
            assert_eq!(b.model_id, "fixture-model-b");
        })
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "select_model called with effective_access != LocalOnly and no")]
    async fn select_model_qr_hosted_none_preference_panics() {
        // Panics before ever touching provider_store -- no DB setup needed.
        let _ = select_model(
            &dummy_pool(),
            "general",
            ExternalAccess::AnonymousRequired,
            None,
            None,
        )
        .await;
    }

    /// items.id=465: an unrecognized-but-present provider id is now a real
    /// ConductorError (no provider_models row to resolve), not a panic --
    /// see select_model()'s own doc on why the old hardcoded-match panic
    /// for this case no longer applies.
    #[tokio::test]
    async fn select_model_qr_hosted_unknown_provider_returns_unknown_provider_error() {
        with_temp_shared_db(|pool| async move {
            let err = select_model(
                &pool,
                "general",
                ExternalAccess::AnonymousRequired,
                Some("fixture-absent"),
                None,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(err, ConductorError::UnknownProvider { .. }),
                "got: {err:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn context_window_known_models() {
        with_temp_shared_db(|pool| async move {
            seed_fixture_hosted(&pool, "fixture-host-a", "fixture-model-a", 12345).await;
            seed_fixture_hosted(&pool, "fixture-host-b", "fixture-model-b", 54321).await;

            assert_eq!(get_context_window(&pool, None, "llama3.2:3b").await, 4096);
            assert_eq!(get_context_window(&pool, None, "llama3.1:8b").await, 8192);
            assert_eq!(get_context_window(&pool, None, "qwen2.5:7b").await, 8192);
            assert_eq!(
                get_context_window(&pool, Some("fixture-host-a"), "fixture-model-a").await,
                12345
            );
            assert_eq!(
                get_context_window(&pool, Some("fixture-host-b"), "fixture-model-b").await,
                54321
            );
        })
        .await;
    }

    // -----------------------------------------------------------------------
    // items.id=709: provider row + model row + stored key -> hosted call.
    // Neutral fixtures only; loopback HTTP, no outside network.
    // -----------------------------------------------------------------------

    const FIXTURE_KEY_HEX_BYTE: u8 = 0x77;

    fn fixture_key_hex() -> String {
        crate::auth::registry::key_hex(&[FIXTURE_KEY_HEX_BYTE; crate::auth::kdf::MASTER_KEY_LEN])
    }

    /// Give a fixture provider row everything a hosted call needs and point
    /// it at `base_template`; give its default model a token limit.
    async fn make_fixture_callable(
        pool: &sqlx::SqlitePool,
        provider_id: &str,
        base_template: &str,
        user_variables: &str,
    ) {
        sqlx::query(
            "UPDATE providers SET api_dialect = 'openai_chat', base_url_template = ?,
                    auth_scheme = 'bearer', extra_headers = '{}', models_path = '/models',
                    user_variables = ?
             WHERE id = ?",
        )
        .bind(base_template)
        .bind(user_variables)
        .bind(provider_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE provider_models SET request_options = '{\"max_completion_tokens\":128}'
             WHERE provider_id = ?",
        )
        .bind(provider_id)
        .execute(pool)
        .await
        .unwrap();
    }

    fn loopback_chat_ok() -> crate::providers::loopback_test_server::Canned {
        crate::providers::loopback_test_server::Canned::json(
            200,
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":4,"completion_tokens":2}}"#,
        )
    }

    fn loopback_client() -> HostedClient {
        HostedClient::for_loopback_tests(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(2),
            1024 * 1024,
        )
    }

    async fn default_model(pool: &sqlx::SqlitePool, provider_id: &str) -> ProviderModel {
        provider_store::get_default_model(pool, provider_id)
            .await
            .unwrap()
            .expect("fixture model")
    }

    fn generate_request(prompt: &str) -> GenerateRequest {
        GenerateRequest {
            provider_id: Some("fixture-host-a".to_owned()),
            model_id: "fixture-model-a".to_owned(),
            prompt: prompt.to_owned(),
            images: None,
            task_type: "general".to_owned(),
            stream: Some(false),
            options: Some(GenerateOptions {
                temperature: 0.5,
                top_p: 0.9,
                num_ctx: 4096,
                num_predict: 1024,
            }),
        }
    }

    #[tokio::test]
    async fn hosted_call_uses_the_stored_key_and_the_rows() {
        use crate::providers::loopback_test_server::LoopbackServer;
        with_temp_shared_db(|pool| async move {
            seed_fixture_hosted(&pool, "fixture-host-a", "fixture-model-a", 4096).await;
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            make_fixture_callable(
                &pool,
                "fixture-host-a",
                &format!("{}/v1", server.base()),
                "[]",
            )
            .await;
            let key_hex = fixture_key_hex();
            integration_keys_store::upsert_key(
                "user-x",
                &key_hex,
                "fixture-host-a",
                QR_HOSTED_KEY_TYPE,
                "stored-key-value",
                None,
                Some("api_key"),
                None,
            )
            .await
            .unwrap();

            let model = default_model(&pool, "fixture-host-a").await;
            let inputs = resolve_hosted_inputs(&pool, "user-x", &key_hex, "fixture-host-a", &model)
                .await
                .unwrap();
            let call = CallContext::build_for_loopback(
                &inputs.route,
                &inputs.key.credential,
                &inputs.key.extra_metadata,
            )
            .unwrap();
            let request = hosted_request_from(
                &std::collections::HashMap::new(),
                &generate_request("hi"),
                &ReviewedPrompt::for_test("hi"),
            );
            let out = loopback_client().generate(&call, &request).await.unwrap();
            assert_eq!(out.response.content, "ok");

            let sent = server.captured();
            assert_eq!(
                sent[0].header("authorization"),
                Some("Bearer stored-key-value")
            );
            let body = sent[0].json();
            assert_eq!(body["model"], "fixture-model-a");
            // The executor's local default of 1024 is NOT forwarded: the row's
            // limit applies.
            assert_eq!(body["max_completion_tokens"], 128);
        })
        .await;
    }

    #[tokio::test]
    async fn hosted_call_fills_user_variables_from_the_key_records_metadata() {
        use crate::providers::loopback_test_server::LoopbackServer;
        use sqlx::ConnectOptions;
        with_temp_shared_db(|pool| async move {
            seed_fixture_hosted(&pool, "fixture-host-a", "fixture-model-a", 4096).await;
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            make_fixture_callable(
                &pool,
                "fixture-host-a",
                &format!("{}/accounts/{{account_id}}/v1", server.base()),
                r#"["account_id"]"#,
            )
            .await;
            let key_hex = fixture_key_hex();
            integration_keys_store::upsert_key(
                "user-y",
                &key_hex,
                "fixture-host-a",
                QR_HOSTED_KEY_TYPE,
                "stored-key-value",
                None,
                Some("api_key"),
                None,
            )
            .await
            .unwrap();
            // Nothing writes extra_metadata yet (items.id=714): set it directly.
            let mut conn = crate::providers::utils::connect_options_encrypted(
                &crate::providers::utils::db_path_integration_keys("user-y"),
                &crate::auth::subkeys::test_key_hex(
                    "user-y",
                    &key_hex,
                    crate::auth::subkeys::DbPurpose::IntegrationKeys,
                    crate::auth::subkeys::ACCOUNT_SCOPE,
                ),
            )
            .create_if_missing(false)
            .connect()
            .await
            .unwrap();
            sqlx::query("UPDATE integration_keys SET extra_metadata = ?")
                .bind(r#"{"account_id":"acct-42"}"#)
                .execute(&mut conn)
                .await
                .unwrap();
            drop(conn);

            let model = default_model(&pool, "fixture-host-a").await;
            let inputs = resolve_hosted_inputs(&pool, "user-y", &key_hex, "fixture-host-a", &model)
                .await
                .unwrap();
            let call = CallContext::build_for_loopback(
                &inputs.route,
                &inputs.key.credential,
                &inputs.key.extra_metadata,
            )
            .unwrap();
            let request = hosted_request_from(
                &std::collections::HashMap::new(),
                &generate_request("hi"),
                &ReviewedPrompt::for_test("hi"),
            );
            loopback_client().generate(&call, &request).await.unwrap();
            assert_eq!(
                server.captured()[0].path,
                "/accounts/acct-42/v1/chat/completions"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn hosted_call_without_a_stored_key_is_missing_api_key() {
        with_temp_shared_db(|pool| async move {
            seed_fixture_hosted(&pool, "fixture-host-a", "fixture-model-a", 4096).await;
            make_fixture_callable(&pool, "fixture-host-a", "https://api.example.test/v1", "[]")
                .await;
            let model = default_model(&pool, "fixture-host-a").await;

            let err = resolve_hosted_inputs(
                &pool,
                "user-none",
                &fixture_key_hex(),
                "fixture-host-a",
                &model,
            )
            .await
            .map(|_| ())
            .unwrap_err();
            match err {
                ProviderError::MissingApiKey { plain_language, .. } => {
                    assert!(plain_language.contains("Settings"), "{plain_language}");
                    assert!(!plain_language.to_lowercase().contains("environment"));
                }
                other => panic!("expected MissingApiKey, got {other:?}"),
            }

            // No unlocked session (empty key_hex) reads the same way.
            let err = resolve_hosted_inputs(&pool, "user-none", "", "fixture-host-a", &model)
                .await
                .map(|_| ())
                .unwrap_err();
            assert!(
                matches!(err, ProviderError::MissingApiKey { .. }),
                "{err:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn hosted_call_for_a_provider_with_no_row_is_unknown_provider() {
        with_temp_shared_db(|pool| async move {
            seed_fixture_hosted(&pool, "fixture-host-a", "fixture-model-a", 4096).await;
            let model = default_model(&pool, "fixture-host-a").await;
            let err = resolve_hosted_inputs(
                &pool,
                "user-x",
                &fixture_key_hex(),
                "fixture-absent",
                &model,
            )
            .await
            .map(|_| ())
            .unwrap_err();
            assert!(
                matches!(err, ProviderError::UnknownProvider { .. }),
                "{err:?}"
            );
            assert!(matches!(
                ConductorError::from(err),
                ConductorError::UnknownProvider { .. }
            ));
        })
        .await;
    }

    #[tokio::test]
    async fn a_retired_model_row_is_refused_before_any_request() {
        use crate::providers::loopback_test_server::LoopbackServer;
        with_temp_shared_db(|pool| async move {
            seed_fixture_hosted(&pool, "fixture-host-a", "fixture-model-a", 4096).await;
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            make_fixture_callable(
                &pool,
                "fixture-host-a",
                &format!("{}/v1", server.base()),
                "[]",
            )
            .await;
            sqlx::query("UPDATE provider_models SET status = 'retired'")
                .execute(&pool)
                .await
                .unwrap();
            let key_hex = fixture_key_hex();
            integration_keys_store::upsert_key(
                "user-z",
                &key_hex,
                "fixture-host-a",
                QR_HOSTED_KEY_TYPE,
                "k",
                None,
                Some("api_key"),
                None,
            )
            .await
            .unwrap();
            let model = default_model(&pool, "fixture-host-a").await;
            let inputs = resolve_hosted_inputs(&pool, "user-z", &key_hex, "fixture-host-a", &model)
                .await
                .unwrap();
            let err = inputs.build_call().unwrap_err();
            assert!(
                matches!(err, ProviderError::ModelNotFound { .. }),
                "{err:?}"
            );
            assert_eq!(server.connections(), 0);
        })
        .await;
    }

    #[test]
    fn hosted_request_forwards_a_budget_only_when_the_step_set_one() {
        let request = generate_request("p");
        let none = hosted_request_from(
            &std::collections::HashMap::new(),
            &request,
            &ReviewedPrompt::for_test(&request.prompt),
        );
        assert_eq!(none.max_output_tokens, None);
        assert_eq!(none.temperature, Some(0.5));
        assert_eq!(none.prompt, "p");
        assert!(none.system.is_none());

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("num_predict".to_owned(), serde_json::json!(777));
        let some = hosted_request_from(
            &overrides,
            &request,
            &ReviewedPrompt::for_test(&request.prompt),
        );
        assert_eq!(some.max_output_tokens, Some(777));

        // A value that is not a usable number is not a budget.
        overrides.insert("num_predict".to_owned(), serde_json::json!("lots"));
        assert_eq!(
            hosted_request_from(
                &overrides,
                &request,
                &ReviewedPrompt::for_test(&request.prompt)
            )
            .max_output_tokens,
            None
        );
    }

    /// No environment variable is a key source (items.id=709): the executor
    /// and the hosted client sources read none.
    #[test]
    fn no_hosted_code_path_reads_a_key_from_the_environment() {
        let needle_key = concat!("API", "_KEY");
        let executor = include_str!("executor.rs");
        assert!(!executor.contains(needle_key));
        for source in [
            include_str!("../providers/hosted_call.rs"),
            include_str!("../providers/hosted_client.rs"),
        ] {
            assert!(!source.contains(concat!("env", "::var")));
            assert!(!source.contains(needle_key));
        }
    }

    #[tokio::test]
    async fn context_window_unknown_model_defaults() {
        with_temp_shared_db(|pool| async move {
            assert_eq!(
                get_context_window(&pool, Some("unknown"), "model").await,
                2048
            );
        })
        .await;
    }

    #[test]
    fn collect_output_vars_filters_empty_key() {
        let steps = vec![
            TaskStep {
                action_id: "s1".to_owned(),
                output_var: Some("draft".to_owned()),
                content: "hello".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
                personal_flagged: false,
                audit_unavailable: false,
            },
            TaskStep {
                action_id: "s2".to_owned(),
                output_var: Some(String::new()),
                content: "world".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
                personal_flagged: false,
                audit_unavailable: false,
            },
        ];
        let vars = collect_output_vars(&steps);
        assert_eq!(vars.len(), 1);
        assert_eq!(vars.get("draft").map(|s| s.as_str()), Some("hello"));
        assert!(!vars.contains_key(""));
    }

    #[test]
    fn collect_output_vars_last_write_wins() {
        let steps = vec![
            TaskStep {
                action_id: "s1".to_owned(),
                output_var: Some("result".to_owned()),
                content: "first".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
                personal_flagged: false,
                audit_unavailable: false,
            },
            TaskStep {
                action_id: "s2".to_owned(),
                output_var: Some("result".to_owned()),
                content: "second".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
                personal_flagged: false,
                audit_unavailable: false,
            },
        ];
        let vars = collect_output_vars(&steps);
        assert_eq!(vars.get("result").map(|s| s.as_str()), Some("second"));
    }

    #[test]
    fn step_4_5_unset_preference_is_missing_config() {
        let err = missing_provider_error(ExternalAccess::Unrestricted, None, None)
            .expect("an external step with no provider must fail");
        match err {
            ConductorError::MissingQrHostedConfig { plain_language } => {
                assert_eq!(
                    plain_language,
                    "No Tier 1.5 AI provider is set up yet. Choose a provider in \
                     Settings to continue. [Open Settings] [Get help]"
                );
            }
            other => panic!("expected MissingQrHostedConfig, got {other:?}"),
        }
    }

    #[test]
    fn step_4_5_ceiling_excluded_provider_is_a_distinct_failure() {
        let err = missing_provider_error(
            ExternalAccess::AnonymousRequired,
            None,
            Some("Fixture Host"),
        )
        .expect("an excluded provider must fail");
        match err {
            ConductorError::ProviderExcludedByCeiling { plain_language } => {
                assert_eq!(
                    plain_language,
                    "This Focus is set to use only providers that don't require an account, \
                     so it can't use your hosted provider (Fixture Host). To use it, change this \
                     Focus's 'Maximum permitted tier' setting to allow providers that \
                     require an account. [Open Settings] [Get help]"
                );
                // Plain language only: no internal tier numbers.
                assert!(!plain_language.contains("Tier 1"));
                assert!(!plain_language.contains("Tier 2"));
            }
            other => panic!("expected ProviderExcludedByCeiling, got {other:?}"),
        }
    }

    #[test]
    fn step_4_5_passes_when_selected_or_local() {
        assert!(
            missing_provider_error(ExternalAccess::Unrestricted, Some("fixture-host-a"), None)
                .is_none()
        );
        assert!(missing_provider_error(ExternalAccess::LocalOnly, None, None).is_none());
        assert!(
            missing_provider_error(ExternalAccess::LocalOnly, None, Some("Fixture Host")).is_none()
        );
    }

    #[test]
    fn to_gate_track_converts_fields() {
        use crate::conductor::types::PersonalField as ConductorField;
        let mut ct = PersonalTrack::new();
        ct.add_field(ConductorField {
            field_name: "city".to_owned(),
            field_value: "Portland".to_owned(),
            sensitivity: "personal".to_owned(),
            sensitivity_severity: 2,
            source_id: "personal-specialist".to_owned(),
            abstraction_tier2: "pass".to_owned(),
            abstraction_tier3: "omit".to_owned(),
        })
        .unwrap();
        ct.seal();
        let gt = to_gate_track(&ct);
        assert!(gt.is_sealed());
        let gf = gt.fields().get("city").unwrap();
        assert_eq!(gf.field_name, "city");
        assert_eq!(gf.field_value, "Portland");
        assert_eq!(gf.sensitivity_severity, 2);
        assert!(matches!(gf.sensitivity, Sensitivity::Personal));
        assert!(matches!(gf.abstraction_tier2, AbstractionPolicy::Pass));
        assert!(matches!(gf.abstraction_tier3, AbstractionPolicy::Omit));
    }

    #[test]
    fn to_gate_track_unknown_sensitivity_defaults_general() {
        use crate::conductor::types::PersonalField as ConductorField;
        let mut ct = PersonalTrack::new();
        ct.add_field(ConductorField {
            field_name: "x".to_owned(),
            field_value: "y".to_owned(),
            sensitivity: "unknown_label".to_owned(),
            sensitivity_severity: 1,
            source_id: "s".to_owned(),
            abstraction_tier2: "pass".to_owned(),
            abstraction_tier3: "pass".to_owned(),
        })
        .unwrap();
        ct.seal();
        let gt = to_gate_track(&ct);
        let gf = gt.fields().get("x").unwrap();
        assert!(matches!(gf.sensitivity, Sensitivity::General));
    }

    #[test]
    fn step_sensitivity_tier2_uses_projected_fields() {
        use crate::conductor::tokens::{ActionDefinition, StepType};
        use crate::conductor::types::PersonalField as CF;
        let mut pt = PersonalTrack::new();
        pt.add_field(CF {
            field_name: "name".to_owned(),
            field_value: "Alice".to_owned(),
            sensitivity: "personal".to_owned(),
            sensitivity_severity: 2,
            source_id: "s".to_owned(),
            abstraction_tier2: "pass".to_owned(),
            abstraction_tier3: "omit".to_owned(),
        })
        .unwrap();
        pt.add_field(CF {
            field_name: "diagnosis".to_owned(),
            field_value: "X".to_owned(),
            sensitivity: "medical".to_owned(),
            sensitivity_severity: 3,
            source_id: "s".to_owned(),
            abstraction_tier2: "omit".to_owned(),
            abstraction_tier3: "omit".to_owned(),
        })
        .unwrap();
        pt.seal();
        let ctx = StepContext {
            stop: None,
            step: ActionDefinition {
                action_id: "s1".to_owned(),
                display_name: "Step 1".to_owned(),
                guide_id: "g".to_owned(),
                task_type: "general".to_owned(),
                routing_tier: ExternalAccess::AnonymousRequired,
                step_type: StepType::default(),
                output_var: None,
                prompt_template: "Hello {name}".to_owned(),
                field_requirements: std::collections::HashMap::new(),
                options_override: std::collections::HashMap::new(),
                external_access_override: Some(ExternalAccess::AnonymousRequired),
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
            },
            focus_id: "f".to_owned(),
            focus_run_id: "fr".to_owned(),
            user_input: "".to_owned(),
            external_input: "".to_owned(),
            persona_context: "".to_owned(),
            focus_external_access: ExternalAccess::AnonymousRequired,
            effective_access: ExternalAccess::AnonymousRequired,
            execution_tier: 2,
            abstraction_tier: 2,
            raw_abstraction: 1,
            floor_consent_preference: None,
            qr_hosted_provider_preference: None,
            qr_hosted_ceiling_excluded: None,
            next_execution_tier: None,
            retry_count: 0,
            focus_name: "test".to_owned(),
            user_id: "u".to_owned(),
            persona_id: "p".to_owned(),
            key_hex: String::new(),
            attendance: RunAttendance::Attended,
            stream: None,
            bound: None,
            store_tokens: Default::default(),
            plan: None,
        };
        let mut projected = HashMap::new();
        projected.insert("name".to_owned(), "Alice (abstracted)".to_owned());
        assert_eq!(
            compute_step_sensitivity(&ctx, &ordinary(&ctx), &pt, &projected),
            2
        );
    }

    #[test]
    fn step_sensitivity_tier1_uses_template_scan() {
        use crate::conductor::tokens::{ActionDefinition, StepType};
        use crate::conductor::types::PersonalField as CF;
        let mut pt = PersonalTrack::new();
        pt.add_field(CF {
            field_name: "name".to_owned(),
            field_value: "Alice".to_owned(),
            sensitivity: "personal".to_owned(),
            sensitivity_severity: 2,
            source_id: "s".to_owned(),
            abstraction_tier2: "pass".to_owned(),
            abstraction_tier3: "omit".to_owned(),
        })
        .unwrap();
        pt.seal();
        let ctx = StepContext {
            stop: None,
            step: ActionDefinition {
                action_id: "s1".to_owned(),
                display_name: "Step 1".to_owned(),
                guide_id: "g".to_owned(),
                task_type: "general".to_owned(),
                routing_tier: ExternalAccess::LocalOnly,
                step_type: StepType::default(),
                output_var: None,
                prompt_template: "Hello {name}".to_owned(),
                field_requirements: std::collections::HashMap::new(),
                options_override: std::collections::HashMap::new(),
                external_access_override: Some(ExternalAccess::LocalOnly),
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
            },
            focus_id: "f".to_owned(),
            focus_run_id: "fr".to_owned(),
            user_input: "".to_owned(),
            external_input: "".to_owned(),
            persona_context: "".to_owned(),
            focus_external_access: ExternalAccess::LocalOnly,
            effective_access: ExternalAccess::LocalOnly,
            execution_tier: 1,
            abstraction_tier: 1,
            raw_abstraction: 1,
            floor_consent_preference: None,
            qr_hosted_provider_preference: None,
            qr_hosted_ceiling_excluded: None,
            next_execution_tier: None,
            retry_count: 0,
            focus_name: "test".to_owned(),
            user_id: "u".to_owned(),
            persona_id: "p".to_owned(),
            key_hex: String::new(),
            attendance: RunAttendance::Attended,
            stream: None,
            bound: None,
            store_tokens: Default::default(),
            plan: None,
        };
        assert_eq!(
            compute_step_sensitivity(&ctx, &ordinary(&ctx), &pt, &HashMap::new()),
            2
        );
    }

    #[test]
    fn step_sensitivity_no_personal_fields_is_1() {
        use crate::conductor::tokens::{ActionDefinition, StepType};
        let pt = PersonalTrack::new();
        let ctx = StepContext {
            stop: None,
            step: ActionDefinition {
                action_id: "s1".to_owned(),
                display_name: "Step 1".to_owned(),
                guide_id: "g".to_owned(),
                task_type: "general".to_owned(),
                routing_tier: ExternalAccess::LocalOnly,
                step_type: StepType::default(),
                output_var: None,
                prompt_template: "Hello {user_input}".to_owned(),
                field_requirements: std::collections::HashMap::new(),
                options_override: std::collections::HashMap::new(),
                external_access_override: Some(ExternalAccess::LocalOnly),
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
            },
            focus_id: "f".to_owned(),
            focus_run_id: "fr".to_owned(),
            user_input: "".to_owned(),
            external_input: "".to_owned(),
            persona_context: "".to_owned(),
            focus_external_access: ExternalAccess::LocalOnly,
            effective_access: ExternalAccess::LocalOnly,
            execution_tier: 1,
            abstraction_tier: 1,
            raw_abstraction: 1,
            floor_consent_preference: None,
            qr_hosted_provider_preference: None,
            qr_hosted_ceiling_excluded: None,
            next_execution_tier: None,
            retry_count: 0,
            focus_name: "test".to_owned(),
            user_id: "u".to_owned(),
            persona_id: "p".to_owned(),
            key_hex: String::new(),
            attendance: RunAttendance::Attended,
            stream: None,
            bound: None,
            store_tokens: Default::default(),
            plan: None,
        };
        assert_eq!(
            compute_step_sensitivity(&ctx, &ordinary(&ctx), &pt, &HashMap::new()),
            1
        );
    }

    // -----------------------------------------------------------------------
    // Spend gate in the hosted branch (items.id=710). Neutral fixtures, a
    // loopback server, no live calls. The invariant under test: nothing
    // reaches the network unless the pre-flight clears.
    // -----------------------------------------------------------------------

    use crate::persistence::provider_spend_store as spend_store;
    use crate::providers::loopback_test_server::{Canned, LoopbackServer};
    use crate::spend::disclosure::attributes_hash;

    const SPEND_USER: &str = "user-spend";
    const PRICE_TEXT: &str =
        r#"{"input_per_mtok":0.50,"output_per_mtok":1.50,"as_of":"2026-10-11","source":"curated"}"#;

    fn hosted_ctx(attendance: RunAttendance, key_hex: &str) -> StepContext {
        use crate::conductor::tokens::{ActionDefinition, StepType};
        StepContext {
            stop: None,
            step: ActionDefinition {
                action_id: "s1".to_owned(),
                display_name: "Step 1".to_owned(),
                guide_id: "g".to_owned(),
                task_type: "general".to_owned(),
                routing_tier: ExternalAccess::AnonymousRequired,
                step_type: StepType::default(),
                output_var: None,
                prompt_template: "Hello".to_owned(),
                field_requirements: HashMap::new(),
                options_override: HashMap::new(),
                external_access_override: Some(ExternalAccess::AnonymousRequired),
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
            },
            focus_id: "focus-1".to_owned(),
            focus_run_id: "run-1".to_owned(),
            user_input: String::new(),
            external_input: String::new(),
            persona_context: String::new(),
            focus_external_access: ExternalAccess::AnonymousRequired,
            effective_access: ExternalAccess::AnonymousRequired,
            execution_tier: 2,
            abstraction_tier: 2,
            raw_abstraction: 1,
            floor_consent_preference: None,
            qr_hosted_provider_preference: Some("fixture-host-a".to_owned()),
            qr_hosted_ceiling_excluded: None,
            next_execution_tier: None,
            retry_count: 0,
            focus_name: "test".to_owned(),
            user_id: SPEND_USER.to_owned(),
            persona_id: "persona-1".to_owned(),
            key_hex: key_hex.to_owned(),
            attendance,
            stream: None,
            bound: None,
            store_tokens: Default::default(),
            plan: None,
        }
    }

    /// A callable fixture host priced at `price` (None = unpriced), with a
    /// stored key, pointed at `server`.
    async fn spend_rig(
        pool: &sqlx::SqlitePool,
        server: &LoopbackServer,
        price: Option<&str>,
    ) -> (String, ProviderModel) {
        seed_fixture_hosted(pool, "fixture-host-a", "fixture-model-a", 4096).await;
        make_fixture_callable(
            pool,
            "fixture-host-a",
            &format!("{}/v1", server.base()),
            "[]",
        )
        .await;
        if let Some(p) = price {
            sqlx::query(
                "UPDATE provider_models SET pricing = ? WHERE provider_id = 'fixture-host-a'",
            )
            .bind(p)
            .execute(pool)
            .await
            .unwrap();
        }
        let key_hex = fixture_key_hex();
        integration_keys_store::upsert_key(
            SPEND_USER,
            &key_hex,
            "fixture-host-a",
            QR_HOSTED_KEY_TYPE,
            "stored-key-value",
            None,
            Some("api_key"),
            None,
        )
        .await
        .unwrap();
        let model = default_model(pool, "fixture-host-a").await;
        (key_hex, model)
    }

    async fn grant_consent(pool: &sqlx::SqlitePool, key_hex: &str, no_cap: bool) {
        let provider = provider_store::get_provider(pool, "fixture-host-a")
            .await
            .unwrap()
            .unwrap();
        let models = provider_store::list_models_for_provider(pool, "fixture-host-a")
            .await
            .unwrap();
        let texts = provider_store::list_model_pricing_text(pool, "fixture-host-a")
            .await
            .unwrap();
        let hash = attributes_hash(&provider, &models, &texts);
        let mut conn = spend_store::open(SPEND_USER, key_hex).await.unwrap();
        spend_store::insert_consent_conn(
            &mut conn,
            "fixture-host-a",
            &hash,
            no_cap,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    }

    async fn set_cost_limit(
        key_hex: &str,
        period: spend_store::LimitPeriod,
        value: i64,
        approves_unattended: bool,
    ) {
        let mut conn = spend_store::open(SPEND_USER, key_hex).await.unwrap();
        spend_store::upsert_limit_conn(
            &mut conn,
            &spend_store::LimitSpec {
                metric: spend_store::LimitMetric::CostUsd,
                period,
                scope_kind: spend_store::ScopeKind::All,
                scope_id: None,
                limit_value: value,
                warn_at_pct: 80,
                approves_unattended,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    }

    fn loopback_build(i: &HostedInputs) -> Result<CallContext, ProviderError> {
        CallContext::build_for_loopback(&i.route, &i.key.credential, &i.key.extra_metadata)
    }

    async fn call_hosted(
        pool: &sqlx::SqlitePool,
        key_hex: &str,
        model: &ProviderModel,
        attendance: RunAttendance,
    ) -> Result<GenerateResponse, ConductorError> {
        hosted_step_call(
            &loopback_client(),
            pool,
            &hosted_ctx(attendance, key_hex),
            "fixture-host-a",
            model,
            &generate_request("hello there"),
            &ReviewedPrompt::for_test("hello there"),
            &chrono::Utc,
            loopback_build,
            None,
        )
        .await
    }

    fn block_code(e: &ConductorError) -> String {
        match e {
            ConductorError::SpendBlock { params, .. } => {
                params["code"].as_str().unwrap_or("<no code>").to_owned()
            }
            other => panic!("expected a spend block, got {other:?}"),
        }
    }

    async fn ledger_count(key_hex: &str) -> i64 {
        let mut conn = spend_store::open(SPEND_USER, key_hex).await.unwrap();
        sqlx::query_scalar("SELECT COUNT(*) FROM provider_usage_ledger")
            .fetch_one(&mut conn)
            .await
            .unwrap()
    }

    // -- items.id=711: the stored model selection reaches the call -------------

    use crate::persistence::provider_models_store::{
        record_refresh_conn, set_user_output_limit_conn,
    };
    use crate::persistence::user_provider_preference_store::{set_model_selection, SelectionScope};
    use crate::providers::hosted_client::LiveModel;

    async fn seed_user(pool: &sqlx::SqlitePool) {
        sqlx::query(
            "INSERT INTO users (id, display_name, role, is_primary, auth_enabled, created_at) \
             VALUES (?, 'Fixture', 'user', 1, 1, '2026-01-01T00:00:00Z')",
        )
        .bind(SPEND_USER)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn add_curated_model(
        pool: &sqlx::SqlitePool,
        model_id: &str,
        options: &str,
        status: &str,
    ) {
        sqlx::query(
            "INSERT INTO provider_models \
                (id, provider_id, model_id, context_window_tokens, is_default, created_at, \
                 pricing, request_options, status) \
             VALUES (?, 'fixture-host-a', ?, 4096, 0, 't', ?, ?, ?)",
        )
        .bind(format!("fixture-host-a:{model_id}"))
        .bind(model_id)
        .bind(PRICE_TEXT)
        .bind(options)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn select_for_step(
        pool: &sqlx::SqlitePool,
        key_hex: &str,
    ) -> Result<SelectedModel, ConductorError> {
        select_model(
            pool,
            "general",
            ExternalAccess::AnonymousRequired,
            Some("fixture-host-a"),
            Some(SelectionLookup {
                user_id: SPEND_USER,
                key_hex,
                persona_id: "persona-1",
                focus_id: "focus-1",
            }),
        )
        .await
    }

    async fn pick_account_model(key_hex: &str, model_id: Option<&str>) {
        let mut conn = spend_store::open(SPEND_USER, key_hex).await.unwrap();
        set_model_selection(
            &mut conn,
            SPEND_USER,
            &SelectionScope::Account,
            None,
            "fixture-host-a",
            model_id,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn the_stored_selection_decides_which_model_is_sent() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, _default) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            seed_user(&pool).await;
            add_curated_model(
                &pool,
                "fixture-model-b",
                r#"{"max_completion_tokens":64}"#,
                "active",
            )
            .await;
            grant_consent(&pool, &key_hex, true).await;

            // No selection: the provider's default.
            let picked = select_for_step(&pool, &key_hex).await.unwrap();
            assert_eq!(picked.model_id, "fixture-model-a");

            pick_account_model(&key_hex, Some("fixture-model-b")).await;
            let picked = select_for_step(&pool, &key_hex).await.unwrap();
            assert_eq!(picked.model_id, "fixture-model-b");
            let model = picked.hosted_model.expect("a hosted row");

            call_hosted(&pool, &key_hex, &model, RunAttendance::Attended)
                .await
                .unwrap();
            let sent = server.captured()[0].json();
            assert_eq!(sent["model"], "fixture-model-b");
            assert_eq!(sent["max_completion_tokens"], 64);
            let mut conn = spend_store::open(SPEND_USER, &key_hex).await.unwrap();
            let ledger_model: String =
                sqlx::query_scalar("SELECT model_id FROM provider_usage_ledger")
                    .fetch_one(&mut conn)
                    .await
                    .unwrap();
            assert_eq!(ledger_model, "fixture-model-b");
        })
        .await;
    }

    // -- items.id=716: a bound API Chat run uses exactly its bound model ---------

    fn bound_to(model_id: &str) -> crate::api_chat::BoundDestination {
        crate::api_chat::BoundDestination {
            provider_id: "fixture-host-a".to_owned(),
            model_id: model_id.to_owned(),
        }
    }

    async fn call_bound(
        pool: &sqlx::SqlitePool,
        key_hex: &str,
        bound: &crate::api_chat::BoundDestination,
    ) -> Result<GenerateResponse, ConductorError> {
        let mut ctx = hosted_ctx(RunAttendance::Attended, key_hex);
        ctx.bound = Some(bound.clone());
        let picked = select_bound_model(pool, &ctx, bound).await?;
        let model = picked.hosted_model.expect("a hosted row");
        hosted_step_call(
            &loopback_client(),
            pool,
            &ctx,
            "fixture-host-a",
            &model,
            &generate_request("hello there"),
            &ReviewedPrompt::for_test("hello there"),
            &chrono::Utc,
            loopback_build,
            None,
        )
        .await
    }

    #[tokio::test]
    async fn a_bound_run_sends_its_bound_model_not_the_stored_selection() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, _default) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            seed_user(&pool).await;
            add_curated_model(
                &pool,
                "fixture-model-b",
                r#"{"max_completion_tokens":64}"#,
                "active",
            )
            .await;
            add_curated_model(
                &pool,
                "fixture-model-c",
                r#"{"max_completion_tokens":65}"#,
                "active",
            )
            .await;
            grant_consent(&pool, &key_hex, true).await;
            // The user's stored selection says model-c; the conversation is
            // bound to model-b. The binding wins, and the ledger agrees.
            pick_account_model(&key_hex, Some("fixture-model-c")).await;

            call_bound(&pool, &key_hex, &bound_to("fixture-model-b"))
                .await
                .unwrap();
            let sent = server.captured()[0].json();
            assert_eq!(sent["model"], "fixture-model-b");
            assert_eq!(sent["max_completion_tokens"], 64);
            let mut conn = spend_store::open(SPEND_USER, &key_hex).await.unwrap();
            let ledger_model: String =
                sqlx::query_scalar("SELECT model_id FROM provider_usage_ledger")
                    .fetch_one(&mut conn)
                    .await
                    .unwrap();
            assert_eq!(ledger_model, "fixture-model-b");
        })
        .await;
    }

    #[tokio::test]
    async fn a_bound_run_goes_through_the_unchanged_consent_gate() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, _default) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            seed_user(&pool).await;
            add_curated_model(
                &pool,
                "fixture-model-b",
                r#"{"max_completion_tokens":64}"#,
                "active",
            )
            .await;
            // No consent was ever given for this provider.
            let err = call_bound(&pool, &key_hex, &bound_to("fixture-model-b"))
                .await
                .unwrap_err();
            assert_eq!(block_code(&err), "spend_block.consent_required");
            assert_eq!(server.connections(), 0, "nothing reached the network");
            assert_eq!(ledger_count(&key_hex).await, 0);
        })
        .await;
    }

    #[tokio::test]
    async fn a_bound_model_that_is_retired_or_gone_stops_and_never_uses_the_default() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, _default) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            seed_user(&pool).await;
            add_curated_model(
                &pool,
                "fixture-model-old",
                r#"{"max_completion_tokens":64}"#,
                "retired",
            )
            .await;
            grant_consent(&pool, &key_hex, true).await;

            let err = call_bound(&pool, &key_hex, &bound_to("fixture-model-old"))
                .await
                .unwrap_err();
            assert_eq!(block_code(&err), "spend_block.model_retired");
            let err = call_bound(&pool, &key_hex, &bound_to("fixture-model-ghost"))
                .await
                .unwrap_err();
            assert_eq!(block_code(&err), "spend_block.model_unavailable");
            assert_eq!(server.connections(), 0);
        })
        .await;
    }

    // -- items.id=716: what the user saw is kept; a cut-off answer says so ------

    const PARTIAL_KEY: &str = "0606060606060606060606060606060606060606060606060606060606060606";

    async fn placeholder(context_key: &str, run_id: &str) {
        crate::persistence::message_store::save_message(
            SPEND_USER,
            "persona-1",
            PARTIAL_KEY,
            context_key,
            "assistant",
            "",
            Some(run_id),
        )
        .await
        .unwrap();
    }

    async fn reply_of(run_id: &str) -> crate::persistence::message_store::MessageRecord {
        crate::persistence::message_store::find_assistant_message_by_focus_run_id(
            SPEND_USER,
            "persona-1",
            PARTIAL_KEY,
            run_id,
        )
        .await
        .unwrap()
        .unwrap()
    }

    #[tokio::test]
    async fn a_cancelled_or_cut_stream_keeps_the_text_the_user_saw_marked_with_how_it_ended() {
        use crate::persistence::message_store::ReplyStatus;
        with_temp_shared_db(|_pool| async move {
            placeholder("chat-a", "run-cancel").await;
            placeholder("chat-a", "run-cut").await;
            placeholder("chat-a", "run-empty").await;

            let mut ctx = hosted_ctx(RunAttendance::Attended, PARTIAL_KEY);
            ctx.focus_run_id = "run-cancel".to_owned();
            keep_partial_reply(
                &ctx,
                &StreamTail {
                    received_text: "half an answ".to_owned(),
                    cancelled: true,
                },
                None,
            )
            .await;
            let r = reply_of("run-cancel").await;
            assert_eq!(
                (r.content.as_str(), r.reply_status),
                ("half an answ", ReplyStatus::Cancelled)
            );

            ctx.focus_run_id = "run-cut".to_owned();
            keep_partial_reply(
                &ctx,
                &StreamTail {
                    received_text: "the first part".to_owned(),
                    cancelled: false,
                },
                None,
            )
            .await;
            let r = reply_of("run-cut").await;
            assert_eq!(
                (r.content.as_str(), r.reply_status),
                ("the first part", ReplyStatus::Cut)
            );

            // Nothing was shown, nothing is kept.
            ctx.focus_run_id = "run-empty".to_owned();
            keep_partial_reply(
                &ctx,
                &StreamTail {
                    received_text: String::new(),
                    cancelled: true,
                },
                None,
            )
            .await;
            let r = reply_of("run-empty").await;
            assert_eq!(
                (r.content.as_str(), r.reply_status),
                ("", ReplyStatus::Complete)
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_truncated_reply_is_marked_and_a_retried_one_does_not_keep_an_old_cut_mark() {
        use crate::persistence::message_store::ReplyStatus;
        with_temp_shared_db(|_pool| async move {
            placeholder("chat-b", "run-trunc").await;
            placeholder("chat-b", "run-retry").await;
            placeholder("chat-b", "run-plain").await;
            let response = |status| GenerateResponse {
                content: "text".to_owned(),
                model: "m".to_owned(),
                prompt_token_count: 1,
                output_token_count: 1,
                latency_ms: 1.0,
                completion_status: status,
            };

            // Truncated by the output limit: marked, whatever kind of run.
            let mut ctx = hosted_ctx(RunAttendance::Attended, PARTIAL_KEY);
            ctx.focus_run_id = "run-trunc".to_owned();
            record_completed_reply(&ctx, &response(CompletionStatus::Truncated), None).await;
            assert_eq!(
                reply_of("run-trunc").await.reply_status,
                ReplyStatus::Truncated
            );

            // An earlier attempt was cut, the retry completed: the mark is reset.
            ctx.focus_run_id = "run-retry".to_owned();
            ctx.bound = Some(bound_to("fixture-model-a"));
            crate::persistence::message_store::set_reply_outcome(
                SPEND_USER,
                "persona-1",
                PARTIAL_KEY,
                "run-retry",
                ReplyStatus::Cut,
                Some("old"),
            )
            .await
            .unwrap();
            record_completed_reply(&ctx, &response(CompletionStatus::Complete), None).await;
            assert_eq!(
                reply_of("run-retry").await.reply_status,
                ReplyStatus::Complete
            );

            // A run that is neither bound, streamed nor truncated is left alone.
            ctx.focus_run_id = "run-plain".to_owned();
            ctx.bound = None;
            crate::persistence::message_store::set_reply_outcome(
                SPEND_USER,
                "persona-1",
                PARTIAL_KEY,
                "run-plain",
                ReplyStatus::Cut,
                Some("x"),
            )
            .await
            .unwrap();
            record_completed_reply(&ctx, &response(CompletionStatus::Complete), None).await;
            assert_eq!(reply_of("run-plain").await.reply_status, ReplyStatus::Cut);
        })
        .await;
    }

    /// The bound-run guard has to run BEFORE step 4.5: missing_provider_error
    /// treats a LocalOnly step as needing no provider, which for a bound run
    /// would let it fall through to the local model. A source scan, because
    /// execute_once has no unit harness; the guard's rules are tested in
    /// api_chat::tests.
    #[test]
    fn the_bound_guard_runs_before_the_provider_gate_and_before_the_local_branch() {
        let src = include_str!("executor.rs");
        // Only the production part of the file.
        let prod = &src[..src.find("#[cfg(test)]").expect("test module marker")];
        let guard = prod
            .find("crate::api_chat::bound_run_stop(")
            .expect("the bound guard call");
        let gate = prod
            .find("if let Some(error) = missing_provider_error(")
            .expect("the 4.5 gate");
        let local = prod
            .find("ollama_client().generate(&request)")
            .expect("the local branch");
        assert!(guard < gate, "the guard must precede step 4.5");
        assert!(gate < local);
    }

    #[tokio::test]
    async fn a_selection_that_cannot_be_honored_stops_and_never_uses_the_default() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, _default) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            seed_user(&pool).await;
            add_curated_model(
                &pool,
                "fixture-model-old",
                r#"{"max_completion_tokens":64}"#,
                "retired",
            )
            .await;

            pick_account_model(&key_hex, Some("fixture-model-old")).await;
            let err = select_for_step(&pool, &key_hex).await.unwrap_err();
            assert_eq!(block_code(&err), "spend_block.model_retired");

            pick_account_model(&key_hex, Some("fixture-model-ghost")).await;
            let err = select_for_step(&pool, &key_hex).await.unwrap_err();
            assert_eq!(block_code(&err), "spend_block.model_unavailable");

            assert_eq!(server.connections(), 0);
        })
        .await;
    }

    #[tokio::test]
    async fn an_uncurated_model_needs_a_price_and_carries_the_providers_required_options() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, _default) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            seed_user(&pool).await;
            sqlx::query(
                "UPDATE providers SET required_request_options = '{\"fixture_routing\":{\"fixture_collect\":\"deny\"}}' \
                 WHERE id = 'fixture-host-a'",
            )
            .execute(&pool)
            .await
            .unwrap();
            grant_consent(&pool, &key_hex, false).await;
            set_cost_limit(&key_hex, spend_store::LimitPeriod::Month, 5_000_000, false).await;

            // The user refreshed their list, picked a model QR has not
            // reviewed and chose its output limit.
            let mut conn = spend_store::open(SPEND_USER, &key_hex).await.unwrap();
            record_refresh_conn(
                &mut conn,
                "fixture-host-a",
                &[LiveModel { id: "live-x".into(), context_tokens: Some(9000), retires_at: None }],
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            set_user_output_limit_conn(&mut conn, "fixture-host-a", "live-x", Some(77)).await.unwrap();
            pick_account_model(&key_hex, Some("live-x")).await;

            let picked = select_for_step(&pool, &key_hex).await.unwrap();
            assert_eq!(picked.model_id, "live-x");
            let model = picked.hosted_model.expect("a synthesized row");
            assert!(model.pricing.is_none());

            // Unpriced: it stops and asks, and nothing leaves the machine.
            let err = call_hosted(&pool, &key_hex, &model, RunAttendance::Attended).await.unwrap_err();
            assert_eq!(block_code(&err), "spend_block.price_unknown");
            assert_eq!(server.connections(), 0);

            // The user prices it. Now it goes out: the model, the limit the
            // user chose (under the key the provider's default row uses) and
            // the provider-wide required option all ride on the request.
            sqlx::query("INSERT INTO price_overrides (provider_id, model_id, pricing, updated_at) VALUES ('fixture-host-a', 'live-x', ?, 't')")
                .bind(r#"{"input_per_mtok":1,"output_per_mtok":2,"as_of":"2026-10-13","source":"user"}"#)
                .execute(&mut conn)
                .await
                .unwrap();
            call_hosted(&pool, &key_hex, &model, RunAttendance::Attended).await.unwrap();
            let sent = server.captured()[0].json();
            assert_eq!(sent["model"], "live-x");
            assert_eq!(sent["max_completion_tokens"], 77);
            assert_eq!(sent["fixture_routing"]["fixture_collect"], "deny");
            let (source, model_id): (String, String) = sqlx::query_as("SELECT price_source, model_id FROM provider_usage_ledger")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!((source.as_str(), model_id.as_str()), ("user", "live-x"));
        })
        .await;
    }

    #[tokio::test]
    async fn a_model_row_that_weakens_the_required_option_never_reaches_the_network() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, _default) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            seed_user(&pool).await;
            sqlx::query(
                "UPDATE providers SET required_request_options = '{\"fixture_routing\":{\"fixture_collect\":\"deny\"}}' \
                 WHERE id = 'fixture-host-a'",
            )
            .execute(&pool)
            .await
            .unwrap();
            add_curated_model(
                &pool,
                "fixture-model-weak",
                r#"{"max_completion_tokens":64,"fixture_routing":{"fixture_collect":"allow"}}"#,
                "active",
            )
            .await;
            grant_consent(&pool, &key_hex, true).await;
            pick_account_model(&key_hex, Some("fixture-model-weak")).await;

            let picked = select_for_step(&pool, &key_hex).await.unwrap();
            let model = picked.hosted_model.expect("a hosted row");
            let err = call_hosted(&pool, &key_hex, &model, RunAttendance::Attended).await.unwrap_err();
            assert!(
                !matches!(err, ConductorError::SpendBlock { .. }),
                "the request builder refuses before the spend gate: {err:?}"
            );
            assert_eq!(server.connections(), 0);
            assert_eq!(ledger_count(&key_hex).await, 0);
        })
        .await;
    }

    #[tokio::test]
    async fn without_consent_nothing_reaches_the_network() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            let err = call_hosted(&pool, &key_hex, &model, RunAttendance::Attended)
                .await
                .unwrap_err();
            assert_eq!(block_code(&err), "spend_block.consent_required");
            assert_eq!(server.connections(), 0, "no request may be made");
            assert_eq!(ledger_count(&key_hex).await, 0);
        })
        .await;
    }

    #[tokio::test]
    async fn an_unpriced_model_never_reaches_the_network() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, model) = spend_rig(&pool, &server, None).await;
            grant_consent(&pool, &key_hex, true).await;
            let err = call_hosted(&pool, &key_hex, &model, RunAttendance::Attended)
                .await
                .unwrap_err();
            assert_eq!(block_code(&err), "spend_block.price_unknown");
            assert_eq!(server.connections(), 0);
        })
        .await;
    }

    #[tokio::test]
    async fn a_priced_model_without_a_cap_never_reaches_the_network() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, false).await;
            let err = call_hosted(&pool, &key_hex, &model, RunAttendance::Attended)
                .await
                .unwrap_err();
            assert_eq!(block_code(&err), "spend_block.cap_required");
            assert_eq!(server.connections(), 0);
        })
        .await;
    }

    #[tokio::test]
    async fn a_cleared_call_goes_out_and_is_settled_in_the_ledger() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, false).await;
            set_cost_limit(&key_hex, spend_store::LimitPeriod::Month, 5_000_000, false).await;

            let out = call_hosted(&pool, &key_hex, &model, RunAttendance::Attended)
                .await
                .expect("a cleared call succeeds");
            assert_eq!(out.content, "ok");
            assert_eq!(server.connections(), 1);

            use sqlx::Row;
            let mut conn = spend_store::open(SPEND_USER, &key_hex).await.unwrap();
            let row = sqlx::query("SELECT outcome, usage_source, tokens_in, tokens_out, cost_micros, reserved_micros, run_id, persona_id, focus_id, provider_id, model_id FROM provider_usage_ledger")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(row.get::<String, _>("outcome"), "ok");
            assert_eq!(row.get::<String, _>("usage_source"), "reported");
            // The loopback reply reports 4 prompt and 2 completion tokens:
            // ceil(4 x 0.5) + ceil(2 x 1.5) = 2 + 3.
            assert_eq!(row.get::<i64, _>("tokens_in"), 4);
            assert_eq!(row.get::<i64, _>("tokens_out"), 2);
            assert_eq!(row.get::<i64, _>("cost_micros"), 5);
            assert!(row.get::<i64, _>("reserved_micros") > 5, "the worst case was reserved");
            assert_eq!(row.get::<String, _>("run_id"), "run-1");
            assert_eq!(row.get::<String, _>("persona_id"), "persona-1");
            assert_eq!(row.get::<String, _>("focus_id"), "focus-1");
            assert_eq!(row.get::<String, _>("provider_id"), "fixture-host-a");
            assert_eq!(row.get::<String, _>("model_id"), "fixture-model-a");
        })
        .await;
    }

    /// The cap is computed from the limit that is actually sent: the request
    /// body carries the model row's limit, and the reservation holds that
    /// many output tokens.
    #[tokio::test]
    async fn the_reservation_holds_the_output_limit_that_is_sent() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, true).await;
            call_hosted(&pool, &key_hex, &model, RunAttendance::Attended)
                .await
                .unwrap();
            assert_eq!(server.captured()[0].json()["max_completion_tokens"], 128);
            let mut conn = spend_store::open(SPEND_USER, &key_hex).await.unwrap();
            let reserved: i64 =
                sqlx::query_scalar("SELECT reserved_micros FROM provider_usage_ledger")
                    .fetch_one(&mut conn)
                    .await
                    .unwrap();
            // 128 output tokens x 1.5 = 192, plus the input estimate.
            assert!(reserved >= 192, "reserved {reserved}");
        })
        .await;
    }

    /// The scope item 2 case end to end: a call that burned its whole output
    /// budget and returned an error is charged for the tokens.
    #[tokio::test]
    async fn a_budget_exhausted_call_is_charged_in_the_ledger() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![Canned::json(
                200,
                r#"{"choices":[{"message":{"content":""},"finish_reason":"length"}],
                    "usage":{"prompt_tokens":10,"completion_tokens":128,
                             "completion_tokens_details":{"reasoning_tokens":128}}}"#,
            )]);
            let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, true).await;
            let err = call_hosted(&pool, &key_hex, &model, RunAttendance::Attended)
                .await
                .unwrap_err();
            assert!(matches!(err, ConductorError::Provider { .. }));
            use sqlx::Row;
            let mut conn = spend_store::open(SPEND_USER, &key_hex).await.unwrap();
            let row = sqlx::query("SELECT outcome, tokens_out, reasoning_tokens, cost_micros FROM provider_usage_ledger")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(row.get::<String, _>("outcome"), "budget_exhausted");
            assert_eq!(row.get::<i64, _>("tokens_out"), 128);
            assert_eq!(row.get::<Option<i64>, _>("reasoning_tokens"), Some(128));
            // 10 x 0.5 -> 5, 128 x 1.5 -> 192.
            assert_eq!(row.get::<i64, _>("cost_micros"), 197);
        })
        .await;
    }

    #[tokio::test]
    async fn an_unattended_step_parks_without_an_approved_budget() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, true).await;
            let err = call_hosted(&pool, &key_hex, &model, RunAttendance::Unattended)
                .await
                .unwrap_err();
            assert_eq!(block_code(&err), "spend_block.budget_approval_required");
            assert_eq!(server.connections(), 0, "a parked run sends nothing");

            // The block parks the run (not retried, not failed) and carries
            // the estimate and what is missing, as data.
            let handler = FailureHandler::new(ExternalAccess::AnonymousRequired);
            let r = handler.handle(&err, Some("s1"), Some("focus-1"), 0);
            assert_eq!(r.action, FailureAction::AwaitUser);
            let m = r.metadata.unwrap();
            assert_eq!(m["code"], "spend_block.budget_approval_required");
            assert_eq!(m["missing_approval"], "per_run_cost_limit");
            assert!(m["estimate_micros"].as_i64().unwrap() > 0);

            // The same step attended goes out.
            call_hosted(&pool, &key_hex, &model, RunAttendance::Attended)
                .await
                .expect("attended runs are not subject to the run-budget rule");
            assert_eq!(server.connections(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn an_unattended_step_spends_within_an_approved_budget_only() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, true).await;
            // A budget of 400 micro-USD: the worst case for one call (128
            // output tokens, about 200 micro) fits once, not twice.
            set_cost_limit(&key_hex, spend_store::LimitPeriod::Run, 400, true).await;
            call_hosted(&pool, &key_hex, &model, RunAttendance::Unattended)
                .await
                .expect("within the approved budget");
            // The first call settled at its actual 5 micro, so there is room
            // for another; exhaust it by reserving past the budget instead.
            let mut conn = spend_store::open(SPEND_USER, &key_hex).await.unwrap();
            spend_store::insert_reservation_conn(
                &mut conn,
                &spend_store::NewReservation {
                    provider_id: "fixture-host-a",
                    model_id: "fixture-model-a",
                    persona_id: Some("persona-1"),
                    focus_id: Some("focus-1"),
                    run_id: Some("run-1"),
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_micros: 300,
                    price_source: "curated",
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            drop(conn);
            let before = server.connections();
            let err = call_hosted(&pool, &key_hex, &model, RunAttendance::Unattended)
                .await
                .unwrap_err();
            assert_eq!(block_code(&err), "spend_block.limit_reached");
            assert_eq!(
                server.connections(),
                before,
                "an exhausted budget sends nothing"
            );
        })
        .await;
    }

    /// Each attempt is one request. A 429 then a 200 (what the retry loop
    /// does: re-run the step) leaves a failed row at no cost and an ok row.
    #[tokio::test]
    async fn every_retry_counts_as_a_request_and_a_429_costs_nothing() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![
                Canned::json(429, "{}").with_header("Retry-After", "2"),
                loopback_chat_ok(),
            ]);
            let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, true).await;

            let err = call_hosted(&pool, &key_hex, &model, RunAttendance::Attended)
                .await
                .unwrap_err();
            // The provider's Retry-After reaches the failure handler as data.
            let handler = FailureHandler::new(ExternalAccess::AnonymousRequired);
            let r = handler.handle(&err, Some("s1"), Some("focus-1"), 0);
            assert_eq!(r.action, FailureAction::Retry);
            assert_eq!(retry_after_of(&r), Some(2));

            call_hosted(&pool, &key_hex, &model, RunAttendance::Attended)
                .await
                .expect("the retry succeeds");

            let mut conn = spend_store::open(SPEND_USER, &key_hex).await.unwrap();
            let totals = spend_store::usage_totals_conn(
                &mut conn,
                &spend_store::UsageFilter {
                    run_id: Some("run-1"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(totals.requests, 2, "both attempts count as requests");
            assert_eq!(totals.cost_micros, 5, "only the successful attempt costs");
            let failed: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM provider_usage_ledger WHERE outcome = 'failed' AND cost_micros = 0",
            )
            .fetch_one(&mut conn)
            .await
            .unwrap();
            assert_eq!(failed, 1);
        })
        .await;
    }

    #[tokio::test]
    async fn rate_limit_headers_are_recorded_for_the_usage_summary() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()
                .with_header("x-ratelimit-remaining-requests", "17")
                .with_header("x-ratelimit-limit-requests", "50")]);
            let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, true).await;
            call_hosted(&pool, &key_hex, &model, RunAttendance::Attended)
                .await
                .unwrap();
            let mut conn = spend_store::open(SPEND_USER, &key_hex).await.unwrap();
            let snap = spend_store::get_rate_snapshot_conn(&mut conn, "fixture-host-a")
                .await
                .unwrap()
                .expect("a snapshot");
            assert_eq!(snap.requests_remaining, Some(17));
            assert_eq!(snap.requests_limit, Some(50));
        })
        .await;
    }

    // -- Retry-After ------------------------------------------------------

    fn retry_result(secs: Option<u64>) -> FailureResult {
        let mut metadata = None;
        if let Some(s) = secs {
            let mut m = HashMap::new();
            m.insert(RETRY_AFTER_METADATA_KEY.to_owned(), serde_json::json!(s));
            metadata = Some(m);
        }
        FailureResult {
            action: FailureAction::Retry,
            failure_mode: Some("F10".to_owned()),
            plain_language: "429".to_owned(),
            is_recoverable: true,
            severity: FailureSeverity::Suggest,
            action_id: Some("s1".to_owned()),
            focus_id: Some("focus-1".to_owned()),
            metadata,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_is_waited_out_before_the_retry() {
        let start = tokio::time::Instant::now();
        assert!(
            wait_before_retry(&retry_result(Some(2)), 0, None, |_| 0, None)
                .await
                .is_none()
        );
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_with_no_retry_after_does_not_wait() {
        let start = tokio::time::Instant::now();
        assert!(wait_before_retry(&retry_result(None), 0, None, |_| 0, None)
            .await
            .is_none());
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_at_the_cap_is_slept_and_one_past_it_parks() {
        let start = tokio::time::Instant::now();
        assert!(wait_before_retry(
            &retry_result(Some(RETRY_AFTER_CAP_SECS)),
            0,
            None,
            |_| 0,
            None,
        )
        .await
        .is_none());
        assert_eq!(
            start.elapsed(),
            std::time::Duration::from_secs(RETRY_AFTER_CAP_SECS)
        );

        let start = tokio::time::Instant::now();
        let parked = wait_before_retry(
            &retry_result(Some(RETRY_AFTER_CAP_SECS + 1)),
            0,
            None,
            |_| 0,
            None,
        )
        .await
        .expect("a long wait parks the run");
        assert_eq!(start.elapsed(), std::time::Duration::ZERO, "nothing slept");
        assert_eq!(parked.action, FailureAction::AwaitUser);
        assert_eq!(parked.severity, FailureSeverity::Require);
        let m = parked.metadata.unwrap();
        assert_eq!(m["code"], "spend_block.rate_limited_long_wait");
        assert_eq!(m[RETRY_AFTER_METADATA_KEY], RETRY_AFTER_CAP_SECS + 1);
        assert_eq!(m["wait_cap_secs"], RETRY_AFTER_CAP_SECS);
    }

    /// The whole wait across a run of retries is bounded by MAX_RETRIES x the cap.
    #[test]
    fn the_total_retry_wait_is_bounded() {
        assert!(u64::from(MAX_RETRIES) * RETRY_AFTER_CAP_SECS <= 300);
    }

    // -- Hosted backoff and cancellation (items.id=727) --------------------

    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn with_mode(mut r: FailureResult, mode: &str) -> FailureResult {
        r.failure_mode = Some(mode.to_owned());
        r
    }

    /// A hosted (F10) Retry result carrying `secs` as its Retry-After, if any.
    fn hosted_retry(secs: Option<u64>) -> FailureResult {
        with_mode(retry_result(secs), "F10")
    }

    /// A local-Ollama (F1) Retry result: no metadata.
    fn local_retry() -> FailureResult {
        with_mode(retry_result(None), "F1")
    }

    /// The backoff total is bounded too: the wait before one retry is the
    /// larger of the two caps (a Retry-After floor and a computed backoff are
    /// never added), a run of retries at most MAX_RETRIES times that.
    #[test]
    fn the_total_backoff_wait_is_bounded() {
        assert!(u64::from(MAX_RETRIES) * BACKOFF_CAP_SECS <= 300);
        let per_retry = RETRY_AFTER_CAP_SECS.max(BACKOFF_CAP_SECS);
        assert!(u64::from(MAX_RETRIES) * per_retry <= 300);
        // And the computed ceilings really are under the cap, whatever the attempt.
        for n in 0..200 {
            assert!(backoff_ceiling(n) <= Duration::from_secs(BACKOFF_CAP_SECS));
        }
    }

    #[test]
    fn the_backoff_ceiling_doubles_per_attempt_and_stops_at_the_cap() {
        let ms = |n| backoff_ceiling(n).as_millis();
        assert_eq!(
            [ms(0), ms(1), ms(2), ms(3), ms(4)],
            [1_000, 2_000, 4_000, 8_000, 16_000]
        );
        assert_eq!(ms(5), 30_000, "32 s is clamped to the cap");
        assert_eq!(ms(6), 30_000);
        assert_eq!(ms(63), 30_000, "the largest shift does not overflow");
        assert_eq!(ms(64), 30_000, "a shift past the width does not overflow");
        assert_eq!(ms(u32::MAX), 30_000);
    }

    #[test]
    fn jitter_stays_inside_zero_and_the_ceiling() {
        let c = Duration::from_millis(4_000);
        assert_eq!(jittered(c, |_| 0), Duration::ZERO);
        assert_eq!(jittered(c, |m| m), c);
        assert_eq!(jittered(c, |m| m / 2), Duration::from_millis(2_000));
        // The sampler is told the ceiling in ms ...
        assert_eq!(
            jittered(c, |m| if m == 4_000 { 7 } else { 9_999 }),
            Duration::from_millis(7)
        );
        // ... and a misbehaving one cannot exceed it.
        assert_eq!(jittered(c, |_| u64::MAX), c);
    }

    #[test]
    fn the_production_sampler_stays_inside_its_bound() {
        for max in [0u64, 1, 999, 30_000] {
            for _ in 0..200 {
                assert!(random_ms(max) <= max);
            }
        }
        assert_eq!(random_ms(0), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hosted_retry_with_no_retry_after_waits_a_jittered_backoff_that_grows() {
        let mut waits = Vec::new();
        for attempt in 0..MAX_RETRIES {
            let start = tokio::time::Instant::now();
            let r = wait_before_retry(&hosted_retry(None), attempt, None, |m| m / 2, None).await;
            assert!(r.is_none(), "a plain backoff ends in a retry");
            waits.push(start.elapsed());
        }
        assert_eq!(
            waits,
            [
                Duration::from_millis(500),
                Duration::from_millis(1_000),
                Duration::from_millis(2_000)
            ]
        );
        // Jitter off the bottom of the range: no wait at all.
        let start = tokio::time::Instant::now();
        assert!(wait_before_retry(&hosted_retry(None), 2, None, |_| 0, None)
            .await
            .is_none());
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hosted_timeout_and_a_429_without_retry_after_both_back_off() {
        // Same F10 Retry shape either way (failure.rs gives both metadata None).
        for secs in [None, None] {
            let start = tokio::time::Instant::now();
            assert!(wait_before_retry(&hosted_retry(secs), 1, None, |m| m, None)
                .await
                .is_none());
            assert_eq!(start.elapsed(), Duration::from_secs(2));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_is_the_floor_of_the_wait() {
        // Retry-After longer than the backoff: exactly the Retry-After.
        let start = tokio::time::Instant::now();
        assert!(
            wait_before_retry(&hosted_retry(Some(5)), 0, None, |m| m, None)
                .await
                .is_none()
        );
        assert_eq!(start.elapsed(), Duration::from_secs(5));

        // Backoff longer than the Retry-After: the backoff, not the sum.
        let start = tokio::time::Instant::now();
        assert!(
            wait_before_retry(&hosted_retry(Some(1)), 2, None, |m| m, None)
                .await
                .is_none()
        );
        assert_eq!(start.elapsed(), Duration::from_secs(4));

        // Jitter at zero: the Retry-After alone.
        let start = tokio::time::Instant::now();
        assert!(
            wait_before_retry(&hosted_retry(Some(3)), 2, None, |_| 0, None)
                .await
                .is_none()
        );
        assert_eq!(start.elapsed(), Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_after_over_the_cap_still_parks_and_waits_for_nothing() {
        let start = tokio::time::Instant::now();
        let parked = wait_before_retry(
            &hosted_retry(Some(RETRY_AFTER_CAP_SECS + 1)),
            0,
            None,
            |m| m,
            None,
        )
        .await
        .expect("parks");
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(parked.action, FailureAction::AwaitUser);
    }

    #[tokio::test(start_paused = true)]
    async fn a_local_retry_waits_for_nothing_and_ignores_the_stop() {
        let stop = CancellationToken::new();
        stop.cancel();
        for mode in ["F1", "F2"] {
            let start = tokio::time::Instant::now();
            let r = with_mode(local_retry(), mode);
            assert!(wait_before_retry(&r, 2, Some(&stop), |m| m, None)
                .await
                .is_none());
            assert_eq!(start.elapsed(), Duration::ZERO);
        }
        // Unchanged: a non-Retry result is not a hosted retry either.
        let mut r = hosted_retry(None);
        r.action = FailureAction::AwaitUser;
        assert!(!is_hosted_retry(&r));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_during_a_backoff_wait_ends_it_at_once_without_a_retry() {
        let stop = CancellationToken::new();
        let start = tokio::time::Instant::now();
        let last = hosted_retry(None);
        let (r, ()) = tokio::join!(
            wait_before_retry(&last, 2, Some(&stop), |m| m, None), // would wait 4 s
            async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                stop.cancel();
            }
        );
        assert_eq!(
            start.elapsed(),
            Duration::from_millis(100),
            "ended promptly"
        );
        let r = r.expect("a stopped wait is not a retry");
        assert_eq!(r.action, FailureAction::Retry, "the last result, unchanged");
        assert_eq!(r.failure_mode, last.failure_mode);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_during_a_retry_after_wait_ends_it_at_once_without_a_retry() {
        let stop = CancellationToken::new();
        let start = tokio::time::Instant::now();
        let last = hosted_retry(Some(30));
        let (r, ()) = tokio::join!(
            wait_before_retry(&last, 0, Some(&stop), |m| m, None),
            async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                stop.cancel();
            }
        );
        assert_eq!(start.elapsed(), Duration::from_millis(250));
        assert_eq!(r.expect("stopped").action, FailureAction::Retry);

        // With no backoff to speak of, a stop during the Retry-After alone does the same.
        let stop = CancellationToken::new();
        let start = tokio::time::Instant::now();
        let (r, ()) = tokio::join!(
            wait_before_retry(&last, 0, Some(&stop), |_| 0, None),
            async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                stop.cancel();
            }
        );
        assert_eq!(start.elapsed(), Duration::from_millis(10));
        assert!(r.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn an_already_stopped_run_does_not_wait_even_for_a_zero_delay() {
        let stop = CancellationToken::new();
        stop.cancel();
        let start = tokio::time::Instant::now();
        // Backoff sampled to zero, no Retry-After: nothing to sleep, still stops.
        assert!(
            wait_before_retry(&hosted_retry(None), 0, Some(&stop), |_| 0, None)
                .await
                .is_some()
        );
        assert!(
            wait_before_retry(&hosted_retry(Some(0)), 0, Some(&stop), |_| 0, None)
                .await
                .is_some()
        );
        assert!(!sleep_or_stop(Duration::ZERO, Some(&stop)).await);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    // -- The retry loop, against a scripted attempt ------------------------

    /// Plays a script of attempt outcomes and records the retry_count of each
    /// call. With a scheduler it also takes and gives back the inference slot
    /// inside each attempt, the way execute_once does.
    struct Scripted {
        script: std::collections::VecDeque<Result<Option<FailureResult>, ConductorError>>,
        seen: Vec<u32>,
        scheduler: Option<Arc<ConductorScheduler>>,
        /// items.id=817: where waits are announced, and how many events had
        /// been recorded when each attempt began.
        notice: Option<Arc<crate::retry_wait::test_support::RecordingNotice>>,
        events_at_attempt: Vec<usize>,
    }

    impl Scripted {
        fn new(script: Vec<Result<Option<FailureResult>, ConductorError>>) -> Self {
            Self {
                script: script.into(),
                seen: Vec::new(),
                scheduler: None,
                notice: None,
                events_at_attempt: Vec::new(),
            }
        }
    }

    impl StepAttempt for Scripted {
        fn retry_wait_notice(&self) -> Option<Arc<dyn RetryWaitNotice>> {
            self.notice.clone().map(|n| n as Arc<dyn RetryWaitNotice>)
        }

        fn once(
            &mut self,
            retry_count: u32,
        ) -> impl std::future::Future<Output = Result<Option<FailureResult>, ConductorError>> + Send
        {
            self.seen.push(retry_count);
            if let Some(n) = &self.notice {
                self.events_at_attempt.push(n.events.lock().unwrap().len());
            }
            let next = self.script.pop_front().expect("the script ran out");
            let scheduler = self.scheduler.clone();
            async move {
                if let Some(s) = scheduler {
                    assert!(
                        s.acquire_inference_slot("run-1", PathPriority::Interactive)
                            .await,
                        "the slot must be free at the start of every attempt"
                    );
                    s.release_inference_slot("run-1");
                }
                next
            }
        }
    }

    fn loop_ctx(stop: Option<CancellationToken>) -> StepContext {
        StepContext {
            stop,
            ..hosted_ctx(RunAttendance::Attended, "00")
        }
    }

    fn handler() -> FailureHandler {
        FailureHandler::new(ExternalAccess::AnonymousRequired)
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_retries_with_a_growing_backoff_up_to_max_retries_then_returns_the_last() {
        let mut a = Scripted::new(
            (0..=MAX_RETRIES)
                .map(|_| Ok(Some(hosted_retry(None))))
                .collect(),
        );
        let start = tokio::time::Instant::now();
        let r = retry_loop(&mut a, &loop_ctx(None), &handler(), |m| m).await;
        assert_eq!(a.seen, [0, 1, 2, 3], "one attempt plus MAX_RETRIES retries");
        assert_eq!(r.expect("exhausted").action, FailureAction::Retry);
        // Ceilings 1 s + 2 s + 4 s with the sampler at its maximum.
        assert_eq!(start.elapsed(), Duration::from_secs(7));
        assert!(start.elapsed() <= Duration::from_secs(u64::from(MAX_RETRIES) * 60));
    }

    /// items.id=831: Try again resumes a parked run, which runs the step
    /// through a fresh retry loop. The retry count is a local of that loop, so
    /// a resumed run starts at 0 and gets the full MAX_RETRIES again; nothing of
    /// the failed run's count is carried over.
    #[tokio::test(start_paused = true)]
    async fn a_resumed_run_starts_its_retry_loop_at_zero() {
        let mut first = Scripted::new(
            (0..=MAX_RETRIES)
                .map(|_| Ok(Some(hosted_retry(None))))
                .collect(),
        );
        let r = retry_loop(&mut first, &loop_ctx(None), &handler(), |m| m).await;
        assert!(r.is_some());
        assert_eq!(first.seen, [0, 1, 2, 3]);

        // The resume: a new attempt object and a new loop for the same step.
        let mut resumed = Scripted::new(vec![
            Ok(Some(hosted_retry(None))),
            Ok(Some(hosted_retry(None))),
            Ok(None),
        ]);
        let r = retry_loop(&mut resumed, &loop_ctx(None), &handler(), |m| m).await;
        assert!(r.is_none());
        assert_eq!(
            resumed.seen,
            [0, 1, 2],
            "starts at 0, not at the failed run's MAX_RETRIES"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_returns_success_after_a_retry() {
        let mut a = Scripted::new(vec![Ok(Some(hosted_retry(Some(2)))), Ok(None)]);
        let start = tokio::time::Instant::now();
        let r = retry_loop(&mut a, &loop_ctx(None), &handler(), |_| 0).await;
        assert!(r.is_none());
        assert_eq!(a.seen, [0, 1]);
        assert_eq!(start.elapsed(), Duration::from_secs(2), "the Retry-After");
    }

    #[tokio::test(start_paused = true)]
    async fn a_local_retry_in_the_loop_is_immediate_as_before() {
        let mut a = Scripted::new(vec![Ok(Some(local_retry())), Ok(None)]);
        let start = tokio::time::Instant::now();
        assert!(retry_loop(&mut a, &loop_ctx(None), &handler(), |m| m)
            .await
            .is_none());
        assert_eq!(a.seen, [0, 1]);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_during_a_hosted_wait_means_no_further_attempt() {
        let stop = CancellationToken::new();
        let mut a = Scripted::new(vec![Ok(Some(hosted_retry(None))), Ok(None)]);
        let ctx = loop_ctx(Some(stop.clone()));
        let h = handler();
        let start = tokio::time::Instant::now();
        let (r, ()) = tokio::join!(retry_loop(&mut a, &ctx, &h, |m| m), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            stop.cancel();
        });
        assert_eq!(a.seen, [0], "the model is not called again after the stop");
        assert_eq!(r.expect("the last result").action, FailureAction::Retry);
        assert_eq!(start.elapsed(), Duration::from_millis(300));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_during_a_retry_after_wait_means_no_further_attempt() {
        let stop = CancellationToken::new();
        let mut a = Scripted::new(vec![Ok(Some(hosted_retry(Some(45)))), Ok(None)]);
        let ctx = loop_ctx(Some(stop.clone()));
        let h = handler();
        let start = tokio::time::Instant::now();
        let (r, ()) = tokio::join!(retry_loop(&mut a, &ctx, &h, |m| m), async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            stop.cancel();
        });
        assert_eq!(a.seen, [0]);
        assert!(r.is_some());
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }

    /// Every class: a tripped token stops a retry before any call or wait.
    #[tokio::test(start_paused = true)]
    async fn a_tripped_stop_prevents_any_retry_of_any_class() {
        for first in [local_retry(), hosted_retry(None), hosted_retry(Some(5))] {
            let stop = CancellationToken::new();
            stop.cancel();
            let mut a = Scripted::new(vec![Ok(Some(first.clone())), Ok(None)]);
            let start = tokio::time::Instant::now();
            let r = retry_loop(&mut a, &loop_ctx(Some(stop)), &handler(), |m| m).await;
            assert_eq!(a.seen, [0]);
            let r = r.expect("the last result");
            assert_eq!(r.action, first.action);
            assert_eq!(r.failure_mode, first.failure_mode);
            assert_eq!(start.elapsed(), Duration::ZERO);
        }
    }

    // -- items.id=817: the retry-wait event -----------------------------------

    use crate::retry_wait::test_support::RecordingNotice;
    use crate::retry_wait::RetryWaitState;

    /// A hosted Retry result as `execute_once` returns it: the Retry-After (if
    /// any) plus the wait context `with_wait_context` adds.
    fn announced_retry(secs: Option<u64>) -> FailureResult {
        let mut r = hosted_retry(secs);
        crate::retry_wait::attach_context(&mut r, RetryWaitReason::Busy, "prov-1", "model-1");
        r
    }

    fn announcer(sink: &RecordingNotice) -> RetryWaitIdentity<'_> {
        RetryWaitIdentity {
            sink,
            focus_run_id: "run-1",
            focus_name: "Focus One",
            step_display_name: "Step 1",
        }
    }

    fn states(sink: &RecordingNotice) -> Vec<(RetryWaitState, u64)> {
        sink.payloads()
            .iter()
            .map(|p| (p.state, p.seconds))
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_is_announced_before_the_sleep_with_the_total_wait() {
        // (Retry-After, retry_count, sampler, expected total in seconds)
        type Case = (Option<u64>, u32, fn(u64) -> u64, u64);
        let cases: [Case; 4] = [
            (Some(5), 0, |m| m, 5), // Retry-After longer than the 1 s backoff
            (Some(1), 2, |m| m, 4), // backoff (4 s) longer than the Retry-After
            (Some(3), 2, |_| 0, 3), // jitter at zero: the Retry-After alone
            (None, 1, |m| m, 2),    // no Retry-After: the backoff alone
        ];
        for (retry_after, retry_count, sample, total) in cases {
            let sink = RecordingNotice::default();
            let start = tokio::time::Instant::now();
            let r = wait_before_retry(
                &announced_retry(retry_after),
                retry_count,
                None,
                sample,
                Some(&announcer(&sink)),
            )
            .await;
            assert!(r.is_none());
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 2, "one waiting, one clear");
            let (at, first) = &events[0];
            assert_eq!(first.state, RetryWaitState::Waiting);
            assert_eq!(*at, start, "announced before any of the sleep");
            assert_eq!(first.seconds, total, "the TOTAL, not the sum or the floor");
            assert_eq!(first.attempt, retry_count + 1);
            assert_eq!(first.max_attempts, MAX_RETRIES);
            assert_eq!(first.focus_name, "Focus One");
            assert_eq!(first.step_display_name, "Step 1");
            assert_eq!(first.provider_id, "prov-1");
            assert_eq!(first.model_id, "model-1");
            let (at, last) = &events[1];
            assert_eq!(last.state, RetryWaitState::Clear);
            assert_eq!(
                *at,
                start + Duration::from_secs(total),
                "cleared at the end"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_retry_loop_announces_the_wait_and_clears_it_before_the_next_attempt() {
        let sink = Arc::new(RecordingNotice::default());
        let mut a = Scripted::new(vec![Ok(Some(announced_retry(None))), Ok(None)]);
        a.notice = Some(sink.clone());
        let ctx = loop_ctx(None);
        let r = retry_loop(&mut a, &ctx, &handler(), |m| m).await;
        assert!(r.is_none());
        assert_eq!(a.seen, [0, 1]);
        let got = sink.payloads();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].state, RetryWaitState::Waiting);
        assert_eq!(got[0].focus_run_id, ctx.focus_run_id);
        assert_eq!(got[0].focus_name, ctx.focus_name);
        assert_eq!(got[0].step_display_name, ctx.step.display_name);
        assert_eq!(got[1].state, RetryWaitState::Clear);
        // Nothing was recorded when attempt 1 began; both events when attempt 2 did.
        assert_eq!(a.events_at_attempt, [0, 2], "the clear precedes the retry");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_during_the_wait_clears_it_at_once() {
        let sink = Arc::new(RecordingNotice::default());
        let stop = CancellationToken::new();
        let mut a = Scripted::new(vec![Ok(Some(announced_retry(Some(30)))), Ok(None)]);
        a.notice = Some(sink.clone());
        let ctx = loop_ctx(Some(stop.clone()));
        let h = handler();
        let start = tokio::time::Instant::now();
        let (r, ()) = tokio::join!(retry_loop(&mut a, &ctx, &h, |m| m), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stop.cancel();
        });
        assert!(r.is_some());
        assert_eq!(a.seen, [0], "no retry after a stop");
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1.seconds, 30);
        assert_eq!(events[1].1.state, RetryWaitState::Clear);
        assert_eq!(events[1].0, start + Duration::from_millis(100));
    }

    /// cancel_run trips the run's lease token, which is `ctx.stop` (items.id=816).
    #[tokio::test(start_paused = true)]
    async fn a_cancel_of_the_run_during_the_wait_clears_it() {
        let sink = Arc::new(RecordingNotice::default());
        let lease_token = CancellationToken::new();
        let reg = crate::chat_stream::ChatStreamRegistry::default();
        let ctx = api_chat_ctx(&lease_token, &reg);
        let mut a = Scripted::new(vec![Ok(Some(announced_retry(None))), Ok(None)]);
        a.notice = Some(sink.clone());
        let h = handler();
        let start = tokio::time::Instant::now();
        let (r, ()) = tokio::join!(retry_loop(&mut a, &ctx, &h, |m| m), async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            lease_token.cancel();
        });
        assert!(r.is_some());
        assert_eq!(a.seen, [0]);
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].1.state, RetryWaitState::Clear);
        assert_eq!(events[1].0, start + Duration::from_millis(400));
    }

    /// An API Chat Stop trips only the stream's child token.
    #[tokio::test(start_paused = true)]
    async fn an_api_chat_stop_during_the_wait_clears_it() {
        let sink = Arc::new(RecordingNotice::default());
        let parent = CancellationToken::new();
        let reg = crate::chat_stream::ChatStreamRegistry::default();
        let ctx = api_chat_ctx(&parent, &reg);
        let mut a = Scripted::new(vec![Ok(Some(announced_retry(Some(45)))), Ok(None)]);
        a.notice = Some(sink.clone());
        let h = handler();
        let start = tokio::time::Instant::now();
        let (r, ()) = tokio::join!(retry_loop(&mut a, &ctx, &h, |m| m), async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            user_stop(&reg);
        });
        assert!(r.is_some());
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1.seconds, 45);
        assert_eq!(events[1].1.state, RetryWaitState::Clear);
        assert_eq!(events[1].0, start + Duration::from_secs(1));
    }

    /// The waiting future itself being dropped (the run's task aborted) still
    /// clears the row: no explicit code path runs, the guard's drop does.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_waiting_future_clears_the_wait() {
        let sink = RecordingNotice::default();
        let start = tokio::time::Instant::now();
        let timed_out = tokio::time::timeout(
            Duration::from_millis(100),
            wait_before_retry(
                &announced_retry(Some(30)),
                0,
                None,
                |m| m,
                Some(&announcer(&sink)),
            ),
        )
        .await;
        assert!(timed_out.is_err(), "the wait was cut off by the timeout");
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].1.state, RetryWaitState::Clear);
        assert_eq!(events[1].0, start + Duration::from_millis(100));
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_announced_when_there_is_no_wait_to_show() {
        // A local (F1/F2) retry waits for nothing.
        let sink = RecordingNotice::default();
        for mode in ["F1", "F2"] {
            let r = with_mode(local_retry(), mode);
            assert!(
                wait_before_retry(&r, 2, None, |m| m, Some(&announcer(&sink)))
                    .await
                    .is_none()
            );
        }
        // A zero-length wait.
        assert!(wait_before_retry(
            &announced_retry(None),
            0,
            None,
            |_| 0,
            Some(&announcer(&sink))
        )
        .await
        .is_none());
        // A Retry-After over the cap parks the run: nothing is slept or shown.
        let parked = wait_before_retry(
            &announced_retry(Some(RETRY_AFTER_CAP_SECS + 1)),
            0,
            None,
            |m| m,
            Some(&announcer(&sink)),
        )
        .await;
        assert_eq!(parked.expect("parks").action, FailureAction::AwaitUser);
        // A hosted wait whose result lacks the context still waits, silently.
        let start = tokio::time::Instant::now();
        assert!(wait_before_retry(
            &hosted_retry(Some(2)),
            0,
            None,
            |m| m,
            Some(&announcer(&sink))
        )
        .await
        .is_none());
        assert_eq!(start.elapsed(), Duration::from_secs(2));
        assert!(sink.payloads().is_empty(), "{:?}", states(&sink));
    }

    #[test]
    fn a_provider_failure_gets_its_wait_context_and_other_failures_do_not() {
        let h = handler();
        let handle = |e: &ConductorError, retry_count: u32| {
            h.handle(e, Some("s1"), Some("focus-1"), retry_count)
        };
        let busy = ConductorError::ProviderRateLimit {
            plain_language: "x".to_owned(),
            retry_after_secs: Some(7),
        };
        let slow = ConductorError::ProviderTimeout {
            plain_language: "x".to_owned(),
        };
        let down = ConductorError::ProviderUnavailable {
            plain_language: "x".to_owned(),
        };
        for (e, reason) in [(&busy, "busy"), (&slow, "slow"), (&down, "unavailable")] {
            let r = with_wait_context(handle(e, 0), e, Some("prov-1"), "model-1");
            let m = r.metadata.expect("context attached");
            assert_eq!(m[retry_wait::RETRY_REASON_KEY], reason);
            assert_eq!(m[retry_wait::RETRY_PROVIDER_KEY], "prov-1");
            assert_eq!(m[retry_wait::RETRY_MODEL_KEY], "model-1");
        }
        // A Retry-After already on the result survives.
        let r = with_wait_context(handle(&busy, 0), &busy, Some("prov-1"), "model-1");
        assert_eq!(retry_after_of(&r), Some(7));
        // No provider chosen: unchanged.
        let r = with_wait_context(handle(&busy, 0), &busy, None, "model-1");
        assert!(!r
            .metadata
            .unwrap()
            .contains_key(retry_wait::RETRY_REASON_KEY));
        // Retries used up: the escalated result is not a Retry, so it is unchanged.
        let r = with_wait_context(handle(&busy, MAX_RETRIES), &busy, Some("prov-1"), "m");
        assert!(r
            .metadata
            .is_none_or(|m| !m.contains_key(retry_wait::RETRY_REASON_KEY)));
        // A failure that is not a provider wait is unchanged.
        let key = ConductorError::MissingApiKey {
            plain_language: "x".to_owned(),
        };
        let r = with_wait_context(handle(&key, 0), &key, Some("prov-1"), "m");
        assert!(r
            .metadata
            .is_none_or(|m| !m.contains_key(retry_wait::RETRY_REASON_KEY)));
    }

    // -- items.id=831: the final-failure message --------------------------

    fn lazy_pool() -> sqlx::SqlitePool {
        sqlx::SqlitePool::connect_lazy_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(":memory:"),
        )
    }

    #[tokio::test]
    async fn only_the_exhaustion_of_a_hosted_provider_error_becomes_the_final_failure() {
        let ctx = hosted_ctx(RunAttendance::Attended, "00");
        let busy = ConductorError::ProviderRateLimit {
            plain_language: "429".to_owned(),
            retry_after_secs: None,
        };
        let pool = lazy_pool();
        let exhausted = || {
            FailureHandler::new(ExternalAccess::Unrestricted).handle(&busy, None, None, MAX_RETRIES)
        };

        // Exhausted, a provider chosen: marked, with the Focus wording first
        // (this ctx is not bound) and the ids; the unreadable row names nothing.
        let r = with_final_failure(
            exhausted(),
            &busy,
            MAX_RETRIES,
            &ctx,
            Some("prov-1"),
            "model-1",
            &pool,
        )
        .await;
        assert_eq!(r.action, FailureAction::AwaitUser);
        assert!(
            r.plain_language.contains("is paused."),
            "{}",
            r.plain_language
        );
        assert!(
            r.plain_language.contains("too busy"),
            "{}",
            r.plain_language
        );
        let m = r.metadata.expect("context attached");
        assert_eq!(m[final_failure::FINAL_FAILURE_KEY], true);
        assert_eq!(m[retry_wait::RETRY_PROVIDER_KEY], "prov-1");

        // No provider chosen, a non-provider error, or retries not used up:
        // unchanged.
        let unchanged = |r: &FailureResult| {
            r.plain_language == exhausted().plain_language && r.metadata.is_none()
        };
        assert!(unchanged(
            &with_final_failure(exhausted(), &busy, MAX_RETRIES, &ctx, None, "m", &pool).await
        ));
        let key = ConductorError::MissingApiKey {
            plain_language: "x".to_owned(),
        };
        assert!(unchanged(
            &with_final_failure(exhausted(), &key, MAX_RETRIES, &ctx, Some("p"), "m", &pool).await
        ));
        assert!(unchanged(
            &with_final_failure(
                exhausted(),
                &busy,
                MAX_RETRIES - 1,
                &ctx,
                Some("p"),
                "m",
                &pool
            )
            .await
        ));

        // An API Chat run (bound) shows the sentence alone.
        let bound = StepContext {
            bound: Some(crate::api_chat::BoundDestination {
                provider_id: "prov-1".to_owned(),
                model_id: "model-1".to_owned(),
            }),
            ..hosted_ctx(RunAttendance::Attended, "00")
        };
        let r = with_final_failure(
            exhausted(),
            &busy,
            MAX_RETRIES,
            &bound,
            Some("prov-1"),
            "model-1",
            &pool,
        )
        .await;
        assert!(
            !r.plain_language.contains("is paused."),
            "{}",
            r.plain_language
        );
        assert!(
            r.plain_language.contains("too busy"),
            "{}",
            r.plain_language
        );
    }

    // -- items.id=816: an API Chat Stop ends a retry wait ---------------------

    /// An API Chat step: a streaming hosted step whose stream token is a CHILD
    /// of the lease token (api_chat.rs), as `register_child_of` builds it.
    fn api_chat_ctx(
        parent: &CancellationToken,
        reg: &crate::chat_stream::ChatStreamRegistry,
    ) -> StepContext {
        StepContext {
            stream: Some(reg.register_child_of("u1", "run-1", parent)),
            ..loop_ctx(Some(parent.clone()))
        }
    }

    /// The real Stop of `cancel_chat_stream`.
    fn user_stop(reg: &crate::chat_stream::ChatStreamRegistry) {
        crate::commands::chat_stream::cancel_for_user(reg, "u1", "run-1").unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_during_a_retry_after_wait_of_an_api_chat_run_ends_it_with_no_further_attempt() {
        let parent = CancellationToken::new();
        let reg = crate::chat_stream::ChatStreamRegistry::default();
        let ctx = api_chat_ctx(&parent, &reg);
        assert!(step_streams(&ctx));
        let mut a = Scripted::new(vec![Ok(Some(hosted_retry(Some(45)))), Ok(None)]);
        let h = handler();
        let start = tokio::time::Instant::now();
        let (r, ()) = tokio::join!(retry_loop(&mut a, &ctx, &h, |m| m), async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            user_stop(&reg);
        });
        assert_eq!(a.seen, [0], "no further attempt, so no spend pre-flight");
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(1),
            "the wait ended at the Stop"
        );
        assert!(is_user_stop(&r.expect("a stop result")));
        assert!(
            !parent.is_cancelled(),
            "the Stop leaves the lease token alone"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_during_a_backoff_wait_of_an_api_chat_run_ends_it_with_no_further_attempt() {
        let parent = CancellationToken::new();
        let reg = crate::chat_stream::ChatStreamRegistry::default();
        let ctx = api_chat_ctx(&parent, &reg);
        let mut a = Scripted::new(vec![Ok(Some(hosted_retry(None))), Ok(None)]);
        let h = handler();
        let start = tokio::time::Instant::now();
        // The sampler at its maximum: a 1 s backoff before the first retry.
        let (r, ()) = tokio::join!(retry_loop(&mut a, &ctx, &h, |m| m), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            user_stop(&reg);
        });
        assert_eq!(a.seen, [0]);
        assert_eq!(start.elapsed(), Duration::from_millis(300));
        assert!(is_user_stop(&r.expect("a stop result")));
        assert!(!parent.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_before_the_retry_is_considered_means_no_wait_and_no_attempt() {
        let parent = CancellationToken::new();
        let reg = crate::chat_stream::ChatStreamRegistry::default();
        let ctx = api_chat_ctx(&parent, &reg);
        user_stop(&reg);
        let mut a = Scripted::new(vec![Ok(Some(hosted_retry(Some(45)))), Ok(None)]);
        let start = tokio::time::Instant::now();
        let r = retry_loop(&mut a, &ctx, &handler(), |m| m).await;
        assert_eq!(a.seen, [0]);
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert!(is_user_stop(&r.expect("a stop result")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_parent_trip_of_an_api_chat_run_ends_the_wait_and_returns_the_retry_unchanged() {
        // A rotation's 'Stop and save' or cancel_run on an API Chat run: the
        // child is tripped with the parent, the wait ends, and the lifecycle
        // sees the lease token (so the result is the existing 727 one).
        let parent = CancellationToken::new();
        let reg = crate::chat_stream::ChatStreamRegistry::default();
        let ctx = api_chat_ctx(&parent, &reg);
        let mut a = Scripted::new(vec![Ok(Some(hosted_retry(Some(45)))), Ok(None)]);
        let h = handler();
        let start = tokio::time::Instant::now();
        let (r, ()) = tokio::join!(retry_loop(&mut a, &ctx, &h, |m| m), async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            parent.cancel();
        });
        assert_eq!(a.seen, [0]);
        assert_eq!(start.elapsed(), Duration::from_secs(2));
        assert_eq!(r.expect("the last result").action, FailureAction::Retry);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_with_no_wait_in_progress_changes_nothing_in_the_loop() {
        // What a normal Stop looks like to the loop: the attempt itself ends
        // (a cancelled stream is F10 AwaitUser, or a step that finished), and
        // the loop returns that untouched.
        let parent = CancellationToken::new();
        let reg = crate::chat_stream::ChatStreamRegistry::default();
        let ctx = api_chat_ctx(&parent, &reg);
        user_stop(&reg);
        let mut awaiting = hosted_retry(None);
        awaiting.action = FailureAction::AwaitUser;
        let mut a = Scripted::new(vec![Ok(Some(awaiting.clone()))]);
        let r = retry_loop(&mut a, &ctx, &handler(), |m| m).await;
        assert_eq!(a.seen, [0]);
        let r = r.expect("the attempt's own failure");
        assert_eq!(r.action, FailureAction::AwaitUser);
        assert!(!is_user_stop(&r));

        let mut a = Scripted::new(vec![Ok(None)]);
        assert!(retry_loop(&mut a, &ctx, &handler(), |m| m).await.is_none());
    }

    #[test]
    fn the_retry_machinery_watches_the_stream_token_only_when_the_step_streams() {
        let parent = CancellationToken::new();
        let reg = crate::chat_stream::ChatStreamRegistry::default();
        let streaming = api_chat_ctx(&parent, &reg);
        assert!(effective_stop(&streaming).is_some());
        user_stop(&reg);
        assert!(effective_stop(&streaming).unwrap().is_cancelled());
        assert!(!parent.is_cancelled());

        // No stream: exactly ctx.stop, as before this item.
        let plain = loop_ctx(Some(parent.clone()));
        assert!(!effective_stop(&plain).unwrap().is_cancelled());
        parent.cancel();
        assert!(effective_stop(&plain).unwrap().is_cancelled());
        assert!(effective_stop(&loop_ctx(None)).is_none());

        // A stream on a LOCAL step is not streamed: ctx.stop is what counts.
        let mut local = api_chat_ctx(&CancellationToken::new(), &reg);
        local.effective_access = ExternalAccess::LocalOnly;
        assert!(!step_streams(&local));
        assert!(!effective_stop(&local).unwrap().is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn a_floor_consent_wait_and_a_non_retry_failure_are_returned_untouched() {
        for action in [FailureAction::AwaitFloorConsent, FailureAction::AwaitUser] {
            let mut r = hosted_retry(None);
            r.action = action.clone();
            let mut a = Scripted::new(vec![Ok(Some(r))]);
            let out = retry_loop(&mut a, &loop_ctx(None), &handler(), |m| m).await;
            assert_eq!(out.expect("returned").action, action);
            assert_eq!(a.seen, [0]);
        }
    }

    /// The wait runs between attempts, outside the inference slot. The scripted
    /// attempt takes and gives back the real scheduler's slot as execute_once
    /// does; while the loop sits in its backoff, another run must be able to
    /// take the slot at once.
    #[tokio::test(start_paused = true)]
    async fn the_inference_slot_is_free_during_the_wait() {
        let scheduler = Arc::new(ConductorScheduler::new());
        let mut a = Scripted::new(vec![Ok(Some(hosted_retry(Some(3)))), Ok(None)]);
        a.scheduler = Some(scheduler.clone());
        let ctx = loop_ctx(None);
        let h = handler();
        let probe_sched = scheduler.clone();
        let (r, probed) = tokio::join!(retry_loop(&mut a, &ctx, &h, |m| m), async move {
            tokio::time::sleep(Duration::from_secs(1)).await; // inside the 3 s wait
            let got = tokio::time::timeout(
                Duration::from_millis(1),
                probe_sched.acquire_inference_slot("other", PathPriority::Interactive),
            )
            .await;
            probe_sched.release_inference_slot("other");
            got
        });
        assert!(r.is_none());
        assert_eq!(probed, Ok(true), "another run takes the slot mid-wait");
        assert_eq!(a.seen, [0, 1], "and the retry's own acquire succeeded");
    }

    /// execute_once has no unit harness, so the placement of the wait is also
    /// pinned by a source scan: the loop never touches the slot, and the wait
    /// comes after the attempt returns; the one acquire is in execute_once and
    /// is released on the stop path and the normal path.
    #[test]
    fn the_wait_sits_between_attempts_and_never_takes_the_slot() {
        let src = include_str!("executor.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("test module marker")];
        let start = prod.find("async fn retry_loop<").expect("retry_loop");
        let body = &prod[start..start + prod[start..].find("\n}\n").expect("end")];
        assert!(
            !body.contains("inference_slot"),
            "the loop never holds the slot"
        );
        let attempt = body.find("attempt.once(").expect("the attempt call");
        let wait = body.find("wait_before_retry(").expect("the wait call");
        assert!(attempt < wait, "the wait follows the attempt");
        assert_eq!(prod.matches("acquire_inference_slot(").count(), 1);
        assert_eq!(prod.matches("release_inference_slot(").count(), 2);
        let acquire = prod.find("acquire_inference_slot(").unwrap();
        assert!(acquire > prod.find("async fn execute_once").unwrap());
    }

    // -----------------------------------------------------------------------
    // items.id=715: streamed hosted steps. Loopback only; the same spend rig.
    // -----------------------------------------------------------------------

    mod streaming {
        use super::*;
        use crate::providers::loopback_test_server::StreamStep;
        use std::sync::Mutex;
        use tokio_util::sync::CancellationToken;

        #[derive(Default)]
        struct Rec {
            deltas: Mutex<Vec<String>>,
            ends: Mutex<Vec<StreamEnd>>,
            first_delta: tokio::sync::Notify,
        }

        impl StreamSink for Rec {
            fn delta(&self, _seq: u32, text: &str) {
                self.deltas.lock().unwrap().push(text.to_owned());
                // notify_one keeps a permit, so a delta that lands before
                // anyone waits is not lost.
                self.first_delta.notify_one();
            }
            fn end(&self, end: &StreamEnd) {
                self.ends.lock().unwrap().push(*end);
            }
        }

        impl Rec {
            fn text(&self) -> String {
                self.deltas.lock().unwrap().concat()
            }

            /// Wait until the sink has been handed its first delta, at most
            /// `CANCEL_WAIT`. On a timeout this returns anyway: the caller
            /// cancels and the test's own assertions then fail, so a missing
            /// delta shows up as a failure and never as a hang (items.id=767).
            async fn wait_first_delta(&self) {
                let _ = tokio::time::timeout(CANCEL_WAIT, self.first_delta.notified()).await;
            }
        }

        /// How long a canceller waits for the event it cancels after.
        const CANCEL_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

        fn frame(v: serde_json::Value) -> Vec<u8> {
            format!("data: {v}\n\n").into_bytes()
        }
        fn delta(t: &str) -> Vec<u8> {
            frame(serde_json::json!({"choices":[{"delta":{"content":t},"finish_reason":null}]}))
        }
        fn finish() -> Vec<u8> {
            frame(serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}]}))
        }
        fn usage() -> Vec<u8> {
            frame(
                serde_json::json!({"choices":[],"usage":{"prompt_tokens":4,"completion_tokens":2}}),
            )
        }
        fn steps(frames: Vec<Vec<u8>>) -> Vec<StreamStep> {
            frames.into_iter().map(StreamStep::Chunk).collect()
        }
        fn with_usage() -> Canned {
            Canned::sse(steps(vec![
                delta("Hel"),
                delta("lo"),
                finish(),
                usage(),
                b"data: [DONE]\n\n".to_vec(),
            ]))
        }

        async fn streamed(
            pool: &sqlx::SqlitePool,
            key_hex: &str,
            model: &ProviderModel,
            rec: &Rec,
            token: &CancellationToken,
            tail: &mut StreamTail,
        ) -> Result<GenerateResponse, ConductorError> {
            hosted_step_call(
                &loopback_client(),
                pool,
                &hosted_ctx(RunAttendance::Attended, key_hex),
                "fixture-host-a",
                model,
                &generate_request("hello there"),
                &ReviewedPrompt::for_test("hello there"),
                &chrono::Utc,
                loopback_build,
                Some(StreamParts {
                    sink: rec,
                    cancel: token,
                    tail,
                }),
            )
            .await
        }

        /// outcome, usage_source, tokens_in, tokens_out, cost, reserved
        async fn only_ledger_row(key_hex: &str) -> (String, String, i64, i64, i64, i64) {
            use sqlx::Row;
            let mut conn = spend_store::open(SPEND_USER, key_hex).await.unwrap();
            let row = sqlx::query(
                "SELECT outcome, usage_source, tokens_in, tokens_out, cost_micros, \
                 reserved_micros FROM provider_usage_ledger",
            )
            .fetch_one(&mut conn)
            .await
            .unwrap();
            (
                row.get("outcome"),
                row.get("usage_source"),
                row.get("tokens_in"),
                row.get("tokens_out"),
                row.get("cost_micros"),
                row.get("reserved_micros"),
            )
        }

        async fn cleared(
            pool: &sqlx::SqlitePool,
            server: &LoopbackServer,
        ) -> (String, ProviderModel) {
            let (key_hex, model) = spend_rig(pool, server, Some(PRICE_TEXT)).await;
            grant_consent(pool, &key_hex, false).await;
            set_cost_limit(&key_hex, spend_store::LimitPeriod::Month, 5_000_000, false).await;
            (key_hex, model)
        }

        #[tokio::test]
        async fn a_streamed_call_goes_through_the_gate_and_settles_as_reported() {
            with_temp_shared_db(|pool| async move {
                let server = LoopbackServer::start(vec![with_usage()]);
                let (key_hex, model) = cleared(&pool, &server).await;
                let rec = Rec::default();
                let mut tail = StreamTail::default();
                let out = streamed(
                    &pool,
                    &key_hex,
                    &model,
                    &rec,
                    &CancellationToken::new(),
                    &mut tail,
                )
                .await
                .expect("a cleared streamed call succeeds");
                assert_eq!(out.content, "Hello");
                assert_eq!(rec.text(), "Hello");
                assert_eq!(tail.received_text, "Hello");
                assert!(!tail.cancelled);

                let body = server.captured()[0].json();
                assert_eq!(body["stream"], true);
                assert_eq!(body["stream_options"]["include_usage"], true);
                assert_eq!(body["max_completion_tokens"], 128);

                let (outcome, source, tin, tout, cost, reserved) = only_ledger_row(&key_hex).await;
                assert_eq!((outcome.as_str(), source.as_str()), ("ok", "reported"));
                assert_eq!((tin, tout, cost), (4, 2, 5));
                assert!(reserved > cost, "the worst case was reserved first");
            })
            .await;
        }

        #[tokio::test]
        async fn a_completed_stream_with_no_usage_is_estimated() {
            with_temp_shared_db(|pool| async move {
                let server = LoopbackServer::start(vec![Canned::sse(steps(vec![
                    delta("Hello world"),
                    finish(),
                    b"data: [DONE]\n\n".to_vec(),
                ]))]);
                let (key_hex, model) = cleared(&pool, &server).await;
                let mut tail = StreamTail::default();
                streamed(
                    &pool,
                    &key_hex,
                    &model,
                    &Rec::default(),
                    &CancellationToken::new(),
                    &mut tail,
                )
                .await
                .unwrap();
                let (outcome, source, _, tout, _, _) = only_ledger_row(&key_hex).await;
                assert_eq!((outcome.as_str(), source.as_str()), ("ok", "estimated"));
                assert_eq!(tout, 3, "ceil(11 bytes / 4)");
            })
            .await;
        }

        async fn assert_interrupted_at_worst_case(canned: Canned) {
            with_temp_shared_db(|pool| async move {
                let server = LoopbackServer::start(vec![canned]);
                let (key_hex, model) = cleared(&pool, &server).await;
                let mut tail = StreamTail::default();
                let err = streamed(
                    &pool,
                    &key_hex,
                    &model,
                    &Rec::default(),
                    &CancellationToken::new(),
                    &mut tail,
                )
                .await
                .unwrap_err();
                assert!(
                    matches!(
                        err,
                        ConductorError::Provider { .. }
                            | ConductorError::ProviderUnavailable { .. }
                    ),
                    "{err:?}"
                );
                let (outcome, source, _, _, cost, reserved) = only_ledger_row(&key_hex).await;
                assert_eq!((outcome.as_str(), source.as_str()), ("interrupted", "none"));
                assert_eq!(cost, reserved, "charged at the reserved worst case");
            })
            .await;
        }

        #[tokio::test]
        async fn an_in_stream_error_as_the_first_event_keeps_the_reserved_worst_case() {
            assert_interrupted_at_worst_case(Canned::sse(steps(vec![frame(serde_json::json!({
                "error": {"code": "server_error"},
                "choices":[{"delta":{"content":""},"finish_reason":"error"}]
            }))])))
            .await;
        }

        #[tokio::test]
        async fn a_cut_after_text_keeps_the_reserved_worst_case() {
            assert_interrupted_at_worst_case(Canned::sse(vec![
                StreamStep::Chunk(delta("Hel")),
                StreamStep::Cut,
            ]))
            .await;
        }

        #[tokio::test]
        async fn a_failure_after_text_is_terminal_and_never_retried() {
            with_temp_shared_db(|pool| async move {
                let server = LoopbackServer::start(vec![Canned::sse(vec![
                    StreamStep::Chunk(delta("Hel")),
                    StreamStep::Cut,
                ])]);
                let (key_hex, model) = cleared(&pool, &server).await;
                let mut tail = StreamTail::default();
                let err = streamed(
                    &pool,
                    &key_hex,
                    &model,
                    &Rec::default(),
                    &CancellationToken::new(),
                    &mut tail,
                )
                .await
                .unwrap_err();
                // Terminal class: the failure handler parks it instead of
                // retrying over text the user already read.
                assert!(matches!(err, ConductorError::Provider { .. }), "{err:?}");
                assert_eq!(tail.received_text, "Hel");
            })
            .await;
        }

        #[tokio::test]
        async fn cancel_mid_stream_stops_the_read_settles_as_interrupted_and_returns_the_text() {
            with_temp_shared_db(|pool| async move {
                let server = LoopbackServer::start(vec![Canned::sse(vec![
                    StreamStep::Chunk(delta("Hi")),
                    StreamStep::Hold(std::time::Duration::from_secs(20)),
                ])]);
                let (key_hex, model) = cleared(&pool, &server).await;
                let token = CancellationToken::new();
                let t2 = token.clone();
                let rec = std::sync::Arc::new(Rec::default());
                let seen = rec.clone();
                tokio::spawn(async move {
                    seen.wait_first_delta().await;
                    t2.cancel();
                });
                let mut tail = StreamTail::default();
                let started = std::time::Instant::now();
                let err = streamed(&pool, &key_hex, &model, &rec, &token, &mut tail)
                    .await
                    .unwrap_err();
                assert!(started.elapsed() < std::time::Duration::from_secs(5));
                assert!(matches!(err, ConductorError::Provider { .. }));
                assert!(tail.cancelled);
                assert_eq!(tail.received_text, "Hi");
                let (outcome, _, _, _, cost, reserved) = only_ledger_row(&key_hex).await;
                assert_eq!(outcome, "interrupted");
                assert_eq!(cost, reserved);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while server.client_closed() == 0 && std::time::Instant::now() < deadline {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                assert_eq!(server.client_closed(), 1, "the HTTP read must really stop");
            })
            .await;
        }

        /// items.id=716: the real cancel path (`cancel_chat_stream`'s body,
        /// owner-checked, on the registry handle the bound run holds) stops a
        /// bound run's HTTP read. A foreign user's cancel trips nothing.
        #[tokio::test]
        async fn cancel_chat_stream_stops_a_bound_run() {
            with_temp_shared_db(|pool| async move {
                let server = LoopbackServer::start(vec![Canned::sse(vec![
                    StreamStep::Chunk(delta("Hi")),
                    StreamStep::Hold(std::time::Duration::from_secs(20)),
                ])]);
                let (key_hex, _default) = cleared(&pool, &server).await;
                let bound = bound_to("fixture-model-a");
                let mut ctx = hosted_ctx(RunAttendance::Attended, &key_hex);
                ctx.bound = Some(bound.clone());
                let model = select_bound_model(&pool, &ctx, &bound)
                    .await
                    .unwrap()
                    .hosted_model
                    .expect("a hosted row");

                // The same registration send_api_chat_message makes.
                let registry = crate::chat_stream::ChatStreamRegistry::default();
                let handle = registry.register(SPEND_USER, &ctx.focus_run_id);
                let token = handle.token().clone();

                let rec = std::sync::Arc::new(Rec::default());
                let seen = rec.clone();
                let reg = registry.clone();
                let run_id = ctx.focus_run_id.clone();
                let canceller = tokio::spawn(async move {
                    seen.wait_first_delta().await;
                    let foreign = crate::commands::chat_stream::cancel_for_user(
                        &reg,
                        "someone-else",
                        &run_id,
                    );
                    let own =
                        crate::commands::chat_stream::cancel_for_user(&reg, SPEND_USER, &run_id);
                    (foreign, own)
                });

                let mut tail = StreamTail::default();
                let started = std::time::Instant::now();
                let err = hosted_step_call(
                    &loopback_client(),
                    &pool,
                    &ctx,
                    "fixture-host-a",
                    &model,
                    &generate_request("hello there"),
                    &ReviewedPrompt::for_test("hello there"),
                    &chrono::Utc,
                    loopback_build,
                    Some(StreamParts {
                        sink: &*rec,
                        cancel: &token,
                        tail: &mut tail,
                    }),
                )
                .await
                .unwrap_err();
                let (foreign, own) = canceller.await.unwrap();

                assert_eq!(
                    foreign,
                    Err(crate::commands::chat_stream::STREAM_NOT_FOUND.to_owned()),
                    "another user's cancel is refused and trips nothing"
                );
                assert_eq!(own, Ok(()));
                assert!(started.elapsed() < std::time::Duration::from_secs(5));
                assert!(matches!(err, ConductorError::Provider { .. }));
                assert!(tail.cancelled, "it stopped because of the cancel");
                assert_eq!(tail.received_text, "Hi", "what the user saw is returned");
                let (outcome, _, _, _, cost, reserved) = only_ledger_row(&key_hex).await;
                assert_eq!(outcome, "interrupted");
                assert_eq!(cost, reserved, "charged at the reserved worst case");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while server.client_closed() == 0 && std::time::Instant::now() < deadline {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                assert_eq!(server.client_closed(), 1, "the HTTP read must really stop");
                drop(handle);
            })
            .await;
        }

        #[tokio::test]
        async fn cancel_before_the_first_byte_is_interrupted_never_released() {
            with_temp_shared_db(|pool| async move {
                // The server holds its reply well past the cancel, so the
                // cancel lands before the first byte however slow the client
                // is (items.id=767).
                let server = LoopbackServer::start(vec![
                    with_usage().with_delay(std::time::Duration::from_secs(5))
                ]);
                let (key_hex, model) = cleared(&pool, &server).await;
                let token = CancellationToken::new();
                let mut tail = StreamTail::default();
                let rec = Rec::default();
                // Cancel once the server has the request: after dispatch, so
                // the reservation is kept, and before any reply byte.
                let (streamed_result, ()) = tokio::join!(
                    streamed(&pool, &key_hex, &model, &rec, &token, &mut tail),
                    async {
                        server.wait_for_request(CANCEL_WAIT).await;
                        token.cancel();
                    }
                );
                streamed_result.unwrap_err();
                assert!(tail.cancelled);
                assert_eq!(ledger_count(&key_hex).await, 1, "the reservation is kept");
                let (outcome, _, _, _, cost, reserved) = only_ledger_row(&key_hex).await;
                assert_eq!(outcome, "interrupted");
                assert_eq!(cost, reserved);
            })
            .await;
        }

        #[tokio::test]
        async fn cancel_before_dispatch_sends_nothing_and_releases_the_reservation() {
            with_temp_shared_db(|pool| async move {
                let server = LoopbackServer::start(vec![with_usage()]);
                let (key_hex, model) = cleared(&pool, &server).await;
                let token = CancellationToken::new();
                token.cancel();
                let mut tail = StreamTail::default();
                streamed(&pool, &key_hex, &model, &Rec::default(), &token, &mut tail)
                    .await
                    .unwrap_err();
                assert_eq!(server.connections(), 0);
                assert_eq!(
                    ledger_count(&key_hex).await,
                    0,
                    "provably never sent: released"
                );
            })
            .await;
        }

        #[tokio::test]
        async fn streaming_stays_behind_consent_price_and_cap() {
            with_temp_shared_db(|pool| async move {
                // No consent.
                let server = LoopbackServer::start(vec![with_usage()]);
                let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
                let mut tail = StreamTail::default();
                let err = streamed(
                    &pool,
                    &key_hex,
                    &model,
                    &Rec::default(),
                    &CancellationToken::new(),
                    &mut tail,
                )
                .await
                .unwrap_err();
                assert_eq!(block_code(&err), "spend_block.consent_required");
                assert_eq!(server.connections(), 0);
                assert!(tail.received_text.is_empty());
                // Consent but no cap.
                grant_consent(&pool, &key_hex, false).await;
                let err = streamed(
                    &pool,
                    &key_hex,
                    &model,
                    &Rec::default(),
                    &CancellationToken::new(),
                    &mut tail,
                )
                .await
                .unwrap_err();
                assert_eq!(block_code(&err), "spend_block.cap_required");
                assert_eq!(server.connections(), 0);
            })
            .await;
        }

        #[test]
        fn only_a_hosted_step_with_the_seam_streams() {
            let reg = crate::chat_stream::ChatStreamRegistry::default();
            let handle = reg.register(SPEND_USER, "run-1");
            let mut ctx = hosted_ctx(RunAttendance::Attended, "");
            assert!(!step_streams(&ctx), "no seam, no stream");
            ctx.stream = Some(handle.clone());
            assert!(step_streams(&ctx));
            ctx.effective_access = ExternalAccess::LocalOnly;
            assert!(!step_streams(&ctx), "a local step ignores the seam");
        }

        #[tokio::test]
        async fn a_call_without_the_seam_still_sends_stream_false() {
            with_temp_shared_db(|pool| async move {
                let server = LoopbackServer::start(vec![loopback_chat_ok()]);
                let (key_hex, model) = cleared(&pool, &server).await;
                call_hosted(&pool, &key_hex, &model, RunAttendance::Attended)
                    .await
                    .unwrap();
                let body = server.captured()[0].json();
                assert_eq!(body["stream"], false);
                assert!(body.get("stream_options").is_none());
            })
            .await;
        }
    }

    // -----------------------------------------------------------------------
    // items.id=735 (decisions.id=883): Gate 2 flags and notifies for every
    // external reply, streamed or not. One helper, reply_gate2.
    // -----------------------------------------------------------------------

    mod reply_gate2_rule {
        use super::*;
        use crate::conductor::privacy::logger::{FailLogger, TestLogger};

        fn track_of(fields: &[(&str, &str, u8)]) -> GateTrack {
            let mut gt = GateTrack::new();
            for (name, value, severity) in fields {
                gt.add_field(GateField {
                    field_name: (*name).to_owned(),
                    field_value: (*value).to_owned(),
                    sensitivity: Sensitivity::Personal,
                    sensitivity_severity: *severity,
                    source_id: "s".to_owned(),
                    abstraction_tier2: AbstractionPolicy::Omit,
                    abstraction_tier3: AbstractionPolicy::Omit,
                })
                .unwrap();
            }
            gt.seal();
            gt
        }

        fn gate_track_with(name: &str, value: &str) -> GateTrack {
            track_of(&[(name, value, 2)])
        }

        /// A migrated messages.db holding the run's assistant placeholder.
        async fn seed_reply(key_hex: &str, run_id: &str) {
            crate::persistence::migrations::migrate_messages_db(SPEND_USER, "persona-1", key_hex)
                .await
                .unwrap();
            crate::persistence::message_store::save_message(
                SPEND_USER,
                "persona-1",
                key_hex,
                "c1",
                "assistant",
                "the reply",
                Some(run_id),
            )
            .await
            .unwrap();
        }

        async fn reply_flagged(key_hex: &str) -> bool {
            crate::persistence::message_store::list_messages(SPEND_USER, "persona-1", key_hex, "c1")
                .await
                .unwrap()[0]
                .personal_flagged
        }

        #[tokio::test]
        async fn a_hit_flags_the_reply_logs_and_never_alters_it() {
            with_temp_shared_db(|_pool| async move {
                let key_hex = fixture_key_hex();
                seed_reply(&key_hex, "run-1").await;
                let ctx = hosted_ctx(RunAttendance::Attended, &key_hex);
                let gw = PrivacyGateway::new(TestLogger::new());
                let track = gate_track_with("home_city", "Springfield");
                let g2 = reply_gate2(
                    &ctx,
                    &ordinary(&ctx),
                    &gw,
                    &track,
                    "You live in Springfield, right?",
                    Some("fixture-host-a".to_owned()),
                    &[],
                    None,
                )
                .await;
                assert_eq!(
                    g2,
                    ReplyGate2 {
                        flagged: true,
                        audit_unavailable: false,
                        matched_severity: 2
                    }
                );
                assert_eq!(gw.logger.entry_count(), 1);
                assert_eq!(
                    gw.logger.entries()[0].event_type,
                    "gate2_contamination_detected"
                );
                assert!(reply_flagged(&key_hex).await);
                // The content is untouched.
                let m = crate::persistence::message_store::list_messages(
                    SPEND_USER,
                    "persona-1",
                    &key_hex,
                    "c1",
                )
                .await
                .unwrap();
                assert_eq!(m[0].content, "the reply");
                assert!(!m[0].is_error);
            })
            .await;
        }

        #[tokio::test]
        async fn no_hit_an_intentionally_shared_value_and_empty_text_flag_nothing() {
            with_temp_shared_db(|_pool| async move {
                let key_hex = fixture_key_hex();
                seed_reply(&key_hex, "run-1").await;
                let ctx = hosted_ctx(RunAttendance::Attended, &key_hex);
                let gw = PrivacyGateway::new(TestLogger::new());
                let track = gate_track_with("home_city", "Springfield");
                for (text, shared) in [
                    ("Nothing personal here", vec![]),
                    ("You live in Springfield", vec!["home_city".to_owned()]),
                    ("", vec![]),
                ] {
                    let g2 = reply_gate2(
                        &ctx,
                        &ordinary(&ctx),
                        &gw,
                        &track,
                        text,
                        None,
                        &shared,
                        None,
                    )
                    .await;
                    assert_eq!(g2, ReplyGate2::default(), "{text}");
                }
                assert_eq!(gw.logger.entry_count(), 0);
                assert!(!reply_flagged(&key_hex).await);
            })
            .await;
        }

        #[tokio::test]
        async fn an_audit_log_failure_keeps_the_reply_flags_it_and_still_reports_severity() {
            with_temp_shared_db(|_pool| async move {
                let key_hex = fixture_key_hex();
                seed_reply(&key_hex, "run-1").await;
                let ctx = hosted_ctx(RunAttendance::Attended, &key_hex);
                let gw = PrivacyGateway::new(FailLogger);
                let track = track_of(&[("diagnosis", "Hypertension", 3)]);
                let g2 = reply_gate2(
                    &ctx,
                    &ordinary(&ctx),
                    &gw,
                    &track,
                    "You have hypertension",
                    None,
                    &[],
                    None,
                )
                .await;
                // Not a failure of the step: flagged, with the audit entry
                // reported as unavailable, and the severity recovered.
                assert_eq!(
                    g2,
                    ReplyGate2 {
                        flagged: true,
                        audit_unavailable: true,
                        matched_severity: 3
                    }
                );
                assert!(reply_flagged(&key_hex).await);
            })
            .await;
        }

        #[tokio::test]
        async fn the_same_text_gives_the_same_outcome_streamed_or_not() {
            with_temp_shared_db(|_pool| async move {
                let reg = crate::chat_stream::ChatStreamRegistry::default();
                let handle = reg.register(SPEND_USER, "run-1");
                let mut whole = hosted_ctx(RunAttendance::Attended, "");
                let mut streamed = hosted_ctx(RunAttendance::Attended, "");
                whole.stream = None;
                streamed.stream = Some(handle);
                let track =
                    track_of(&[("home_city", "Springfield", 2), ("bank", "Acme Credit", 4)]);
                for text in [
                    "Springfield and Acme Credit",
                    "Only Springfield",
                    "Nothing here",
                ] {
                    let a = reply_gate2(
                        &whole,
                        &ordinary(&whole),
                        &PrivacyGateway::new(TestLogger::new()),
                        &track,
                        text,
                        None,
                        &[],
                        None,
                    )
                    .await;
                    let b = reply_gate2(
                        &streamed,
                        &ordinary(&streamed),
                        &PrivacyGateway::new(TestLogger::new()),
                        &track,
                        text,
                        None,
                        &[],
                        None,
                    )
                    .await;
                    assert_eq!(a, b, "{text}");
                }
            })
            .await;
        }

        #[tokio::test]
        async fn a_local_step_can_never_flag() {
            // Gate 1 at Raw abstraction shares every field, and gate2 skips
            // shared fields, so qr_local is inert (verified, not assumed).
            let gw = PrivacyGateway::new(TestLogger::new());
            let track = track_of(&[("home_city", "Springfield", 2), ("bank", "Acme Credit", 4)]);
            let g1 = gw
                .gate1(
                    "s1",
                    "run-1",
                    &track,
                    1,
                    1,
                    1,
                    ExternalAccess::LocalOnly,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(g1.fields_shared.len(), 2);
            let mut ctx = hosted_ctx(RunAttendance::Attended, "");
            ctx.effective_access = ExternalAccess::LocalOnly;
            ctx.execution_tier = 1;
            let g2 = reply_gate2(
                &ctx,
                &ordinary(&ctx),
                &gw,
                &track,
                "Springfield and Acme Credit",
                None,
                &g1.fields_shared,
                None,
            )
            .await;
            assert_eq!(g2, ReplyGate2::default());
            assert!(gw
                .logger
                .entries()
                .iter()
                .all(|e| e.event_type != "gate2_contamination_detected"));
            // The same track under an external floor does flag.
            let g1_ext = gw
                .gate1(
                    "s1",
                    "run-1",
                    &track,
                    2,
                    1,
                    2,
                    ExternalAccess::AnonymousRequired,
                    None,
                )
                .await
                .unwrap();
            let ext = hosted_ctx(RunAttendance::Attended, "");
            let g2_ext = reply_gate2(
                &ext,
                &ordinary(&ext),
                &gw,
                &track,
                "Springfield and Acme Credit",
                None,
                &g1_ext.fields_shared,
                None,
            )
            .await;
            assert!(g2_ext.flagged);
        }

        // -- Step 12: the recorded step ---------------------------------------

        fn hit(severity: i32) -> ReplyGate2 {
            ReplyGate2 {
                flagged: true,
                audit_unavailable: false,
                matched_severity: severity,
            }
        }

        #[test]
        fn a_hit_continues_the_run_and_records_the_flag_on_the_step() {
            let ctx = hosted_ctx(RunAttendance::Attended, "");
            let step = completed_step(&ctx, "the reply text", 1, 2, &hit(2));
            assert_eq!(step.content, "the reply text");
            assert!(step.personal_flagged);
            assert!(!step.audit_unavailable);
            let mut tt = TaskTrack::new();
            tt.add_step(step);
            assert_eq!(tt.last_output(), Some("the reply text"));
        }

        #[test]
        fn no_hit_leaves_the_step_exactly_as_before() {
            let ctx = hosted_ctx(RunAttendance::Attended, "");
            let step = completed_step(&ctx, "fine", 2, 2, &ReplyGate2::default());
            assert_eq!(step.sensitivity_severity, 2);
            assert!(!step.personal_flagged && !step.audit_unavailable);
            assert_eq!(step.routing_tier_used, 2);
            assert_eq!(step.output_var.as_deref(), Some("s1"));
        }

        #[test]
        fn a_severity_3_or_4_match_raises_the_step_and_the_ceiling() {
            let ctx = hosted_ctx(RunAttendance::Attended, "");
            for matched in [3, 4] {
                let step = completed_step(&ctx, "r", 1, 2, &hit(matched));
                assert_eq!(step.sensitivity_severity, matched);
                let mut tt = TaskTrack::new();
                tt.add_step(step);
                assert_eq!(tt.sensitivity_ceiling(), matched);
            }
        }

        #[test]
        fn a_severity_1_or_2_match_changes_nothing_observable() {
            let ctx = hosted_ctx(RunAttendance::Attended, "");
            for matched in [1, 2] {
                let step = completed_step(&ctx, "r", 2, 2, &hit(matched));
                assert_eq!(step.sensitivity_severity, 2, "matched {matched}");
            }
        }

        #[test]
        fn the_raise_is_a_max_and_never_lowers_the_computed_value() {
            let ctx = hosted_ctx(RunAttendance::Attended, "");
            let step = completed_step(&ctx, "r", 4, 2, &hit(2));
            assert_eq!(step.sensitivity_severity, 4);
            assert_eq!(ReplyGate2::default().apply_to(3), 3);
        }

        #[tokio::test]
        async fn gate_3_then_blocks_a_raised_step_toward_a_higher_tier() {
            let ctx = hosted_ctx(RunAttendance::Attended, "");
            async fn promote(step: &TaskStep) -> bool {
                PrivacyGateway::new(TestLogger::new())
                    .gate3(
                        "s1",
                        "run-1",
                        "test",
                        "s1",
                        &step.content,
                        step.sensitivity_severity as u8,
                        3,
                        ExternalAccess::Unrestricted,
                        2,
                        None,
                        None,
                        true,
                        SPEND_USER,
                        "persona-1",
                        "",
                    )
                    .await
                    .unwrap()
                    .blocked
            }
            // The step looked general (1); the reply echoed a financial field.
            assert!(promote(&completed_step(&ctx, "r", 1, 2, &hit(4))).await);
            assert!(promote(&completed_step(&ctx, "r", 1, 2, &hit(3))).await);
            // A personal-level match, or no hit, is not blocked.
            assert!(!promote(&completed_step(&ctx, "r", 1, 2, &hit(2))).await);
            assert!(!promote(&completed_step(&ctx, "r", 1, 2, &ReplyGate2::default())).await);
        }

        #[test]
        fn step_11_has_one_implementation_and_no_hold() {
            // Pin the source: the F6 hold is gone, and gate2 is called from
            // exactly one place, reply_gate2, which both paths share.
            let src = include_str!("executor.rs");
            let production = src.split("#[cfg(test)]").next().unwrap();
            assert!(!production.contains("InboundContamination"));
            assert_eq!(production.matches(".gate2(").count(), 1);
            let helper = production
                .split("async fn reply_gate2")
                .nth(1)
                .expect("reply_gate2");
            assert!(helper.contains(".gate2("));
            assert!(production.contains("let g2 = reply_gate2("));
        }
    }

    // -- items.id=778: Stop and save ----------------------------------------

    #[tokio::test]
    async fn a_call_is_dropped_when_the_stop_is_tripped() {
        let stop = tokio_util::sync::CancellationToken::new();
        let waiting = until_stopped(Some(&stop), std::future::pending::<u8>());
        stop.cancel();
        assert_eq!(waiting.await, None);
    }

    #[tokio::test]
    async fn a_call_that_finishes_first_keeps_its_result_and_no_token_just_awaits() {
        let stop = tokio_util::sync::CancellationToken::new();
        assert_eq!(until_stopped(Some(&stop), async { 7u8 }).await, Some(7));
        assert_eq!(until_stopped(None, async { 9u8 }).await, Some(9));
    }

    #[test]
    fn a_stopped_step_fails_with_a_stop_the_lifecycle_turns_into_a_cancel() {
        let f = stopped_failure(&hosted_ctx(RunAttendance::Attended, "k"));
        assert_eq!(f.action, FailureAction::Stop);
        assert_eq!(f.failure_mode.as_deref(), Some("USER_STOP"));
        assert_eq!(f.failure_mode.as_deref(), Some(USER_STOP_MODE));
        assert!(is_user_stop(&f));
        assert_eq!(f.plain_language, "This was stopped.");
        assert!(!f.is_recoverable);
    }

    // -----------------------------------------------------------------------
    // items.id=896 stage 3: resolved model-block steps. Real migrated shared.db
    // (the seeded local model rows and capability cells), loopback stand-ins for
    // the local model, no live calls.
    // -----------------------------------------------------------------------

    use crate::conductor::concurrency::ConductorScheduler;
    use crate::conductor::model_block::config::{
        Destination, Difficulty, ModelRange, ModelRequirement, ModelTaskType, OutputSize,
        QualityBar, QualityFloor, TaskNeed,
    };
    use crate::conductor::privacy::logger::TestLogger;

    /// Trust flag and forced port for a local-model call, reset on drop.
    struct LocalModelStub {
        server: LoopbackServer,
        _port: crate::ollama_sidecar::ForcedPortGuard,
    }

    impl LocalModelStub {
        fn start(replies: usize) -> Self {
            crate::ollama_sidecar::force_trust_for_test(true);
            let canned = (0..replies)
                .map(|_| {
                    Canned::json(
                        200,
                        r#"{"response":"A drafted reply.","done":true,"model":"stub"}"#,
                    )
                })
                .collect();
            let server = LoopbackServer::start(canned);
            let port: u16 = server.base().rsplit(':').next().unwrap().parse().unwrap();
            let _port = crate::ollama_sidecar::ForcedPortGuard::set(port);
            Self { server, _port }
        }
    }

    impl Drop for LocalModelStub {
        fn drop(&mut self) {
            crate::ollama_sidecar::force_trust_for_test(false);
        }
    }

    fn requirement(
        min: QualityBar,
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
                quality_floor: QualityFloor {
                    min,
                    max: QualityBar::High,
                },
                highest_tier: tier,
                destinations: destinations.to_vec(),
            },
            class_preference: None,
        }
    }

    /// A context for a resolved step: `access` is the ceiling, the document type
    /// is seeded under the bound name when given.
    fn resolved_ctx(
        req: Option<ModelRequirement>,
        access: ExternalAccess,
        document_type: Option<&str>,
        template: &str,
    ) -> StepContext {
        let mut ctx = hosted_ctx(RunAttendance::Attended, &fixture_key_hex());
        ctx.step.task_type = "creative_writing".to_owned();
        ctx.step.prompt_template = template.to_owned();
        ctx.step.requirement = req;
        ctx.step.routing_tier = access;
        ctx.step.external_access_override = Some(access);
        ctx.focus_external_access = access;
        ctx.effective_access = access;
        ctx.qr_hosted_provider_preference = None;
        let local = access == ExternalAccess::LocalOnly;
        ctx.execution_tier = if local { 1 } else { 2 };
        ctx.abstraction_tier = if local { 1 } else { 2 };
        ctx.raw_abstraction = 1;
        if let Some(d) = document_type {
            ctx.store_tokens.seeds.insert(
                crate::conductor::model_resolver_input::DOCUMENT_TYPE_NAME.to_owned(),
                d.to_owned(),
            );
        }
        ctx
    }

    async fn install_local_models(pool: &sqlx::SqlitePool) {
        sqlx::query("UPDATE providers SET installed = 1 WHERE provider_type = 'local_model'")
            .execute(pool)
            .await
            .unwrap();
    }

    async fn run_once(
        ctx: &StepContext,
        pool: &sqlx::SqlitePool,
    ) -> (
        Result<Option<FailureResult>, ConductorError>,
        TaskTrack,
        PrivacyGateway<TestLogger>,
    ) {
        run_once_with(ctx, pool, &PersonalTrack::new()).await
    }

    /// run_once with a personal track (items.id=921: the privacy treatment is
    /// observable through what Gate 1 does with the track's fields).
    async fn run_once_with(
        ctx: &StepContext,
        pool: &sqlx::SqlitePool,
        personal: &PersonalTrack,
    ) -> (
        Result<Option<FailureResult>, ConductorError>,
        TaskTrack,
        PrivacyGateway<TestLogger>,
    ) {
        let mut track = TaskTrack::new();
        let gateway = PrivacyGateway::new(TestLogger::new());
        let out = StepExecutor::new()
            .execute_once(
                ctx,
                0,
                personal,
                &mut track,
                &mut SharedStateTrack::new(),
                &FailureHandler::new(ctx.effective_access),
                &gateway,
                &Arc::new(ConductorScheduler::new()),
                None,
                pool,
            )
            .await;
        (out, track, gateway)
    }

    fn parked_code(r: &FailureResult) -> String {
        assert_eq!(r.action, FailureAction::AwaitUser, "{r:?}");
        assert_eq!(r.failure_mode.as_deref(), Some("F10"));
        r.metadata.as_ref().unwrap()["code"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn local_req() -> ModelRequirement {
        requirement(
            QualityBar::Basic,
            &[Destination::Local],
            ExternalAccess::AnonymousPreferred,
        )
    }

    // -- items.id=901 stage 7: the resolver's stop as the park a run keeps -------

    /// The real producer: resolver_stop's parameters, turned into the failure the handler
    /// makes (failure.rs copies the params to metadata unchanged), then into the stored park.
    /// Options are carried in rank order with their standing words, the producer's own cap
    /// (16) is reported as "omitted", and the JSON stays inside the store's cap.
    #[test]
    fn the_resolvers_too_weak_stop_becomes_a_park_with_its_ranked_options() {
        use crate::conductor::model_capability::{AttainedBar, CapabilitySubject};
        use crate::conductor::model_resolver::{
            CostTier, ModelRef, OptionEntry, Resolution, Standing,
        };
        let entry = |i: usize, standing: Standing| OptionEntry {
            model: ModelRef {
                subject: CapabilitySubject::Local,
                provider_id: String::new(),
                model_id: format!("m-{i:02}"),
            },
            cost_tier: CostTier::Local,
            est_cost_micros: 0,
            standing,
        };
        let options: Vec<OptionEntry> = (0..20)
            .map(|i| {
                entry(
                    i,
                    if i < 2 {
                        Standing::BelowBar(AttainedBar::Basic)
                    } else {
                        Standing::NotMeasured
                    },
                )
            })
            .collect();
        let stop = resolver_stop(&Resolution::TooWeak {
            required: crate::conductor::model_block::config::QualityBar::Standard,
            best_below_bar: Some(entry(0, Standing::BelowBar(AttainedBar::Basic))),
            options,
        })
        .expect("a stop");
        let ConductorError::SpendBlock { params, .. } = stop else {
            panic!("a spend block")
        };
        let meta = params.as_object().unwrap().clone().into_iter().collect();
        let failure = FailureResult {
            action: FailureAction::AwaitUser,
            failure_mode: Some("F10".to_owned()),
            plain_language: "resolver.too_weak".to_owned(),
            is_recoverable: true,
            severity: crate::conductor::failure::FailureSeverity::Require,
            action_id: Some("draft".to_owned()),
            focus_id: Some("f".to_owned()),
            metadata: Some(meta),
        };
        let park = crate::conductor::run_park::from_failure(&failure).expect("a park");
        assert_eq!(park.code, Code::ResolverTooWeak);
        assert!(park.params_json.len() <= crate::persistence::run_park_store::PARAMS_MAX_BYTES);
        let v: serde_json::Value = serde_json::from_str(&park.params_json).unwrap();
        assert_eq!(v["required"], "standard");
        let kept = v["options"].as_array().unwrap();
        assert_eq!(kept.len(), 16, "the producer's own cap");
        assert_eq!(v["options_total"], 20);
        assert_eq!(v["options_omitted"], 4);
        for (i, o) in kept.iter().enumerate() {
            assert_eq!(o["model_id"], format!("m-{i:02}"), "rank order");
            assert_eq!(o["subject"], "local");
        }
        assert_eq!(kept[0]["standing"], "below_bar");
        assert_eq!(kept[0]["attained"], "basic");
        assert_eq!(kept[5]["standing"], "not_measured");
        assert_eq!(v["best_below_bar"]["model_id"], "m-00");
    }

    // -- condition 3: no document type, the step parks through TooWeak ---------

    #[tokio::test]
    async fn without_the_bound_document_type_the_step_parks_with_too_weak() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let ctx = resolved_ctx(
                Some(local_req()),
                ExternalAccess::LocalOnly,
                None,
                "Write it.",
            );
            let (out, track, gateway) = run_once(&ctx, &pool).await;
            let failure = out.unwrap().expect("the step parks");
            assert_eq!(parked_code(&failure), "resolver.too_weak");
            let meta = failure.metadata.unwrap();
            assert_eq!(meta["required"], "basic");
            assert!(meta["best_below_bar"].is_null(), "nothing was measured");
            let options = meta["options"].as_array().unwrap();
            assert_eq!(options.len(), 3, "every ready local model is listed");
            assert!(options.iter().all(|o| o["standing"] == "not_measured"));
            // Nothing ran: no gate, no model, no step recorded.
            assert_eq!(gateway.logger.entry_count(), 0);
            assert!(track.steps().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn an_unknown_document_type_is_no_profile_and_parks_the_same_way() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let ctx = resolved_ctx(
                Some(local_req()),
                ExternalAccess::LocalOnly,
                Some("not-a-shipped-type"),
                "Write it.",
            );
            let (out, _, _) = run_once(&ctx, &pool).await;
            assert_eq!(parked_code(&out.unwrap().unwrap()), "resolver.too_weak");
        })
        .await;
    }

    /// The i18n parity of the two new stops: every resolver code has a key in the
    /// frontend's en.json (placeholder copy, Chat-BRAND owns the wording).
    #[test]
    fn every_resolver_code_has_an_i18n_key() {
        let en: serde_json::Value =
            serde_json::from_str(include_str!("../../../frontend/src/i18n/en.json")).unwrap();
        for code in [Code::ResolverTooWeak, Code::ResolverNoCandidate] {
            let tail = code.as_str().strip_prefix("resolver.").unwrap();
            let text = en["resolver"]["stop"][tail]
                .as_str()
                .unwrap_or_else(|| panic!("en.json has no resolver.stop.{tail}"));
            assert!(!text.trim().is_empty());
            // Placeholder copy names no model and no provider.
            assert!(!text.contains("llama") && !text.contains("qwen"), "{text}");
        }
    }

    // -- selection --------------------------------------------------------------

    #[tokio::test]
    async fn the_selected_document_type_decides_the_model_and_the_ranked_fallbacks() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let ctx = resolved_ctx(
                Some(local_req()),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                "Write it.",
            );
            let sel = select_model_for_block(&pool, &ctx, ctx.step.requirement.as_ref().unwrap())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            // llama3.2:3b is the smallest model but Jason read it below basic for this type
            // (items.id=899), so the smallest one that reaches the bar is chosen.
            assert_eq!(sel.selected.model_id, "llama3.1:8b");
            assert!(sel.selected.provider_id.is_none() && sel.selected.hosted_model.is_none());
            // The model below the bar is not a fallback; the other qualifier is.
            assert_eq!(
                sel.fallbacks
                    .iter()
                    .map(|f| f.model_id.as_str())
                    .collect::<Vec<_>>(),
                ["qwen2.5:7b"]
            );
        })
        .await;
    }

    /// The pin for an uncalibrated LOCAL cell at the glue level: the only ready
    /// model has no cell for this type, so it is listed as not measured and is not
    /// run.
    #[tokio::test]
    async fn an_uncalibrated_local_cell_is_not_chosen_and_is_listed_as_not_measured() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            // Turn off the two other models, and remove qwen2.5:7b's cell for this type so
            // it is unmeasured (the shipped data rates every model for every shipped type
            // since items.id=899, so the unmeasured case is made here).
            sqlx::query(
                "UPDATE providers SET qr_disabled_by_user = 1
                  WHERE local_model_tag IN ('llama3.2:3b', 'llama3.1:8b')",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "DELETE FROM model_capabilities
                  WHERE model_id = 'qwen2.5:7b' AND profile = 'rma_letter'",
            )
            .execute(&pool)
            .await
            .unwrap();
            let ctx = resolved_ctx(
                Some(local_req()),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                "Write it.",
            );
            let err = select_model_for_block(&pool, &ctx, ctx.step.requirement.as_ref().unwrap())
                .await
                .err()
                .expect("an unmeasured model is not selected");
            assert_eq!(block_code(&err), "resolver.too_weak");
            let ConductorError::SpendBlock { params, .. } = &err else {
                panic!()
            };
            let options = params["options"].as_array().unwrap();
            assert_eq!(options.len(), 1);
            assert_eq!(options[0]["model_id"], "qwen2.5:7b");
            assert_eq!(options[0]["standing"], "not_measured");
        })
        .await;
    }

    /// And a HOSTED model with no cell, ready in every other way (key, consent,
    /// price): excluded and listed as not measured. With a cell it is chosen, and
    /// the selection carries the provider and the model row together.
    #[tokio::test]
    async fn an_unmeasured_hosted_model_is_excluded_and_a_measured_one_is_selected_whole() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, _model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, true).await;
            let req = requirement(
                QualityBar::Basic,
                &[Destination::HostedApi],
                ExternalAccess::AnonymousPreferred,
            );
            let mut ctx = resolved_ctx(
                Some(req),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                "Write it.",
            );
            ctx.key_hex = key_hex.clone();

            let err = select_model_for_block(&pool, &ctx, ctx.step.requirement.as_ref().unwrap())
                .await
                .err()
                .expect("an unmeasured hosted model is not selected");
            assert_eq!(block_code(&err), "resolver.too_weak");
            let ConductorError::SpendBlock { params, .. } = &err else {
                panic!()
            };
            let hosted: Vec<&serde_json::Value> = params["options"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|o| o["subject"] == "hosted")
                .collect();
            assert_eq!(hosted.len(), 1);
            assert_eq!(hosted[0]["provider_id"], "fixture-host-a");
            assert_eq!(hosted[0]["standing"], "not_measured");

            // Measured: now it is the choice, and the pair is whole.
            sqlx::query(
                "INSERT INTO model_capabilities
                   (subject_kind, provider_id, model_id, task_type, profile, attained_bar,
                    model_class, sample_count, measured_at, provenance)
                 VALUES ('hosted', 'fixture-host-a', 'fixture-model-a', 'creative_writing',
                         'rma_letter', 'basic', 'large', 1, '2026-11-13', 'harness_hosted')",
            )
            .execute(&pool)
            .await
            .unwrap();
            let sel = select_model_for_block(&pool, &ctx, ctx.step.requirement.as_ref().unwrap())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert_eq!(sel.selected.provider_id.as_deref(), Some("fixture-host-a"));
            let row = sel.selected.hosted_model.as_ref().expect("the model row");
            assert_eq!(row.provider_id, "fixture-host-a");
            assert_eq!(sel.selected.model_id, row.model_id);
            assert!(sel.fallbacks.is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn a_hosted_model_without_a_key_a_consent_or_a_price_is_not_a_candidate() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let req = requirement(
                QualityBar::Basic,
                &[Destination::HostedApi],
                ExternalAccess::AnonymousPreferred,
            );
            let measured = |pool: sqlx::SqlitePool| async move {
                sqlx::query(
                    "INSERT OR REPLACE INTO model_capabilities
                       (subject_kind, provider_id, model_id, task_type, profile, attained_bar,
                        model_class, sample_count, measured_at, provenance)
                     VALUES ('hosted', 'fixture-host-a', 'fixture-model-a', 'creative_writing',
                             'rma_letter', 'high', 'large', 1, '2026-11-13', 'harness_hosted')",
                )
                .execute(&pool)
                .await
                .unwrap();
            };
            // Priced, keyed, but never consented.
            let (key_hex, _m) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            measured(pool.clone()).await;
            let mut ctx = resolved_ctx(
                Some(req.clone()),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                "Write it.",
            );
            ctx.key_hex = key_hex.clone();
            let err = select_model_for_block(&pool, &ctx, &req)
                .await
                .err()
                .expect("no consent");
            assert_eq!(block_code(&err), "resolver.no_candidate");
            // Consented: chosen.
            grant_consent(&pool, &key_hex, true).await;
            assert!(select_model_for_block(&pool, &ctx, &req).await.is_ok());
            // No session key: nothing hosted is usable.
            ctx.key_hex = String::new();
            assert!(select_model_for_block(&pool, &ctx, &req).await.is_err());
            ctx.key_hex = key_hex.clone();
            // Unpriced: never chosen automatically, even measured and consented.
            sqlx::query("UPDATE provider_models SET pricing = NULL")
                .execute(&pool)
                .await
                .unwrap();
            let err = select_model_for_block(&pool, &ctx, &req)
                .await
                .err()
                .expect("an unpriced model");
            assert_eq!(block_code(&err), "resolver.no_candidate");
        })
        .await;
    }

    #[tokio::test]
    async fn a_hosted_range_under_a_local_only_ceiling_resolves_to_nothing() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, _m) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, true).await;
            let req = requirement(
                QualityBar::Basic,
                &[Destination::HostedApi],
                ExternalAccess::Unrestricted,
            );
            // The run's own ceiling is local only: the requirement cannot widen it.
            let mut ctx = resolved_ctx(
                Some(req.clone()),
                ExternalAccess::LocalOnly,
                Some("rma_letter"),
                "Write it.",
            );
            ctx.key_hex = key_hex;
            let err = select_model_for_block(&pool, &ctx, &req)
                .await
                .err()
                .expect("no destination");
            assert_eq!(block_code(&err), "resolver.no_candidate");
            let ConductorError::SpendBlock { params, .. } = &err else {
                panic!()
            };
            assert_eq!(params["reason"], "no_destination");
        })
        .await;
    }

    // -- the network predicate ---------------------------------------------------

    /// A resolved step whose ceiling allows hosted but whose chosen model is local
    /// takes the LOCAL call, with no hosted traffic and no panic in the hosted
    /// branch. The ordinary predicate would have sent it down the hosted branch.
    #[tokio::test]
    async fn a_local_choice_under_an_external_ceiling_takes_the_local_path() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let stub = LocalModelStub::start(1);
            let ctx = resolved_ctx(
                Some(requirement(
                    QualityBar::Basic,
                    &[Destination::Local, Destination::HostedApi],
                    ExternalAccess::AnonymousPreferred,
                )),
                ExternalAccess::AnonymousPreferred,
                Some("complaint_email"),
                "Write it.",
            );
            let (out, track, gateway) = run_once(&ctx, &pool).await;
            assert!(out.unwrap().is_none(), "the step completed");
            let seen = stub.server.captured();
            assert_eq!(seen.len(), 1, "one local call");
            assert!(seen[0].path.contains("/api/generate"));
            assert_eq!(seen[0].json()["model"], "llama3.2:3b");
            assert_eq!(track.steps().len(), 1);
            // Gate 1 ran once, for a local destination (no provider).
            assert!(gateway
                .logger
                .entries()
                .iter()
                .all(|e| e.provider.is_none()));
        })
        .await;
    }

    // -- the context fallback ----------------------------------------------------

    /// A template of fixed text whose size estimate is inside the first model's
    /// hard limit, so the resolver keeps it and the post-render check rejects it.
    fn template_that_overflows_the_first_window() -> String {
        let mut n = 0;
        loop {
            let text = "x".repeat(n);
            let est =
                crate::providers::ollama_client::estimate_token_count(&text, "creative_writing");
            if est as f64 / 4096.0 >= 0.95 {
                assert!(est <= 4096, "the estimate stepped past the window");
                return text;
            }
            n += 4;
        }
    }

    #[tokio::test]
    async fn the_context_fallback_changes_the_model_only() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let template = template_that_overflows_the_first_window();

            // The same step with a short template: no fallback, the first model.
            let stub = LocalModelStub::start(1);
            let short = resolved_ctx(
                Some(local_req()),
                ExternalAccess::AnonymousPreferred,
                Some("complaint_email"),
                "Write it.",
            );
            let (out, _, gateway_short) = run_once(&short, &pool).await;
            assert!(out.unwrap().is_none());
            assert_eq!(stub.server.captured()[0].json()["model"], "llama3.2:3b");
            let entries_short = gateway_short.logger.entries().len();
            drop(stub);

            // The long one does not fit that model's window, so it moves to the
            // next ranked local model, and nothing else about the call changes.
            let stub = LocalModelStub::start(1);
            let long = resolved_ctx(
                Some(local_req()),
                ExternalAccess::AnonymousPreferred,
                Some("complaint_email"),
                &template,
            );
            let (out, track, gateway) = run_once(&long, &pool).await;
            assert!(
                out.unwrap().is_none(),
                "the step completed after the fallback"
            );
            let seen = stub.server.captured();
            assert_eq!(seen.len(), 1, "one call, to the fallback only");
            let body = seen[0].json();
            assert_eq!(body["model"], "llama3.1:8b");
            assert_eq!(
                body["prompt"],
                template.as_str(),
                "the same prompt, not re-rendered"
            );
            assert_eq!(track.steps().len(), 1);
            // The disclosure log: Gate 1 ran once, as it does without a fallback
            // (the same number of entries), every entry for a local destination.
            let entries = gateway.logger.entries();
            assert_eq!(entries.len(), entries_short);
            assert!(entries.iter().all(|e| e.provider.is_none()));
        })
        .await;
    }

    #[tokio::test]
    async fn a_step_that_fixes_its_own_window_never_falls_back() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let stub = LocalModelStub::start(1);
            let mut ctx = resolved_ctx(
                Some(local_req()),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                &template_that_overflows_the_first_window(),
            );
            ctx.step
                .options_override
                .insert("num_ctx".to_owned(), serde_json::json!(4096));
            let (out, _, _) = run_once(&ctx, &pool).await;
            let failure = out.unwrap().expect("the step stops");
            assert!(
                failure.plain_language.contains("too long"),
                "{}",
                failure.plain_language
            );
            assert!(stub.server.captured().is_empty(), "no model was called");
        })
        .await;
    }

    // -- steps that must not change ----------------------------------------------

    /// A step with no requirement keeps Step 4.5: at an external ceiling with no
    /// provider set up it parks with the "no provider" failure, exactly as before.
    /// The same context WITH a requirement goes to the resolver instead.
    #[tokio::test]
    async fn a_step_without_a_requirement_keeps_step_4_5_and_a_resolved_one_skips_it() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let plain = resolved_ctx(None, ExternalAccess::AnonymousPreferred, None, "Write it.");
            let (out, _, _) = run_once(&plain, &pool).await;
            let failure = out.unwrap().expect("parks");
            assert!(
                failure
                    .plain_language
                    .contains("No Tier 1.5 AI provider is set up yet"),
                "{}",
                failure.plain_language
            );
            assert_ne!(failure.failure_mode.as_deref(), Some("F10_RESOLVER"));

            let resolved = resolved_ctx(
                Some(local_req()),
                ExternalAccess::AnonymousPreferred,
                None,
                "Write it.",
            );
            let (out, _, _) = run_once(&resolved, &pool).await;
            let failure = out.unwrap().expect("parks");
            assert_eq!(parked_code(&failure), "resolver.too_weak");
        })
        .await;
    }

    /// A step with no requirement keeps select_model()'s own choice: the default
    /// local model for its task type, whatever the capability data says.
    #[tokio::test]
    async fn a_step_without_a_requirement_keeps_the_hardcoded_local_choice() {
        with_temp_shared_db(|pool| async move {
            // Nothing is installed: select_model() does not look.
            let stub = LocalModelStub::start(1);
            let ctx = resolved_ctx(None, ExternalAccess::LocalOnly, None, "Write it.");
            let (out, _, _) = run_once(&ctx, &pool).await;
            assert!(out.unwrap().is_none());
            // task_type creative_writing is the default arm of select_model().
            assert_eq!(stub.server.captured()[0].json()["model"], "llama3.1:8b");
        })
        .await;
    }

    /// A bound API Chat run keeps its bound destination whatever the step carries:
    /// a step with a requirement on a bound run is NOT resolved. The binding names
    /// a provider that is not in the catalog, so select_bound_model() stops the
    /// run with its own error, and the resolver's code never appears.
    #[tokio::test]
    async fn a_bound_api_chat_run_ignores_the_requirement() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let mut ctx = resolved_ctx(
                Some(local_req()),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                "Write it.",
            );
            ctx.bound = Some(crate::api_chat::BoundDestination {
                provider_id: "no-such-provider".to_owned(),
                model_id: "no-such-model".to_owned(),
            });
            ctx.qr_hosted_provider_preference = Some("no-such-provider".to_owned());
            let (out, track, gateway) = run_once(&ctx, &pool).await;
            match out {
                Err(ConductorError::SpendBlock { params, .. }) => {
                    let code = params["code"].as_str().unwrap();
                    assert!(!code.starts_with("resolver."), "{code}");
                }
                Ok(Some(f)) => {
                    assert!(
                        !f.metadata
                            .as_ref()
                            .and_then(|m| m["code"].as_str().map(str::to_owned))
                            .unwrap_or_default()
                            .starts_with("resolver."),
                        "{f:?}"
                    );
                }
                other => panic!("the bound run must stop on its own binding: {other:?}"),
            }
            assert!(track.steps().is_empty());
            assert_eq!(gateway.logger.entry_count(), 0);
        })
        .await;
    }

    // -- the privacy-critical sites ------------------------------------------------

    /// SelectedModel's two constructors never produce a provider without its model
    /// row or the reverse: the generation branch's `unreachable!` depends on it.
    #[test]
    fn a_selected_model_is_either_wholly_local_or_wholly_hosted() {
        let local = SelectedModel::local("some-tag".to_owned());
        assert!(local.provider_id.is_none() && local.hosted_model.is_none());
        let row = ProviderModel {
            id: "p:m".to_owned(),
            provider_id: "p".to_owned(),
            model_id: "m".to_owned(),
            context_window_tokens: 1000,
            is_default: false,
            created_at: "t".to_owned(),
            pricing: None,
            request_options: None,
            token_factor: 1.0,
            output_overhead: 1.0,
            status: crate::persistence::provider_store::ModelStatus::Active,
            retires_at: None,
            requires_paid: false,
            qr_recommended: false,
            qr_recommendation_rank: None,
            retains_data_override: None,
            trains_on_data_override: None,
            retention_days_override: None,
            context_window_source: None,
            context_window_as_of: None,
        };
        let hosted = SelectedModel::hosted(row);
        assert_eq!(hosted.provider_id.as_deref(), Some("p"));
        assert_eq!(hosted.model_id, "m");
        assert_eq!(hosted.hosted_model.as_ref().unwrap().provider_id, "p");
    }

    // -- items.id=921: the privacy treatment keys off the selected destination -----

    /// A track whose one field is clamped by the floor: shared as it is at tier 1,
    /// withheld at tier 2 (so an external treatment has something to ask about).
    fn clamping_track() -> PersonalTrack {
        use crate::conductor::types::PersonalField as CF;
        let mut pt = PersonalTrack::new();
        pt.add_field(CF {
            field_name: "diagnosis".to_owned(),
            field_value: "X".to_owned(),
            sensitivity: "medical".to_owned(),
            sensitivity_severity: 3,
            source_id: "s".to_owned(),
            abstraction_tier2: "omit".to_owned(),
            abstraction_tier3: "omit".to_owned(),
        })
        .unwrap();
        pt
    }

    fn gate1_entries(gateway: &PrivacyGateway<TestLogger>) -> Vec<DisclosureLogEntry> {
        gateway
            .logger
            .entries()
            .into_iter()
            .filter(|e| e.event_type == "gate1_pass")
            .collect()
    }

    /// A fixture host the resolver can pick: priced, keyed, consented, measured.
    async fn hosted_pick_rig(pool: &sqlx::SqlitePool) -> String {
        let server = LoopbackServer::start(vec![loopback_chat_ok()]);
        let (key_hex, _m) = spend_rig(pool, &server, Some(PRICE_TEXT)).await;
        sqlx::query(
            "INSERT OR REPLACE INTO model_capabilities
               (subject_kind, provider_id, model_id, task_type, profile, attained_bar,
                model_class, sample_count, measured_at, provenance)
             VALUES ('hosted', 'fixture-host-a', 'fixture-model-a', 'creative_writing',
                     'rma_letter', 'high', 'large', 1, '2026-11-13', 'harness_hosted')",
        )
        .execute(pool)
        .await
        .unwrap();
        grant_consent(pool, &key_hex, true).await;
        key_hex
    }

    fn hosted_row() -> ProviderModel {
        ProviderModel {
            id: "p:m".to_owned(),
            provider_id: "p".to_owned(),
            model_id: "m".to_owned(),
            context_window_tokens: 1000,
            is_default: false,
            created_at: "t".to_owned(),
            pricing: None,
            request_options: None,
            token_factor: 1.0,
            output_overhead: 1.0,
            status: crate::persistence::provider_store::ModelStatus::Active,
            retires_at: None,
            requires_paid: false,
            qr_recommended: false,
            qr_recommendation_rank: None,
            retains_data_override: None,
            trains_on_data_override: None,
            retention_days_override: None,
            context_window_source: None,
            context_window_as_of: None,
        }
    }

    /// The case items.id=921 exists for. Under an external ceiling the resolver
    /// picks a LOCAL model: nothing leaves the device, so Gate 1 runs as it does for
    /// a local step (tier 1, no provider, nothing clamped), there is no floor-consent
    /// prompt, a contaminated voice attribute is stripped rather than halting the
    /// step, the sensitivity comes from the template, and the step is recorded at
    /// tier 1.
    #[tokio::test]
    async fn a_local_pick_under_an_external_ceiling_is_gated_and_recorded_as_local() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let stub = LocalModelStub::start(1);
            let ctx = resolved_ctx(
                Some(requirement(
                    QualityBar::Basic,
                    &[Destination::Local, Destination::HostedApi],
                    ExternalAccess::AnonymousPreferred,
                )),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                "Write it. {diagnosis}",
            );
            // The ceiling values are the external ones lifecycle computes (the
            // fixture sets 2/2/1): without the fix this track is clamped and the
            // step parks on floor consent.
            assert_eq!((ctx.execution_tier, ctx.abstraction_tier), (2, 2));
            let mut personal = clamping_track();
            let mut voice = IndexMap::new();
            voice.insert(
                "tone".to_owned(),
                "mail me at someone@example.com".to_owned(),
            );
            personal.set_voice_profile(voice).unwrap();

            let (out, track, gateway) = run_once_with(&ctx, &pool, &personal).await;
            assert!(out.unwrap().is_none(), "no floor-consent prompt, no halt");
            assert_eq!(stub.server.captured().len(), 1, "one local call");

            let g1 = gate1_entries(&gateway);
            assert_eq!(g1.len(), 1);
            assert_eq!(g1[0].execution_tier, 1);
            assert_eq!(g1[0].abstraction_tier, Some(1));
            assert!(g1[0].provider.is_none());
            assert_eq!(g1[0].fields_shared, vec!["diagnosis".to_owned()]);
            assert!(g1[0].fields_withheld.is_empty());
            let all = gateway.logger.entries();
            assert!(!all.iter().any(|e| e.event_type == "floor_consent_auto"));
            let contamination: Vec<_> = all
                .iter()
                .filter(|e| e.event_type == "voice_profile_contamination")
                .collect();
            assert_eq!(contamination.len(), 1, "stripped and logged, not fatal");
            assert_eq!(contamination[0].execution_tier, 1);

            let step = &track.steps()[0];
            assert_eq!(step.routing_tier_used, 1);
            assert_eq!(
                step.sensitivity_severity, 3,
                "a local destination scores the template's tokens"
            );
        })
        .await;
    }

    /// A HOSTED pick keeps the external treatment at its reach: Gate 1 abstracts at
    /// tier 2 or more for its provider, the clamped field is withheld, and the step
    /// parks on floor consent before anything is sent.
    #[tokio::test]
    async fn a_hosted_pick_keeps_the_external_treatment_and_the_floor_consent_prompt() {
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let mut ctx = resolved_ctx(
                Some(requirement(
                    QualityBar::Basic,
                    &[Destination::HostedApi],
                    ExternalAccess::AnonymousPreferred,
                )),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                "Write it.",
            );
            ctx.key_hex = key_hex;
            ctx.plan = hosted_pin_approval();
            let (out, track, gateway) = run_once_with(&ctx, &pool, &clamping_track()).await;
            let failure = out.unwrap().expect("the step parks");
            assert_eq!(failure.action, FailureAction::AwaitFloorConsent);
            let meta = failure.metadata.unwrap();
            assert_eq!(
                meta["floor_clamped_fields"],
                serde_json::json!(["diagnosis"])
            );
            assert_eq!(meta["abstraction_tier"], 2);
            assert_eq!(
                meta["execution_tier"], 3,
                "the reach of anonymous_preferred"
            );
            let g1 = gate1_entries(&gateway);
            assert_eq!(g1.len(), 1);
            assert_eq!(g1[0].provider.as_deref(), Some("fixture-host-a"));
            assert_eq!(g1[0].abstraction_tier, Some(2));
            assert_eq!(g1[0].execution_tier, 3);
            assert_eq!(g1[0].fields_withheld, vec!["diagnosis".to_owned()]);
            assert!(track.steps().is_empty(), "nothing ran");
        })
        .await;
    }

    /// The resolved hosted pick and an ORDINARY external step park on the same
    /// floor consent with the same clamped fields: a resolved hosted step is gated
    /// no looser than an ordinary one at the same ceiling.
    #[tokio::test]
    async fn a_hosted_pick_is_gated_like_an_ordinary_external_step() {
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let mut resolved = resolved_ctx(
                Some(requirement(
                    QualityBar::Basic,
                    &[Destination::HostedApi],
                    ExternalAccess::AnonymousPreferred,
                )),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                "Write it.",
            );
            resolved.key_hex = key_hex.clone();
            resolved.plan = hosted_pin_approval();
            let mut ordinary_ctx =
                resolved_ctx(None, ExternalAccess::AnonymousPreferred, None, "Write it.");
            ordinary_ctx.key_hex = key_hex;
            ordinary_ctx.qr_hosted_provider_preference = Some("fixture-host-a".to_owned());
            let (a, _, ga) = run_once_with(&resolved, &pool, &clamping_track()).await;
            let (b, _, gb) = run_once_with(&ordinary_ctx, &pool, &clamping_track()).await;
            let (a, b) = (a.unwrap().expect("parks"), b.unwrap().expect("parks"));
            assert_eq!(a.action, FailureAction::AwaitFloorConsent);
            assert_eq!(b.action, FailureAction::AwaitFloorConsent);
            assert_eq!(
                a.metadata.as_ref().unwrap()["floor_clamped_fields"],
                b.metadata.as_ref().unwrap()["floor_clamped_fields"]
            );
            let (ea, eb) = (gate1_entries(&ga), gate1_entries(&gb));
            assert_eq!(ea.len(), 1);
            assert_eq!(eb.len(), 1);
            assert_eq!(ea[0].abstraction_tier, eb[0].abstraction_tier);
            assert_eq!(ea[0].fields_withheld, eb[0].fields_withheld);
            assert_eq!(ea[0].provider, eb[0].provider);
        })
        .await;
    }

    /// A step with no requirement: the treatment is exactly the ceiling values, for
    /// every ceiling (the ordinary external step above also parks as it always did).
    #[test]
    fn a_step_without_a_requirement_is_treated_by_its_ceiling_values() {
        for access in [
            ExternalAccess::LocalOnly,
            ExternalAccess::AnonymousRequired,
            ExternalAccess::AnonymousPreferred,
            ExternalAccess::Unrestricted,
        ] {
            let ctx = resolved_ctx(None, access, None, "x");
            assert!(!is_resolved(&ctx));
            let t = derive_treatment(&ctx, None, &SelectedModel::local("any".to_owned())).unwrap();
            assert_eq!(t, ordinary(&ctx), "{access:?}");
        }
    }

    /// A bound API Chat run carrying a requirement is NOT resolved: its treatment is
    /// the ceiling values, and it still streams at an external ceiling.
    #[test]
    fn a_bound_api_chat_run_is_treated_by_its_ceiling_values_and_still_streams() {
        let reg = crate::chat_stream::ChatStreamRegistry::default();
        let mut ctx = resolved_ctx(
            Some(local_req()),
            ExternalAccess::AnonymousPreferred,
            Some("rma_letter"),
            "x",
        );
        ctx.bound = Some(crate::api_chat::BoundDestination {
            provider_id: "p".to_owned(),
            model_id: "m".to_owned(),
        });
        ctx.stream = Some(reg.register(SPEND_USER, "run-1"));
        assert!(!is_resolved(&ctx));
        assert!(step_streams(&ctx), "a bound external run streams");
        let t = derive_treatment(&ctx, None, &SelectedModel::hosted(hosted_row())).unwrap();
        assert_eq!(t, ordinary(&ctx));
    }

    /// A resolved step never streams (the retry loop cannot see its pick), even at
    /// an external ceiling with a stream attached.
    #[test]
    fn a_resolved_step_never_streams() {
        let reg = crate::chat_stream::ChatStreamRegistry::default();
        let mut ctx = resolved_ctx(
            Some(local_req()),
            ExternalAccess::AnonymousPreferred,
            Some("rma_letter"),
            "x",
        );
        ctx.stream = Some(reg.register(SPEND_USER, "run-2"));
        assert!(is_resolved(&ctx));
        assert!(!step_streams(&ctx));
        // The retry machinery then watches the lease token, as for a local step.
        assert!(
            effective_stop(&ctx).is_none(),
            "this ctx has no lease token"
        );
    }

    /// The disagreements derive_treatment refuses, through the executor's own
    /// glue: a hosted pick under a local-only ceiling (the shipped wording), a hosted
    /// pick from a range that reaches only the device, and a half-hosted selection.
    #[test]
    fn a_disagreement_between_the_ceiling_and_the_selection_is_refused() {
        use crate::conductor::step_treatment::TreatmentError;
        let req = |tier| requirement(QualityBar::Basic, &[Destination::HostedApi], tier);

        let local_only = resolved_ctx(
            Some(req(ExternalAccess::AnonymousPreferred)),
            ExternalAccess::LocalOnly,
            None,
            "x",
        );
        let e = derive_treatment(
            &local_only,
            local_only.step.requirement.as_ref(),
            &SelectedModel::hosted(hosted_row()),
        )
        .unwrap_err();
        assert_eq!(e, TreatmentError::HostedUnderLocalOnly);
        assert_eq!(
            e.plain_language("s1"),
            "Step 's1' resolved to a hosted model under a local-only ceiling. [Get help]"
        );

        let device_only = resolved_ctx(
            Some(req(ExternalAccess::LocalOnly)),
            ExternalAccess::Unrestricted,
            None,
            "x",
        );
        assert_eq!(
            derive_treatment(
                &device_only,
                device_only.step.requirement.as_ref(),
                &SelectedModel::hosted(hosted_row())
            )
            .unwrap_err(),
            TreatmentError::HostedAboveRange
        );

        let external = resolved_ctx(
            Some(req(ExternalAccess::AnonymousPreferred)),
            ExternalAccess::AnonymousPreferred,
            None,
            "x",
        );
        let half = SelectedModel {
            provider_id: Some("p".to_owned()),
            model_id: "m".to_owned(),
            hosted_model: None,
        };
        assert_eq!(
            derive_treatment(&external, external.step.requirement.as_ref(), &half).unwrap_err(),
            TreatmentError::InconsistentSelection
        );
        let other_half = SelectedModel {
            provider_id: None,
            model_id: "m".to_owned(),
            hosted_model: Some(hosted_row()),
        };
        assert_eq!(
            derive_treatment(&external, external.step.requirement.as_ref(), &other_half)
                .unwrap_err(),
            TreatmentError::InconsistentSelection
        );
    }

    /// A disagreement stops the step through the failure handler BEFORE any gate:
    /// no disclosure-log entry, no step recorded. (A hosted pick cannot come from the
    /// resolver under a local-only ceiling, so the broken selection is injected at
    /// the derivation; this pins that execute_once routes the error to a stop.)
    #[tokio::test]
    async fn the_executor_stops_a_step_on_a_derivation_error_before_any_gate() {
        let src = include_str!("executor.rs");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let flat: String = code.split_whitespace().collect::<Vec<_>>().join(" ");
        let derive_at = flat
            .find("let treatment = match derive_treatment(ctx, resolver_req, &selected_model) {")
            .expect("the derivation");
        let gate1_at = flat.find(".gate1(").expect("gate 1");
        assert!(derive_at < gate1_at, "the derivation comes before Gate 1");
        assert!(flat[derive_at..gate1_at].contains(
            "return Ok(Some(failure_handler.handle( &ConductorError::TierBoundaryViolation {"
        ));
    }

    /// The derived treatment is a local of one attempt: nothing stores it, so a retry
    /// or a resumed run derives it again from its own selection.
    #[test]
    fn the_treatment_is_never_stored() {
        for (name, src) in [
            ("lifecycle.rs", include_str!("lifecycle.rs")),
            ("types.rs", include_str!("types.rs")),
            ("tokens.rs", include_str!("tokens.rs")),
        ] {
            assert!(
                !src.contains("StepTreatment"),
                "{name} mentions StepTreatment"
            );
        }
        let src = include_str!("executor.rs");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let ctx_struct = code
            .split("pub struct StepContext {")
            .nth(1)
            .and_then(|r| r.split("\n}\n").next())
            .expect("StepContext");
        assert!(!ctx_struct.contains("treatment"));
    }

    /// Every read of the CEILING values in the production code of this file, pinned.
    /// execute_once reads ctx.effective_access for the three pre-selection decisions
    /// only (the bound-run stop, Step 4.5 and select_model); its tier fields only in
    /// the floor-invariant check on the ceiling values. Gate 1, the floor consent
    /// gate, the render, the voice scan, Gate 2, the sensitivity score, the recorded
    /// tier and Gate 3 read the treatment.
    #[test]
    fn the_ceiling_values_are_read_only_where_reviewed() {
        let src = include_str!("executor.rs");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let live: String = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // execute_once alone.
        let body = live
            .split("async fn execute_once<")
            .nth(1)
            .and_then(|r| r.split("pub(crate) fn render_template(").next())
            .expect("execute_once");
        assert_eq!(body.matches("ctx.effective_access").count(), 3);
        assert_eq!(body.matches("ctx.execution_tier").count(), 1);
        assert_eq!(body.matches("ctx.abstraction_tier").count(), 1);
        assert_eq!(body.matches("ctx.raw_abstraction").count(), 1);
        // The whole production file. items.id=901 added two reads in select_pinned_model
        // (the reach and the resolver's ceiling): the same use as select_model_for_block's,
        // the resolver's candidate set, outside execute_once.
        assert_eq!(live.matches("ctx.effective_access").count(), 9);
        assert_eq!(live.matches("ctx.execution_tier").count(), 2);
        assert_eq!(live.matches("ctx.abstraction_tier").count(), 2);
        assert_eq!(live.matches("ctx.raw_abstraction").count(), 2);
        // The gates take the treatment.
        let flat: String = live.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains(
            "treatment.abstraction_tier, treatment.raw_abstraction, treatment.execution_tier, treatment.access, selected_model.provider_id.clone(), )"
        ));
        assert!(flat.contains("let hosted_step = treatment.hosted;"));
        assert!(flat.contains("let prompt = if treatment.hosted {"));
        assert!(flat.contains("if next_tier > treatment.execution_tier {"));
        assert!(
            flat.contains("scan_voice_profile(ctx, &treatment, personal_track, privacy_gateway)")
        );
    }

    // -- items.id=921 stage 3: retries and resumed runs --------------------------
    //
    // Characterization first: a hosted pick that parks on floor consent and is
    // resumed must behave exactly like an ordinary external step at the same
    // ceiling. A resume and a retry both re-enter execute_once from the top (the
    // lifecycle rebuilds the StepContext, the retry loop calls the attempt again),
    // so each attempt selects, derives its treatment and runs Gate 1 afresh. These
    // tests drive execute_once the way a resume does. The ceiling values are the
    // ones lifecycle computes for anonymous_preferred (execution tier 3, floor
    // clamp to abstraction 2 over a raw 1).

    /// What an attempt left in the disclosure log, comparable across runs: the
    /// event, the tiers, the provider and the field lists (never a value).
    fn entry_signature(gateway: &PrivacyGateway<TestLogger>) -> Vec<String> {
        gateway
            .logger
            .entries()
            .iter()
            .map(|e| {
                format!(
                    "{}|exec={}|abs={:?}|provider={:?}|shared={:?}|abstracted={:?}|withheld={:?}",
                    e.event_type,
                    e.execution_tier,
                    e.abstraction_tier,
                    e.provider,
                    e.fields_shared,
                    e.fields_abstracted,
                    e.fields_withheld
                )
            })
            .collect()
    }

    /// The outcome of an attempt as a comparable string.
    fn outcome_shape(out: &Result<Option<FailureResult>, ConductorError>) -> String {
        match out {
            Ok(None) => "done".to_owned(),
            Ok(Some(f)) => format!(
                "{:?}|{:?}|{:?}|{}",
                f.action, f.failure_mode, f.severity, f.plain_language
            ),
            Err(e) => format!("err:{e:?}"),
        }
    }

    /// The same pair of contexts every characterization uses: a resolved step whose
    /// range is hosted-only (so the resolver can only pick the fixture host) and an
    /// ordinary step with that host as its preference, both at lifecycle's values.
    fn resumable_pair(key_hex: &str) -> (StepContext, StepContext) {
        let mut resolved = resolved_ctx(
            Some(requirement(
                QualityBar::Basic,
                &[Destination::HostedApi],
                ExternalAccess::AnonymousPreferred,
            )),
            ExternalAccess::AnonymousPreferred,
            Some("rma_letter"),
            "Write it.",
        );
        resolved.key_hex = key_hex.to_owned();
        resolved.plan = hosted_pin_approval();
        resolved.execution_tier = 3;
        let mut ordinary_ctx =
            resolved_ctx(None, ExternalAccess::AnonymousPreferred, None, "Write it.");
        ordinary_ctx.key_hex = key_hex.to_owned();
        ordinary_ctx.qr_hosted_provider_preference = Some("fixture-host-a".to_owned());
        ordinary_ctx.execution_tier = 3;
        (resolved, ordinary_ctx)
    }

    /// A resume with no saved preference: a one-time "proceed" is recorded in
    /// consent_decisions but nothing reads it on resume (only the saved
    /// preference in the Persona's personal.db is read, by lifecycle), so the step
    /// parks on floor consent again. This is today's behaviour for every external
    /// step; the hosted pick must match it, attempt after attempt, with one
    /// gate1_pass row per attempt and identical rows for the two kinds of step.
    #[tokio::test]
    async fn a_resumed_hosted_pick_parks_on_floor_consent_exactly_like_an_ordinary_step() {
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let (resolved, ordinary_ctx) = resumable_pair(&key_hex);
            for attempt in 0..2 {
                let (a, ta, ga) = run_once_with(&resolved, &pool, &clamping_track()).await;
                let (b, tb, gb) = run_once_with(&ordinary_ctx, &pool, &clamping_track()).await;
                assert_eq!(outcome_shape(&a), outcome_shape(&b), "attempt {attempt}");
                assert_eq!(
                    a.as_ref().unwrap().as_ref().unwrap().action,
                    FailureAction::AwaitFloorConsent
                );
                assert_eq!(
                    a.unwrap().unwrap().metadata,
                    b.unwrap().unwrap().metadata,
                    "the prompt metadata is identical, tiers included"
                );
                assert_eq!(
                    entry_signature(&ga),
                    entry_signature(&gb),
                    "attempt {attempt}"
                );
                assert_eq!(gate1_entries(&ga).len(), 1, "one Gate 1 row per attempt");
                assert!(ta.steps().is_empty() && tb.steps().is_empty());
            }
        })
        .await;
    }

    /// A resume WITH the saved "modified" preference (what lifecycle reads from the
    /// Persona's personal.db when the user chose to remember): the floor prompt is
    /// skipped and a floor_consent_auto row is written, and what follows (the hosted
    /// call) is the same for the resolved pick and the ordinary step.
    #[tokio::test]
    async fn a_resumed_hosted_pick_with_a_saved_floor_consent_proceeds_like_an_ordinary_step() {
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let (mut resolved, mut ordinary_ctx) = resumable_pair(&key_hex);
            resolved.floor_consent_preference = Some("modified".to_owned());
            ordinary_ctx.floor_consent_preference = Some("modified".to_owned());
            let (a, _, ga) = run_once_with(&resolved, &pool, &clamping_track()).await;
            let (b, _, gb) = run_once_with(&ordinary_ctx, &pool, &clamping_track()).await;
            assert_eq!(outcome_shape(&a), outcome_shape(&b));
            assert_ne!(
                a.as_ref()
                    .ok()
                    .and_then(|o| o.as_ref())
                    .map(|f| f.action.clone()),
                Some(FailureAction::AwaitFloorConsent),
                "the saved consent skips the prompt"
            );
            let sig = entry_signature(&ga);
            assert_eq!(sig, entry_signature(&gb));
            assert!(sig.iter().any(|e| e.starts_with("gate1_pass|")));
            assert!(sig.iter().any(|e| e.starts_with("floor_consent_auto|")));
            assert!(
                sig.iter()
                    .filter(|e| e.starts_with("floor_consent_auto|"))
                    .all(|e| e.contains("provider=Some(\"fixture-host-a\")")),
                "{sig:?}"
            );
        })
        .await;
    }

    /// Each retry resolves again: an attempt that parked on a hosted pick is
    /// followed, once a local model is ready, by an attempt that picks LOCAL and
    /// is gated as local. Nothing is carried from the first attempt.
    #[tokio::test]
    async fn each_attempt_selects_and_derives_its_treatment_again() {
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let mut ctx = resolved_ctx(
                Some(requirement(
                    QualityBar::Basic,
                    &[Destination::Local, Destination::HostedApi],
                    ExternalAccess::AnonymousPreferred,
                )),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                "Write it.",
            );
            ctx.key_hex = key_hex;
            ctx.execution_tier = 3;
            // items.id=901: attempt 1 runs under an approved pin of the host (a hosted pick
            // needs one); attempt 2 under a zero-pin approval, which permits resolver LOCAL
            // picks only, so it shows the retry selecting again.
            ctx.plan = hosted_pin_approval();
            // Attempt 1: no local model is ready, so the pinned host is used.
            let (first, _, g1) = run_once_with(&ctx, &pool, &clamping_track()).await;
            assert_eq!(
                first.unwrap().unwrap().action,
                FailureAction::AwaitFloorConsent
            );
            let first_rows = gate1_entries(&g1);
            assert_eq!(first_rows[0].provider.as_deref(), Some("fixture-host-a"));
            assert_eq!(first_rows[0].execution_tier, 3);

            // A local model becomes ready: the next attempt.
            install_local_models(&pool).await;
            ctx.plan = approval_with(vec![]);
            let stub = LocalModelStub::start(1);
            let (second, track, g2) = run_once_with(&ctx, &pool, &clamping_track()).await;
            assert!(
                second.unwrap().is_none(),
                "no prompt: it stays on the device"
            );
            assert_eq!(stub.server.captured().len(), 1);
            let second_rows = gate1_entries(&g2);
            assert_eq!(second_rows.len(), 1);
            assert!(second_rows[0].provider.is_none());
            assert_eq!(second_rows[0].execution_tier, 1);
            assert_eq!(second_rows[0].abstraction_tier, Some(1));
            assert_eq!(track.steps()[0].routing_tier_used, 1);
        })
        .await;
    }

    /// The retry loop calls the attempt (a fresh execute_once) every time and holds
    /// nothing of the previous one: execute_once is the only place a treatment is
    /// derived, and retry_loop never names it.
    #[test]
    fn the_retry_loop_holds_no_treatment() {
        let src = include_str!("executor.rs");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let retry = code
            .split("async fn retry_loop<")
            .nth(1)
            .and_then(|r| r.split("\n}\n").next())
            .expect("retry_loop");
        assert!(!retry.contains("treatment") && !retry.contains("StepTreatment"));
        assert!(retry.contains("attempt.once(retry_count)"));
        assert_eq!(
            code.matches("derive_treatment(ctx, resolver_req").count(),
            1
        );
    }

    // -- items.id=921: Gate 3 follows the tier the step really ran at ------------

    /// A resolved step whose range allows hosted, with a local model ready, an
    /// external ceiling at lifecycle's values (execution tier 3), and `next` as the
    /// look-ahead tier of the step after it.
    fn local_pick_ctx(template: &str, next: Option<u8>) -> StepContext {
        let mut ctx = resolved_ctx(
            Some(requirement(
                QualityBar::Basic,
                &[Destination::Local, Destination::HostedApi],
                ExternalAccess::AnonymousPreferred,
            )),
            ExternalAccess::AnonymousPreferred,
            Some("rma_letter"),
            template,
        );
        ctx.execution_tier = 3;
        ctx.next_execution_tier = next;
        ctx
    }

    fn gate3_events(gateway: &PrivacyGateway<TestLogger>) -> Vec<String> {
        gateway
            .logger
            .entries()
            .into_iter()
            .filter(|e| e.event_type.starts_with("gate3"))
            .map(|e| e.event_type)
            .collect()
    }

    /// A local pick under an external ceiling ran on the device (tier 1). When the
    /// next step may go out (look-ahead tier 3), its output is a cross-tier
    /// promotion and Gate 3 reviews it. Keyed on the ceiling (tier 3 against 3) it
    /// would have been skipped, which is what the old behaviour did.
    #[tokio::test]
    async fn the_output_of_a_local_pick_is_reviewed_by_gate_3_before_an_external_next_step() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let _stub = LocalModelStub::start(1);
            let ctx = local_pick_ctx("Write it.", Some(3));
            let (out, track, gateway) = run_once(&ctx, &pool).await;
            assert!(out.unwrap().is_none(), "approved: nothing sensitive in it");
            assert_eq!(track.steps()[0].routing_tier_used, 1);
            assert_eq!(
                gate3_events(&gateway),
                vec!["gate3_promotion_approved".to_owned()],
                "Gate 3 ran for the promotion from tier 1 to tier 3"
            );
        })
        .await;
    }

    /// The same, with a sensitive field named by the template: Gate 3 holds the
    /// content back, so a local draft built from a severe field cannot flow to an
    /// external step unreviewed.
    #[tokio::test]
    async fn a_sensitive_local_output_is_held_back_from_an_external_next_step() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let _stub = LocalModelStub::start(1);
            let ctx = local_pick_ctx("Write about {diagnosis}.", Some(3));
            let (out, track, gateway) = run_once_with(&ctx, &pool, &clamping_track()).await;
            let failure = out.unwrap().expect("Gate 3 stops the step");
            assert_ne!(failure.action, FailureAction::AwaitFloorConsent);
            assert_eq!(
                gate3_events(&gateway),
                vec!["gate3_sensitivity_block".to_owned()]
            );
            assert_eq!(track.steps()[0].sensitivity_severity, 3);
        })
        .await;
    }

    /// No promotion, no Gate 3: a local pick followed by a local step (look-ahead
    /// tier 1) or by nothing at all leaves Gate 3 alone.
    #[tokio::test]
    async fn a_local_pick_followed_by_a_local_step_or_nothing_is_not_reviewed() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let _stub = LocalModelStub::start(2);
            for next in [Some(1), None] {
                let ctx = local_pick_ctx("Write it.", next);
                let (out, _, gateway) = run_once(&ctx, &pool).await;
                assert!(out.unwrap().is_none());
                assert!(gate3_events(&gateway).is_empty(), "{next:?}");
            }
        })
        .await;
    }

    /// The three sites keyed on the network predicate, pinned in the source of
    /// execute_once itself. Since items.id=921 two of them follow the derived
    /// treatment: hosted_step and the render branch both read `treatment.hosted`
    /// (the ceiling test for an ordinary step, the chosen model for a resolved one),
    /// and the hosted call's provider still comes from the selection for a resolved
    /// step and from the preference otherwise.
    #[test]
    fn the_three_network_sites_are_as_reviewed() {
        let src = include_str!("executor.rs");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let flat: String = code.split_whitespace().collect::<Vec<_>>().join(" ");
        // Site 1.
        assert!(flat.contains("let hosted_step = treatment.hosted;"));
        // Site 2: the render keys on the same flag.
        assert!(flat.contains(
            "let prompt = if treatment.hosted { let disclosure = shared_state.read_disclosure_buffer(&ctx.step.action_id);"
        ));
        // Site 3: the provider still comes from the selection for a resolved step and
        // from the preference otherwise. items.id=884 added a third element to the
        // match, the reviewed prompt, and nothing else: a hosted call cannot be made
        // without one.
        assert!(flat.contains(
            "let provider_for_call = match resolver_req { Some(_) => selected_model.provider_id.as_deref(), None => ctx.qr_hosted_provider_preference.as_deref(), }; match ( provider_for_call, selected_model.hosted_model.as_ref(), reviewed.as_ref(), ) {"
        ));
        // The panic arm is the original one, once, with its message extended by the
        // review (items.id=884).
        assert_eq!(code.matches("unreachable!(").count(), 2);
        assert!(code.contains("a hosted step with no provider, no resolved model or no reviewed"));
        // A hosted selection under a local-only ceiling is refused before any gate,
        // by the derivation.
        assert!(flat.contains(
            "let treatment = match derive_treatment(ctx, resolver_req, &selected_model) {"
        ));
        let derive_src = include_str!("step_treatment.rs");
        assert!(derive_src.contains("return Err(TreatmentError::HostedUnderLocalOnly);"));
    }

    // -- items.id=884: the outbound review through execute_once ---------------------

    fn hosted_pick_ctx(key_hex: &str, template: &str) -> StepContext {
        let mut ctx = resolved_ctx(
            Some(requirement(
                QualityBar::Basic,
                &[Destination::HostedApi],
                ExternalAccess::AnonymousPreferred,
            )),
            ExternalAccess::AnonymousPreferred,
            Some("rma_letter"),
            template,
        );
        ctx.key_hex = key_hex.to_owned();
        ctx.plan = hosted_pin_approval();
        ctx
    }

    async fn review_rows(
        ctx: &StepContext,
    ) -> Vec<crate::persistence::step_review_store::StepReviewRow> {
        let mut conn = crate::persistence::output_store::open_outputs_db(
            &ctx.user_id,
            &ctx.persona_id,
            &ctx.key_hex,
        )
        .await
        .unwrap();
        crate::persistence::step_review_store::list_reviews(
            &mut conn,
            &ctx.focus_run_id,
            &ctx.step.action_id,
        )
        .await
        .unwrap()
    }

    fn code_of(r: &FailureResult) -> String {
        r.metadata.as_ref().unwrap()["code"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    // -- items.id=882: {persona_context} renders empty at an external destination ----

    const PERSONA_FACT: &str = "KNOWN-FACT-ZEBRA-42";

    /// A scanner that finds nothing and records every text it was asked to scan: the
    /// text execute_once rendered for the hosted call, before the call.
    fn capturing_scanner() -> (
        step_review::forced::Forced,
        std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    ) {
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = std::rc::Rc::clone(&seen);
        let mut forced = step_review::forced::clean();
        forced.result = std::rc::Rc::new(move |content: &str| {
            sink.borrow_mut().push(content.to_owned());
            Ok(crate::conductor::privacy::types::Gate3Result {
                approved: true,
                ..Default::default()
            })
        });
        (forced, seen)
    }

    const PERSONA_TEMPLATE: &str = "Plan the work. Context: {persona_context} End of context.";

    /// Test 1a: an ORDINARY external step (no requirement) whose template names the
    /// token, with a filled slice on the context: the text that goes to review and out
    /// carries neither the fact nor the token.
    #[tokio::test]
    async fn an_ordinary_external_step_renders_persona_context_empty() {
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let mut ctx = resolved_ctx(
                None,
                ExternalAccess::AnonymousPreferred,
                None,
                PERSONA_TEMPLATE,
            );
            ctx.key_hex = key_hex;
            ctx.qr_hosted_provider_preference = Some("fixture-host-a".to_owned());
            ctx.persona_context = PERSONA_FACT.to_owned();
            let (forced, seen) = capturing_scanner();
            let _g = step_review::forced::arm(&ctx, forced).await;

            let _ = run_once(&ctx, &pool).await;
            let seen = seen.borrow();
            assert_eq!(seen.len(), 1, "the review saw exactly one prompt");
            assert!(!seen[0].contains(PERSONA_FACT), "{}", seen[0]);
            assert!(!seen[0].contains("{persona_context}"), "{}", seen[0]);
            assert_eq!(seen[0], "Plan the work. Context:  End of context.");
        })
        .await;
    }

    /// Test 1b: the same on a RESOLVED hosted pick.
    #[tokio::test]
    async fn a_resolved_hosted_pick_renders_persona_context_empty() {
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let mut ctx = hosted_pick_ctx(&key_hex, PERSONA_TEMPLATE);
            ctx.persona_context = PERSONA_FACT.to_owned();
            let (forced, seen) = capturing_scanner();
            let _g = step_review::forced::arm(&ctx, forced).await;

            let _ = run_once(&ctx, &pool).await;
            let seen = seen.borrow();
            assert_eq!(seen.len(), 1, "the review saw exactly one prompt");
            assert!(!seen[0].contains(PERSONA_FACT), "{}", seen[0]);
            assert!(!seen[0].contains("{persona_context}"), "{}", seen[0]);
        })
        .await;
    }

    /// Test 2: local still gets it. The local render inserts the slice, the external
    /// render of the SAME context does not.
    #[test]
    fn the_local_render_still_inserts_persona_context_and_the_external_one_does_not() {
        let mut ctx = hosted_ctx(RunAttendance::Attended, "unused");
        ctx.step.prompt_template = PERSONA_TEMPLATE.to_owned();
        ctx.persona_context = PERSONA_FACT.to_owned();
        let vars = HashMap::new();
        let local = render_prompt(&ctx, &vars, "", "");
        assert_eq!(
            local,
            format!("Plan the work. Context: {PERSONA_FACT} End of context.")
        );
        let external = render_prompt_with_disclosure(&ctx, &vars, "", &HashMap::new(), "");
        assert_eq!(external, "Plan the work. Context:  End of context.");
        // A disclosure entry cannot smuggle the name back in: the empty value is
        // inserted after the disclosure fields.
        let mut disclosure = HashMap::new();
        disclosure.insert("persona_context".to_owned(), PERSONA_FACT.to_owned());
        let external = render_prompt_with_disclosure(&ctx, &vars, "", &disclosure, "");
        assert!(!external.contains(PERSONA_FACT), "{external}");
    }

    // -- items.id=883: an external render gets the user's own turns, never a model turn --

    const MODEL_FACT: &str = "MODEL-TURN-FACT-QUOKKA-77";
    const USER_TURN: &str = "my printer is an HP LaserJet";
    const WINDOW_TEMPLATE: &str = "Help with: {user_input} End.";

    /// The full blended window (what the local render sees) and the user-only form
    /// (what an external render sees), as send_message builds them.
    fn with_window(ctx: &mut StepContext) {
        ctx.user_input =
            format!("User: {USER_TURN}\nAssistant: Since you work at {MODEL_FACT}, try this\n");
        ctx.external_input = format!("User: {USER_TURN}\n");
    }

    /// An ORDINARY external step: the text that goes to review and out carries the
    /// user's turn verbatim and no model turn.
    #[tokio::test]
    async fn an_ordinary_external_step_never_renders_a_model_turn() {
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let mut ctx = resolved_ctx(
                None,
                ExternalAccess::AnonymousPreferred,
                None,
                WINDOW_TEMPLATE,
            );
            ctx.key_hex = key_hex;
            ctx.qr_hosted_provider_preference = Some("fixture-host-a".to_owned());
            with_window(&mut ctx);
            let (forced, seen) = capturing_scanner();
            let _g = step_review::forced::arm(&ctx, forced).await;

            let _ = run_once(&ctx, &pool).await;
            let seen = seen.borrow();
            assert_eq!(seen.len(), 1, "the review saw exactly one prompt");
            assert!(!seen[0].contains(MODEL_FACT), "{}", seen[0]);
            assert!(!seen[0].contains("Assistant:"), "{}", seen[0]);
            assert_eq!(seen[0], format!("Help with: User: {USER_TURN}\n End."));
        })
        .await;
    }

    /// The same on a RESOLVED hosted pick.
    #[tokio::test]
    async fn a_resolved_hosted_pick_never_renders_a_model_turn() {
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let mut ctx = hosted_pick_ctx(&key_hex, WINDOW_TEMPLATE);
            with_window(&mut ctx);
            let (forced, seen) = capturing_scanner();
            let _g = step_review::forced::arm(&ctx, forced).await;

            let _ = run_once(&ctx, &pool).await;
            let seen = seen.borrow();
            assert_eq!(seen.len(), 1, "the review saw exactly one prompt");
            assert!(!seen[0].contains(MODEL_FACT), "{}", seen[0]);
            assert!(seen[0].contains(USER_TURN), "{}", seen[0]);
        })
        .await;
    }

    /// Local steps are unchanged: the local render inserts the full window, the
    /// external render of the SAME context inserts the user's turns only.
    #[test]
    fn the_local_render_keeps_the_full_window_and_the_external_one_has_user_turns_only() {
        let mut ctx = hosted_ctx(RunAttendance::Attended, "unused");
        ctx.step.prompt_template = WINDOW_TEMPLATE.to_owned();
        with_window(&mut ctx);
        let vars = HashMap::new();
        let local = render_prompt(&ctx, &vars, "", "");
        assert_eq!(local, format!("Help with: {} End.", ctx.user_input));
        assert!(local.contains(MODEL_FACT));
        let external = render_prompt_with_disclosure(&ctx, &vars, "", &HashMap::new(), "");
        assert_eq!(external, format!("Help with: User: {USER_TURN}\n End."));
        // A disclosure entry cannot smuggle the window back in: the system token is
        // inserted after the disclosure fields.
        let mut disclosure = HashMap::new();
        disclosure.insert("user_input".to_owned(), ctx.user_input.clone());
        let external = render_prompt_with_disclosure(&ctx, &vars, "", &disclosure, "");
        assert!(!external.contains(MODEL_FACT), "{external}");
    }

    /// items.id=883, fail closed: with no external copy (a resumed run whose stored copy
    /// was NULL) an external render has an EMPTY {user_input} while the local render
    /// still sees the window.
    #[test]
    fn an_empty_external_input_renders_empty_while_the_local_render_keeps_the_window() {
        let mut ctx = hosted_ctx(RunAttendance::Attended, "unused");
        ctx.step.prompt_template = WINDOW_TEMPLATE.to_owned();
        with_window(&mut ctx);
        ctx.external_input = String::new();
        let vars = HashMap::new();
        let external = render_prompt_with_disclosure(&ctx, &vars, "", &HashMap::new(), "");
        assert_eq!(external, "Help with:  End.");
        assert!(!external.contains(USER_TURN) && !external.contains(MODEL_FACT));
        let local = render_prompt(&ctx, &vars, "", "");
        assert!(local.contains(USER_TURN) && local.contains(MODEL_FACT));
    }

    /// Scan: the external render reads ctx.external_input and never ctx.user_input, and
    /// the local render is the one reader of the full window in this file.
    #[test]
    fn the_external_render_never_reads_the_full_window() {
        let src = include_str!("executor.rs");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let live: String = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let after = live
            .split("fn render_prompt_with_disclosure(")
            .nth(1)
            .expect("render_prompt_with_disclosure");
        let end = [
            "\nfn ",
            "\nasync fn ",
            "\npub fn ",
            "\npub(crate) fn ",
            "\npub async fn ",
        ]
        .iter()
        .filter_map(|m| after.find(m))
        .min()
        .expect("a following top-level fn");
        let body = &after[..end];
        assert!(body.contains("render_template("), "whole function: {body}");
        assert!(!body.contains("ctx.user_input"), "{body}");
        assert!(body.contains("ctx.external_input.clone()"), "{body}");
        // The local render does read the window, so the scan is not vacuous.
        assert_eq!(live.matches("ctx.user_input").count(), 1);
        assert_eq!(live.matches("ctx.external_input").count(), 1);
    }

    /// Scan: render_prompt_with_disclosure never reads ctx.persona_context.
    #[test]
    fn the_external_render_never_reads_the_persona_context_slice() {
        let src = include_str!("executor.rs");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let live: String = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // The body ends at the next top-level fn (comment lines are filtered out of
        // `live`, so a comment banner cannot be the end marker).
        let after = live
            .split("fn render_prompt_with_disclosure(")
            .nth(1)
            .expect("render_prompt_with_disclosure");
        let end = [
            "\nfn ",
            "\nasync fn ",
            "\npub fn ",
            "\npub(crate) fn ",
            "\npub async fn ",
        ]
        .iter()
        .filter_map(|m| after.find(m))
        .min()
        .expect("a following top-level fn");
        let body = &after[..end];
        assert!(
            body.contains("render_template("),
            "the body is the whole function, not a prefix: {body}"
        );
        assert!(
            !body.contains("scan_voice_profile"),
            "the body stops before the next function"
        );
        assert!(!body.contains("persona_context()"), "{body}");
        assert!(!body.contains("ctx.persona_context"), "{body}");
        assert!(body.contains("tokens.insert(\"persona_context\".to_owned(), String::new());"));
        // The local render does read it, so the scan above is not vacuous.
        let local = live
            .split("pub(crate) fn render_prompt(")
            .nth(1)
            .and_then(|r| r.split("fn render_prompt_with_disclosure(").next())
            .expect("render_prompt");
        assert!(local.contains("ctx.persona_context.clone()"));
        // The slice has one production reader: the local render.
        assert_eq!(live.matches("ctx.persona_context").count(), 1);
    }

    /// Test 7, a CHARACTERISATION (no tier-filtering is asserted, none is claimed):
    /// what the Memory Broker returns for a focus whose domain_context.db holds a
    /// standing summary, asked as a tier-1 and as a tier-2 run. The observed values
    /// are recorded in the asserts so a change to the broker shows up here. Then the
    /// rendered slice is put on an external step's context: the prompt still has none
    /// of it.
    #[tokio::test]
    async fn broker_characterisation_and_the_external_prompt_still_empty() {
        use crate::conductor::memory_broker::MemoryBroker;
        use sqlx::{ConnectOptions, Connection};
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let (user, persona, focus) = (SPEND_USER, "persona-1", "focus-1");

            // A standing summary written the way the store reads it: the table row
            // that migration seeds, updated. (No production writer is called: the
            // block writers have no callers, items.id=535's guard.)
            crate::persistence::domain_context_store::ensure_domain_context_db(
                user, persona, focus, &key_hex,
            )
            .await
            .expect("domain_context.db");
            let path = crate::persistence::domain_context_store::get_domain_context_path(
                user, persona, focus,
            );
            let db_key = crate::auth::subkeys::db_key_hex(
                user,
                &key_hex,
                crate::auth::subkeys::DbPurpose::DomainContext,
                &crate::auth::subkeys::scope(&[persona, focus]).unwrap(),
            )
            .unwrap();
            let mut conn = crate::providers::utils::quiet_connect_options()
                .filename(&path)
                .create_if_missing(false)
                .pragma("key", format!("\"x'{}'\"", db_key.as_str()))
                .pragma("cipher_compatibility", "4")
                .pragma("journal_mode", "DELETE")
                .connect()
                .await
                .expect("open domain_context.db");
            sqlx::query("UPDATE standing_summary SET content = ?, token_count = 9 WHERE id = 1")
                .bind(PERSONA_FACT)
                .execute(&mut conn)
                .await
                .unwrap();
            conn.close().await.unwrap();

            let mut rendered_by_tier = Vec::new();
            for tier in [1, 2] {
                let mut slice = MemoryBroker::new()
                    .assemble_context(
                        user, persona, focus, None, &key_hex, tier, 8192, false, None, None,
                    )
                    .await;
                let scopes: Vec<String> = slice
                    .retrieved_blocks
                    .iter()
                    .map(|b| b.visibility_scope.clone())
                    .collect();
                rendered_by_tier.push((slice.render(), scopes));
                slice.clear();
            }
            // Observed today: the slice is the same text at tier 1 and at tier 2, as
            // one block labelled tier2_permitted. Recorded, not endorsed.
            assert_eq!(rendered_by_tier[0].0, PERSONA_FACT);
            assert_eq!(rendered_by_tier[1].0, PERSONA_FACT);
            assert_eq!(rendered_by_tier[0].1, vec!["tier2_permitted".to_owned()]);
            assert_eq!(rendered_by_tier[1].1, vec!["tier2_permitted".to_owned()]);

            // That text, on an external step's context, is not in the prompt.
            let mut ctx = hosted_pick_ctx(&key_hex, PERSONA_TEMPLATE);
            ctx.persona_context = rendered_by_tier[1].0.clone();
            let (forced, seen) = capturing_scanner();
            let _g = step_review::forced::arm(&ctx, forced).await;
            let _ = run_once(&ctx, &pool).await;
            let seen = seen.borrow();
            assert_eq!(seen.len(), 1);
            assert!(!seen[0].contains(PERSONA_FACT), "{}", seen[0]);
        })
        .await;
    }

    // -- items.id=882 stage 4: the declared field_requirements path, from the REAL
    // shipped templates. END-TO-END LIMIT: the personal track here is a FIXTURE
    // (add_field in the test). In production PersonalTrack.fields is left empty at
    // INITIALIZE (personal_store.rs, items.id=170) and nothing bridges entity facts to
    // Gate 1 (items.id=929), so today these fields arrive empty in a real run. What
    // these tests prove is the path from Gate 1 to the prompt, not the source.

    async fn shipped_step(
        pool: &sqlx::SqlitePool,
        focus: &str,
        step: &str,
    ) -> crate::conductor::tokens::ActionDefinition {
        let def = crate::conductor::lifecycle::load_focus_definition(pool, focus)
            .await
            .unwrap_or_else(|e| panic!("{focus} must load: {e}"));
        def.steps
            .into_iter()
            .find(|s| s.action_id == step)
            .unwrap_or_else(|| panic!("{focus} has no step {step}"))
    }

    fn fixture_field(
        name: &str,
        value: &str,
        sensitivity: &str,
        severity: i32,
        tier2: &str,
    ) -> crate::conductor::types::PersonalField {
        crate::conductor::types::PersonalField {
            field_name: name.to_owned(),
            field_value: value.to_owned(),
            sensitivity: sensitivity.to_owned(),
            sensitivity_severity: severity,
            source_id: "personal-specialist".to_owned(),
            abstraction_tier2: tier2.to_owned(),
            abstraction_tier3: "omit".to_owned(),
        }
    }

    /// job_title, employer and industry at `tier2`, plus income_range (not declared
    /// by either shipped step) at `pass`, so only the step's declaration keeps it out.
    fn professional_track(tier2: &str) -> PersonalTrack {
        let mut pt = PersonalTrack::new();
        for f in [
            fixture_field("job_title", "Staff Engineer", "personal", 2, tier2),
            fixture_field("employer", "Acme Robotics", "personal", 2, tier2),
            fixture_field("industry", "Logistics", "personal", 2, tier2),
            fixture_field("income_range", "INCOME-185000", "financial", 3, "pass"),
        ] {
            pt.add_field(f).unwrap();
        }
        pt
    }

    /// The text the outbound review saw for the shipped step, run through execute_once
    /// at tier 2 with `track`.
    async fn shipped_prompt(
        pool: &sqlx::SqlitePool,
        focus: &str,
        step: &str,
        track: &PersonalTrack,
    ) -> (String, PrivacyGateway<TestLogger>) {
        let key_hex = hosted_pick_rig(pool).await;
        let mut ctx = hosted_ctx(RunAttendance::Attended, &key_hex);
        ctx.step = shipped_step(pool, focus, step).await;
        ctx.effective_access = ctx.step.routing_tier;
        ctx.focus_external_access = ctx.step.routing_tier;
        ctx.execution_tier = 2;
        ctx.abstraction_tier = 2;
        ctx.raw_abstraction = 1;
        // The saved "modified" preference, so a field that summarize changes does not
        // park the step on floor consent: this test reads the prompt.
        ctx.floor_consent_preference = Some(FLOOR_CONSENT_MODIFIED.to_owned());
        ctx.persona_context = PERSONA_FACT.to_owned();
        let (forced, seen) = capturing_scanner();
        let _g = step_review::forced::arm(&ctx, forced).await;
        let (_out, _track, gateway) = run_once_with(&ctx, pool, track).await;
        let seen = seen.borrow();
        assert_eq!(seen.len(), 1, "the review saw exactly one prompt");
        (seen[0].clone(), gateway)
    }

    /// fit_analysis, real template, fields at `pass`: the values reach the tier-2 prompt;
    /// the undeclared income_range and the persona slice never do.
    #[tokio::test]
    async fn fit_analysis_shows_declared_fields_at_pass() {
        with_temp_shared_db(|pool| async move {
            let (prompt, gateway) = shipped_prompt(
                &pool,
                "role-assessment",
                "fit_analysis",
                &professional_track("pass"),
            )
            .await;
            assert!(
                prompt.contains("Current role, if provided: Staff Engineer"),
                "{prompt}"
            );
            assert!(
                prompt.contains("Current employer, if provided: Acme Robotics"),
                "{prompt}"
            );
            assert!(
                prompt.contains("Current industry, if provided: Logistics"),
                "{prompt}"
            );
            assert!(!prompt.contains("INCOME-185000"), "{prompt}");
            assert!(!prompt.contains(PERSONA_FACT), "{prompt}");
            assert!(!prompt.contains("{persona_context}"), "{prompt}");
            // The disclosure log row Gate 1 wrote: all four are shared as they are
            // (nothing abstracted). NOTE it lists every field Gate 1 approved, so it
            // names income_range although projection kept it out of the prompt: the
            // log OVER-reports against the prompt, never under-reports.
            let rows = gate1_entries(&gateway);
            assert_eq!(rows.len(), 1);
            let mut shared = rows[0].fields_shared.clone();
            shared.sort();
            assert_eq!(
                shared,
                ["employer", "income_range", "industry", "job_title"]
            );
            assert!(rows[0].fields_abstracted.is_empty());
            assert!(rows[0].fields_withheld.is_empty());
            assert_eq!(rows[0].abstraction_tier, Some(2));
        })
        .await;
    }

    /// The same step at summarize: the exact labels summarize_label produces for a
    /// personal-sensitivity field, and no value.
    #[tokio::test]
    async fn fit_analysis_shows_the_summarize_labels_at_summarize() {
        with_temp_shared_db(|pool| async move {
            let (prompt, gateway) = shipped_prompt(
                &pool,
                "role-assessment",
                "fit_analysis",
                &professional_track("summarize"),
            )
            .await;
            assert!(
                prompt.contains("Current role, if provided: personal job title information"),
                "{prompt}"
            );
            assert!(
                prompt.contains("Current employer, if provided: personal employer information"),
                "{prompt}"
            );
            assert!(
                prompt.contains("Current industry, if provided: personal industry information"),
                "{prompt}"
            );
            for raw in [
                "Staff Engineer",
                "Acme Robotics",
                "Logistics",
                "INCOME-185000",
            ] {
                assert!(!prompt.contains(raw), "{raw} leaked: {prompt}");
            }
            let rows = gate1_entries(&gateway);
            assert_eq!(rows.len(), 1);
            let mut abstracted: Vec<_> = rows[0].fields_abstracted.keys().cloned().collect();
            abstracted.sort();
            assert_eq!(abstracted, ["employer", "industry", "job_title"]);
            assert_eq!(
                rows[0].fields_abstracted["job_title"],
                "personal job title information"
            );
            // income_range (pass) is the one field shared as it is, and it is not in
            // the prompt.
            assert_eq!(rows[0].fields_shared, ["income_range"]);
        })
        .await;
    }

    /// The real case today (no field arrives): the prompt is the stage 1 withheld
    /// render, with nothing left of the fields or the persona slice.
    #[tokio::test]
    async fn fit_analysis_with_no_fields_reads_empty_after_each_label() {
        with_temp_shared_db(|pool| async move {
            let (prompt, _) =
                shipped_prompt(&pool, "role-assessment", "fit_analysis", &PersonalTrack::new())
                    .await;
            assert!(prompt.contains("Current role, if provided:  Current employer, if provided:  Current industry, if provided:"), "{prompt}");
            assert!(prompt.trim_end().ends_with("Current industry, if provided:"), "{prompt}");
            assert!(!prompt.contains(PERSONA_FACT), "{prompt}");
        })
        .await;
    }

    /// research, real template: location_city is the one declared field.
    #[tokio::test]
    async fn research_shows_only_the_declared_location_city() {
        with_temp_shared_db(|pool| async move {
            let mut track = professional_track("pass");
            track
                .add_field(fixture_field("location_city", "Portland", "general", 1, "pass"))
                .unwrap();
            let (prompt, _) = shipped_prompt(&pool, "research-and-buy", "research", &track).await;
            assert!(
                prompt.contains(
                    "Region (use for availability and pricing if present, ignore if empty): Portland"
                ),
                "{prompt}"
            );
            for hidden in ["Staff Engineer", "Acme Robotics", "Logistics", "INCOME-185000", PERSONA_FACT] {
                assert!(!prompt.contains(hidden), "{hidden} leaked: {prompt}");
            }
        })
        .await;
    }

    /// The real shipped files load (the lint of stage 3 runs on them) and their real
    /// templates carry no {persona_context}, in the template or the declaration; the
    /// two edited steps declare exactly the fields the plan names.
    #[tokio::test]
    async fn the_shipped_focuses_load_and_no_template_names_persona_context() {
        with_temp_shared_db(|pool| async move {
            for focus in [
                "quick-ask",
                "writing-assistant",
                "research-and-buy",
                "role-assessment",
                "api-chat",
                "qr-admin",
            ] {
                let def = crate::conductor::lifecycle::load_focus_definition(&pool, focus)
                    .await
                    .unwrap_or_else(|e| panic!("{focus} must load: {e}"));
                for s in &def.steps {
                    assert!(
                        !find_tokens(&s.prompt_template)
                            .iter()
                            .any(|t| t == "persona_context"),
                        "{focus}/{}",
                        s.action_id
                    );
                    assert!(
                        !s.declared_inputs.iter().any(|d| d == "persona_context"),
                        "{focus}/{}",
                        s.action_id
                    );
                }
            }
            let names = |s: &crate::conductor::tokens::ActionDefinition| {
                let mut n: Vec<String> = s.field_requirements.keys().cloned().collect();
                n.sort();
                n
            };
            let research = shipped_step(&pool, "research-and-buy", "research").await;
            assert_eq!(names(&research), ["location_city"]);
            let fit = shipped_step(&pool, "role-assessment", "fit_analysis").await;
            assert_eq!(names(&fit), ["employer", "industry", "job_title"]);
            assert!(!names(&fit).contains(&"income_range".to_owned()));
        })
        .await;
    }

    /// No filter, no external step: an unbound hosted pick parks before the context
    /// check, the inference slot and the spend gate, and nothing is sent.
    #[tokio::test]
    async fn an_unbound_hosted_step_with_no_filter_parks_before_the_spend_gate() {
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let ctx = hosted_pick_ctx(&key_hex, "Write it.");
            let mut forced = step_review::forced::clean();
            forced.available = false;
            let _g = step_review::forced::arm(&ctx, forced).await;

            let (out, track, _) = run_once(&ctx, &pool).await;
            let failure = out.unwrap().expect("the step parks");
            assert_eq!(failure.action, FailureAction::AwaitUser);
            assert_eq!(failure.failure_mode.as_deref(), Some("F4"));
            assert_eq!(code_of(&failure), "step_review.privacy_filter_unavailable");
            assert!(track.steps().is_empty(), "nothing ran");
            let rows = review_rows(&ctx).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].outcome,
                crate::persistence::step_review_store::ReviewOutcome::Blocked
            );
        })
        .await;
    }

    /// A clean review lets the step on to the hosted call (here the production
    /// client refuses the loopback rig's non-https address before any traffic): the
    /// review row exists, so the review ran first.
    #[tokio::test]
    async fn a_clean_review_runs_first_and_the_step_goes_on_to_the_hosted_call() {
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let ctx = hosted_pick_ctx(&key_hex, "Write it.");
            let _g = step_review::forced::arm(&ctx, step_review::forced::clean()).await;

            let (out, track, _) = run_once(&ctx, &pool).await;
            // Past the review the production client refuses a loopback (non-https)
            // address before any traffic: the step got as far as the hosted call.
            let failure = out.unwrap().expect("the call fails past the review");
            assert!(
                failure
                    .plain_language
                    .contains("not a secure (https) address"),
                "{failure:?}"
            );
            assert!(track.steps().is_empty());
            let rows = review_rows(&ctx).await;
            assert_eq!(rows.len(), 1, "one review, before the gate");
            assert_eq!(
                rows[0].outcome,
                crate::persistence::step_review_store::ReviewOutcome::Clean
            );
            assert_eq!(rows[0].provider_id.as_deref(), Some("fixture-host-a"));
        })
        .await;
    }

    /// Flagged text parks the step for the user (AwaitConsent), and a resume with the
    /// recorded answers applies them, asks nobody again and goes on to the hosted call.
    #[tokio::test]
    async fn flagged_text_parks_the_step_and_a_resume_applies_the_answers() {
        use std::cell::Cell;
        use std::rc::Rc;
        with_temp_shared_db(|pool| async move {
            let key_hex = hosted_pick_rig(&pool).await;
            let ctx = hosted_pick_ctx(&key_hex, "Write it for Ann Smith.");
            let scans = Rc::new(Cell::new(0usize));
            let counted = Rc::clone(&scans);
            let mut forced = step_review::forced::clean();
            forced.result = Rc::new(move |content: &str| {
                counted.set(counted.get() + 1);
                Ok(crate::conductor::privacy::types::Gate3Result {
                    pending_consent: true,
                    review_spans: vec![step_review::fake::asked_span(
                        "s-ann",
                        content,
                        "Ann Smith",
                    )],
                    ..Default::default()
                })
            });
            let _g = step_review::forced::arm(&ctx, forced).await;

            let (out, _, _) = run_once(&ctx, &pool).await;
            let failure = out.unwrap().expect("parked for the user");
            assert_eq!(failure.action, FailureAction::AwaitConsent);
            assert_eq!(code_of(&failure), "step_review.required");
            assert_eq!(scans.get(), 1);

            // The user answers (as submit_element_consent_decision records it).
            let mut conn = crate::persistence::output_store::open_outputs_db(
                &ctx.user_id,
                &ctx.persona_id,
                &ctx.key_hex,
            )
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO consent_decisions
                 (id, focus_run_id, decision_type, decision, span_id, created_at)
                 VALUES ('d1', ?, 'element_consent', 'keep_private', 's-ann', 't')",
            )
            .bind(&ctx.focus_run_id)
            .execute(&mut conn)
            .await
            .unwrap();
            drop(conn);

            let (out, _, _) = run_once(&ctx, &pool).await;
            let failure = out.unwrap().expect("the call fails past the review");
            assert!(
                failure
                    .plain_language
                    .contains("not a secure (https) address"),
                "{failure:?}"
            );
            assert_eq!(scans.get(), 1, "nobody is asked again");
            let rows = review_rows(&ctx).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].outcome,
                crate::persistence::step_review_store::ReviewOutcome::Redacted
            );
        })
        .await;
    }

    /// A local pick under an external ceiling is not reviewed (decisions.id=947): the
    /// scanner is never consulted (it would park the step if it were), and no review
    /// row is written.
    #[tokio::test]
    async fn a_local_pick_under_an_external_ceiling_is_not_reviewed() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let stub = LocalModelStub::start(1);
            let ctx = resolved_ctx(
                Some(requirement(
                    QualityBar::Basic,
                    &[Destination::Local, Destination::HostedApi],
                    ExternalAccess::AnonymousPreferred,
                )),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                "Write it.",
            );
            let mut forced = step_review::forced::clean();
            forced.available = false;
            let _g = step_review::forced::arm(&ctx, forced).await;

            let (out, track, _) = run_once(&ctx, &pool).await;
            assert!(out.unwrap().is_none(), "the local step ran");
            assert_eq!(stub.server.captured().len(), 1);
            assert_eq!(track.steps().len(), 1);
            assert!(
                review_rows(&ctx).await.is_empty(),
                "no review of a local pick"
            );
        })
        .await;
    }

    /// The text that reaches the network is the text the review released: its hash
    /// matches the stored outbound hash, and the original words are not in it.
    #[tokio::test]
    async fn the_text_sent_hashes_to_the_stored_outbound_hash() {
        use crate::conductor::step_review::{
            fake::{asked_span, FakeScanner},
            review_prompt, ReviewDecision, ReviewInput,
        };
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, true).await;
            let ctx = hosted_ctx(RunAttendance::Attended, &key_hex);
            let prompt = "Write to Ann Smith about the invoice";
            let _g = step_review::forced::arm(&ctx, step_review::forced::clean()).await;

            let mut conn = crate::persistence::output_store::open_outputs_db(
                &ctx.user_id,
                &ctx.persona_id,
                &ctx.key_hex,
            )
            .await
            .unwrap();
            let scanner = FakeScanner::pending(vec![asked_span("s1", prompt, "Ann Smith")]);
            let input = ReviewInput {
                run_id: &ctx.focus_run_id,
                action_id: &ctx.step.action_id,
                prompt,
                provider_id: Some("fixture-host-a"),
            };
            assert!(matches!(
                review_prompt(&mut conn, &scanner, &input).await,
                ReviewDecision::Park(_)
            ));
            sqlx::query(
                "INSERT INTO consent_decisions
                 (id, focus_run_id, decision_type, decision, span_id, suggestion_text, created_at)
                 VALUES ('d1', ?, 'element_consent', 'generalize', 's1', '[person]', 't')",
            )
            .bind(&ctx.focus_run_id)
            .execute(&mut conn)
            .await
            .unwrap();
            let ReviewDecision::Send(reviewed) = review_prompt(&mut conn, &scanner, &input).await
            else {
                panic!("the answered review sends")
            };

            hosted_step_call(
                &loopback_client(),
                &pool,
                &ctx,
                "fixture-host-a",
                &model,
                &generate_request(reviewed.as_str()),
                &reviewed,
                &chrono::Utc,
                loopback_build,
                None,
            )
            .await
            .unwrap();

            let sent = server.captured()[0].json();
            let body_text = sent["messages"][0]["content"].as_str().unwrap().to_owned();
            assert_eq!(body_text, "Write to [person] about the invoice");
            assert!(!body_text.contains("Ann"));
            let rows = crate::persistence::step_review_store::list_reviews(
                &mut conn,
                &ctx.focus_run_id,
                &ctx.step.action_id,
            )
            .await
            .unwrap();
            assert_eq!(
                rows[0].outbound_sha.as_deref(),
                Some(step_review::sha256_hex(&body_text).as_str())
            );
            assert_eq!(rows[0].outbound_len, Some(body_text.len()));
        })
        .await;
    }

    /// Text other than the reviewed text never reaches a request: the call refuses.
    #[tokio::test]
    async fn a_request_that_differs_from_the_reviewed_text_is_refused() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, true).await;
            let err = hosted_step_call(
                &loopback_client(),
                &pool,
                &hosted_ctx(RunAttendance::Attended, &key_hex),
                "fixture-host-a",
                &model,
                &generate_request("the original, unreviewed words"),
                &ReviewedPrompt::for_test("the reviewed words"),
                &chrono::Utc,
                loopback_build,
                None,
            )
            .await
            .unwrap_err();
            assert!(matches!(err, ConductorError::TierBoundaryViolation { .. }));
            assert!(server.captured().is_empty(), "nothing was sent");
        })
        .await;
    }
    // -----------------------------------------------------------------------
    // items.id=901: the run plan's envelope. A resolved step may run on a local model
    // the resolver itself picks, or on the model an approved pin names. Any other pick
    // stops with plan.exceeded BEFORE the treatment and every gate, with nothing sent.
    // -----------------------------------------------------------------------

    use crate::conductor::plan_approval::Approval;
    use crate::conductor::run_plan::{OfferStrategy, Pin};

    fn approval_with(pins: Vec<Pin>) -> Option<Arc<Approval>> {
        Some(Arc::new(Approval {
            digest: "d".repeat(64),
            ceiling_micros: 1_000,
            pins,
        }))
    }

    fn pin_of(strategy: OfferStrategy, provider: &str, model: &str) -> Pin {
        Pin {
            action_id: "s1".to_owned(),
            strategy,
            provider_id: provider.to_owned(),
            model_id: model.to_owned(),
        }
    }

    /// An approval whose one pin is the fixture host's model, for the tests that exercise a
    /// hosted pick under the downstream gates.
    fn hosted_pin_approval() -> Option<Arc<Approval>> {
        approval_with(vec![pin_of(
            OfferStrategy::PaidApi,
            "fixture-host-a",
            "fixture-model-a",
        )])
    }

    fn plan_reason(r: &FailureResult) -> String {
        assert_eq!(parked_code(r), "plan.exceeded");
        r.metadata.as_ref().unwrap()["reason"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// A hosted-only resolved step on a measured hosted model, so the resolver itself
    /// picks the hosted model. Returns the context with the fixture key.
    async fn measured_hosted_ctx(pool: &sqlx::SqlitePool, server: &LoopbackServer) -> StepContext {
        let (key_hex, _model) = spend_rig(pool, server, Some(PRICE_TEXT)).await;
        grant_consent(pool, &key_hex, true).await;
        sqlx::query(
            "INSERT INTO model_capabilities
               (subject_kind, provider_id, model_id, task_type, profile, attained_bar,
                model_class, sample_count, measured_at, provenance)
             VALUES ('hosted', 'fixture-host-a', 'fixture-model-a', 'creative_writing',
                     'rma_letter', 'basic', 'large', 1, '2026-11-13', 'harness_hosted')",
        )
        .execute(pool)
        .await
        .unwrap();
        let req = requirement(
            QualityBar::Basic,
            &[Destination::HostedApi],
            ExternalAccess::AnonymousPreferred,
        );
        let mut ctx = resolved_ctx(
            Some(req),
            ExternalAccess::AnonymousPreferred,
            Some("rma_letter"),
            "Write it.",
        );
        ctx.key_hex = key_hex;
        ctx
    }

    /// The resolver picks a hosted model and the run has no approval: the step stops
    /// before the treatment, Gate 1, the review and the spend gate, and no request is made.
    #[tokio::test]
    async fn a_hosted_pick_with_no_approval_stops_before_any_gate_and_nothing_is_sent() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let ctx = measured_hosted_ctx(&pool, &server).await;
            assert!(ctx.plan.is_none());
            // The resolver does pick it: this is the plan's check, not the resolver's.
            let sel = select_model_for_block(&pool, &ctx, ctx.step.requirement.as_ref().unwrap())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(sel.selected.provider_id.is_some());

            let (out, track, gateway) = run_once(&ctx, &pool).await;
            let failure = out.unwrap().expect("the step parked");
            assert_eq!(plan_reason(&failure), "no_approval");
            assert_eq!(failure.action, FailureAction::AwaitUser);
            assert_eq!(gateway.logger.entry_count(), 0, "no gate ran");
            assert!(track.steps().is_empty());
            assert_eq!(server.connections(), 0, "the mock listener saw nothing");
            assert!(server.captured().is_empty());
        })
        .await;
    }

    /// A zero-pin approval permits resolver LOCAL picks only: a hosted pick is still not
    /// planned.
    #[tokio::test]
    async fn a_zero_pin_approval_does_not_permit_a_hosted_pick() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let mut ctx = measured_hosted_ctx(&pool, &server).await;
            ctx.plan = approval_with(vec![]);
            let (out, track, gateway) = run_once(&ctx, &pool).await;
            assert_eq!(plan_reason(&out.unwrap().unwrap()), "model_not_planned");
            assert_eq!(gateway.logger.entry_count(), 0);
            assert!(track.steps().is_empty());
            assert_eq!(server.connections(), 0);

            // A pin on ANOTHER step does not cover this one either.
            ctx.plan = approval_with(vec![Pin {
                action_id: "another-step".to_owned(),
                ..pin_of(OfferStrategy::PaidApi, "fixture-host-a", "fixture-model-a")
            }]);
            let (out, _, _) = run_once(&ctx, &pool).await;
            assert_eq!(plan_reason(&out.unwrap().unwrap()), "model_not_planned");
            assert_eq!(server.connections(), 0);
        })
        .await;
    }

    /// The resolver's own local pick runs under a zero-pin approval, and with no approval
    /// at all (the run needed none).
    #[tokio::test]
    async fn a_resolver_local_pick_runs_with_or_without_a_zero_pin_approval() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            for plan in [None, approval_with(vec![])] {
                let stub = LocalModelStub::start(1);
                let mut ctx = resolved_ctx(
                    Some(requirement(
                        QualityBar::Basic,
                        &[Destination::Local, Destination::HostedApi],
                        ExternalAccess::AnonymousPreferred,
                    )),
                    ExternalAccess::AnonymousPreferred,
                    Some("rma_letter"),
                    "Write it.",
                );
                ctx.plan = plan;
                let (out, track, _) = run_once(&ctx, &pool).await;
                assert!(out.unwrap().is_none(), "the local step ran");
                assert_eq!(stub.server.captured().len(), 1);
                assert_eq!(track.steps().len(), 1);
            }
        })
        .await;
    }

    /// An approved pin replaces the resolver. A hosted model nobody has measured, which the
    /// resolver would refuse (TooWeak), is selected whole when the user pinned it.
    #[tokio::test]
    async fn an_approved_pin_selects_the_model_the_resolver_would_refuse() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let (key_hex, _m) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, true).await;
            let req = requirement(
                QualityBar::Basic,
                &[Destination::HostedApi],
                ExternalAccess::AnonymousPreferred,
            );
            let mut ctx = resolved_ctx(
                Some(req.clone()),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                "Write it.",
            );
            ctx.key_hex = key_hex;
            // Unpinned: the resolver refuses (no measured score).
            let err = select_under_plan(&pool, &ctx, &req).await.err().unwrap();
            assert_eq!(block_code(&err), "resolver.too_weak");

            ctx.plan = approval_with(vec![pin_of(
                OfferStrategy::PaidApi,
                "fixture-host-a",
                "fixture-model-a",
            )]);
            let sel = select_under_plan(&pool, &ctx, &req)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert_eq!(sel.selected.provider_id.as_deref(), Some("fixture-host-a"));
            let row = sel.selected.hosted_model.as_ref().expect("the model row");
            assert_eq!(sel.selected.model_id, row.model_id);
            assert!(sel.fallbacks.is_empty());
            assert_eq!(server.connections(), 0, "choosing sends nothing");
        })
        .await;
    }

    /// An approved pin of an uncalibrated LOCAL model is used where the resolver parks.
    #[tokio::test]
    async fn an_approved_pin_selects_an_unmeasured_local_model() {
        with_temp_shared_db(|pool| async move {
            install_local_models(&pool).await;
            let req = local_req();
            let ctx = resolved_ctx(
                Some(req.clone()),
                ExternalAccess::LocalOnly,
                None,
                "Write it.",
            );
            // No document profile and no task-wide cell: every local model is unmeasured.
            let err = select_under_plan(&pool, &ctx, &req).await.err().unwrap();
            assert_eq!(block_code(&err), "resolver.too_weak");
            let ConductorError::SpendBlock { params, .. } = &err else {
                panic!()
            };
            let tag = params["options"][0]["model_id"]
                .as_str()
                .unwrap()
                .to_owned();

            let mut ctx = ctx;
            ctx.plan = approval_with(vec![pin_of(OfferStrategy::LocalUnmeasured, "", &tag)]);
            let sel = select_under_plan(&pool, &ctx, &req)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert_eq!(sel.selected.model_id, tag);
            assert!(sel.selected.provider_id.is_none() && sel.selected.hosted_model.is_none());

            // Measured BELOW the bar since: never selected, pin or not.
            sqlx::query(
                "INSERT INTO model_capabilities
                   (subject_kind, provider_id, model_id, task_type, profile, attained_bar,
                    model_class, sample_count, measured_at, provenance)
                 VALUES ('local', '', ?, 'creative_writing', '', 'below_basic', 'small', 1,
                         '2026-11-13', 'jason_read')",
            )
            .bind(&tag)
            .execute(&pool)
            .await
            .unwrap();
            let err = select_under_plan(&pool, &ctx, &req).await.err().unwrap();
            assert_eq!(block_code(&err), "plan.exceeded");
        })
        .await;
    }

    /// A pin that cannot be used stops the step: no silent switch to another model or
    /// strategy, and nothing reaches the listener.
    #[tokio::test]
    async fn a_pin_that_is_no_longer_available_stops_the_step() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok()]);
            let mut ctx = measured_hosted_ctx(&pool, &server).await;
            install_local_models(&pool).await;
            let good_key = ctx.key_hex.clone();

            let cases: Vec<(&str, Pin, &str)> = vec![
                (
                    "a model that is not a candidate",
                    pin_of(OfferStrategy::PaidApi, "fixture-host-a", "no-such-model"),
                    &good_key,
                ),
                (
                    "a paid pin whose provider has no candidate",
                    pin_of(OfferStrategy::PaidApi, "no-such-host", "fixture-model-a"),
                    &good_key,
                ),
                (
                    "a hosted model with no key",
                    pin_of(OfferStrategy::PaidApi, "fixture-host-a", "fixture-model-a"),
                    "",
                ),
                (
                    "a local pin naming a hosted model",
                    pin_of(OfferStrategy::LocalUnmeasured, "", "fixture-model-a"),
                    &good_key,
                ),
                (
                    "a pin of a model the resolver picks anyway",
                    pin_of(OfferStrategy::Local, "", "anything"),
                    &good_key,
                ),
            ];
            for (label, pin, key) in cases {
                ctx.plan = approval_with(vec![pin]);
                ctx.key_hex = key.to_owned();
                let (out, track, gateway) = run_once(&ctx, &pool).await;
                let failure = out.unwrap().unwrap_or_else(|| panic!("{label}: it ran"));
                assert_eq!(plan_reason(&failure), "pin_unavailable", "{label}");
                assert_eq!(gateway.logger.entry_count(), 0, "{label}: no gate ran");
                assert!(track.steps().is_empty(), "{label}");
            }
            assert_eq!(server.connections(), 0, "no request, whatever the pin");
        })
        .await;
    }

    /// The plan's refusal is made in the selection, before the treatment and before any
    /// gate; ctx.plan is read only by the two reviewed functions; Code::PlanExceeded is built
    /// in one place.
    #[test]
    fn the_plan_is_read_only_in_the_selection_and_checked_before_the_treatment() {
        let src = include_str!("executor.rs");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let live: String = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let flat: String = live.split_whitespace().collect::<Vec<_>>().join(" ");
        // The three readers: the approved pin, the envelope check, and the cost ceiling
        // handed to the spend gate.
        assert_eq!(
            flat.matches("ctx.plan").count(),
            3,
            "plan_pin, check_plan_envelope and plan_cap_micros"
        );
        let cap_fn = flat
            .split("fn plan_cap_micros(")
            .nth(1)
            .unwrap()
            .split("/// A plan stop as the error")
            .next()
            .unwrap();
        assert!(cap_fn.contains("if !is_resolved(ctx) { return None; }"));
        assert!(cap_fn.contains("ctx.plan .as_deref() .map_or(0,"));
        // The one place a SpendCall takes it.
        assert_eq!(
            flat.matches("plan_cap_micros: plan_cap_micros(ctx)")
                .count(),
            1
        );
        let pin_fn = flat
            .split("fn plan_pin(")
            .nth(1)
            .unwrap()
            .split("fn plan_exceeded(")
            .next()
            .unwrap();
        assert!(pin_fn.contains("ctx.plan .as_deref()"));
        let env_fn = flat
            .split("fn check_plan_envelope(")
            .nth(1)
            .unwrap()
            .split("async fn select_under_plan(")
            .next()
            .unwrap();
        assert!(env_fn.contains("ctx.plan.is_none()"));
        // One place builds the stop.
        assert_eq!(flat.matches("Code::PlanExceeded").count(), 1);
        // execute_once: the selection under the plan, then the treatment, then Gate 1.
        let body = live
            .split("async fn execute_once<")
            .nth(1)
            .and_then(|r| r.split("pub(crate) fn render_template(").next())
            .expect("execute_once");
        let sel = body
            .find("select_under_plan(pool, ctx, req)")
            .expect("selection");
        let treat = body
            .find("derive_treatment(ctx, resolver_req, &selected_model)")
            .unwrap();
        let g1 = body.find(".gate1(").unwrap();
        assert!(sel < treat && treat < g1);
        // The plain resolver call is not used by execute_once any more.
        assert!(!body.contains("select_model_for_block(pool, ctx, req)"));
        // A bound run and an ordinary step never reach the plan.
        assert!(flat.contains("(Some(bound), _) => select_bound_model(pool, ctx, bound).await?"));
    }
    /// The ceiling handed to the spend gate: the approval's ceiling for a call of a resolved
    /// step; ZERO for a resolved step with no approval (the stricter direction); None for
    /// every other call (an ordinary step, a bound API Chat run), which the gate then decides
    /// exactly as before.
    #[test]
    fn the_spend_gate_gets_a_ceiling_only_for_a_resolved_step() {
        let req = requirement(
            QualityBar::Basic,
            &[Destination::HostedApi],
            ExternalAccess::AnonymousPreferred,
        );
        let mut resolved = resolved_ctx(
            Some(req),
            ExternalAccess::AnonymousPreferred,
            None,
            "Write it.",
        );
        assert_eq!(plan_cap_micros(&resolved), Some(0), "no approval, no spend");
        resolved.plan = approval_with(vec![]);
        assert_eq!(plan_cap_micros(&resolved), Some(1_000));
        // An ordinary step of the same run never carries the cap.
        let mut ordinary = resolved_ctx(None, ExternalAccess::AnonymousPreferred, None, "x");
        ordinary.plan = approval_with(vec![]);
        assert_eq!(plan_cap_micros(&ordinary), None);
        // Neither does a bound API Chat run, whatever the step carries.
        resolved.bound = Some(crate::api_chat::BoundDestination {
            provider_id: "fixture-host-a".to_owned(),
            model_id: "fixture-model-a".to_owned(),
        });
        assert_eq!(plan_cap_micros(&resolved), None);
    }

    /// End to end through the hosted call: a resolved step under an approval is held to its
    /// ceiling by the spend gate (nothing sent when over), and an ordinary step carrying an
    /// approval with a zero ceiling is not.
    #[tokio::test]
    async fn the_spend_gate_holds_a_resolved_step_to_the_approved_ceiling() {
        with_temp_shared_db(|pool| async move {
            let server = LoopbackServer::start(vec![loopback_chat_ok(), loopback_chat_ok()]);
            let (key_hex, model) = spend_rig(&pool, &server, Some(PRICE_TEXT)).await;
            grant_consent(&pool, &key_hex, true).await;
            let req = requirement(
                QualityBar::Basic,
                &[Destination::HostedApi],
                ExternalAccess::AnonymousPreferred,
            );
            let mut resolved = resolved_ctx(
                Some(req),
                ExternalAccess::AnonymousPreferred,
                Some("rma_letter"),
                "Write it.",
            );
            resolved.key_hex = key_hex.clone();
            let with_ceiling = |ceiling_micros: u64| {
                Some(Arc::new(Approval {
                    digest: "d".repeat(64),
                    ceiling_micros,
                    pins: vec![],
                }))
            };
            async fn call(
                pool: &sqlx::SqlitePool,
                model: &ProviderModel,
                ctx: &StepContext,
            ) -> Result<GenerateResponse, ConductorError> {
                hosted_step_call(
                    &loopback_client(),
                    pool,
                    ctx,
                    "fixture-host-a",
                    model,
                    &generate_request("hello there"),
                    &ReviewedPrompt::for_test("hello there"),
                    &chrono::Utc,
                    loopback_build,
                    None,
                )
                .await
            }

            // Over the approved ceiling (zero): refused by the gate, nothing sent.
            resolved.plan = with_ceiling(0);
            let err = call(&pool, &model, &resolved).await.unwrap_err();
            assert_eq!(block_code(&err), "plan.exceeded");
            let ConductorError::SpendBlock { params, .. } = &err else {
                panic!("{err:?}")
            };
            assert_eq!(params["reason"], "over_ceiling");
            assert_eq!(server.connections(), 0, "the mock listener saw nothing");

            // Under a generous ceiling the same call goes through.
            resolved.plan = with_ceiling(1_000_000);
            assert!(call(&pool, &model, &resolved).await.is_ok());
            assert_eq!(server.captured().len(), 1);

            // An ordinary external step is not held to the plan's ceiling, even if its
            // context carries an approval with a zero ceiling.
            let mut ordinary = hosted_ctx(RunAttendance::Attended, &key_hex);
            ordinary.plan = with_ceiling(0);
            assert!(call(&pool, &model, &ordinary).await.is_ok());
            assert_eq!(server.captured().len(), 2);
        })
        .await;
    }
    /// THE COUPLING (items.id=901 stage 6): Gate 3's look-ahead may count a following planned
    /// step as tier 1 ONLY because the executor stops a hosted pick on it. The two facts are
    /// pinned together over every kind of approval: a step the look-ahead counts as local is
    /// exactly a step on which the executor can produce no hosted selection, and a step with
    /// a paid pin is exactly the one it can.
    #[test]
    fn the_look_ahead_counts_a_step_as_local_exactly_when_the_executor_stops_a_hosted_pick() {
        use crate::conductor::lifecycle::next_execution_tier_under;
        use crate::conductor::run_plan::tests::{hosted_req, planned, step as plain_step};

        let ceiling = ExternalAccess::AnonymousPreferred;
        let hosted_tier = crate::conductor::lifecycle::execution_tier_ordinal(ceiling);
        let hosted_pick = || {
            SelectedModel::hosted(ProviderModel {
                id: "row".to_owned(),
                provider_id: "fixture-host-a".to_owned(),
                model_id: "fixture-model-a".to_owned(),
                context_window_tokens: 4096,
                is_default: true,
                created_at: "t".to_owned(),
                pricing: None,
                request_options: None,
                token_factor: 1.0,
                output_overhead: 1.0,
                status: crate::persistence::provider_store::ModelStatus::Active,
                retires_at: None,
                requires_paid: false,
                qr_recommended: false,
                qr_recommendation_rank: None,
                retains_data_override: None,
                trains_on_data_override: None,
                retention_days_override: None,
                context_window_source: None,
                context_window_as_of: None,
            })
        };
        let steps = vec![plain_step("now"), planned("s1", hosted_req())];
        let cases: Vec<(&str, Vec<Pin>)> = vec![
            ("no pin", vec![]),
            (
                "a pin of an unmeasured local model",
                vec![pin_of(OfferStrategy::LocalUnmeasured, "", "some-local")],
            ),
            (
                "a paid pin",
                vec![pin_of(
                    OfferStrategy::PaidApi,
                    "fixture-host-a",
                    "fixture-model-a",
                )],
            ),
            (
                "a paid pin for another step",
                vec![Pin {
                    action_id: "another".to_owned(),
                    ..pin_of(OfferStrategy::PaidApi, "fixture-host-a", "fixture-model-a")
                }],
            ),
        ];
        for (label, pins) in cases {
            let approval = Approval {
                digest: "d".repeat(64),
                ceiling_micros: 1_000,
                pins,
            };
            // The look-ahead's answer for step s1.
            let counted_local =
                next_execution_tier_under(&steps, 0, ceiling, Some(&approval)) == Some(1);
            // The executor's: can a hosted selection come out of the plan check for s1?
            let mut ctx = resolved_ctx(
                steps[1].requirement.clone(),
                ExternalAccess::AnonymousPreferred,
                None,
                "x",
            );
            ctx.plan = Some(Arc::new(approval.clone()));
            let executor_can_go_hosted = match plan_pin(&ctx) {
                // A pin replaces the resolver: it yields hosted only for a paid pin.
                Some(p) => p.strategy == OfferStrategy::PaidApi,
                // No pin: the resolver's pick goes through the envelope check, which refuses
                // every hosted selection.
                None => {
                    let refused = check_plan_envelope(&ctx, &hosted_pick()).is_err();
                    assert!(
                        refused,
                        "{label}: a hosted pick must be refused without a pin"
                    );
                    false
                }
            };
            assert_eq!(counted_local, !executor_can_go_hosted, "{label}");
            // And the look-ahead's predicate is the pin the executor reads.
            assert_eq!(
                approval.has_paid_pin("s1"),
                executor_can_go_hosted,
                "{label}"
            );
            if executor_can_go_hosted {
                assert_eq!(
                    next_execution_tier_under(&steps, 0, ceiling, Some(&approval)),
                    Some(hosted_tier),
                    "{label}: a paid pin keeps the real tier"
                );
            }
        }
        // No approval at all: the look-ahead does not refine (today's over-prompt), even
        // though the executor would also refuse.
        assert_eq!(
            next_execution_tier_under(&steps, 0, ceiling, None),
            Some(hosted_tier)
        );
    }
}
