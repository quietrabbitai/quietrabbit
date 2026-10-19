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
use crate::conductor::tokens::{ExternalAccess, StepDefinition};
use crate::conductor::types::{PersonalTrack, SharedStateTrack, TaskStep, TaskTrack};
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
    pub step: StepDefinition,
    pub focus_id: String,
    pub focus_run_id: String,
    pub user_input: String,
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
    /// external_access_override (StepDefinition::external_access_override)
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
        let mut retry_count = 0u32;

        loop {
            match self
                .execute_once(
                    &ctx,
                    retry_count,
                    personal_track,
                    task_track,
                    shared_state,
                    failure_handler,
                    privacy_gateway,
                    scheduler,
                    app_handle,
                    pool,
                )
                .await
            {
                // DisclosureLogWrite (F_SYSTEM) and VoiceProfileContamination (F4 subtype)
                // propagate out of execute_once as Err(). Convert to FailureResult.
                // Python oracle: except (DisclosureLogWriteError, VoiceProfileContaminationError)
                Err(e) => {
                    return Some(failure_handler.handle(
                        &e,
                        Some(&ctx.step.step_id),
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
                        // items.id=710: honor a provider's Retry-After. A wait
                        // longer than the cap parks the run instead of
                        // sleeping through it. The inference slot was
                        // released before this point.
                        if let Some(parked) = honor_retry_after(&result).await {
                            return Some(parked);
                        }
                        retry_count += 1;
                        continue;
                    }
                    return Some(result);
                }
            }
        }
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
        let execution_tier = ctx.execution_tier;
        let abstraction_tier = ctx.abstraction_tier;
        let raw_abstraction = ctx.raw_abstraction;

        // -- Floor invariants (ADR-012 Amendment 3, D6-348) --
        // Explicit Err() returns — never debug_assert!, which is stripped in release builds.
        // These are load-bearing privacy checks, not programmer-error guards.
        if execution_tier > 1 {
            if abstraction_tier < 2 {
                return Err(ConductorError::TierBoundaryViolation {
                    plain_language: format!(
                        "Floor invariant: execution_tier={execution_tier} requires \
                         abstraction_tier >= 2, got {abstraction_tier}. \
                         Lifecycle configuration error. [Get help]"
                    ),
                });
            }
            if raw_abstraction > abstraction_tier {
                return Err(ConductorError::TierBoundaryViolation {
                    plain_language: format!(
                        "Floor invariant: raw_abstraction ({raw_abstraction}) \
                         > abstraction_tier ({abstraction_tier}). \
                         Lifecycle configuration error. [Get help]"
                    ),
                });
            }
        } else if abstraction_tier != raw_abstraction {
            return Err(ConductorError::TierBoundaryViolation {
                plain_language: format!(
                    "Tier 1 invariant: abstraction_tier ({abstraction_tier}) \
                     != raw_abstraction ({raw_abstraction}) at execution_tier=1. \
                     Lifecycle configuration error. [Get help]"
                ),
            });
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
                            ctx.step.step_id,
                            override_.as_str(),
                            ctx.focus_external_access.as_str()
                        ),
                    },
                    Some(&ctx.step.step_id),
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
                    Some(&ctx.step.step_id),
                    Some(&ctx.focus_id),
                    retry_count,
                )));
            }
        }

        // -- Step 4.5 — Tier 1.5 (qr_hosted) provider preference gate (F10) --
        // No prescribed default (architecture: "User choice at install. No
        // prescribed default."). An unset preference is a real gap, not
        // silently resolved to any default — short-circuits before any provider
        // is touched. Only checked when this step requires external access
        // at all; qr_local never needs a provider. A provider the user HAS
        // set up but this step's ceiling excludes (items.id=692) fails
        // separately, naming the setting to change.
        if let Some(error) = missing_provider_error(
            ctx.effective_access,
            ctx.qr_hosted_provider_preference.as_deref(),
            ctx.qr_hosted_ceiling_excluded.as_deref(),
        ) {
            return Ok(Some(failure_handler.handle(
                &error,
                Some(&ctx.step.step_id),
                Some(&ctx.focus_id),
                retry_count,
            )));
        }

        let selected_model = match &ctx.bound {
            Some(bound) => select_bound_model(pool, ctx, bound).await?,
            None => {
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

        // Build gate track for Gate1/Gate2 calls.
        // privacy::types::PersonalTrack (typed enum fields) differs from
        // conductor::types::PersonalTrack (sqlx-friendly String fields).
        // to_gate_track() converts between them. See module comment.
        let gate_track = to_gate_track(personal_track);

        // -- Steps 6-7 — PG_GATE_1: field approval + abstraction --
        let g1 = privacy_gateway
            .gate1(
                &ctx.step.step_id,
                &ctx.focus_run_id,
                &gate_track,
                abstraction_tier,
                raw_abstraction,
                execution_tier,
                ctx.effective_access,
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
                        step_id: ctx.step.step_id.clone(),
                        focus_run_id: ctx.focus_run_id.clone(),
                        execution_tier,
                        abstraction_tier: Some(abstraction_tier),
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
                    "step_id".to_owned(),
                    serde_json::Value::String(ctx.step.step_id.clone()),
                );
                meta.insert(
                    "execution_tier".to_owned(),
                    serde_json::Value::Number(execution_tier.into()),
                );
                meta.insert(
                    "abstraction_tier".to_owned(),
                    serde_json::Value::Number(abstraction_tier.into()),
                );
                return Ok(Some(FailureResult {
                    action: FailureAction::AwaitFloorConsent,
                    failure_mode: None,
                    plain_language: "Quiet Rabbit modified some of your fields to maintain \
                         privacy for external use. Please review and choose."
                        .to_owned(),
                    is_recoverable: true,
                    severity: FailureSeverity::Pause,
                    step_id: Some(ctx.step.step_id.clone()),
                    focus_id: Some(ctx.focus_id.clone()),
                    metadata: Some(meta),
                }));
            }
        }

        // -- Write projected fields to disclosure buffer (pre-Step 8) --
        shared_state.write_disclosure_buffer(&ctx.step.step_id, projected_fields.clone());

        // -- Step 8 — assemble final prompt --
        // Collect output_vars and previous_output before render (read-only task_track access).
        // Step 12's add_step() hasn't run yet; borrow is released before the mutable call.
        let output_vars = collect_output_vars(task_track.steps());
        let previous_output = task_track.last_output().unwrap_or("").to_owned();

        // scan_voice_profile is async (writes disclosure audit on contamination).
        // Called once; result passed to both render paths.
        let cleaned_voice_profile =
            scan_voice_profile(ctx, personal_track, privacy_gateway).await?;
        let voice_profile_str = format_voice_profile(&cleaned_voice_profile);

        let prompt = if ctx.effective_access != ExternalAccess::LocalOnly {
            let disclosure = shared_state.read_disclosure_buffer(&ctx.step.step_id);
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

        let effective_ctx = match options_raw.get("num_ctx").and_then(|v| v.as_u64()) {
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

        let ctx_status = check_context_window(&prompt, &ctx.step.task_type, effective_ctx);
        if ctx_status.status == ContextWindowStatusKind::Exceeded {
            return Ok(Some(failure_handler.handle(
                &ConductorError::ContextWindowExceeded {
                    plain_language: ctx_status.plain_language.unwrap_or_else(|| {
                        "This is too long for local processing. \
                         [Use an external service] [Shorten the document]"
                            .to_owned()
                    }),
                },
                Some(&ctx.step.step_id),
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
                    Some(&ctx.step.step_id),
                    Some(&ctx.focus_id),
                    retry_count,
                ),
            ));
        }

        // items.id=715: a hosted step streams only when the run's owner asked
        // for it (API Chat). A local step never does.
        let hosted_step = ctx.effective_access != ExternalAccess::LocalOnly;
        let stream_sink: Option<Box<dyn StreamSink>> = match (&ctx.stream, step_streams(ctx)) {
            (Some(_), true) => Some(match app_handle {
                Some(h) => Box::new(crate::chat_stream::TauriDeltaSink::new(
                    h.clone(),
                    ctx.focus_run_id.clone(),
                    ctx.step.step_id.clone(),
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
                match (
                    ctx.qr_hosted_provider_preference.as_deref(),
                    selected_model.hosted_model.as_ref(),
                ) {
                    (Some(provider_id), Some(model)) => {
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
                            &chrono::Local,
                            HostedInputs::build_call,
                            parts,
                        )
                        .await
                    }
                    _ => unreachable!(
                        "effective_access != LocalOnly with no qr_hosted_provider_preference \
                     or no resolved model -- the Step 4.5 guard and select_model() must \
                     already have short-circuited"
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
                        keep_partial_reply(ctx, &stream_tail).await;
                        let g2 = reply_gate2(
                            ctx,
                            privacy_gateway,
                            &gate_track,
                            &stream_tail.received_text,
                            selected_model.provider_id.clone(),
                            &g1.fields_shared,
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
                return Ok(Some(failure_handler.handle(
                    &e,
                    Some(&ctx.step.step_id),
                    Some(&ctx.focus_id),
                    retry_count,
                )));
            }
        };

        // -- Step 11 — PG_GATE_2: inbound contamination scan --
        // items.id=735 (decisions.id=883): for any external reply Gate 2
        // flags and notifies; it never holds, discards, edits or retracts it,
        // and the run continues to steps 12 and 13 whether or not it hit. A
        // streamed reply and a whole-body reply take this one path.
        let g2 = reply_gate2(
            ctx,
            privacy_gateway,
            &gate_track,
            &response.content,
            selected_model.provider_id.clone(),
            &g1.fields_shared,
        )
        .await;
        if let Some(sink) = stream_sink.as_deref() {
            sink.end(&StreamEnd {
                outcome: StreamOutcome::Complete,
                personal_flagged: g2.flagged,
                audit_unavailable: g2.audit_unavailable,
            });
        }
        record_completed_reply(ctx, &response).await;

        // -- Step 12 — update TaskTrack --
        // D4-040: content = model output ONLY — never prompt-expanded input.
        // step_sensitivity: from projected fields (Tier 2) or template token scan (qr_local),
        // raised to the highest matched Gate 2 field (completed_step).
        let step_sensitivity = compute_step_sensitivity(ctx, personal_track, &projected_fields);

        let step = completed_step(
            ctx,
            &response.content,
            step_sensitivity,
            execution_tier,
            &g2,
        );
        let step_sensitivity = step.sensitivity_severity;
        task_track.add_step(step);

        // -- Step 13 — PG_GATE_3: cross-tier content promotion (conditional) --
        // Only when next step has higher execution_tier.
        if let Some(next_tier) = ctx.next_execution_tier {
            if next_tier > execution_tier {
                let content_key = ctx.step.output_var.as_deref().unwrap_or(&ctx.step.step_id);

                let g3 = privacy_gateway
                    .gate3(
                        &ctx.step.step_id,
                        &ctx.focus_run_id,
                        &ctx.focus_name,
                        content_key,
                        &response.content,
                        step_sensitivity as u8,
                        next_tier,
                        ctx.focus_external_access,
                        execution_tier,
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
                        Some(&ctx.step.step_id),
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
                        step_id: Some(ctx.step.step_id.clone()),
                        focus_id: Some(ctx.focus_id.clone()),
                        metadata: None,
                    }));
                }

                if g3.approved {
                    shared_state.promote_content(&ctx.step.step_id, content_key, &response.content);
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
fn render_template(
    template: &str,
    tokens: &HashMap<String, String>,
    step_id: &str,
    focus_id: &str,
) -> String {
    let mut result = template.to_owned();
    for (token, value) in tokens {
        result = result.replace(&format!("{{{token}}}"), value);
    }
    let unresolved = find_tokens(&result);
    if !unresolved.is_empty() {
        log::debug!(
            "executor: unresolved tokens stripped: {:?} step={step_id} focus={focus_id}",
            unresolved
        );
        result = strip_tokens(&result);
    }
    result.trim().to_owned()
}

/// qr_local prompt render. Never reads disclosure buffer.
/// Token merge order: output_vars -> SYSTEM_TOKENS.
/// Python oracle: StepExecutor._render_prompt()
fn render_prompt(
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
    render_template(
        &ctx.step.prompt_template,
        &tokens,
        &ctx.step.step_id,
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
    tokens.insert("user_input".to_owned(), ctx.user_input.clone());
    tokens.insert("previous_output".to_owned(), previous_output.to_owned());
    tokens.insert("focus_context".to_owned(), ctx.focus_id.clone());
    tokens.insert("persona_context".to_owned(), ctx.persona_context.clone());
    tokens.insert("voice_profile".to_owned(), voice_profile_str.to_owned());
    render_template(
        &ctx.step.prompt_template,
        &tokens,
        &ctx.step.step_id,
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
                ctx.execution_tier,
                ctx.focus_id,
                ctx.step.step_id,
            );

            // Audit: log the contamination event via the disclosure logger.
            // This replaces Python's ctx.privacy_gateway.record_voice_profile_contamination().
            let _ = privacy_gateway
                .logger
                .write(DisclosureLogEntry {
                    step_id: ctx.step.step_id.clone(),
                    focus_run_id: ctx.focus_run_id.clone(),
                    execution_tier: ctx.execution_tier,
                    abstraction_tier: Some(ctx.abstraction_tier),
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

            if ctx.effective_access != ExternalAccess::LocalOnly {
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
) -> HostedRequest {
    HostedRequest {
        system: None,
        prompt: request.prompt.clone(),
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

/// Wait out a provider's Retry-After before a retry (items.id=710). Returns
/// `Some(parked)` - the run parks for the user, nothing is slept - when the
/// provider asked for more than RETRY_AFTER_CAP_SECS; otherwise sleeps for the
/// requested time (none when the result carries no Retry-After) and returns
/// `None`, and the caller retries.
async fn honor_retry_after(result: &FailureResult) -> Option<FailureResult> {
    let secs = retry_after_of(result)?;
    if secs > RETRY_AFTER_CAP_SECS {
        return Some(long_wait_result(result.clone(), secs));
    }
    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
    None
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
    tz: &Tz,
    build: fn(&HostedInputs) -> Result<CallContext, ProviderError>,
    stream: Option<StreamParts<'_>>,
) -> Result<GenerateResponse, ConductorError> {
    let hosted_request = hosted_request_from(&ctx.step.options_override, request);
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
fn step_streams(ctx: &StepContext) -> bool {
    ctx.stream.is_some() && ctx.effective_access != ExternalAccess::LocalOnly
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
async fn reply_gate2<L: DisclosureLogger>(
    ctx: &StepContext,
    privacy_gateway: &PrivacyGateway<L>,
    gate_track: &GateTrack,
    text: &str,
    provider: Option<String>,
    fields_shared: &[String],
) -> ReplyGate2 {
    if text.is_empty() {
        return ReplyGate2::default();
    }
    let found = privacy_gateway
        .gate2(
            &ctx.step.step_id,
            &ctx.focus_run_id,
            text,
            gate_track,
            ctx.execution_tier,
            ctx.effective_access,
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
                ctx.step.step_id
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
    match crate::persistence::message_store::mark_reply_personal_flagged(
        &ctx.user_id,
        &ctx.persona_id,
        &ctx.key_hex,
        &ctx.focus_run_id,
    )
    .await
    {
        Ok(_) => {}
        Err(e) => log::warn!(
            "executor: could not mark the reply of run {} personal-flagged: {e}",
            ctx.focus_run_id
        ),
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

/// The failure a step returns when the user chose 'Stop and save' for a key
/// rotation (items.id=778). It is never shown, retried or parked: the run's
/// lifecycle sees the stop token and cancels the run itself.
fn stopped_failure(ctx: &StepContext) -> FailureResult {
    FailureResult {
        action: FailureAction::Stop,
        failure_mode: Some("USER_STOP".to_owned()),
        plain_language: "This was stopped because the account's keys are being changed.".to_owned(),
        is_recoverable: false,
        severity: FailureSeverity::Stop,
        step_id: Some(ctx.step.step_id.clone()),
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
async fn keep_partial_reply(ctx: &StepContext, tail: &StreamTail) {
    if tail.received_text.is_empty() || ctx.key_hex.is_empty() {
        return;
    }
    let status = if tail.cancelled {
        crate::persistence::message_store::ReplyStatus::Cancelled
    } else {
        crate::persistence::message_store::ReplyStatus::Cut
    };
    if let Err(e) = crate::persistence::message_store::set_reply_outcome(
        &ctx.user_id,
        &ctx.persona_id,
        &ctx.key_hex,
        &ctx.focus_run_id,
        status,
        Some(&tail.received_text),
    )
    .await
    {
        log::warn!(
            "executor: could not keep the partial reply of run {}: {e}",
            ctx.focus_run_id
        );
    }
}

/// A reply that completed: mark it `truncated` when the provider stopped at
/// the output limit (items.id=716, the ruling of items.id=709 that a truncated
/// hosted answer must be surfaced), and for a streamed or bound run reset the
/// status to `complete` otherwise, so an earlier attempt that was cut and then
/// retried does not leave its mark on the finished reply. Not a chat send: a
/// no-op.
async fn record_completed_reply(ctx: &StepContext, response: &GenerateResponse) {
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
    if let Err(e) = crate::persistence::message_store::set_reply_outcome(
        &ctx.user_id,
        &ctx.persona_id,
        &ctx.key_hex,
        &ctx.focus_run_id,
        status,
        None,
    )
    .await
    {
        log::warn!(
            "executor: could not record the outcome of run {}'s reply: {e}",
            ctx.focus_run_id
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
        .or_else(|| Some(ctx.step.step_id.clone()))
        .filter(|v| !v.is_empty());
    TaskStep {
        step_id: ctx.step.step_id.clone(),
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
async fn get_context_window(
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
    personal_track: &PersonalTrack,
    projected_fields: &HashMap<String, String>,
) -> i32 {
    if ctx.effective_access != ExternalAccess::LocalOnly && !projected_fields.is_empty() {
        let severities: Vec<i32> = projected_fields
            .keys()
            .filter_map(|name| personal_track.fields().get(name))
            .map(|pf| pf.sensitivity_severity)
            .collect();
        return severities.into_iter().max().unwrap_or(1);
    }

    if ctx.effective_access == ExternalAccess::LocalOnly {
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
fn collect_output_vars(steps: &[TaskStep]) -> HashMap<String, String> {
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
fn find_tokens(template: &str) -> Vec<String> {
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
            let request =
                hosted_request_from(&std::collections::HashMap::new(), &generate_request("hi"));
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
            let request =
                hosted_request_from(&std::collections::HashMap::new(), &generate_request("hi"));
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
        let none = hosted_request_from(&std::collections::HashMap::new(), &request);
        assert_eq!(none.max_output_tokens, None);
        assert_eq!(none.temperature, Some(0.5));
        assert_eq!(none.prompt, "p");
        assert!(none.system.is_none());

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("num_predict".to_owned(), serde_json::json!(777));
        let some = hosted_request_from(&overrides, &request);
        assert_eq!(some.max_output_tokens, Some(777));

        // A value that is not a usable number is not a budget.
        overrides.insert("num_predict".to_owned(), serde_json::json!("lots"));
        assert_eq!(
            hosted_request_from(&overrides, &request).max_output_tokens,
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
                step_id: "s1".to_owned(),
                output_var: Some("draft".to_owned()),
                content: "hello".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
                personal_flagged: false,
                audit_unavailable: false,
            },
            TaskStep {
                step_id: "s2".to_owned(),
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
                step_id: "s1".to_owned(),
                output_var: Some("result".to_owned()),
                content: "first".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
                personal_flagged: false,
                audit_unavailable: false,
            },
            TaskStep {
                step_id: "s2".to_owned(),
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
        use crate::conductor::tokens::{StepDefinition, StepType};
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
            step: StepDefinition {
                step_id: "s1".to_owned(),
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
            },
            focus_id: "f".to_owned(),
            focus_run_id: "fr".to_owned(),
            user_input: "".to_owned(),
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
        };
        let mut projected = HashMap::new();
        projected.insert("name".to_owned(), "Alice (abstracted)".to_owned());
        assert_eq!(compute_step_sensitivity(&ctx, &pt, &projected), 2);
    }

    #[test]
    fn step_sensitivity_tier1_uses_template_scan() {
        use crate::conductor::tokens::{StepDefinition, StepType};
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
            step: StepDefinition {
                step_id: "s1".to_owned(),
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
            },
            focus_id: "f".to_owned(),
            focus_run_id: "fr".to_owned(),
            user_input: "".to_owned(),
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
        };
        assert_eq!(compute_step_sensitivity(&ctx, &pt, &HashMap::new()), 2);
    }

    #[test]
    fn step_sensitivity_no_personal_fields_is_1() {
        use crate::conductor::tokens::{StepDefinition, StepType};
        let pt = PersonalTrack::new();
        let ctx = StepContext {
            stop: None,
            step: StepDefinition {
                step_id: "s1".to_owned(),
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
            },
            focus_id: "f".to_owned(),
            focus_run_id: "fr".to_owned(),
            user_input: "".to_owned(),
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
        };
        assert_eq!(compute_step_sensitivity(&ctx, &pt, &HashMap::new()), 1);
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
        use crate::conductor::tokens::{StepDefinition, StepType};
        StepContext {
            stop: None,
            step: StepDefinition {
                step_id: "s1".to_owned(),
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
            },
            focus_id: "focus-1".to_owned(),
            focus_run_id: "run-1".to_owned(),
            user_input: String::new(),
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

    async fn pick_account_model(pool: &sqlx::SqlitePool, model_id: Option<&str>) {
        set_model_selection(
            pool,
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

            pick_account_model(&pool, Some("fixture-model-b")).await;
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
            pick_account_model(&pool, Some("fixture-model-c")).await;

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
            record_completed_reply(&ctx, &response(CompletionStatus::Truncated)).await;
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
            record_completed_reply(&ctx, &response(CompletionStatus::Complete)).await;
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
            record_completed_reply(&ctx, &response(CompletionStatus::Complete)).await;
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

            pick_account_model(&pool, Some("fixture-model-old")).await;
            let err = select_for_step(&pool, &key_hex).await.unwrap_err();
            assert_eq!(block_code(&err), "spend_block.model_retired");

            pick_account_model(&pool, Some("fixture-model-ghost")).await;
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
            pick_account_model(&pool, Some("live-x")).await;

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
            pick_account_model(&pool, Some("fixture-model-weak")).await;

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
            step_id: Some("s1".to_owned()),
            focus_id: Some("focus-1".to_owned()),
            metadata,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_is_waited_out_before_the_retry() {
        let start = tokio::time::Instant::now();
        assert!(honor_retry_after(&retry_result(Some(2))).await.is_none());
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_with_no_retry_after_does_not_wait() {
        let start = tokio::time::Instant::now();
        assert!(honor_retry_after(&retry_result(None)).await.is_none());
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_at_the_cap_is_slept_and_one_past_it_parks() {
        let start = tokio::time::Instant::now();
        assert!(honor_retry_after(&retry_result(Some(RETRY_AFTER_CAP_SECS)))
            .await
            .is_none());
        assert_eq!(
            start.elapsed(),
            std::time::Duration::from_secs(RETRY_AFTER_CAP_SECS)
        );

        let start = tokio::time::Instant::now();
        let parked = honor_retry_after(&retry_result(Some(RETRY_AFTER_CAP_SECS + 1)))
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

    // -----------------------------------------------------------------------
    // items.id=715: streamed hosted steps. Loopback only; the same spend rig.
    // -----------------------------------------------------------------------

    mod streaming {
        use super::*;
        use crate::conductor::privacy::logger::{FailLogger, TestLogger};
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
                    &gw,
                    &track,
                    "You live in Springfield, right?",
                    Some("fixture-host-a".to_owned()),
                    &[],
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
                    let g2 = reply_gate2(&ctx, &gw, &track, text, None, &shared).await;
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
                let g2 = reply_gate2(&ctx, &gw, &track, "You have hypertension", None, &[]).await;
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
                        &PrivacyGateway::new(TestLogger::new()),
                        &track,
                        text,
                        None,
                        &[],
                    )
                    .await;
                    let b = reply_gate2(
                        &streamed,
                        &PrivacyGateway::new(TestLogger::new()),
                        &track,
                        text,
                        None,
                        &[],
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
                &gw,
                &track,
                "Springfield and Acme Credit",
                None,
                &g1.fields_shared,
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
                &gw,
                &track,
                "Springfield and Acme Credit",
                None,
                &g1_ext.fields_shared,
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
        assert!(!f.is_recoverable);
    }
}
