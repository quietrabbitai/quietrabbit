# conductor/executor.py
# StepExecutor — full 15-step execution sequence per Architecture Section 6.3.
# TaskStep.content = model output ONLY — never prompt-expanded input. (D4-040)
#
# Layer 6 (ADR-012 Amendments 2 + 3) + post-integration review fixes:
# - resolved_tier REMOVED (replaced by execution_tier + abstraction_tier)
# - effective_tier REMOVED (lifecycle owns tier computation)
# - privacy_default_tier REMOVED from StepContext
# - path_max_routing_tier REMOVED from StepContext
# - path_display_name NOT on StepContext
# - execution_tier: model selection, Step 8 render, Step 10/11/12/13
# - abstraction_tier: Gate1 field policy only
# - raw_abstraction: pre-floor preference, gate1 floor detection ONLY
# - next_execution_tier: Gate3 look-ahead
# - floor_consent_preference: if "modified", skip Floor Consent Gate +
#   write floor_consent_auto disclosure log event. One-run scope.
# - Step 12 fix: Tier 1 step_sensitivity derived from fields actually
#   referenced in the prompt template (regex scan), not PersonalTrack ceiling.
# - DisclosureLogWriteError caught ONCE in execute() outer loop (F_SYSTEM).
# - await_floor_consent exits retry loop immediately (never retried).
#
# Post-Layer 6 integration fix (field projection layer):
# - Gate1 evaluates full PersonalTrack (privacy guarantee preserved).
# - After Gate1, approved_fields projected to step.field_requirements.
# - step_sensitivity for Tier 2 derived from projected_fields.
#
# Post-Layer 6 integration fix (Gate2 disclosure-aware scan):
# - gate2() call passes fields_shared=g1.fields_shared.
#
# Post-Layer 6 integration fix (prompt renderer unification — D5-157):
# - _render_template() is the single token resolution engine.
# - Token contract: only snake_case tokens valid ({[a-z_][a-z0-9_]*}).
# - Merge order invariant: output_vars → (disclosure) → SYSTEM_TOKENS.
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   StepContext: path_id → focus_id, path_run_id → focus_run_id,
#     space_max_permitted_tier retained (still the correct semantic)
#   All gate calls: path_run_id → focus_run_id
#   Prompt tokens: path_context → focus_context, space_context → life_context → persona_context (D6-323)
#   guide_id replaces specialist_id in StepDefinition (via tokens.py)

from __future__ import annotations

import logging
import re
from dataclasses import dataclass, replace

from conductor.context import (
    PersonalTrack, TaskTrack, SharedStateTrack, TaskStep,
)
from conductor.concurrency import ConductorScheduler, PathPriority
from conductor.failure import FailureHandler, FailureResult
from conductor.privacy import PrivacyGateway
from conductor.tokens import StepDefinition
from providers.errors import (
    OllamaUnavailableError,
    ContextWindowExceededError, TierBoundaryViolationError,
    InboundContaminationError, ContentPromotionBlockedError,
    DisclosureLogWriteError, VoiceProfileContaminationError,
)
from providers.groq import GroqProvider
from providers.ollama_client import generate, check_context_window
from providers.types import GenerateRequest, GenerateOptions

log = logging.getLogger(__name__)

MAX_RETRIES = 3

# Module-level Groq provider instance.
# GroqProvider is stateless — safe to share across steps and runs.
_groq_provider = GroqProvider()

# Token pattern for prompt template scanning — matches {snake_case_name}.
_TOKEN_PATTERN = re.compile(r"\{([a-z_][a-z0-9_]*)\}")

# PII detection patterns for voice profile value scanning.
# These are policy-level constants — update here when detection rules change.
_VP_EMAIL_RE = re.compile(r"\S+@\S+\.\S+")
_VP_DIGIT_RE = re.compile(r"\d{7,}")
_VP_MIN_FIELD_LENGTH = 8  # minimum personal field value length for word-boundary match


# -- Voice profile allowlist --------------------------------------------------

ALLOWED_VOICE_ATTRIBUTES: frozenset[str] = frozenset({
    "tone",
    "formality",
    "directness",
    "length_preference",
    "pacing",
})


def _format_voice_profile(voice_profile: dict[str, str]) -> str:
    """
    Assemble voice profile dict into a prompt-injectable string.
    Only ALLOWED_VOICE_ATTRIBUTES are injected — unknown attrs excluded + logged.
    Sorted output for prompt reproducibility.
    """
    if not voice_profile:
        return ""

    approved: dict[str, str] = {}
    unknown: list[str] = []
    for k, v in voice_profile.items():
        if k in ALLOWED_VOICE_ATTRIBUTES:
            approved[k] = v
        else:
            unknown.append(k)

    if unknown:
        log.warning(
            "Voice profile attributes excluded (not in ALLOWED_VOICE_ATTRIBUTES): %s",
            unknown,
        )

    if not approved:
        return ""
    return ", ".join(f"{k}={approved[k]}" for k in sorted(approved))


@dataclass
class StepContext:
    """
    Fully-resolved execution state for a single step.
    All tier values are pre-computed by lifecycle._execute_step().
    Executor is a pure consumer — no tier computation happens here.

    floor_consent_preference: read from life extra_metadata by lifecycle.
      "modified" → skip Floor Consent Gate this run, write floor_consent_auto.
      None       → normal gate evaluation.
      One-run scope: applies for this StepContext only; not persisted here.
    """
    step: StepDefinition
    focus_id: str
    focus_run_id: str
    user_input: str
    personal_track: PersonalTrack
    task_track: TaskTrack
    shared_state: SharedStateTrack
    failure_handler: FailureHandler
    privacy_gateway: PrivacyGateway
    scheduler: ConductorScheduler
    space_max_permitted_tier: int
    execution_tier: int
    abstraction_tier: int
    raw_abstraction: int
    floor_consent_preference: str | None = None  # "modified" | "local" | None
    next_execution_tier: int | None = None
    retry_count: int = 0
    persona_context: str = ""  # rendered Memory Broker output — injected at Phase 3


class StepExecutor:
    def execute(self, ctx: StepContext) -> FailureResult | None:
        """
        Execute through the full 15-step sequence.
        DisclosureLogWriteError caught once here — F_SYSTEM halt.
        await_floor_consent exits immediately — never retried.
        """
        current_ctx = ctx

        while True:
            try:
                result = self._execute_once(current_ctx)
            except (DisclosureLogWriteError, VoiceProfileContaminationError) as e:
                return current_ctx.failure_handler.handle(
                    e,
                    step_id=current_ctx.step.step_id,
                    focus_id=current_ctx.focus_id,
                    retry_count=current_ctx.retry_count,
                )

            if result is None:
                return None

            if result.action == "await_floor_consent":
                return result

            if (
                result.action == "retry"
                and current_ctx.retry_count < MAX_RETRIES
            ):
                current_ctx = replace(
                    current_ctx,
                    retry_count=current_ctx.retry_count + 1,
                )
                continue

            return result

    def _execute_once(self, ctx: StepContext) -> FailureResult | None:

        # Floor invariant assertions (ADR-012 Amendment 3).
        if ctx.execution_tier > 1:
            assert ctx.abstraction_tier >= 2, (
                f"Floor invariant violated: execution_tier={ctx.execution_tier} "
                f"requires abstraction_tier >= 2, got {ctx.abstraction_tier}."
            )
            assert ctx.raw_abstraction <= ctx.abstraction_tier, (
                f"Floor invariant: raw_abstraction ({ctx.raw_abstraction}) "
                f"> abstraction_tier ({ctx.abstraction_tier})."
            )
        else:
            assert ctx.abstraction_tier == ctx.raw_abstraction, (
                f"Tier 1 invariant: abstraction_tier ({ctx.abstraction_tier}) "
                f"!= raw_abstraction ({ctx.raw_abstraction}) at execution_tier=1."
            )

        execution_tier = ctx.execution_tier
        abstraction_tier = ctx.abstraction_tier
        raw_abstraction = ctx.raw_abstraction

        # Step 3 — tier gate
        if ctx.step.routing_tier > ctx.space_max_permitted_tier:
            return ctx.failure_handler.handle(
                TierBoundaryViolationError(
                    requested_tier=ctx.step.routing_tier,
                    permitted_tier=ctx.space_max_permitted_tier,
                    plain_language=(
                        f"Step '{ctx.step.step_id}' requires tier "
                        f"{ctx.step.routing_tier} but this life only "
                        f"permits tier {ctx.space_max_permitted_tier}. [Get help]"
                    ),
                ),
                step_id=ctx.step.step_id,
                focus_id=ctx.focus_id,
                retry_count=ctx.retry_count,
            )

        # Step 4 — Tier 3 boundary handled in lifecycle.py

        model_id = self._select_model(ctx.step.task_type, execution_tier)

        # Steps 6-7 — PG_GATE_1
        g1 = ctx.privacy_gateway.gate1(
            step_id=ctx.step.step_id,
            focus_run_id=ctx.focus_run_id,
            personal_track=ctx.personal_track,
            abstraction_tier=abstraction_tier,
            raw_abstraction=raw_abstraction,
            execution_tier=execution_tier,
            provider=model_id if execution_tier >= 2 else None,
        )

        if g1.blocked:
            return ctx.failure_handler.handle(
                g1.block_error,
                step_id=ctx.step.step_id,
                focus_id=ctx.focus_id,
                retry_count=ctx.retry_count,
            )

        # Field projection — step-scope boundary.
        if ctx.step.field_requirements:
            projected_fields = {
                name: value
                for name, value in g1.approved_fields.items()
                if name in ctx.step.field_requirements
            }
        else:
            projected_fields = {}

        # Floor Consent Gate (ADR-012 Amendment 3).
        if g1.floor_clamped_fields:
            if ctx.floor_consent_preference == "modified":
                try:
                    ctx.privacy_gateway._write_disclosure_log(
                        step_id=ctx.step.step_id,
                        focus_run_id=ctx.focus_run_id,
                        execution_tier=execution_tier,
                        abstraction_tier=abstraction_tier,
                        provider=model_id if execution_tier >= 2 else None,
                        fields_shared=list(projected_fields.keys()),
                        fields_abstracted={},
                        fields_withheld=g1.withheld_fields,
                        override_declined=False,
                        event_type="floor_consent_auto",
                    )
                except Exception:
                    pass  # audit log of consent record; non-fatal
            else:
                return FailureResult(
                    action="await_floor_consent",
                    failure_mode=None,
                    plain_language=(
                        "Quiet Rabbit modified some of your fields to maintain "
                        "privacy for external use. Please review and choose."
                    ),
                    is_recoverable=True,
                    severity="pause",
                    metadata={
                        "floor_clamped_fields": g1.floor_clamped_fields,
                        "approved_fields": projected_fields,
                        "step_id": ctx.step.step_id,
                        "execution_tier": execution_tier,
                        "abstraction_tier": abstraction_tier,
                    },
                )

        # Write projected fields to disclosure buffer.
        ctx.shared_state.write_disclosure_buffer(
            ctx.step.step_id, projected_fields
        )

        # Step 8 — assemble final prompt.
        if execution_tier >= 2:
            disclosure = ctx.shared_state.read_disclosure_buffer(ctx.step.step_id)
            prompt = self._render_prompt_with_disclosure(ctx, disclosure)
        else:
            prompt = self._render_prompt(ctx)

        # Step 5 (after Step 8) — context window check.
        options_raw = {
            "temperature": 0.5,
            "top_p": 0.90,
            "num_predict": 1024,
            **dict(ctx.step.options_override),
        }
        effective_ctx = int(
            options_raw.get("num_ctx", self._get_context_window(model_id))
        )
        ctx_status = check_context_window(
            model_id=model_id,
            prompt=prompt,
            task_type=ctx.step.task_type,
            context_window=effective_ctx,
        )
        if ctx_status.status == "exceeded":
            return ctx.failure_handler.handle(
                ContextWindowExceededError(
                    plain_language=(
                        ctx_status.plain_language
                        or "This is too long for local processing. [Shorten the document]"
                    )
                ),
                step_id=ctx.step.step_id,
                focus_id=ctx.focus_id,
                retry_count=ctx.retry_count,
            )

        options = self._build_options(options_raw, effective_ctx)

        # Step 10 — acquire slot, execute, release in finally.
        inference_acquired = ctx.scheduler.acquire_inference_slot(
            ctx.focus_run_id, PathPriority.INTERACTIVE
        )
        if not inference_acquired:
            return ctx.failure_handler.handle(
                OllamaUnavailableError(
                    plain_language=(
                        "Quiet Rabbit is busy with another task. [Try again] [Get help]"
                    )
                ),
                step_id=ctx.step.step_id,
                focus_id=ctx.focus_id,
                retry_count=ctx.retry_count,
            )

        generation_error: Exception | None = None
        response = None
        try:
            request = GenerateRequest(
                model=model_id,
                prompt=prompt,
                task_type=ctx.step.task_type,
                stream=False,
                options=options,
            )
            if execution_tier >= 2:
                response = _groq_provider.generate(request)
            else:
                response = generate(request)
        except Exception as e:
            generation_error = e
        finally:
            ctx.scheduler.release_inference_slot(ctx.focus_run_id)

        if generation_error is not None:
            return ctx.failure_handler.handle(
                generation_error,
                step_id=ctx.step.step_id,
                focus_id=ctx.focus_id,
                retry_count=ctx.retry_count,
            )

        assert response is not None

        # Step 11 — PG_GATE_2
        g2 = ctx.privacy_gateway.gate2(
            step_id=ctx.step.step_id,
            focus_run_id=ctx.focus_run_id,
            response_content=response.content,
            personal_track=ctx.personal_track,
            execution_tier=execution_tier,
            provider=model_id if execution_tier >= 2 else None,
            fields_shared=g1.fields_shared,
        )

        if g2.flagged:
            return ctx.failure_handler.handle(
                InboundContaminationError(
                    plain_language=(
                        "The response may contain personal information. "
                        "[Review and continue] [Discard] [Get help]"
                    )
                ),
                step_id=ctx.step.step_id,
                focus_id=ctx.focus_id,
                retry_count=ctx.retry_count,
            )

        # Step 12 — update TaskTrack.
        # D4-040: content = model output ONLY.
        if execution_tier >= 2 and projected_fields:
            projected_severities = [
                ctx.personal_track.fields[name].sensitivity_severity
                for name in projected_fields
                if name in ctx.personal_track.fields
            ]
            step_sensitivity = max(projected_severities) if projected_severities else 1
        elif execution_tier == 1:
            template = ctx.step.prompt_template
            used_fields = {
                m.group(1)
                for m in _TOKEN_PATTERN.finditer(template)
                if m.group(1) in ctx.personal_track.fields
            }
            if used_fields:
                step_sensitivity = max(
                    ctx.personal_track.fields[name].sensitivity_severity
                    for name in used_fields
                )
            else:
                step_sensitivity = 1
        else:
            step_sensitivity = 1

        ctx.task_track.add_step(TaskStep(
            step_id=ctx.step.step_id,
            output_var=ctx.step.output_var or ctx.step.step_id,
            content=response.content,
            sensitivity_severity=step_sensitivity,
            routing_tier_used=execution_tier,
        ))

        # Step 13 — PG_GATE_3
        if (
            ctx.next_execution_tier is not None
            and ctx.next_execution_tier > execution_tier
        ):
            g3 = ctx.privacy_gateway.gate3(
                step_id=ctx.step.step_id,
                focus_run_id=ctx.focus_run_id,
                content_key=ctx.step.output_var or ctx.step.step_id,
                content=response.content,
                content_sensitivity_severity=step_sensitivity,
                target_tier=ctx.next_execution_tier,
                space_max_permitted_tier=ctx.space_max_permitted_tier,
                execution_tier=execution_tier,
            )

            if g3.blocked:
                return ctx.failure_handler.handle(
                    ContentPromotionBlockedError(
                        plain_language=(
                            g3.plain_language or
                            "This content can't be shared with a higher-tier service. "
                            "[Use local only]"
                        )
                    ),
                    step_id=ctx.step.step_id,
                    focus_id=ctx.focus_id,
                    retry_count=ctx.retry_count,
                )
            if g3.approved:
                ctx.shared_state.promote_content(
                    step_id=ctx.step.step_id,
                    content_key=ctx.step.output_var or ctx.step.step_id,
                    content=response.content,
                )

        return None  # success

    # -- Prompt rendering -----------------------------------------------------

    def _render_template(
        self, template: str, tokens: dict[str, str], ctx: StepContext
    ) -> str:
        """
        Single token resolution engine for all prompt rendering.
        Merge order invariant: output_vars → (disclosure) → SYSTEM_TOKENS.
        Unresolved tokens logged at DEBUG — expected at Tier 1 for optional fields.
        """
        for token, value in tokens.items():
            template = template.replace(f"{{{token}}}", value)
        unresolved = _TOKEN_PATTERN.findall(template)
        if unresolved:
            log.debug(
                "Unresolved tokens stripped: %s step=%s focus=%s",
                unresolved, ctx.step.step_id, ctx.focus_id,
            )
            template = _TOKEN_PATTERN.sub("", template)
        return template.strip()

    def _render_prompt(self, ctx: StepContext) -> str:
        """
        Tier 1 prompt render.
        Token merge order (later wins): output_vars → SYSTEM_TOKENS.
        NEVER reads disclosure buffer.
        """
        tokens: dict[str, str] = {
            **{k: str(v) for k, v in ctx.task_track.output_vars.items()},
            "user_input": ctx.user_input,
            "previous_output": ctx.task_track.last_output() or "",
            "focus_context": ctx.focus_id,
            "persona_context": ctx.persona_context,
            "voice_profile": _format_voice_profile(self._scan_voice_profile(ctx)),
        }
        return self._render_template(ctx.step.prompt_template, tokens, ctx)

    def _render_prompt_with_disclosure(
        self, ctx: StepContext, disclosure: dict[str, str]
    ) -> str:
        """
        Tier 2+ prompt render. NEVER reads PersonalTrack directly.
        Token merge order (later wins): output_vars → disclosure → SYSTEM_TOKENS.
        """
        tokens: dict[str, str] = {
            **{k: str(v) for k, v in ctx.task_track.output_vars.items()},
            **{k: str(v) for k, v in disclosure.items()},
            "user_input": ctx.user_input,
            "previous_output": ctx.task_track.last_output() or "",
            "focus_context": ctx.focus_id,
            "persona_context": ctx.persona_context,
            "voice_profile": _format_voice_profile(self._scan_voice_profile(ctx)),
        }
        return self._render_template(ctx.step.prompt_template, tokens, ctx)

    def _scan_voice_profile(self, ctx: StepContext) -> dict[str, str]:
        """
        Scan voice profile values for likely personal information before
        prompt assembly. Called by both render methods before
        _format_voice_profile().

        Detection signals (three rules):
          1. personal_field_match — value contains a PersonalTrack field value
             as a whole word (word-boundary match, case-insensitive).
             Only field values of _VP_MIN_FIELD_LENGTH+ characters are tested
             to avoid false positives from short common words.
          2. email_pattern — value matches _VP_EMAIL_RE.
          3. digit_dense — value contains 7+ consecutive digits (_VP_DIGIT_RE).

        Tier 1: contaminated attributes stripped, execution continues.
          Audit event written via record_voice_profile_contamination().
        Tier 2+: VoiceProfileContaminationError raised on first match.
          Audit event written before raise.

        This is secondary containment. Write-time validation in
        personal_store.py is the required primary prevention (D6-326).

        Returns a cleaned dict with contaminated attributes removed.
        At Tier 2+, raises before returning.
        """
        personal_values: list[str] = [
            pf.field_value
            for pf in ctx.personal_track.fields.values()
            if len(pf.field_value) >= _VP_MIN_FIELD_LENGTH
        ]

        cleaned: dict[str, str] = {}

        for attr, value in ctx.personal_track.voice_profile.items():
            if attr not in ALLOWED_VOICE_ATTRIBUTES:
                continue  # unknown attrs excluded by _format_voice_profile

            contamination_type: str | None = None

            for pv in personal_values:
                if re.search(
                    r"\b" + re.escape(pv) + r"\b", value, re.IGNORECASE
                ):
                    contamination_type = "personal_field_match"
                    break

            if contamination_type is None:
                if _VP_EMAIL_RE.search(value):
                    contamination_type = "email_pattern"
                elif _VP_DIGIT_RE.search(value):
                    contamination_type = "digit_dense"

            if contamination_type is not None:
                log.warning(
                    "Voice profile contamination detected: attr=%s type=%s "
                    "tier=%d focus=%s step=%s",
                    attr, contamination_type,
                    ctx.execution_tier, ctx.focus_id, ctx.step.step_id,
                )
                ctx.privacy_gateway.record_voice_profile_contamination(
                    step_id=ctx.step.step_id,
                    focus_run_id=ctx.focus_run_id,
                    execution_tier=ctx.execution_tier,
                    abstraction_tier=ctx.abstraction_tier,
                    attribute_name=attr,
                )
                if ctx.execution_tier >= 2:
                    raise VoiceProfileContaminationError(
                        attribute_name=attr,
                        contamination_type=contamination_type,
                        execution_tier=ctx.execution_tier,
                        plain_language=(
                            "One of your communication style settings appears to "
                            "contain personal information. Quiet Rabbit stopped "
                            "this request before sending it outside your device. "
                            "Review your voice profile settings and remove personal "
                            "details before trying again."
                        ),
                    )
                # Tier 1: strip and continue
            else:
                cleaned[attr] = value

        return cleaned

    # -- Model selection and options ------------------------------------------

    def _select_model(self, task_type: str, tier: int) -> str:
        if tier == 1:
            if task_type == "code":
                return "qwen2.5:7b"
            if task_type in ("quick_response", "summarization"):
                return "llama3.2:3b"
            return "llama3.1:8b"
        return "groq:llama-3.1-8b-instant"

    def _get_context_window(self, model_id: str) -> int:
        return {
            "llama3.2:3b": 4096,
            "llama3.1:8b": 8192,
            "qwen2.5:7b": 8192,
            "groq:llama-3.1-8b-instant": 8192,
        }.get(model_id, 2048)

    def _build_options(self, options_raw: dict, effective_ctx: int) -> GenerateOptions:
        return GenerateOptions(
            temperature=float(options_raw.get("temperature", 0.5)),
            top_p=float(options_raw.get("top_p", 0.90)),
            num_ctx=effective_ctx,
            num_predict=int(options_raw.get("num_predict", 1024)),
        )
