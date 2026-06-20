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
//   Step 4  — Tier 3 handled by lifecycle; never reaches executor
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
//   - GroqProvider and OllamaClient are module-level OnceLock singletons (D6-349).
//   - No regex crate available; TOKEN_PATTERN scanning and PII detection use manual
//     char-level implementations with semantically identical behaviour to the Python
//     oracle's re.compile() patterns.
//   - scan_voice_profile() is async and returns Result<HashMap, ConductorError>
//     rather than raising. Tier 1: Ok(cleaned), contaminated attrs stripped.
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
//   - gate1.blocked always false in current migration (not_permitted path deferred);
//     check is preserved for correctness when gate is fully wired.
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
};
use crate::conductor::privacy::logger::{DisclosureLogEntry, DisclosureLogger};
use crate::conductor::privacy::types::{
    AbstractionPolicy, PersonalField as GateField, PersonalTrack as GateTrack, Sensitivity,
};
use crate::conductor::privacy::{logger::NoopLogger, PrivacyGateway};
use crate::conductor::tokens::StepDefinition;
use crate::conductor::types::{PersonalTrack, SharedStateTrack, TaskStep, TaskTrack};
use crate::providers::groq::GroqProvider;
use crate::providers::ollama_client::{check_context_window, OllamaClient};
use crate::providers::tier2_base::Tier2Provider;
use crate::providers::types::{ContextWindowStatusKind, GenerateOptions, GenerateRequest};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

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
// Module-level provider singletons (D6-349)
// ---------------------------------------------------------------------------
// GroqProvider and OllamaClient hold reqwest::Client instances (Send+Sync).
// Constructed once via OnceLock — safe across threads and focus runs.
//
// Python oracle: module-level _groq_provider = GroqProvider()
// D6-349: GroqProvider instantiation at call sites in executor.rs, not in FocusRun.

static GROQ_PROVIDER: OnceLock<GroqProvider> = OnceLock::new();
static OLLAMA_CLIENT: OnceLock<OllamaClient> = OnceLock::new();

fn groq_provider() -> &'static GroqProvider {
    GROQ_PROVIDER.get_or_init(GroqProvider::new)
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
pub struct StepContext {
    pub step: StepDefinition,
    pub focus_id: String,
    pub focus_run_id: String,
    pub user_input: String,
    pub persona_context: String,              // rendered MemoryBroker output (Phase 3)
    pub space_max_permitted_tier: u8,
    pub execution_tier: u8,
    pub abstraction_tier: u8,
    pub raw_abstraction: u8,
    pub floor_consent_preference: Option<String>,  // "modified" | "local" | None
    pub next_execution_tier: Option<u8>,
    pub retry_count: u32,
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
    fn default() -> Self { Self }
}

impl StepExecutor {
    pub fn new() -> Self { Self }

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
    pub async fn execute(
        &self,
        ctx: StepContext,
        personal_track: &PersonalTrack,
        task_track: &mut TaskTrack,
        shared_state: &mut SharedStateTrack,
        failure_handler: &FailureHandler,
        privacy_gateway: &PrivacyGateway<NoopLogger>,
        scheduler: &Arc<ConductorScheduler>,
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
    async fn execute_once(
        &self,
        ctx: &StepContext,
        retry_count: u32,
        personal_track: &PersonalTrack,
        task_track: &mut TaskTrack,
        shared_state: &mut SharedStateTrack,
        failure_handler: &FailureHandler,
        privacy_gateway: &PrivacyGateway<NoopLogger>,
        scheduler: &Arc<ConductorScheduler>,
    ) -> Result<Option<FailureResult>, ConductorError> {
        let execution_tier  = ctx.execution_tier;
        let abstraction_tier = ctx.abstraction_tier;
        let raw_abstraction  = ctx.raw_abstraction;

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

        // -- Step 3 — tier ceiling gate --
        if ctx.step.routing_tier > ctx.space_max_permitted_tier {
            return Ok(Some(failure_handler.handle(
                &ConductorError::TierBoundaryViolation {
                    plain_language: format!(
                        "Step '{}' requires tier {} but this life only permits \
                         tier {}. [Get help]",
                        ctx.step.step_id, ctx.step.routing_tier,
                        ctx.space_max_permitted_tier
                    ),
                },
                Some(&ctx.step.step_id),
                Some(&ctx.focus_id),
                retry_count,
            )));
        }

        // -- Step 4 — Tier 3 boundary handled by lifecycle; executor never reached --

        let model_id = select_model(&ctx.step.task_type, execution_tier);

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
                if execution_tier >= 2 { Some(model_id.clone()) } else { None },
            )
            .await
            .map_err(|e| ConductorError::DisclosureLogWrite {
                plain_language: e.plain_language.clone(),
            })?;

        if g1.blocked {
            // blocked is always false in current migration (not_permitted path deferred).
            // Preserved for correctness when gate blocking is fully wired.
            return Ok(Some(failure_handler.handle(
                &ConductorError::PrivacyGateBlocked {
                    plain_language: "A required personal field cannot be shared in this \
                        context. [Review privacy settings] [Get help]"
                        .to_owned(),
                },
                Some(&ctx.step.step_id),
                Some(&ctx.focus_id),
                retry_count,
            )));
        }

        // -- Field projection — step-scope boundary --
        // Post-Layer 6 fix: Gate1 evaluates full PersonalTrack; projection scopes
        // approved_fields to the step's declared field_requirements.
        let projected_fields: HashMap<String, String> =
            if !ctx.step.field_requirements.is_empty() {
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
                        provider: if execution_tier >= 2 {
                            Some(model_id.clone())
                        } else {
                            None
                        },
                        fields_shared: projected_fields.keys().cloned().collect(),
                        fields_abstracted: IndexMap::new(),
                        fields_withheld: g1.withheld_fields.clone(),
                        override_declined: false,
                        event_type: "floor_consent_auto".to_owned(),
                    })
                    .await;
            } else {
                // Halt and ask user — AwaitFloorConsent exits the retry loop.
                let mut meta = HashMap::new();
                meta.insert(
                    "floor_clamped_fields".to_owned(),
                    serde_json::to_value(&g1.floor_clamped_fields)
                        .unwrap_or_else(|e| {
                            log::warn!("executor: floor_clamped_fields serialize failed: {e}");
                            serde_json::Value::Array(vec![])
                        }),
                );
                meta.insert(
                    "approved_fields".to_owned(),
                    serde_json::to_value(&projected_fields)
                        .unwrap_or_else(|e| {
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
                    plain_language:
                        "Quiet Rabbit modified some of your fields to maintain \
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

        let prompt = if execution_tier >= 2 {
            let disclosure = shared_state.read_disclosure_buffer(&ctx.step.step_id);
            render_prompt_with_disclosure(
                ctx, &output_vars, &previous_output, &disclosure, &voice_profile_str,
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

        let effective_ctx = options_raw
            .get("num_ctx")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or_else(|| get_context_window(&model_id));

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
            model: model_id.clone(),
            prompt,
            task_type: ctx.step.task_type.clone(),
            stream: Some(false),  // always false in Release 1 — resolved by StepExecutor
            options: Some(options),
        };

        // -- Step 10 — acquire slot; generate; release slot (always, matching finally) --
        let inference_acquired = scheduler
            .acquire_inference_slot(&ctx.focus_run_id, PathPriority::Interactive)
            .await;

        if !inference_acquired {
            return Ok(Some(failure_handler.handle(
                &ConductorError::OllamaUnavailable {
                    plain_language: "Quiet Rabbit is busy with another task. \
                        [Try again] [Get help]"
                        .to_owned(),
                },
                Some(&ctx.step.step_id),
                Some(&ctx.focus_id),
                retry_count,
            )));
        }

        let generate_result: Result<_, ConductorError> = if execution_tier >= 2 {
            groq_provider().generate(&request).await
        } else {
            ollama_client().generate(&request).await
        };

        // Python oracle: finally block — always release regardless of generate result.
        scheduler.release_inference_slot(&ctx.focus_run_id);

        let response = match generate_result {
            Ok(r) => r,
            Err(e) => {
                return Ok(Some(failure_handler.handle(
                    &e,
                    Some(&ctx.step.step_id),
                    Some(&ctx.focus_id),
                    retry_count,
                )));
            }
        };

        // -- Step 11 — PG_GATE_2: inbound contamination scan --
        let g2 = privacy_gateway
            .gate2(
                &ctx.step.step_id,
                &ctx.focus_run_id,
                &response.content,
                &gate_track,
                execution_tier,
                if execution_tier >= 2 { Some(model_id.clone()) } else { None },
                Some(&g1.fields_shared),
            )
            .await
            .map_err(|e| ConductorError::DisclosureLogWrite {
                plain_language: e.plain_language.clone(),
            })?;

        if g2.flagged {
            return Ok(Some(failure_handler.handle(
                &ConductorError::InboundContamination {
                    plain_language: "The response may contain personal information. \
                        [Review and continue] [Discard] [Get help]"
                        .to_owned(),
                },
                Some(&ctx.step.step_id),
                Some(&ctx.focus_id),
                retry_count,
            )));
        }

        // -- Step 12 — update TaskTrack --
        // D4-040: content = model output ONLY — never prompt-expanded input.
        // step_sensitivity: from projected fields (Tier 2) or template token scan (Tier 1).
        let step_sensitivity = compute_step_sensitivity(
            ctx,
            personal_track,
            &projected_fields,
            execution_tier,
        );

        let output_var = ctx
            .step
            .output_var
            .clone()
            .or_else(|| Some(ctx.step.step_id.clone()))
            .filter(|v| !v.is_empty());

        task_track.add_step(TaskStep {
            step_id: ctx.step.step_id.clone(),
            output_var,
            content: response.content.clone(),
            sensitivity_severity: step_sensitivity,
            routing_tier_used: execution_tier as i32,
        });

        // -- Step 13 — PG_GATE_3: cross-tier content promotion (conditional) --
        // Only when next step has higher execution_tier.
        if let Some(next_tier) = ctx.next_execution_tier {
            if next_tier > execution_tier {
                let content_key = ctx.step.output_var.as_deref()
                    .unwrap_or(&ctx.step.step_id);

                let g3 = privacy_gateway
                    .gate3(
                        &ctx.step.step_id,
                        &ctx.focus_run_id,
                        content_key,
                        step_sensitivity as u8,
                        next_tier,
                        ctx.space_max_permitted_tier,
                        execution_tier,
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

                if g3.approved {
                    shared_state.promote_content(
                        &ctx.step.step_id,
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

/// Tier 1 prompt render. Never reads disclosure buffer.
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
    render_template(&ctx.step.prompt_template, &tokens, &ctx.step.step_id, &ctx.focus_id)
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
    render_template(&ctx.step.prompt_template, &tokens, &ctx.step.step_id, &ctx.focus_id)
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
/// Tier 1: contaminated attrs stripped and logged; execution continues. Ok(cleaned).
/// Tier 2+: Err(ConductorError::VoiceProfileContamination) on first contamination.
///   Audit event written before returning Err.
///
/// Returns Ok(cleaned_map) — only ALLOWED_VOICE_ATTRIBUTES present.
///
/// Python oracle: StepExecutor._scan_voice_profile()
async fn scan_voice_profile(
    ctx: &StepContext,
    personal_track: &PersonalTrack,
    privacy_gateway: &PrivacyGateway<NoopLogger>,
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
                })
                .await;

            if ctx.execution_tier >= 2 {
                // Tier 2+: halt — contaminated data must not leave the device.
                return Err(ConductorError::VoiceProfileContamination {
                    plain_language:
                        "One of your communication style settings appears to contain \
                         personal information. Quiet Rabbit stopped this request \
                         before sending it outside your device. Review your voice \
                         profile settings and remove personal details before trying again."
                            .to_owned(),
                });
            }
            // Tier 1: strip attr, continue scanning remaining attrs.
        } else {
            cleaned.insert(attr.to_string(), value);
        }
    }

    Ok(cleaned)
}

// ---------------------------------------------------------------------------
// Model selection and options
// ---------------------------------------------------------------------------

/// Select model ID based on task_type and execution tier.
/// Python oracle: StepExecutor._select_model()
fn select_model(task_type: &str, tier: u8) -> String {
    if tier == 1 {
        match task_type {
            "code" => "qwen2.5:7b".to_owned(),
            "quick_response" | "summarization" => "llama3.2:3b".to_owned(),
            _ => "llama3.1:8b".to_owned(),
        }
    } else {
        "groq:llama-3.1-8b-instant".to_owned()
    }
}

/// Context window size for a given model ID.
/// Python oracle: StepExecutor._get_context_window()
fn get_context_window(model_id: &str) -> u32 {
    match model_id {
        "llama3.2:3b"               => 4096,
        "llama3.1:8b"               => 8192,
        "qwen2.5:7b"                => 8192,
        "groq:llama-3.1-8b-instant" => 8192,
        _                           => 2048,
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

/// Derive step sensitivity severity from projected fields (Tier 2) or
/// template token scan (Tier 1).
///
/// Tier 2: max sensitivity_severity across fields actually referenced in
///   projected_fields that are present in personal_track.
/// Tier 1: max sensitivity_severity across personal_track fields whose
///   field_name appears as a {token} in the prompt template.
/// Default: 1 (general) when no personal fields involved.
///
/// Python oracle: Step 12 sensitivity logic in StepExecutor._execute_once()
fn compute_step_sensitivity(
    ctx: &StepContext,
    personal_track: &PersonalTrack,
    projected_fields: &HashMap<String, String>,
    execution_tier: u8,
) -> i32 {
    if execution_tier >= 2 && !projected_fields.is_empty() {
        let severities: Vec<i32> = projected_fields
            .keys()
            .filter_map(|name| personal_track.fields().get(name))
            .map(|pf| pf.sensitivity_severity)
            .collect();
        return severities.into_iter().max().unwrap_or(1);
    }

    if execution_tier == 1 {
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
            "personal"  => Sensitivity::Personal,
            "medical"   => Sensitivity::Medical,
            "financial" => Sensitivity::Financial,
            _           => Sensitivity::General,  // "general" + unknown -> General (fail-safe)
        };
        let _ = gt.add_field(GateField {
            field_name:           field.field_name.clone(),
            field_value:          field.field_value.clone(),
            sensitivity,
            sensitivity_severity: field.sensitivity_severity as u8,
            source_id:            field.source_id.clone(),
            abstraction_tier2:    AbstractionPolicy::from_str(&field.abstraction_tier2),
            abstraction_tier3:    AbstractionPolicy::from_str(&field.abstraction_tier3),
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
        let first_ok = tc.next().map(|c| c == '_' || c.is_ascii_lowercase()).unwrap_or(false);
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
            h[..abs_pos].chars().next_back().map(|c| !c.is_alphanumeric()).unwrap_or(true)
        };
        let end = abs_pos + n_len;
        let after_ok = if end >= h.len() {
            true
        } else {
            h[end..].chars().next().map(|c| !c.is_alphanumeric()).unwrap_or(true)
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
        .filter_map(|attr| {
            voice_profile
                .get(*attr)
                .map(|v| (*attr, v.as_str()))
        })
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
    fn digit_dense_detects_7_digits() { assert!(has_digit_dense("1234567")); }

    #[test]
    fn digit_dense_detects_embedded_digits() { assert!(has_digit_dense("call 1234567 now")); }

    #[test]
    fn digit_dense_6_digits_no_match() { assert!(!has_digit_dense("123456")); }

    #[test]
    fn digit_dense_interrupted_digits() { assert!(!has_digit_dense("123-456")); }

    #[test]
    fn digit_dense_empty() { assert!(!has_digit_dense("")); }

    #[test]
    fn email_detects_basic_address() { assert!(is_email_pattern("user@example.com")); }

    #[test]
    fn email_no_at_sign() { assert!(!is_email_pattern("notanemail")); }

    #[test]
    fn email_no_dot_after_at() { assert!(!is_email_pattern("user@nodot")); }

    #[test]
    fn email_empty_local() { assert!(!is_email_pattern("@example.com")); }

    #[test]
    fn word_boundary_whole_word_match() {
        assert!(has_word_boundary_match_ci("I live in Portland today", "Portland"));
    }

    #[test]
    fn word_boundary_case_insensitive() {
        assert!(has_word_boundary_match_ci("Tone like portland vibes", "PORTLAND"));
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
        assert!(!has_word_boundary_match_ci("tone: conversational", "Portland"));
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

    #[test]
    fn select_model_tier1_code() { assert_eq!(select_model("code", 1), "qwen2.5:7b"); }

    #[test]
    fn select_model_tier1_quick_response() {
        assert_eq!(select_model("quick_response", 1), "llama3.2:3b");
    }

    #[test]
    fn select_model_tier1_summarization() {
        assert_eq!(select_model("summarization", 1), "llama3.2:3b");
    }

    #[test]
    fn select_model_tier1_general() {
        assert_eq!(select_model("general", 1), "llama3.1:8b");
    }

    #[test]
    fn select_model_tier2_any_type() {
        assert_eq!(select_model("code", 2), "groq:llama-3.1-8b-instant");
        assert_eq!(select_model("general", 2), "groq:llama-3.1-8b-instant");
    }

    #[test]
    fn context_window_known_models() {
        assert_eq!(get_context_window("llama3.2:3b"), 4096);
        assert_eq!(get_context_window("llama3.1:8b"), 8192);
        assert_eq!(get_context_window("qwen2.5:7b"), 8192);
        assert_eq!(get_context_window("groq:llama-3.1-8b-instant"), 8192);
    }

    #[test]
    fn context_window_unknown_model_defaults() {
        assert_eq!(get_context_window("unknown:model"), 2048);
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
            },
            TaskStep {
                step_id: "s2".to_owned(),
                output_var: Some(String::new()),
                content: "world".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
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
            },
            TaskStep {
                step_id: "s2".to_owned(),
                output_var: Some("result".to_owned()),
                content: "second".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
            },
        ];
        let vars = collect_output_vars(&steps);
        assert_eq!(vars.get("result").map(|s| s.as_str()), Some("second"));
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
        }).unwrap();
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
        }).unwrap();
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
            field_name: "name".to_owned(), field_value: "Alice".to_owned(),
            sensitivity: "personal".to_owned(), sensitivity_severity: 2,
            source_id: "s".to_owned(), abstraction_tier2: "pass".to_owned(),
            abstraction_tier3: "omit".to_owned(),
        }).unwrap();
        pt.add_field(CF {
            field_name: "diagnosis".to_owned(), field_value: "X".to_owned(),
            sensitivity: "medical".to_owned(), sensitivity_severity: 3,
            source_id: "s".to_owned(), abstraction_tier2: "omit".to_owned(),
            abstraction_tier3: "omit".to_owned(),
        }).unwrap();
        pt.seal();
        let ctx = StepContext {
            step: StepDefinition {
                step_id: "s1".to_owned(), display_name: "Step 1".to_owned(),
                guide_id: "g".to_owned(), task_type: "general".to_owned(),
                routing_tier: 2, step_type: StepType::default(), output_var: None,
                prompt_template: "Hello {name}".to_owned(),
                field_requirements: std::collections::HashMap::new(),
                options_override: std::collections::HashMap::new(),
            },
            focus_id: "f".to_owned(), focus_run_id: "fr".to_owned(),
            user_input: "".to_owned(), persona_context: "".to_owned(),
            space_max_permitted_tier: 2, execution_tier: 2,
            abstraction_tier: 2, raw_abstraction: 1,
            floor_consent_preference: None, next_execution_tier: None, retry_count: 0,
        };
        let mut projected = HashMap::new();
        projected.insert("name".to_owned(), "Alice (abstracted)".to_owned());
        assert_eq!(compute_step_sensitivity(&ctx, &pt, &projected, 2), 2);
    }

    #[test]
    fn step_sensitivity_tier1_uses_template_scan() {
        use crate::conductor::tokens::{StepDefinition, StepType};
        use crate::conductor::types::PersonalField as CF;
        let mut pt = PersonalTrack::new();
        pt.add_field(CF {
            field_name: "name".to_owned(), field_value: "Alice".to_owned(),
            sensitivity: "personal".to_owned(), sensitivity_severity: 2,
            source_id: "s".to_owned(), abstraction_tier2: "pass".to_owned(),
            abstraction_tier3: "omit".to_owned(),
        }).unwrap();
        pt.seal();
        let ctx = StepContext {
            step: StepDefinition {
                step_id: "s1".to_owned(), display_name: "Step 1".to_owned(),
                guide_id: "g".to_owned(), task_type: "general".to_owned(),
                routing_tier: 1, step_type: StepType::default(), output_var: None,
                prompt_template: "Hello {name}".to_owned(),
                field_requirements: std::collections::HashMap::new(),
                options_override: std::collections::HashMap::new(),
            },
            focus_id: "f".to_owned(), focus_run_id: "fr".to_owned(),
            user_input: "".to_owned(), persona_context: "".to_owned(),
            space_max_permitted_tier: 1, execution_tier: 1,
            abstraction_tier: 1, raw_abstraction: 1,
            floor_consent_preference: None, next_execution_tier: None, retry_count: 0,
        };
        assert_eq!(compute_step_sensitivity(&ctx, &pt, &HashMap::new(), 1), 2);
    }

    #[test]
    fn step_sensitivity_no_personal_fields_is_1() {
        use crate::conductor::tokens::{StepDefinition, StepType};
        let pt = PersonalTrack::new();
        let ctx = StepContext {
            step: StepDefinition {
                step_id: "s1".to_owned(), display_name: "Step 1".to_owned(),
                guide_id: "g".to_owned(), task_type: "general".to_owned(),
                routing_tier: 1, step_type: StepType::default(), output_var: None,
                prompt_template: "Hello {user_input}".to_owned(),
                field_requirements: std::collections::HashMap::new(),
                options_override: std::collections::HashMap::new(),
            },
            focus_id: "f".to_owned(), focus_run_id: "fr".to_owned(),
            user_input: "".to_owned(), persona_context: "".to_owned(),
            space_max_permitted_tier: 1, execution_tier: 1,
            abstraction_tier: 1, raw_abstraction: 1,
            floor_consent_preference: None, next_execution_tier: None, retry_count: 0,
        };
        assert_eq!(compute_step_sensitivity(&ctx, &pt, &HashMap::new(), 1), 1);
    }
}
