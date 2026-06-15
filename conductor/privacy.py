# conductor/privacy.py
# PrivacyGateway — rules-based Privacy Guardian.
# Four gates invoked during the 15-step execution sequence.
#
# Layer 6 (ADR-012 Amendments 2 + 3):
# - gate1() signature: preferred_tier → abstraction_tier (pure policy axis)
#   raw_abstraction added — for floor impact detection only, never policy
#   execution_tier added — AUDIT ONLY, never used for policy decisions
# - Gate1Result gains floor_clamped_fields: fields whose outcome changed
#   because of floor clamping (raw_abstraction vs abstraction_tier)
# - Floor detection: compares _apply_abstraction(field, raw_abstraction) vs
#   approved value — fields that differ are floor-clamped
# - _write_disclosure_log(): execution_tier + abstraction_tier as separate
#   audit columns; FATAL (DisclosureLogWriteError) when execution_tier > 1
# - gate2/3/4(): routing_tier → execution_tier parameter rename (ADR-012)
# - routing_tier column populated with execution_tier for post-ADR-012 rows
#
# Post-Layer 6 integration fix (gate1 not_permitted behaviour):
# - not_permitted fields are WITHHELD (added to withheld_fields), never a
#   step-level block. Gate1 evaluates full PersonalTrack (privacy guarantee).
#   Executor projects approved_fields to step.field_requirements before
#   writing the disclosure buffer — that is the step-scope boundary.
#
# Post-Layer 6 integration fix (gate2 disclosure-aware scan):
# - Gate1Result gains fields_shared: list of field names whose raw values
#   were intentionally sent to the provider (policy=pass, value unchanged).
# - gate2() accepts fields_shared from Gate1Result via executor.
#   Scan set = all PersonalTrack fields - fields_shared.
#
# INVARIANTS:
# - Raw personal field values NEVER written to step_disclosure_buffer.
# - disclosure_log NEVER deleted — permanent audit trail.
# - Write failures: non-fatal at Tier 1 (process counter + deque).
#   FATAL at Tier 2+ (DisclosureLogWriteError).
# - abstraction_tier >= 2 guaranteed for external calls by lifecycle floor.
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   space_id → life_id on PrivacyGateway
#   path_run_id → focus_run_id in disclosure_log INSERT
#   space_id column → life_id column in disclosure_log INSERT
#   open_personal_db signature updated (life_id parameter)
#
# Updated as part of Phase C Persona model migration (D6-298):
#   life_id → persona_id on PrivacyGateway.__init__
#   self.life_id → self.persona_id
#   life_id column → persona_id column in disclosure_log INSERT
#   open_personal_db: life_id → persona_id parameter

from __future__ import annotations

import json
import logging
import threading
import uuid
from collections import deque
from dataclasses import dataclass, field

from conductor.context import PersonalField, PersonalTrack, SharedStateTrack
from providers.errors import (
    PrivacyGateBlockedError,
    ContentPromotionBlockedError,
    DisclosureLogWriteError,
)
from providers.utils import now, open_personal_db

log = logging.getLogger(__name__)


# -- Process-level disclosure log failure counter ----------------------------

_process_disclosure_log_failures: int = 0
_process_failures_lock = threading.Lock()


def get_process_disclosure_log_failures() -> int:
    """Return the process-level disclosure log write failure count."""
    with _process_failures_lock:
        return _process_disclosure_log_failures


def _increment_process_failures() -> None:
    global _process_disclosure_log_failures
    with _process_failures_lock:
        _process_disclosure_log_failures += 1


# -- Abstraction helpers ------------------------------------------------------

def _apply_abstraction(f: PersonalField, tier: int) -> str | None:
    """
    Apply the appropriate abstraction rule for the given abstraction_tier.
    tier == abstraction_tier from StepContext — never execution_tier.
    Returns abstracted value string, or None if field should be omitted.

    ADR-012 Amendment 1: not_permitted enforces at tier >= 2 only.
    ADR-012 Amendment 2: abstraction_tier >= 2 guaranteed for external calls
      by lifecycle floor clamping — this function trusts that invariant.
    ADR-012 Amendment 3: also called with raw_abstraction for floor impact
      detection — comparison only, no side effects.

    Post-Layer 6 integration fix: not_permitted returns None (withhold) at
      tier >= 2 rather than raising PrivacyGateBlockedError.
    """
    if tier <= 1:
        return f.field_value

    policy = f.abstraction_tier2 if tier == 2 else f.abstraction_tier3

    if policy == "pass":
        return f.field_value

    if policy == "omit":
        return None

    if policy == "not_permitted":
        return None

    if policy == "summarize":
        sensitivity_labels = {
            "general":   f"a {f.field_name.replace('_', ' ')}",
            "personal":  f"personal {f.field_name.replace('_', ' ')} information",
            "medical":   f"medical information",
            "financial": f"financial information",
        }
        return sensitivity_labels.get(
            f.sensitivity, f"a {f.field_name.replace('_', ' ')}"
        )

    if policy == "range_only":
        try:
            value_str = (
                f.field_value.strip()
                .replace(",", "")
                .replace("\u00a3", "")
                .replace("$", "")
            )
            numeric = float(value_str)
            low = int((numeric * 0.80) / 5000) * 5000
            high = int((numeric * 1.20) / 5000 + 1) * 5000
            currency = (
                "\u00a3" if "\u00a3" in f.field_value
                else "$" if "$" in f.field_value
                else ""
            )
            if numeric >= 1000:
                return f"{currency}{low // 1000}k-{currency}{high // 1000}k"
            return f"{low}-{high}"
        except (ValueError, TypeError):
            return f"a {f.field_name.replace('_', ' ')}"

    return None  # unknown policy — fail safe with omit


# -- Gate result types --------------------------------------------------------

@dataclass
class Gate1Result:
    approved_fields: dict[str, str]
    withheld_fields: list[str]
    fields_shared: list[str]
    floor_clamped_fields: list[str]
    disclosure_log_id: str
    blocked: bool = False
    block_error: PrivacyGateBlockedError | None = None


@dataclass
class Gate2Result:
    flagged: bool
    matched_field_names: list[str]


@dataclass
class Gate3Result:
    approved: bool
    blocked: bool
    plain_language: str | None = None


@dataclass
class Gate4Result:
    content_approved: bool
    clipboard_blocked: bool
    plain_language: str | None = None


CLIPBOARD_MAX_SENSITIVITY_SEVERITY = 2


# -- PrivacyGateway -----------------------------------------------------------

class PrivacyGateway:
    """
    Rules-based Privacy Guardian.
    Constructed once per FocusRun in lifecycle.py Phase 3 INITIALIZE.
    Holds user_id, persona_id, key_hex for disclosure_log writes.
    Runs Gate1 (field approval), Gate2 (response scan), Gate3 (content
    promotion), Gate4 (Tier 3 boundary) and writes the permanent
    disclosure_log audit trail on every gate invocation.

    All gate invocations append to disclosure_log — NEVER deleted.
    Tier 1 write failures: non-fatal (process counter + instance deque).
    Tier 2+ write failures: FATAL — DisclosureLogWriteError raised.
    """

    def __init__(self, user_id: str, persona_id: str, key_hex: str) -> None:
        self.user_id = user_id
        self.persona_id = persona_id
        self.key_hex = key_hex
        self._disclosure_log_failures: deque[str] = deque(maxlen=32)

    # -- PG_GATE_1: field approval/abstraction --------------------------------

    def gate1(
        self,
        step_id: str,
        focus_run_id: str,
        personal_track: PersonalTrack,
        abstraction_tier: int,
        raw_abstraction: int,
        execution_tier: int,
        provider: str | None = None,
    ) -> Gate1Result:
        """
        Step 6: Evaluate each personal field against abstraction policy.

        Gate1 evaluates the FULL PersonalTrack — this is the privacy guarantee.
        not_permitted fields are withheld, not a step-level block.
        Executor projects approved_fields to step.field_requirements before
        writing the disclosure buffer — that is the step-scope boundary.

        D5-073 invariant: ALWAYS writes disclosure_log + disclosure buffer,
          even if personal_track.fields is empty.
        """
        approved: dict[str, str] = {}
        withheld: list[str] = []

        for field_name, personal_field in personal_track.fields.items():
            abstracted = _apply_abstraction(personal_field, abstraction_tier)
            if abstracted is None:
                withheld.append(field_name)
            else:
                approved[field_name] = abstracted

        # Detect floor clamping impact (ADR-012 Amendment 3).
        floor_clamped: list[str] = []
        if raw_abstraction != abstraction_tier:
            for field_name, personal_field in personal_track.fields.items():
                raw_result = _apply_abstraction(personal_field, raw_abstraction)
                clamped_result = approved.get(field_name)
                if raw_result != clamped_result:
                    floor_clamped.append(field_name)

        # Audit split: shared (pass-through raw), abstracted (transformed).
        fields_shared = [
            name for name, pf in personal_track.fields.items()
            if name in approved and approved[name] == pf.field_value
        ]
        fields_abstracted = {
            name: approved[name]
            for name in approved
            if name not in fields_shared
        }

        log_id = self._write_disclosure_log(
            step_id=step_id,
            focus_run_id=focus_run_id,
            execution_tier=execution_tier,
            abstraction_tier=abstraction_tier,
            provider=provider,
            fields_shared=fields_shared,
            fields_abstracted=fields_abstracted,
            fields_withheld=withheld,
            override_declined=False,
            event_type="gate1_pass",
        )

        return Gate1Result(
            approved_fields=approved,
            withheld_fields=withheld,
            fields_shared=fields_shared,
            floor_clamped_fields=floor_clamped,
            disclosure_log_id=log_id,
            blocked=False,
            block_error=None,
        )

    # -- PG_GATE_2: inbound response scan -------------------------------------

    def gate2(
        self,
        step_id: str,
        focus_run_id: str,
        response_content: str,
        personal_track: PersonalTrack,
        execution_tier: int,
        provider: str | None = None,
        fields_shared: list[str] | None = None,
    ) -> Gate2Result:
        """
        Step 11: Scan inbound response for personal field value leakage.

        Scan set = all PersonalTrack fields - fields_shared.
        fields_shared=None: scans all PersonalTrack fields (backward compat).
        """
        matched: list[str] = []
        MIN_MATCH_LENGTH = 4

        if fields_shared is not None:
            shared_set = set(fields_shared)
            scan_fields = {
                name: pf
                for name, pf in personal_track.fields.items()
                if name not in shared_set
            }
        else:
            scan_fields = personal_track.fields

        for field_name, personal_field in scan_fields.items():
            value = personal_field.field_value
            if (
                len(value) >= MIN_MATCH_LENGTH
                and value.lower() in response_content.lower()
            ):
                matched.append(field_name)

        flagged = len(matched) > 0

        if flagged:
            self._write_disclosure_log(
                step_id=step_id,
                focus_run_id=focus_run_id,
                execution_tier=execution_tier,
                abstraction_tier=None,
                provider=provider,
                fields_shared=[],
                fields_abstracted={},
                fields_withheld=matched,
                override_declined=False,
                event_type="gate2_contamination_detected",
            )

        return Gate2Result(flagged=flagged, matched_field_names=matched)

    # -- PG_GATE_3: cross-tier content promotion ------------------------------

    def gate3(
        self,
        step_id: str,
        focus_run_id: str,
        content_key: str,
        content: str,
        content_sensitivity_severity: int,
        target_tier: int,
        space_max_permitted_tier: int,
        execution_tier: int,
    ) -> Gate3Result:
        """
        Step 13: Approve content for promotion from TaskTrack to SharedStateTrack.
        """
        if target_tier > space_max_permitted_tier:
            self._write_disclosure_log(
                step_id=step_id,
                focus_run_id=focus_run_id,
                execution_tier=execution_tier,
                abstraction_tier=None,
                provider=None,
                fields_shared=[],
                fields_abstracted={},
                fields_withheld=[content_key],
                override_declined=True,
                event_type="gate3_tier_ceiling_block",
            )
            return Gate3Result(
                approved=False,
                blocked=True,
                plain_language=(
                    "This content can't be shared with a higher-tier service "
                    "from this Focus. [Change Focus settings] [Use local only]"
                ),
            )

        if content_sensitivity_severity >= 3 and target_tier >= 2:
            self._write_disclosure_log(
                step_id=step_id,
                focus_run_id=focus_run_id,
                execution_tier=execution_tier,
                abstraction_tier=None,
                provider=None,
                fields_shared=[],
                fields_abstracted={},
                fields_withheld=[content_key],
                override_declined=True,
                event_type="gate3_sensitivity_block",
            )
            return Gate3Result(
                approved=False,
                blocked=True,
                plain_language=(
                    "This content contains medical or financial information "
                    "and can't be shared with external services. "
                    "[Use local only] [Get help]"
                ),
            )

        self._write_disclosure_log(
            step_id=step_id,
            focus_run_id=focus_run_id,
            execution_tier=execution_tier,
            abstraction_tier=None,
            provider=None,
            fields_shared=[content_key],
            fields_abstracted={},
            fields_withheld=[],
            override_declined=False,
            event_type="gate3_promotion_approved",
        )
        return Gate3Result(approved=True, blocked=False)

    # -- PG_GATE_4: validation content preparation ----------------------------

    def gate4(
        self,
        step_id: str,
        focus_run_id: str,
        content: str,
        content_sensitivity_severity: int,
        execution_tier: int,
    ) -> Gate4Result:
        """
        Pre-Tier 3 boundary gate. Layer 4 stub.
        clipboard_blocked: True if severity > CLIPBOARD_MAX_SENSITIVITY_SEVERITY.
        """
        clipboard_blocked = (
            content_sensitivity_severity > CLIPBOARD_MAX_SENSITIVITY_SEVERITY
        )

        self._write_disclosure_log(
            step_id=step_id,
            focus_run_id=focus_run_id,
            execution_tier=execution_tier,
            abstraction_tier=None,
            provider="tier3_validation",
            fields_shared=[],
            fields_abstracted={},
            fields_withheld=[],
            override_declined=False,
            event_type="gate4_stub_validation",
        )

        plain_language = None
        if clipboard_blocked:
            plain_language = (
                "This content contains sensitive information and must be "
                "copied manually — it can't be sent to your clipboard automatically."
            )

        return Gate4Result(
            content_approved=True,
            clipboard_blocked=clipboard_blocked,
            plain_language=plain_language,
        )

    # -- Voice profile contamination audit ------------------------------------

    def record_voice_profile_contamination(
        self,
        step_id: str,
        focus_run_id: str,
        execution_tier: int,
        abstraction_tier: int,
        attribute_name: str,
    ) -> None:
        """
        Write a disclosure log audit event for a voice profile contamination
        detection. Called by StepExecutor._scan_voice_profile().

        attribute_name: the offending key — never the value.
        Non-fatal at Tier 1 (consistent with _write_disclosure_log policy).
        FATAL at Tier 2+ via DisclosureLogWriteError.
        """
        self._write_disclosure_log(
            step_id=step_id,
            focus_run_id=focus_run_id,
            execution_tier=execution_tier,
            abstraction_tier=abstraction_tier,
            provider=None,
            fields_shared=[],
            fields_abstracted={},
            fields_withheld=[attribute_name],
            override_declined=False,
            event_type="voice_profile_contamination_detected",
        )

    # -- Disclosure log writer ------------------------------------------------

    def _write_disclosure_log(
        self,
        step_id: str,
        focus_run_id: str,
        execution_tier: int,
        abstraction_tier: int | None,
        provider: str | None,
        fields_shared: list[str],
        fields_abstracted: dict[str, str],
        fields_withheld: list[str],
        override_declined: bool,
        event_type: str = "generic",
    ) -> str:
        """
        Append a disclosure_log entry to personal.db.
        Returns the log entry id.

        NEVER deleted — permanent audit trail.
        routing_tier column: populated with execution_tier for post-ADR-012
          records (backwards compatible with Layer 1-5 rows).

        Write failure behaviour (ADR-012 Disclosure Log Fatality Policy):
          execution_tier > 1: raise DisclosureLogWriteError — run halts.
          execution_tier == 1: non-fatal, process counter + instance deque.
        """
        log_id = str(uuid.uuid4())
        try:
            with open_personal_db(
                self.user_id, self.persona_id, self.key_hex
            ) as db:
                db.execute(
                    """INSERT INTO disclosure_log
                       (id, user_id, persona_id, focus_run_id, step_id,
                        routing_tier, execution_tier, abstraction_tier,
                        provider, fields_shared, fields_abstracted,
                        fields_withheld, override_declined, declined_at,
                        created_at, extra_metadata)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                    [
                        log_id,
                        self.user_id,
                        self.persona_id,
                        focus_run_id,
                        step_id,
                        execution_tier,    # routing_tier = execution_tier (ADR-012)
                        execution_tier,    # execution_tier column
                        abstraction_tier,  # abstraction_tier column
                        provider,
                        json.dumps(fields_shared),
                        json.dumps(fields_abstracted),
                        json.dumps(fields_withheld),
                        1 if override_declined else 0,
                        now() if override_declined else None,
                        now(),
                        json.dumps({"event_type": event_type}),
                    ],
                )
        except DisclosureLogWriteError:
            raise
        except Exception as e:
            failure_msg = f"[{now()}] {event_type} step={step_id}: {e}"
            if execution_tier > 1:
                log.error(
                    "FATAL disclosure log write failure (Tier 2+): "
                    "step=%s event=%s error=%s", step_id, event_type, e
                )
                raise DisclosureLogWriteError(
                    plain_language=(
                        "Quiet Rabbit couldn't record your privacy preferences "
                        "before sending data to an external service. "
                        "Your data was not sent. [Try again] [Get help]"
                    )
                ) from e
            else:
                log.warning(
                    "Non-fatal disclosure log write failure (Tier 1): "
                    "step=%s event=%s error=%s", step_id, event_type, e
                )
                self._disclosure_log_failures.append(failure_msg)
                _increment_process_failures()

        return log_id
