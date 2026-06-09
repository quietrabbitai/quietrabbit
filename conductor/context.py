# conductor/context.py
# Context track dataclasses for the Conductor execution engine.
# Three tracks hold state during a focus run:
#
# PersonalTrack    — personal fields from personal.db (sealed after INITIALIZE)
# TaskTrack        — accumulates step outputs during execution
# SharedStateTrack — content approved for cross-tier use via PG_GATE_3
#
# PersonalContextManifest — checkpoint metadata with field name + content hashes.
#   Used by resume logic to detect changes in personal.db since last checkpoint.
#
# INVARIANT: PersonalTrack is NEVER serialized to snapshots.
#   Re-fetched fresh from personal.db on every resume.
#   Only field names + content hashes + source versions go into manifest.
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   specialist_id → source_id on PersonalField
#   set_space_context / space_context → set_life_context / life_context

from __future__ import annotations

import copy
import hashlib
from dataclasses import dataclass, field
from types import MappingProxyType
from typing import Literal


# -- Personal field -----------------------------------------------------------

@dataclass
class PersonalField:
    """
    A single personal field loaded from personal.db for this run.
    field_value is the decrypted value — held in memory only,
    never written to snapshots or logs.
    """
    field_name: str
    field_value: str                # decrypted — never logged or serialized
    sensitivity: Literal['general', 'personal', 'medical', 'financial']
    sensitivity_severity: int       # 1=general 2=personal 3=medical 4=financial
    source_id: str
    abstraction_tier2: Literal[
        'pass', 'omit', 'summarize', 'range_only', 'not_permitted'
    ] = 'pass'
    abstraction_tier3: Literal[
        'pass', 'omit', 'summarize', 'range_only', 'not_permitted'
    ] = 'pass'

    def compute_content_hash(self) -> str:
        """
        SHA-256 hash of field_name:field_value.
        Used by PersonalContextManifest to detect value changes between
        snapshots without storing the value itself.
        """
        payload = f"{self.field_name}:{self.field_value}".encode("utf-8")
        return hashlib.sha256(payload).hexdigest()


# -- Personal Track -----------------------------------------------------------

class PersonalTrack:
    """
    Read-only view of personal fields for this focus run.
    Populated at Phase 3 INITIALIZE, then sealed before execution begins.
    After seal(), all dicts are MappingProxyType — mutation raises TypeError.
    NEVER serialized to snapshots. Re-fetched fresh on resume.

    Build pattern:
        track = PersonalTrack()
        track.add_field(PersonalField(...))   # during INITIALIZE
        track.set_voice_profile({...})        # during INITIALIZE
        track.seal()                          # before Phase 4 EXECUTE
        # track is now read-only
    """

    def __init__(self):
        self._fields: dict[str, PersonalField] = {}
        self._voice_profile: dict[str, str] = {}
        self._life_context: dict[str, str] = {}
        self._source_versions: dict[str, str] = {}
        self._sealed = False

    # -- Build methods (called during INITIALIZE, before seal) ----------------

    def add_field(self, f: PersonalField) -> None:
        if self._sealed:
            raise RuntimeError(
                "PersonalTrack is sealed — cannot modify after INITIALIZE"
            )
        self._fields[f.field_name] = f

    def set_voice_profile(self, profile: dict[str, str]) -> None:
        if self._sealed:
            raise RuntimeError("PersonalTrack is sealed")
        self._voice_profile = profile

    def set_life_context(self, context: dict[str, str]) -> None:
        if self._sealed:
            raise RuntimeError("PersonalTrack is sealed")
        self._life_context = context

    def set_source_versions(self, versions: dict[str, str]) -> None:
        if self._sealed:
            raise RuntimeError("PersonalTrack is sealed")
        self._source_versions = versions

    def seal(self) -> None:
        """
        Seal the track after INITIALIZE. Wraps all dicts in MappingProxyType.
        Called by lifecycle.py before Phase 4 EXECUTE begins.
        Cannot be unsealed.
        """
        self._fields = MappingProxyType(self._fields)
        self._voice_profile = MappingProxyType(self._voice_profile)
        self._life_context = MappingProxyType(self._life_context)
        self._source_versions = MappingProxyType(self._source_versions)
        self._sealed = True

    # -- Read methods (safe to call at any time) ------------------------------

    @property
    def fields(self):
        return self._fields

    @property
    def voice_profile(self):
        return self._voice_profile

    @property
    def life_context(self):
        return self._life_context

    @property
    def source_versions(self):
        return self._source_versions

    @property
    def is_sealed(self) -> bool:
        return self._sealed

    def get_field(self, field_name: str) -> PersonalField | None:
        """Returns a deep copy — prevents downstream mutation of stored field."""
        f = self._fields.get(field_name)
        return copy.deepcopy(f) if f else None

    def fields_for_source(self, source_id: str) -> list[PersonalField]:
        """Returns deep copies — prevents field value leakage via shared reference."""
        return [
            copy.deepcopy(f) for f in self._fields.values()
            if f.source_id == source_id
        ]

    def max_sensitivity_severity(self) -> int:
        """Return the highest sensitivity severity across all loaded fields."""
        if not self._fields:
            return 0
        return max(f.sensitivity_severity for f in self._fields.values())


# -- Personal Context Manifest ------------------------------------------------

@dataclass
class PersonalContextManifest:
    """
    Checkpoint metadata — what was active in PersonalTrack at snapshot time.
    Stored in focus_run_snapshots.personal_context_manifest (JSON).
    Used during resume to detect changes in personal.db since last checkpoint.

    Contains field names, SHA-256 content hashes, and source versions.
    NEVER contains field values.
    """
    field_names: list[str] = field(default_factory=list)
    field_hashes: dict[str, str] = field(default_factory=dict)
    source_versions: dict[str, str] = field(default_factory=dict)
    snapshot_taken_at: str = ""

    def matches(self, current: PersonalTrack) -> bool:
        """
        Compare this manifest to the current PersonalTrack.
        Returns True only if field names, content hashes, AND source
        versions all match.
        """
        if sorted(current.fields.keys()) != sorted(self.field_names):
            return False
        if current.source_versions != self.source_versions:
            return False
        for name, f in current.fields.items():
            if self.field_hashes.get(name) != f.compute_content_hash():
                return False
        return True

    @classmethod
    def from_personal_track(
        cls, track: PersonalTrack, snapshot_taken_at: str
    ) -> PersonalContextManifest:
        """Build a manifest from the current PersonalTrack."""
        return cls(
            field_names=sorted(track.fields.keys()),
            field_hashes={
                name: f.compute_content_hash()
                for name, f in track.fields.items()
            },
            source_versions=dict(track.source_versions),
            snapshot_taken_at=snapshot_taken_at,
        )


# -- Task Step ----------------------------------------------------------------

@dataclass(frozen=True)
class TaskStep:
    """
    A single completed step's output, held in TaskTrack.
    Immutable after creation — frozen=True prevents post-execution mutation.
    """
    step_id: str
    output_var: str
    content: str
    sensitivity_severity: int = 1
    routing_tier_used: int = 1


# -- Task Track ---------------------------------------------------------------

@dataclass
class TaskTrack:
    """
    Accumulates step outputs during focus execution.

    steps: ordered audit trail of all completed steps (canonical source).
    output_vars: {var_name: content} cache for O(1) template injection lookup.
    sensitivity_ceiling: highest severity seen, escalates monotonically.

    add_step() is the ONLY writer for both steps and output_vars.
    """
    steps: list[TaskStep] = field(default_factory=list)
    sensitivity_ceiling: int = 1
    output_vars: dict[str, str] = field(default_factory=dict)

    def add_step(self, step: TaskStep) -> None:
        """Add a completed step and update sensitivity ceiling and output cache."""
        self.steps.append(step)
        if step.output_var:
            self.output_vars[step.output_var] = step.content
        self.update_sensitivity_ceiling(step.sensitivity_severity)

    def update_sensitivity_ceiling(self, severity: int) -> None:
        """
        Raise the sensitivity ceiling if severity is higher.
        The ceiling never decreases — monotonic invariant.
        """
        if severity > self.sensitivity_ceiling:
            self.sensitivity_ceiling = severity

    def get_output(self, output_var: str) -> str | None:
        """O(1) lookup of a previous step's output by variable name."""
        return self.output_vars.get(output_var)

    def last_output(self) -> str | None:
        """The most recent step's content, or None."""
        return self.steps[-1].content if self.steps else None


# -- Promoted Content Entry ---------------------------------------------------

@dataclass(frozen=True)
class PromotedContentEntry:
    """
    A single cross-tier content promotion approved by PG_GATE_3.
    Immutable record — provides audit trail for all promotions.
    """
    step_id: str
    content_key: str
    content: str


# -- Shared State Track -------------------------------------------------------

@dataclass
class SharedStateTrack:
    """
    Holds content approved for cross-tier use via PG_GATE_3.

    step_disclosure_buffers: written by PG_GATE_1, read by Step 8 for Tier 2+.
      Contains abstracted field values only — raw personal values NEVER here.

    promotions: append-only list of PG_GATE_3 approved content entries.
    """
    step_disclosure_buffers: dict[str, dict[str, str]] = field(
        default_factory=dict
    )
    promotions: list[PromotedContentEntry] = field(default_factory=list)

    def write_disclosure_buffer(
        self, step_id: str, approved_fields: dict[str, str]
    ) -> None:
        """
        Called by PG_GATE_1. Writes approved/abstracted field values
        for a specific step. Raw values must never be passed here.

        REPLACEMENT semantics — not merge. Any prior entry for this step_id
        is overwritten atomically.
        """
        self.step_disclosure_buffers[step_id] = approved_fields

    def read_disclosure_buffer(self, step_id: str) -> dict[str, str]:
        """
        Called by Step 8 for Tier 2+ prompt assembly.
        Returns abstracted field values for this step, or {} if absent.
        NEVER falls back to PersonalTrack.
        """
        return self.step_disclosure_buffers.get(step_id, {})

    def promote_content(
        self, step_id: str, content_key: str, content: str
    ) -> None:
        """
        Called by PG_GATE_3 after cross-tier approval.
        Appends to promotions list — never overwrites existing entries.
        """
        self.promotions.append(
            PromotedContentEntry(
                step_id=step_id,
                content_key=content_key,
                content=content,
            )
        )

    def get_promoted(self, content_key: str) -> str | None:
        """Return the most recent promoted content for a given key."""
        for entry in reversed(self.promotions):
            if entry.content_key == content_key:
                return entry.content
        return None
