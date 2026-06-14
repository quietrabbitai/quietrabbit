# conductor/lifecycle.py
# FocusRun — the Conductor execution engine's seven-phase lifecycle.
#
# Phase 1 LOAD:       parse .focus file, validate steps
# Phase 2 AUTHORIZE:  tier check, create focus_run record (status=initializing)
# Phase 3 INITIALIZE: open personal.db, build and seal PersonalTrack,
#                     assemble TaskTrack + SharedStateTrack, construct
#                     PrivacyGateway, promote to running
# Phase 4 EXECUTE:    step loop — Tier 3 steps are terminal boundaries
# Phase 5 OUTPUT:     write output to outputs.db, purge snapshots
# Phase 6 FEEDBACK:   async paste-back (called separately, not here)
# Phase 7 CLEANUP:    release connections, enforce snapshot retention
#
# Layer 6 (ADR-012 Amendments 2 + 3) changes to _execute_step():
# - Computes execution_tier AND abstraction_tier independently
# - Floor clamping: abstraction_tier = max(2, raw) when execution_tier > 1
# - Logs life_privacy_default_tier before discarding (not stored on StepContext)
# - Passes execution_tier, abstraction_tier, raw_abstraction to StepContext
# - next_routing_tier RENAMED to next_execution_tier (ADR-012 naming fix)
# - Removes: focus_max_routing_tier, life_privacy_default_tier from StepContext
# - Removes: resolved_tier, effective_tier from StepContext construction
# - floor_consent_preference: read from personas.extra_metadata in shared.db
#   via open_instance_db(). Scoped consent record validated before honoring.
#   "modified" → executor bypasses Floor Consent Gate + writes audit event.
#   Non-fatal if read fails — gate fires normally.
# - _handle_step_failure(): await_floor_consent added to awaiting_user group
#
# D5-152 (floor consent preference scoping):
# Consent record stored in personas.extra_metadata in shared.db (not outputs.db).
# Schema: {"mode": "modified", "abstraction_tier": N,
#           "consent_timestamp": "...", "consent_version": "1"}
# Validation: consent honored if mode=="modified" and stored abstraction_tier
# <= current step's abstraction_tier.
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   PathDefinition → FocusDefinition, path_id → focus_id throughout
#   PathRun → FocusRun, space_id → life_id throughout
#   _space_max_permitted_tier → _life_max_permitted_tier
#   _space_privacy_default_tier → _life_privacy_default_tier
#   _find_path_file → _find_focus_file, paths/ → focuses/
#   _parse_path_definition → _parse_focus_definition
#   specialist_id → guide_id in YAML parsing
#   _get_space_tiers → _get_life_tiers
#   _write_path_run_record → _write_focus_run_record
#   path_runs → focus_runs, path_run_snapshots → focus_run_snapshots in SQL
#   spaces table → lives table, space_id column → life_id column
#   _load_specialist_versions → two separate queries:
#     _load_guide_versions() — artifact_type='guide'
#     _load_operator_versions() — artifact_type='operator'
#   StepContext: path_id → focus_id, path_run_id → focus_run_id
#   space_store → life_store import
#   open_personal_db / open_outputs_db: space_id → life_id param
#   demote_interrupted_runs: focus_runs table

# Phase B — Memory Broker integration (D6-226+):
#   FocusRun gains topic_id and is_quick_ask parameters.
#   Phase 3 INITIALIZE: Memory Broker called after PersonalTrack sealed.
#     Broker always runs — topic_id=None means Tier B skipped, not broker skipped.
#     ContextSlice rendered to _persona_context_rendered string immediately.
#     slice_.clear() called — rendered string only is retained for session.
#   _write_focus_run_record(): topic_id and is_quick_ask written on INSERT.
#   _execute_step(): passes _persona_context_rendered as life_context to StepContext.
#   Phase 5 OUTPUT: run_history entry written after save_output().
#   Phase 7 CLEANUP: _persona_context_rendered cleared.
# Phase C -- Persona model migration (D6-298):
#   FocusRun.__init__: life_id -> persona_id
#   _get_life_tiers() -> _get_focus_tier_ceiling():
#     reads focus_settings_store.get_focus_settings(persona_id, focus_id)
#     returns (max_permitted_tier, privacy_tier) from focus_settings row
#   AUTHORIZE: asserts focus_settings row exists -- hard error if missing (D6-303)
#   All open_outputs_db / open_personal_db: life_id -> persona_id
#   PrivacyGateway constructor: life_id -> persona_id
#   _assemble_life_context() -> _assemble_persona_context()
#   demote_interrupted_runs(): life_id -> persona_id
#   FocusDefinition: life_affinity field dropped (D6-300)
#   D5-152: floor_consent_preference now in personas.extra_metadata

from __future__ import annotations

import hashlib
import json
import logging
import uuid
from dataclasses import dataclass, field
from pathlib import Path

import yaml

from conductor.concurrency import ConductorScheduler
from conductor.context import (
    PersonalTrack,
    PersonalContextManifest,
    TaskTrack,
    SharedStateTrack,
)
from conductor.failure import FailureHandler, FailureResult
from conductor.privacy import PrivacyGateway
from conductor.tokens import StepDefinition, validate_step, SYSTEM_TOKENS
from providers.errors import TaxonomyIntegrityError, DatabaseMigrationError
from providers.utils import now, get_data_root, open_instance_db, open_outputs_db

log = logging.getLogger(__name__)


@dataclass
class FocusDefinition:
    focus_id: str
    display_name: str
    description: str
    version: str
    max_routing_tier: int
    steps: list[StepDefinition]
    output_type: str
    suggest_in_focuses: list[str] = field(default_factory=list)
    multi_source_validation: bool = False


@dataclass
class RunResult:
    focus_run_id: str
    status: str
    output_id: str | None = None
    output_content: str | None = None
    failure: FailureResult | None = None


class FocusRun:
    """
    Orchestrates a single focus run through all seven lifecycle phases.
    scheduler is required — pass the shared ConductorScheduler from app factory.
    """

    def __init__(
        self,
        user_id: str,
        persona_id: str,
        focus_id: str,
        scheduler: ConductorScheduler,
        user_input: str = "",
        is_fast_lane: bool = False,
        key_hex: str | None = None,
        topic_id: str | None = None,
        is_quick_ask: bool = False,
    ):
        self.user_id = user_id
        self.persona_id = persona_id
        self.focus_id = focus_id
        self.scheduler = scheduler
        self.user_input = user_input
        self.is_fast_lane = is_fast_lane
        self.key_hex = key_hex
        self.topic_id = topic_id
        self.is_quick_ask = is_quick_ask

        self.focus_run_id: str | None = None
        self.focus_def: FocusDefinition | None = None
        self.personal_track: PersonalTrack | None = None
        self.task_track: TaskTrack | None = None
        self.shared_state: SharedStateTrack | None = None
        self.failure_handler: FailureHandler | None = None
        self.privacy_gateway: PrivacyGateway | None = None
        self._focus_max_permitted_tier: int = 1
        self._focus_privacy_tier: int = 1
        self._output_id: str | None = None
        self._current_step_index: int = 0
        self._checkpointing_suspended: bool = False
        self._persona_context_rendered: str = ""

    # =========================================================================
    # Phase 1 — LOAD
    # =========================================================================

    def load(self) -> None:
        focus_file = self._find_focus_file()
        raw = yaml.safe_load(focus_file.read_text())
        self.focus_def = self._parse_focus_definition(raw)
        all_errors = []
        for step in self.focus_def.steps:
            errors = validate_step(step)
            all_errors.extend(errors)
        if all_errors:
            raise ValueError(
                f"Focus '{self.focus_id}' failed validation:\n"
                + "\n".join(f"  - {e}" for e in all_errors)
            )

    def _find_focus_file(self) -> Path:
        repo_root = Path(__file__).parent.parent
        candidates = [
            repo_root / "app" / "core_artifacts" / "focuses" / f"{self.focus_id}.focus",
            get_data_root() / "community_artifacts" / "focuses" / f"{self.focus_id}.focus",
        ]
        for candidate in candidates:
            if candidate.exists():
                return candidate
        raise FileNotFoundError(f"Focus file not found: {self.focus_id}.focus")

    def _parse_focus_definition(self, raw: dict) -> FocusDefinition:
        # Conductor-brief uses "id" at top level; legacy used "focus_id".
        # Conductor-brief is the primary format — "id" takes precedence.
        focus_id = raw.get("id") or raw.get("focus_id") or ""

        # Conductor-brief declares guides at focus level, not per step.
        # COMPATIBILITY: StepDefinition still requires a guide_id per step.
        # First entry in the focus-level guides list is inherited by all steps
        # that do not declare their own guide_id. This is a temporary shim —
        # when multi-guide Focuses exist, per-step guide_id will be required.
        focus_level_guides: list[str] = raw.get("guides", [])
        default_guide_id = focus_level_guides[0] if focus_level_guides else "quick-ask-guide"

        # Conductor-brief uses output_types (list); legacy used output_type (str).
        # COMPATIBILITY: FocusDefinition currently holds a single output_type.
        # First entry used until FocusDefinition supports plural output_types.
        output_types_list: list[str] = raw.get("output_types", [])
        output_type: str = (
            output_types_list[0]
            if output_types_list
            else raw.get("output_type", "general")
        )

        # suggest_in_focuses: fall back to legacy suggest_in_paths if absent.
        # Phase A rename retired suggest_in_paths — fallback handles any
        # artifacts that predate the rename.
        suggest_in_focuses: list[str] = (
            raw.get("suggest_in_focuses")
            or raw.get("suggest_in_paths")
            or []
        )

        raw_steps = raw.get("steps", {})

        # Conductor-brief: steps is a dict keyed by step_id — PRIMARY format.
        # Legacy: steps is a list of dicts with explicit step_id keys.
        # List format retained as compatibility code; no new Focuses use it.
        if isinstance(raw_steps, dict):
            step_items = list(raw_steps.items())
        else:
            step_items = [(s["step_id"], s) for s in raw_steps]

        steps = []
        for step_id_key, raw_step in step_items:
            if not isinstance(raw_step, dict):
                raise ValueError(
                    f"Focus '{focus_id}': step '{step_id_key}' must be a mapping, "
                    f"got {type(raw_step).__name__}."
                )

            # In conductor-brief, the dict key is authoritative as step_id.
            # In legacy list format, step_id comes from within the dict.
            step_id = step_id_key if isinstance(raw_steps, dict) else raw_step["step_id"]

            # field_requirements: conductor-brief uses list of {name, scope} dicts.
            # Legacy uses a flat {name: scope} dict.
            raw_fr = raw_step.get("field_requirements", {})
            if isinstance(raw_fr, list):
                field_requirements = {
                    entry["name"]: entry["scope"]
                    for entry in raw_fr
                    if isinstance(entry, dict) and "name" in entry and "scope" in entry
                }
            else:
                field_requirements = dict(raw_fr) if raw_fr else {}

            step = StepDefinition(
                step_id=step_id,
                display_name=raw_step.get("display_name", step_id),
                guide_id=raw_step.get("guide_id", default_guide_id),
                task_type=raw_step.get("task_type", "general"),
                routing_tier=raw_step.get("routing_tier", 1),
                step_type=raw_step.get("step_type", "generate"),
                output_var=raw_step.get("output_var"),
                prompt_template=raw_step.get("prompt_template", ""),
                field_requirements=field_requirements,
                options_override=raw_step.get("options_override", {}),
            )
            steps.append(step)

        return FocusDefinition(
            focus_id=focus_id,
            display_name=raw.get("display_name", focus_id),
            description=raw.get("description", ""),
            version=str(raw.get("version", "1.0")),
            max_routing_tier=raw.get("max_routing_tier", 1),
            steps=steps,
            output_type=output_type,
            suggest_in_focuses=suggest_in_focuses,
            multi_source_validation=raw.get("multi_source_validation", False),
        )

    # =========================================================================
    # Phase 2 — AUTHORIZE
    # =========================================================================

    def authorize(self) -> None:
        assert self.focus_def is not None
        if not self.key_hex:
            from providers.errors import PersonalDBDecryptionError
            raise PersonalDBDecryptionError(
                plain_language="Your session has expired. Please log in again."
            )
        self._focus_max_permitted_tier, self._focus_privacy_tier = (
            self._get_focus_tier_ceiling()
        )
        for step in self.focus_def.steps:
            if step.routing_tier > self._focus_max_permitted_tier:
                raise PermissionError(
                    f"Step '{step.step_id}' requires tier {step.routing_tier} "
                    f"but focus ceiling is {self._focus_max_permitted_tier}."
                )
        self.failure_handler = FailureHandler(
            space_max_permitted_tier=self._focus_max_permitted_tier
        )
        self.focus_run_id = str(uuid.uuid4())
        self._write_focus_run_record(status="initializing")

    def _get_focus_tier_ceiling(self) -> tuple[int, int]:
        from persistence.persona_store import get_persona_for_user
        from persistence.focus_settings_store import get_focus_settings
        persona = get_persona_for_user(self.user_id, self.persona_id)
        if not persona:
            raise LookupError(
                f"Persona '{self.persona_id}' not found for user '{self.user_id}'."
            )
        focus_settings = get_focus_settings(self.persona_id, self.focus_id)
        if not focus_settings:
            raise LookupError(
                f"Focus settings not found for persona='{self.persona_id}' "
                f"focus='{self.focus_id}'. Configure this Focus before running."
            )
        return focus_settings.max_permitted_tier, focus_settings.privacy_tier

    def _write_focus_run_record(self, status: str) -> None:
        assert self.focus_run_id is not None
        assert self.key_hex is not None
        with open_outputs_db(self.user_id, self.persona_id, self.key_hex) as db:
            existing = db.execute(
                "SELECT id FROM focus_runs WHERE id = ?", [self.focus_run_id]
            ).fetchone()
            if existing:
                db.execute(
                    "UPDATE focus_runs SET status = ? WHERE id = ?",
                    [status, self.focus_run_id]
                )
            else:
                db.execute(
                    """INSERT INTO focus_runs
                       (id, focus_id, status, is_fast_lane, is_quick_ask,
                        topic_id, started_at, notes)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
                    [self.focus_run_id, self.focus_id, status,
                     1 if self.is_fast_lane else 0,
                     1 if self.is_quick_ask else 0,
                     self.topic_id, now(), "{}"],
                )

    # =========================================================================
    # Phase 3 — INITIALIZE
    # =========================================================================

    def initialize(self) -> None:
        assert self.focus_run_id is not None
        assert self.focus_def is not None
        self.personal_track = self._build_personal_track()
        self.personal_track.seal()
        self.task_track = TaskTrack()
        self.shared_state = SharedStateTrack()
        self.privacy_gateway = PrivacyGateway(
            self.user_id, self.persona_id, self.key_hex
        )
        self._persona_context_rendered = self._assemble_persona_context()
        self._write_focus_run_record(status="running")

    def _assemble_persona_context(self) -> str:
        """
        Call Memory Broker to assemble context slice for this session.
        Broker always runs — topic_id=None means Tier B skipped, not broker skipped.
        Unnamed runs still load Domain Context standing summary (Tier A).
        ContextSlice rendered to string immediately, then cleared.
        Only the rendered string is retained for the session — not raw blocks.

        execution_tier: uses _focus_max_permitted_tier (run ceiling) so the
        Retrieval Eligibility Check admits all blocks the run could access.

        Failure handling: non-fatal for Quick Ask and unnamed runs (no active
        topic context to lose). WARNING logged for topic-active sessions where
        failure means Domain Context and Plan State are silently absent.
        """
        try:
            from conductor.memory_broker import MemoryBroker
            broker = MemoryBroker()
            model_context_window = int(
                __import__("os").environ.get("QR_DEFAULT_CONTEXT_WINDOW", "8192")
            )
            slice_ = broker.assemble_context(
                user_id=self.user_id,
                persona_id=self.persona_id,
                focus_id=self.focus_id,
                topic_id=self.topic_id,
                key_hex=self.key_hex,
                execution_tier=self._focus_max_permitted_tier,
                model_context_window=model_context_window,
                is_quick_ask=self.is_quick_ask,
            )
            rendered = slice_.render()
            slice_.clear()
            return rendered
        except Exception as e:
            if self.topic_id is not None:
                log.warning(
                    "memory_broker: context assembly FAILED for topic-active session — "
                    "Domain Context and Plan State unavailable for this run. "
                    "focus=%s topic=%s error=%s",
                    self.focus_id, self.topic_id, e,
                )
            else:
                log.debug(
                    "memory_broker: context assembly failed (no active topic) — "
                    "continuing with empty persona_context. focus=%s error=%s",
                    self.focus_id, e,
                )
            return ""

    def _build_personal_track(self) -> PersonalTrack:
        from persistence.personal_store import load_personal_track
        track = load_personal_track(self.user_id, self.persona_id, self.key_hex)
        guide_ids = list({step.guide_id for step in self.focus_def.steps})
        versions = self._load_guide_versions(guide_ids)
        versions.update(self._load_operator_versions())
        track.set_source_versions(versions)
        return track

    def _load_guide_versions(self, guide_ids: list[str]) -> dict[str, str]:
        """Load artifact versions for guides declared in this focus's steps."""
        if not guide_ids:
            return {}
        placeholders = ",".join("?" * len(guide_ids))
        with open_instance_db() as db:
            rows = db.execute(
                f"SELECT artifact_id, version FROM artifact_versions "
                f"WHERE artifact_type = 'guide' "
                f"AND artifact_id IN ({placeholders}) AND revoked = 0",
                guide_ids
            ).fetchall()
        return {row["artifact_id"]: row["version"] for row in rows}

    def _load_operator_versions(self) -> dict[str, str]:
        """Load artifact versions for all active system operators."""
        with open_instance_db() as db:
            rows = db.execute(
                "SELECT artifact_id, version FROM artifact_versions "
                "WHERE artifact_type = 'operator' AND revoked = 0"
            ).fetchall()
        return {row["artifact_id"]: row["version"] for row in rows}

    # =========================================================================
    # Phase 4 — EXECUTE
    # =========================================================================

    def execute(self) -> RunResult | None:
        assert self.personal_track is not None and self.personal_track.is_sealed
        assert self.task_track is not None
        assert self.shared_state is not None
        assert self.focus_def is not None
        assert self.focus_run_id is not None
        assert self.failure_handler is not None
        assert self.privacy_gateway is not None

        checkpoint_counter = 0
        checkpoint_every = int(
            __import__("os").environ.get("QR_CHECKPOINT_EVERY_N_STEPS", "3")
        )

        for i, step in enumerate(
            self.focus_def.steps[self._current_step_index:],
            start=self._current_step_index
        ):
            self._current_step_index = i

            if step.routing_tier == 3:
                if not self._checkpointing_suspended:
                    try:
                        self._write_checkpoint(step.step_id)
                    except Exception:
                        pass
                self._write_focus_run_record(status="awaiting_user")
                return RunResult(focus_run_id=self.focus_run_id, status="awaiting_user")

            step_failure = self._execute_step(step, step_index=i)

            if step_failure is not None:
                if step_failure.action == "degrade":
                    self._checkpointing_suspended = True
                    continue
                return self._handle_step_failure(step_failure)

            checkpoint_counter += 1
            if (
                checkpoint_counter >= checkpoint_every
                and not self._checkpointing_suspended
            ):
                self._write_checkpoint(step.step_id)
                checkpoint_counter = 0

        return None

    def _execute_step(
        self, step: StepDefinition, step_index: int
    ) -> FailureResult | None:
        """
        Compute both tier values per ADR-012 Amendment 3, construct StepContext,
        and delegate to StepExecutor.

        Axis 1 — execution_tier:
          min(life_max_permitted, focus_max_routing, step.routing_tier)

        Axis 2 — abstraction_tier:
          raw_abstraction = min(life_privacy_default, execution_tier)
          abstraction_tier = max(2, raw_abstraction) if execution_tier > 1

        floor_consent_preference (D5-152):
          Read from lives.extra_metadata in shared.db (open_instance_db).
        """
        from conductor.executor import StepExecutor, StepContext

        # Axis 1: execution_tier
        execution_tier = min(
            self._focus_max_permitted_tier,
            self.focus_def.max_routing_tier,
            step.routing_tier,
        )

        # Axis 2: abstraction_tier with floor clamping
        raw_abstraction = min(self._focus_privacy_tier, execution_tier)
        abstraction_tier = (
            max(2, raw_abstraction) if execution_tier > 1 else raw_abstraction
        )

        log.debug(
            "step=%s execution_tier=%d abstraction_tier=%d "
            "raw_abstraction=%d focus_privacy_tier=%d",
            step.step_id, execution_tier, abstraction_tier,
            raw_abstraction, self._focus_privacy_tier,
        )

        # Gate3 look-ahead
        steps = self.focus_def.steps
        next_execution_tier: int | None = None
        if step_index + 1 < len(steps):
            next_step = steps[step_index + 1]
            next_execution_tier = min(
                self._focus_max_permitted_tier,
                self.focus_def.max_routing_tier,
                next_step.routing_tier,
            )

        # Floor consent preference (D5-152).
        # Read scoped consent record from lives.extra_metadata in shared.db.
        floor_consent_preference: str | None = None
        try:
            with open_instance_db() as db:
                row = db.execute(
                    "SELECT extra_metadata FROM personas WHERE id = ?",
                    [self.persona_id]
                ).fetchone()
                if row:
                    meta = json.loads(row["extra_metadata"] or "{}")
                    consent = meta.get("floor_consent_preference")
                    if isinstance(consent, dict):
                        stored_mode = consent.get("mode")
                        stored_tier = consent.get("abstraction_tier")
                        if (
                            stored_mode == "modified"
                            and isinstance(stored_tier, int)
                            and stored_tier <= abstraction_tier
                        ):
                            floor_consent_preference = "modified"
                        elif stored_mode == "local":
                            floor_consent_preference = "local"
        except Exception:
            pass  # non-fatal — consent gate fires normally if read fails

        ctx = StepContext(
            step=step,
            focus_id=self.focus_id,
            focus_run_id=self.focus_run_id,
            user_input=self.user_input,
            personal_track=self.personal_track,
            task_track=self.task_track,
            shared_state=self.shared_state,
            failure_handler=self.failure_handler,
            privacy_gateway=self.privacy_gateway,
            scheduler=self.scheduler,
            space_max_permitted_tier=self._focus_max_permitted_tier,
            execution_tier=execution_tier,
            abstraction_tier=abstraction_tier,
            raw_abstraction=raw_abstraction,
            floor_consent_preference=floor_consent_preference,
            next_execution_tier=next_execution_tier,
            retry_count=0,
            life_context=self._persona_context_rendered,
        )
        return StepExecutor().execute(ctx)

    def _handle_step_failure(self, failure: FailureResult) -> RunResult:
        if failure.action == "stop" and not failure.is_recoverable:
            self._write_focus_run_record(status="failed")
        elif failure.action in (
            "await_user", "hold_for_gate", "offer_tier2",
            "offer_compact", "await_floor_consent",
        ):
            self._write_focus_run_record(status="awaiting_user")
        return RunResult(
            focus_run_id=self.focus_run_id,
            status=self._get_current_status(),
            failure=failure,
        )

    def _get_current_status(self) -> str:
        try:
            with open_outputs_db(self.user_id, self.persona_id, self.key_hex) as db:
                row = db.execute(
                    "SELECT status FROM focus_runs WHERE id = ?",
                    [self.focus_run_id]
                ).fetchone()
            return row["status"] if row else "unknown"
        except Exception:
            return "unknown"

    def _write_checkpoint(self, step_id: str) -> None:
        from providers.errors import SnapshotWriteError

        manifest = PersonalContextManifest.from_personal_track(
            self.personal_track, now()
        )
        task_data = {
            "steps": [
                {
                    "step_id": s.step_id,
                    "output_var": s.output_var,
                    "content": s.content,
                    "sensitivity_severity": s.sensitivity_severity,
                    "routing_tier_used": s.routing_tier_used,
                }
                for s in self.task_track.steps
            ],
            "sensitivity_ceiling": self.task_track.sensitivity_ceiling,
        }
        shared_data = {
            "step_disclosure_buffers": dict(self.shared_state.step_disclosure_buffers),
            "promotions": [
                {"step_id": p.step_id, "content_key": p.content_key, "content": p.content}
                for p in self.shared_state.promotions
            ],
        }
        task_json = json.dumps(task_data, ensure_ascii=False)
        shared_json = json.dumps(shared_data, ensure_ascii=False)
        manifest_json = json.dumps({
            "field_names": manifest.field_names,
            "field_hashes": manifest.field_hashes,
            "source_versions": manifest.source_versions,
            "snapshot_taken_at": manifest.snapshot_taken_at,
        })
        checkpoint_hash = hashlib.sha256(
            (task_json + shared_json + manifest_json).encode()
        ).hexdigest()
        try:
            with open_outputs_db(self.user_id, self.persona_id, self.key_hex) as db:
                db.execute(
                    """INSERT INTO focus_run_snapshots
                       (id, focus_run_id, step_id, phase, task_track_json,
                        shared_state_json, personal_context_manifest,
                        checkpoint_hash, created_at)
                       VALUES (?, ?, ?, 4, ?, ?, ?, ?, ?)""",
                    [str(uuid.uuid4()), self.focus_run_id, step_id,
                     task_json, shared_json, manifest_json, checkpoint_hash, now()],
                )
        except Exception as e:
            raise SnapshotWriteError(
                plain_language=(
                    "Quiet Rabbit couldn't save your progress checkpoint. "
                    "Your work will continue but can't be resumed if interrupted. "
                    "[Continue] [Stop and save manually]"
                )
            ) from e

    # =========================================================================
    # Phase 5 — OUTPUT
    # =========================================================================

    def output(self) -> RunResult:
        assert self.task_track is not None
        assert self.focus_run_id is not None
        assert self.focus_def is not None

        from persistence.output_store import save_output

        final_content = self.task_track.last_output() or ""
        output_id = str(uuid.uuid4())
        save_output(
            user_id=self.user_id,
            life_id=self.persona_id,
            key_hex=self.key_hex,
            focus_run_id=self.focus_run_id,
            output_type=self.focus_def.output_type,
            content=final_content,
            sensitivity=self._output_sensitivity(),
            output_id=output_id,
        )
        self._output_id = output_id
        self._purge_snapshots()
        self._write_run_history(output_id=output_id, output_type=self.focus_def.output_type)
        self._write_focus_run_record(status="awaiting_feedback")
        return RunResult(
            focus_run_id=self.focus_run_id,
            status="awaiting_feedback",
            output_id=output_id,
            output_content=final_content,
        )

    def _output_sensitivity(self) -> str:
        ceiling = self.task_track.sensitivity_ceiling if self.task_track else 1
        return {1: "general", 2: "personal", 3: "medical", 4: "financial"}.get(
            ceiling, "general"
        )

    def _purge_snapshots(self) -> None:
        try:
            with open_outputs_db(self.user_id, self.persona_id, self.key_hex) as db:
                db.execute(
                    "DELETE FROM focus_run_snapshots WHERE focus_run_id = ?",
                    [self.focus_run_id]
                )
        except Exception:
            pass

    def _write_run_history(self, output_id: str, output_type: str) -> None:
        """
        Write a run_history entry after output is saved.
        Non-fatal — run_history is a discovery index, not critical path.
        Quick Ask invariant: promote_window_expires_at is NULL (enforced in topic_store).
        """
        try:
            from persistence.topic_store import create_run_history_entry
            create_run_history_entry(
                user_id=self.user_id,
                life_id=self.persona_id,
                key_hex=self.key_hex,
                focus_run_id=self.focus_run_id,
                focus_id=self.focus_id,
                is_quick_ask=self.is_quick_ask,
                topic_id=self.topic_id,
                output_id=output_id,
                output_type=output_type,
            )
        except Exception as e:
            log.warning(
                "lifecycle: run_history write failed (non-fatal) — "
                "focus=%s focus_run_id=%s error=%s",
                self.focus_id, self.focus_run_id, e,
            )

    # =========================================================================
    # Phase 7 — CLEANUP
    # =========================================================================

    def cleanup(self, final_status: str = "complete") -> None:
        self.personal_track = None
        self.task_track = None
        self.shared_state = None
        self.privacy_gateway = None
        self._persona_context_rendered = ""
        if final_status in ("complete", "cancelled", "failed"):
            self._purge_snapshots()
        self._write_focus_run_record(status=final_status)

    # =========================================================================
    # Convenience: execute_full
    # =========================================================================

    def execute_full(self) -> RunResult:
        try:
            self.load()
            self.authorize()
            self.initialize()
            early_result = self.execute()
            if early_result is not None:
                self.cleanup(final_status=early_result.status)
                return early_result
            result = self.output()
            self.cleanup()
            return result
        except (TaxonomyIntegrityError, DatabaseMigrationError) as e:
            failure = (
                self.failure_handler.handle(e, focus_id=self.focus_id)
                if self.failure_handler
                else FailureResult(
                    action="stop",
                    failure_mode="F_SYSTEM",
                    plain_language=getattr(e, "plain_language", str(e)),
                    is_recoverable=False,
                    severity="stop",
                )
            )
            if self.focus_run_id:
                try:
                    self._write_focus_run_record(status="failed")
                except Exception:
                    pass
            self.personal_track = None
            self.task_track = None
            self.shared_state = None
            self.privacy_gateway = None
            return RunResult(
                focus_run_id=self.focus_run_id or "",
                status="failed",
                failure=failure,
            )
        except Exception:
            if self.focus_run_id:
                try:
                    self._write_focus_run_record(status="failed")
                except Exception:
                    pass
            self.personal_track = None
            self.task_track = None
            self.shared_state = None
            self.privacy_gateway = None
            raise


def demote_interrupted_runs(user_id: str, persona_id: str, key_hex: str) -> int:
    import os
    threshold_minutes = int(os.environ.get("QR_INTERRUPT_THRESHOLD_MINUTES", "5"))
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        result = db.execute(
            """UPDATE focus_runs SET status = 'paused'
               WHERE status IN ('running', 'initializing')
               AND started_at < datetime('now', ? || ' minutes')""",
            [f"-{threshold_minutes}"]
        )
        return result.rowcount
