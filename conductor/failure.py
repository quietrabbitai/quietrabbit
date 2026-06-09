# conductor/failure.py
# FailureHandler — maps F1-F10 failure modes to handling logic.
# Called by StepExecutor and FocusRun lifecycle when errors are raised.
# Every failure produces a plain_language message for the UI and a
# structured action for the Conductor to take.
#
# RETRY CONTRACT:
# retry_count is passed in by the caller (StepExecutor tracks per-step).
# MAX_RETRIES = 3. After max retries: escalate to offer_tier2 or await_user.
# This handler is stateless — caller tracks retry state.
#
# IMMUTABILITY:
# Exception objects are never mutated. step_id and focus_id are passed
# through to FailureResult — never injected back into the exception.
#
# Layer 6 (ADR-012):
# - await_floor_consent added to action Literal (control-flow pause, not error)
# - failure_mode changed to str | None (None for non-failure pauses)
# - severity gains 'pause' value (distinct from 'stop' or 'require')
# - metadata: dict | None added to FailureResult for structured payloads
# - DisclosureLogWriteError mapped to F_SYSTEM (fatal audit failure)
# - UnknownProviderError mapped to F_SYSTEM (misconfiguration, not transient)
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   path_id → focus_id on FailureResult and FailureHandler.handle()

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Literal

from providers.errors import (
    QRAPIError,
    OllamaUnavailableError,
    OllamaTimeoutError,
    OllamaGenerationError,
    OllamaInvalidRequestError,
    QualityBelowFloorError,
    ContextWindowExceededError,
    PrivacyGateBlockedError,
    ContentPromotionBlockedError,
    DisclosureLogWriteError,
    SecurityCheckerFlagError,
    InboundContaminationError,
    PersonalDBNotFoundError,
    PersonalDBDecryptionError,
    SnapshotWriteError,
    LoopDetectedError,
    MissingAPIKeyError,
    InvalidAPIKeyError,
    ProviderRateLimitError,
    ProviderTimeoutError,
    ProviderUnavailableError,
    ProviderError,
    UnknownProviderError,
    TaxonomyIntegrityError,
    DatabaseMigrationError,
)


MAX_RETRIES = 3


@dataclass
class FailureResult:
    """
    The structured outcome of a failure handler decision.

    action: what the Conductor should do next.
      'retry'               — retry the current step (same tier)
      'offer_tier2'         — surface offer to use external service
      'offer_compact'       — surface offer to compact context
      'await_user'          — pause run, surface decision to user
      'stop'                — terminate run, no retry
      'degrade'             — continue with reduced capability (F8 only)
      'hold_for_gate'       — hold response, invoke PG_GATE_2 classification
      'await_floor_consent' — Floor Consent Gate pause (ADR-012 Amendment 3)

    failure_mode: F1-F10, F_SYSTEM, F_UNEXPECTED. None for pauses.
    metadata: structured payload (floor consent data, Gate3 review, etc.)
    """
    action: Literal[
        'retry', 'offer_tier2', 'offer_compact',
        'await_user', 'stop', 'degrade', 'hold_for_gate',
        'await_floor_consent',
    ]
    failure_mode: str | None
    plain_language: str
    is_recoverable: bool = True
    severity: Literal['info', 'suggest', 'require', 'stop', 'pause'] = 'require'
    step_id: str | None = None
    focus_id: str | None = None
    metadata: dict[str, Any] | None = None


class FailureHandler:
    """
    Maps QRAPIError subclasses to FailureResult decisions.
    Stateless — caller (StepExecutor) tracks retry_count per step.
    """

    def __init__(self, space_max_permitted_tier: int = 1):
        self.space_max_permitted_tier = space_max_permitted_tier

    def handle(
        self,
        error: Exception,
        step_id: str | None = None,
        focus_id: str | None = None,
        retry_count: int = 0,
    ) -> FailureResult:
        msg = getattr(error, "plain_language", str(error))

        # F_SYSTEM — integrity failures, fatal audit failures, misconfiguration
        if isinstance(error, (
            TaxonomyIntegrityError,
            DatabaseMigrationError,
            DisclosureLogWriteError,
            UnknownProviderError,
        )):
            return FailureResult(
                action="stop",
                failure_mode="F_SYSTEM",
                plain_language=msg,
                is_recoverable=False,
                severity="stop",
                step_id=step_id,
                focus_id=focus_id,
            )

        # F1 — Ollama unavailable
        if isinstance(error, OllamaUnavailableError):
            return self._handle_f1_unavailable(msg, step_id, focus_id)

        if isinstance(error, (OllamaTimeoutError, OllamaGenerationError)):
            if retry_count >= MAX_RETRIES:
                return self._escalate_failed_retry("F1", step_id, focus_id)
            return FailureResult(
                action="retry",
                failure_mode="F1",
                plain_language=msg,
                severity="require",
                step_id=step_id,
                focus_id=focus_id,
            )

        if isinstance(error, OllamaInvalidRequestError):
            return FailureResult(
                action="stop",
                failure_mode="F1",
                plain_language=msg,
                is_recoverable=False,
                severity="stop",
                step_id=step_id,
                focus_id=focus_id,
            )

        # F2 — Quality below floor
        if isinstance(error, QualityBelowFloorError):
            return self._handle_f2(msg, step_id, focus_id, retry_count)

        # F3 — Context window
        if isinstance(error, ContextWindowExceededError):
            return FailureResult(
                action="offer_compact",
                failure_mode="F3",
                plain_language=msg,
                severity="require",
                step_id=step_id,
                focus_id=focus_id,
            )

        # F4 — Privacy Guardian hard block
        if isinstance(error, (PrivacyGateBlockedError, ContentPromotionBlockedError)):
            return FailureResult(
                action="await_user",
                failure_mode="F4",
                plain_language=msg,
                severity="stop",
                step_id=step_id,
                focus_id=focus_id,
            )

        # F5 — Security Checker flag
        if isinstance(error, SecurityCheckerFlagError):
            return FailureResult(
                action="stop",
                failure_mode="F5",
                plain_language=msg,
                is_recoverable=False,
                severity="stop",
                step_id=step_id,
                focus_id=focus_id,
            )

        # F6 — Inbound contamination
        if isinstance(error, InboundContaminationError):
            return FailureResult(
                action="hold_for_gate",
                failure_mode="F6",
                plain_language=msg,
                severity="require",
                step_id=step_id,
                focus_id=focus_id,
            )

        # F7 — personal.db unavailable
        if isinstance(error, (PersonalDBNotFoundError, PersonalDBDecryptionError)):
            return FailureResult(
                action="stop",
                failure_mode="F7",
                plain_language=msg,
                is_recoverable=False,
                severity="stop",
                step_id=step_id,
                focus_id=focus_id,
            )

        # F8 — Snapshot write failure
        if isinstance(error, SnapshotWriteError):
            return FailureResult(
                action="degrade",
                failure_mode="F8",
                plain_language=(
                    "Quiet Rabbit couldn't save your progress checkpoint. "
                    "Your work will continue but can't be resumed if interrupted. "
                    "[Continue] [Stop and save manually]"
                ),
                severity="suggest",
                step_id=step_id,
                focus_id=focus_id,
            )

        # F9 — Loop detection
        if isinstance(error, LoopDetectedError):
            return FailureResult(
                action="stop",
                failure_mode="F9",
                plain_language=(
                    "Quiet Rabbit detected a loop and stopped to protect "
                    "your session. Your work is saved. [Get help]"
                ),
                is_recoverable=False,
                severity="stop",
                step_id=step_id,
                focus_id=focus_id,
            )

        # F10 — Tier 2/3 provider errors
        if isinstance(error, (MissingAPIKeyError, InvalidAPIKeyError)):
            return FailureResult(
                action="await_user",
                failure_mode="F10",
                plain_language=msg,
                severity="require",
                step_id=step_id,
                focus_id=focus_id,
            )

        if isinstance(error, ProviderRateLimitError):
            if retry_count >= MAX_RETRIES:
                return self._escalate_failed_retry("F10", step_id, focus_id)
            return FailureResult(
                action="retry",
                failure_mode="F10",
                plain_language=msg,
                severity="suggest",
                step_id=step_id,
                focus_id=focus_id,
            )

        if isinstance(error, (ProviderTimeoutError, ProviderUnavailableError)):
            if retry_count >= MAX_RETRIES:
                return self._escalate_failed_retry("F10", step_id, focus_id)
            return FailureResult(
                action="retry",
                failure_mode="F10",
                plain_language=msg,
                severity="require",
                step_id=step_id,
                focus_id=focus_id,
            )

        if isinstance(error, ProviderError):
            return FailureResult(
                action="await_user",
                failure_mode="F10",
                plain_language=msg,
                severity="require",
                step_id=step_id,
                focus_id=focus_id,
            )

        # Unexpected
        return FailureResult(
            action="stop",
            failure_mode="F_UNEXPECTED",
            plain_language=(
                "Something unexpected happened. Your work is saved. "
                "[Try again] [Get help]"
            ),
            is_recoverable=False,
            severity="stop",
            step_id=step_id,
            focus_id=focus_id,
        )

    # -- Private helpers ------------------------------------------------------

    def _handle_f1_unavailable(self, msg, step_id, focus_id):
        if self.space_max_permitted_tier >= 2:
            return FailureResult(
                action="offer_tier2", failure_mode="F1",
                plain_language=msg, severity="require",
                step_id=step_id, focus_id=focus_id,
            )
        return FailureResult(
            action="stop", failure_mode="F1",
            plain_language=(
                "The local AI isn't responding, and this life doesn't "
                "allow external services. [Try again] [Get help]"
            ),
            is_recoverable=False, severity="stop",
            step_id=step_id, focus_id=focus_id,
        )

    def _handle_f2(self, msg, step_id, focus_id, retry_count):
        exhausted = retry_count >= MAX_RETRIES
        if self.space_max_permitted_tier >= 2:
            return FailureResult(
                action="offer_tier2", failure_mode="F2",
                plain_language=(
                    "The result quality fell below standard repeatedly. "
                    "Try using an external service? "
                    "[Use external service] [Keep result] [Get help]"
                ) if exhausted else (
                    "The result wasn't quite right. "
                    "Want to try with an external service? "
                    "[Use external service] [Keep this result] [Try again]"
                ),
                severity="require" if exhausted else "suggest",
                step_id=step_id, focus_id=focus_id,
            )
        return FailureResult(
            action="await_user" if exhausted else "retry",
            failure_mode="F2",
            plain_language=(
                "The local model output quality fell below standard repeatedly. "
                "[Review output] [Try again]"
            ) if exhausted else (
                "The result wasn't quite right. Trying again. [Keep this result]"
            ),
            severity="require" if exhausted else "suggest",
            step_id=step_id, focus_id=focus_id,
        )

    def _escalate_failed_retry(self, mode, step_id, focus_id):
        if self.space_max_permitted_tier >= 2:
            return FailureResult(
                action="offer_tier2", failure_mode=mode,
                plain_language=(
                    "This step has failed repeatedly. "
                    "Switch to an external service? [Use external service] [Stop]"
                ),
                severity="require", step_id=step_id, focus_id=focus_id,
            )
        return FailureResult(
            action="await_user", failure_mode=mode,
            plain_language=(
                "This step failed repeatedly and has been paused. [Try again] [Get help]"
            ),
            severity="stop", step_id=step_id, focus_id=focus_id,
        )
