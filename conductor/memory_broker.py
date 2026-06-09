# conductor/memory_broker.py
# Memory Broker — sole interface between the Conductor and memory stores.
# The Conductor never queries domain_context_store or plan_state_store directly.
#
# Called at Phase 3 INITIALIZE via assemble_context().
# Returns a ContextSlice the Conductor uses for prompt assembly.
# Lifecycle merges PersonalTrack + ContextSlice — Broker never touches PersonalTrack.
#
# Three retrieval tiers (ADR-013 Section 5.3):
#   Tier A — always loaded: Domain Context standing summary (hard token ceiling).
#             Quick Ask: Tier A skipped — no Domain Context loaded.
#   Tier B — session-loaded: domain_context_blocks + plan_state_blocks within
#             remaining budget after Tier A. Unified budget pool — DC first,
#             then PS against whatever remains. Only when topic_id is non-null.
#   Tier C — on-demand: stub only in Phase B. NotImplementedError.
#             Layer 7 wires mid-session retrieval.
#
# Budget ownership: the Broker owns all budget computation (ADR-013 Section 5.8).
#   Input: model_context_window (raw context window of selected model).
#   Optional overrides: tier_a_ceiling, reserve_margin (env var defaults).
#   Tier B budget: remaining after Tier A actual usage + reserve margin.
#   Unused Tier A budget rolls over to Tier B — no fixed partitioning.
#
# Retrieval Eligibility Check (ADR-013 Section 5.6):
#   Pre-filters blocks by visibility_scope vs execution_tier ceiling.
#   Distinct from Gate1 — structural access control only, not privacy policy.
#   Gate1 remains the single abstraction authority.
#
# Isolation invariant (ADR-013 Section 8.9):
#   Context hard-locked to (life_id, focus_id, topic_id).
#   No cross-topic, cross-focus, or cross-life access.
#   Non-empty string assertions applied before any retrieval — fail closed.
#
# Quick Ask invariant (ADR-013 Section 9.7):
#   Quick Ask runs load Profile only — no Domain Context, no Plan State.
#   is_quick_ask=True → Tier A and Tier B both skipped.
#   Phase 7 CLEANUP: zero-trace wipe of volatile block cache.
#
# highest_sensitivity on ContextSlice:
#   Advisory metadata only — not used for routing or Gate1 decisions.
#   Gate1 operates at block level. One locked block does not make the
#   entire context locked for routing. For diagnostics and logging only.
#
# Connection lifecycle:
#   domain_context.db and plan_state.db connections closed after assemble_context().
#   QR_NETWORK_STORAGE=true uses journal_mode=DELETE via open_db() wrapper.
#
# Part of Phase B data model extension (D6-226+).

from __future__ import annotations

import logging
import os
from dataclasses import dataclass, field

from providers.utils import now

log = logging.getLogger(__name__)


# -- Budget constants ---------------------------------------------------------

_DEFAULT_RESERVE_MARGIN: float = float(
    os.environ.get("QR_MEMORY_RESERVE_MARGIN", "0.15")
)

_DEFAULT_TIER_A_CEILING: int = int(
    os.environ.get("QR_TIER_A_TOKEN_CEILING", "512")
)

# Sensitivity preset ordering for ceiling computation.
# Unknown presets default to rank 3 (locked) — fail closed on unrecognised values.
_PRESET_RANK: dict[str, int] = {
    "standard": 0, "sensitive": 1, "private": 2, "locked": 3
}
_RANK_TO_PRESET: dict[int, str] = {v: k for k, v in _PRESET_RANK.items()}


# -- MemoryBlock --------------------------------------------------------------

@dataclass
class MemoryBlock:
    """
    A single retrieved memory block returned to the Conductor.

    source_type:     "domain_context" | "plan_state"
    block_type:      block_type from plan_state_blocks, or "knowledge" for
                     domain context blocks.
    retrieval_tier:  "tier_a" | "tier_b"
    source_topic_id: provenance — which topic this block originated from.
                     None for standing summary (Tier A) or if not applicable.
    dependency_refs: provenance — block IDs this block was derived from.
                     Carries sensitivity inheritance lineage for audit.

    The Conductor receives MemoryBlocks — never opens stores directly.
    Provenance fields (source_topic_id, dependency_refs) are available to
    the system for audit and retrospective — not injected into prompts.
    """
    block_id: str
    content: str
    source_type: str
    block_type: str
    visibility_scope: str
    sensitivity_preset: str
    token_estimate: int
    retrieval_tier: str
    source_topic_id: str | None = None
    dependency_refs: list = field(default_factory=list)


# -- ContextSlice -------------------------------------------------------------

@dataclass
class ContextSlice:
    """
    Assembled memory context for one focus run session.
    Returned by MemoryBroker.assemble_context() to lifecycle at Phase 3 INITIALIZE.
    Lifecycle merges this with PersonalTrack — never the Broker's job.

    retrieved_blocks:      unified list across all sources and tiers.
    token_budget_used:     actual tokens consumed by retrieved blocks.
    token_budget_remaining: tokens available for prompt + user input + output.
    highest_sensitivity:   ADVISORY ONLY — highest sensitivity_preset seen
                           across retrieved blocks. Not used for routing or
                           Gate1 decisions. Gate1 operates at block level.
                           For diagnostics and logging only.
    loaded_block_count:    blocks successfully loaded within budget.
    deferred_block_count:  eligible blocks that exceeded budget (not loaded).
    retrieval_timestamp:   when assemble_context() ran — for manifest delta.
    active_topic_id:       the topic loaded, or None for unnamed/Quick Ask runs.
    """
    retrieved_blocks: list[MemoryBlock] = field(default_factory=list)
    token_budget_used: int = 0
    token_budget_remaining: int = 0
    highest_sensitivity: str = "standard"
    loaded_block_count: int = 0
    deferred_block_count: int = 0
    retrieval_timestamp: str = ""
    active_topic_id: str | None = None

    def render(self) -> str:
        """
        Serialise retrieved block contents into a prompt-injectable string.
        Called by lifecycle to populate life_context in prompt assembly.
        Blocks ordered: Tier A first (standing summary), then Tier B by source.
        Empty string returned if no blocks retrieved — graceful degradation.
        Conductor calls this — never joins block contents manually.
        """
        if not self.retrieved_blocks:
            return ""

        tier_a = [b for b in self.retrieved_blocks if b.retrieval_tier == "tier_a"]
        tier_b = [b for b in self.retrieved_blocks if b.retrieval_tier == "tier_b"]

        parts: list[str] = []

        if tier_a:
            parts.append("\n\n".join(b.content for b in tier_a if b.content))

        if tier_b:
            dc_blocks = [b for b in tier_b if b.source_type == "domain_context"]
            ps_blocks = [b for b in tier_b if b.source_type == "plan_state"]
            if dc_blocks:
                parts.append("\n\n".join(b.content for b in dc_blocks if b.content))
            if ps_blocks:
                parts.append("\n\n".join(b.content for b in ps_blocks if b.content))

        return "\n\n---\n\n".join(p for p in parts if p).strip()

    def clear(self) -> None:
        """
        Explicitly zero block content from memory.
        Called by lifecycle immediately after render() to avoid retaining
        a second in-memory copy of sensitive context for the session duration.
        del slice_ alone is insufficient — Python GC is not deterministic.
        After clear(), the slice is unusable — render() returns empty string.
        """
        self.retrieved_blocks = []

    def _update_highest_sensitivity(self, preset: str | None) -> None:
        """
        Raise the sensitivity ceiling if the new preset is higher.
        Unknown presets default to rank 3 (locked) — fail closed.
        highest_sensitivity is ADVISORY ONLY — not used for routing or Gate1.
        """
        if preset is None:
            return
        current_rank = _PRESET_RANK.get(self.highest_sensitivity, 0)
        # Unknown preset defaults to 3 (locked) — fail closed on unrecognised values.
        new_rank = _PRESET_RANK.get(preset, 3)
        if new_rank > current_rank:
            self.highest_sensitivity = _RANK_TO_PRESET.get(new_rank, "locked")


# -- MemoryBroker -------------------------------------------------------------

class MemoryBroker:
    """
    Sole interface between the Conductor and memory stores.
    Stateless — safe to instantiate per FocusRun in lifecycle Phase 3.
    Conductor never imports domain_context_store or plan_state_store directly.

    Isolation invariant: all reads scoped to (life_id, focus_id, topic_id).
    No cross-topic, cross-focus, or cross-life access.
    Non-empty string assertions applied before retrieval — fail closed.
    """

    def assemble_context(
        self,
        user_id: str,
        life_id: str,
        focus_id: str,
        topic_id: str | None,
        key_hex: str,
        execution_tier: int,
        model_context_window: int,
        is_quick_ask: bool = False,
        tier_a_ceiling: int | None = None,
        reserve_margin: float | None = None,
    ) -> ContextSlice:
        """
        Assemble the memory context slice for a focus run session.
        Called at Phase 3 INITIALIZE.

        Budget computation (Broker-owned, ADR-013 Section 5.8):
            reserve = model_context_window * reserve_margin
            available = model_context_window - reserve
            tier_a_used = min(actual standing summary tokens, tier_a_ceiling)
            tier_b_budget = available - tier_a_used
            Unused Tier A budget rolls over to Tier B automatically.

        tier_a_ceiling: overrides QR_TIER_A_TOKEN_CEILING env var if provided.
        reserve_margin: overrides QR_MEMORY_RESERVE_MARGIN env var if provided.

        Quick Ask invariant: is_quick_ask=True → returns empty ContextSlice.
            No Domain Context, no Plan State loaded.

        topic_id=None and not is_quick_ask → unnamed run.
            Tier A (standing summary) loaded if domain_context.db exists.
            Tier B skipped — no active topic.
        """
        # Isolation assertions — fail closed before any retrieval.
        assert user_id, "MemoryBroker: user_id must be non-empty"
        assert life_id, "MemoryBroker: life_id must be non-empty"
        assert focus_id, "MemoryBroker: focus_id must be non-empty"

        timestamp = now()
        slice_ = ContextSlice(
            retrieval_timestamp=timestamp,
            active_topic_id=topic_id,
        )

        _tier_a_ceiling = tier_a_ceiling if tier_a_ceiling is not None \
            else _DEFAULT_TIER_A_CEILING
        _reserve_margin = reserve_margin if reserve_margin is not None \
            else _DEFAULT_RESERVE_MARGIN

        # Quick Ask invariant — no memory context loaded.
        if is_quick_ask:
            log.debug(
                "memory_broker: Quick Ask run — skipping all context retrieval "
                "focus=%s", focus_id
            )
            reserve = int(model_context_window * _reserve_margin)
            slice_.token_budget_remaining = model_context_window - reserve
            return slice_

        # Budget computation.
        reserve = int(model_context_window * _reserve_margin)
        available = model_context_window - reserve

        # -- Tier A: Domain Context standing summary --------------------------
        tier_a_used = self._load_tier_a(
            slice_=slice_,
            user_id=user_id,
            life_id=life_id,
            focus_id=focus_id,
            key_hex=key_hex,
            tier_a_ceiling=_tier_a_ceiling,
        )

        # Unused Tier A budget rolls over to Tier B — no fixed partitioning.
        tier_b_budget = available - tier_a_used
        slice_.token_budget_used = tier_a_used

        # -- Tier B: session-loaded blocks (only when topic is active) --------
        if topic_id is not None and tier_b_budget > 0:
            self._load_tier_b(
                slice_=slice_,
                user_id=user_id,
                life_id=life_id,
                focus_id=focus_id,
                topic_id=topic_id,
                key_hex=key_hex,
                execution_tier=execution_tier,
                budget=tier_b_budget,
            )

        slice_.token_budget_remaining = (
            model_context_window - reserve - slice_.token_budget_used
        )
        slice_.loaded_block_count = len(slice_.retrieved_blocks)

        log.debug(
            "memory_broker: assembled context — focus=%s topic=%s "
            "blocks=%d deferred=%d budget_used=%d budget_remaining=%d "
            "highest_sensitivity=%s tier=%d",
            focus_id, topic_id,
            slice_.loaded_block_count, slice_.deferred_block_count,
            slice_.token_budget_used, slice_.token_budget_remaining,
            slice_.highest_sensitivity, execution_tier,
        )

        return slice_

    # -- Tier A ---------------------------------------------------------------

    def _load_tier_a(
        self,
        slice_: ContextSlice,
        user_id: str,
        life_id: str,
        focus_id: str,
        key_hex: str,
        tier_a_ceiling: int,
    ) -> int:
        """
        Load the Domain Context standing summary as a Tier A block.
        Returns actual tokens consumed — may be less than ceiling if summary is small.
        Unused Tier A budget rolls over to Tier B automatically.
        Returns 0 if domain_context.db does not exist or summary is empty.
        """
        try:
            from persistence.domain_context_store import get_standing_summary
        except ImportError:
            return 0

        summary = get_standing_summary(user_id, life_id, focus_id, key_hex)
        if summary is None or not summary.content:
            log.debug(
                "memory_broker: no standing summary for focus=%s "
                "(db missing or empty) — full Tier A budget rolls to Tier B",
                focus_id
            )
            return 0

        # Charge actual token_count up to ceiling — not a fixed ceiling charge.
        tokens_charged = min(summary.token_count, tier_a_ceiling)

        if summary.token_count > tier_a_ceiling:
            log.debug(
                "memory_broker: standing summary truncated by Tier A ceiling "
                "focus=%s actual=%d ceiling=%d charged=%d",
                focus_id, summary.token_count, tier_a_ceiling, tokens_charged
            )

        block = MemoryBlock(
            block_id="standing_summary",
            content=summary.content,
            source_type="domain_context",
            block_type="knowledge",
            visibility_scope="tier2_permitted",
            sensitivity_preset="standard",
            token_estimate=tokens_charged,
            retrieval_tier="tier_a",
            source_topic_id=None,
            dependency_refs=[],
        )
        slice_.retrieved_blocks.append(block)
        slice_._update_highest_sensitivity("standard")

        return tokens_charged

    # -- Tier B ---------------------------------------------------------------

    def _load_tier_b(
        self,
        slice_: ContextSlice,
        user_id: str,
        life_id: str,
        focus_id: str,
        topic_id: str,
        key_hex: str,
        execution_tier: int,
        budget: int,
    ) -> None:
        """
        Load Tier B blocks from both domain context and plan state.
        Uses a unified remaining budget — DC blocks loaded first, then PS
        blocks consume whatever remains. No hard 50/50 partition.
        This ensures unused DC budget naturally shifts to PS and vice versa.
        Retrieval Eligibility Check applied by each store's get_eligible_blocks().
        """
        try:
            from persistence.domain_context_store import (
                get_eligible_blocks as get_dc_blocks,
            )
            dc_blocks_available = get_dc_blocks(
                user_id=user_id,
                life_id=life_id,
                focus_id=focus_id,
                key_hex=key_hex,
                execution_tier=execution_tier,
                max_tokens=None,
            )
        except ImportError:
            dc_blocks_available = []

        try:
            from persistence.plan_state_store import (
                get_eligible_blocks as get_ps_blocks,
            )
            ps_blocks_available = get_ps_blocks(
                user_id=user_id,
                life_id=life_id,
                focus_id=focus_id,
                topic_id=topic_id,
                key_hex=key_hex,
                execution_tier=execution_tier,
                max_tokens=None,
            )
        except ImportError:
            ps_blocks_available = []

        tokens_loaded = 0
        deferred = 0

        # Domain context blocks first against unified budget.
        for dc_block in dc_blocks_available:
            if tokens_loaded + dc_block.token_estimate > budget:
                deferred += 1
                log.debug(
                    "memory_broker: DC block deferred — excluded_reason=budget "
                    "block=%s token_estimate=%d remaining=%d",
                    dc_block.id, dc_block.token_estimate,
                    budget - tokens_loaded,
                )
                continue
            block = MemoryBlock(
                block_id=dc_block.id,
                content=dc_block.content,
                source_type="domain_context",
                block_type="knowledge",
                visibility_scope=dc_block.visibility_scope,
                sensitivity_preset=dc_block.sensitivity_preset,
                token_estimate=dc_block.token_estimate,
                retrieval_tier="tier_b",
                source_topic_id=dc_block.source_topic_id,
                dependency_refs=dc_block.dependency_refs,
            )
            slice_.retrieved_blocks.append(block)
            slice_._update_highest_sensitivity(dc_block.sensitivity_preset)
            tokens_loaded += dc_block.token_estimate

        # Plan state blocks against whatever budget remains after DC.
        for ps_block in ps_blocks_available:
            if tokens_loaded + ps_block.token_estimate > budget:
                deferred += 1
                log.debug(
                    "memory_broker: PS block deferred — excluded_reason=budget "
                    "block=%s token_estimate=%d remaining=%d",
                    ps_block.id, ps_block.token_estimate,
                    budget - tokens_loaded,
                )
                continue
            block = MemoryBlock(
                block_id=ps_block.id,
                content=ps_block.content,
                source_type="plan_state",
                block_type=ps_block.block_type,
                visibility_scope=ps_block.visibility_scope,
                sensitivity_preset=ps_block.sensitivity_preset or "standard",
                token_estimate=ps_block.token_estimate,
                retrieval_tier="tier_b",
                source_topic_id=None,
                dependency_refs=ps_block.dependency_refs,
            )
            slice_.retrieved_blocks.append(block)
            slice_._update_highest_sensitivity(ps_block.sensitivity_preset)
            tokens_loaded += ps_block.token_estimate

        slice_.token_budget_used += tokens_loaded
        slice_.deferred_block_count += deferred

    # -- Tier C stub ----------------------------------------------------------

    def retrieve_additional_context(
        self,
        user_id: str,
        life_id: str,
        focus_id: str,
        topic_id: str,
        key_hex: str,
        query: str,
        execution_tier: int,
        max_tokens: int,
    ) -> list[MemoryBlock]:
        """
        Tier C — on-demand mid-session retrieval.
        Stub in Phase B. Layer 7 wires full implementation.
        Tier C blocks are task-scoped and discarded after task completion
        unless promoted to Plan State (ADR-013 Section 5.3).
        """
        raise NotImplementedError(
            "Tier C on-demand retrieval is not implemented in Phase B. "
            "Wire in Layer 7 when conductor-brief focus builds begin."
        )
