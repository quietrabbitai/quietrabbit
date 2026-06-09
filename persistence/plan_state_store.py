# persistence/plan_state_store.py
# Plan State CRUD for plan_state.db.
# Per-user, per-life, per-focus, per-topic encrypted database.
# Path: /users/{user_id}/lives/{life_id}/focuses/{focus_id}/topics/{topic_id}/plan_state.db
#
# Source of truth declaration (ADR-013 Section 8.9):
#   outputs.db topics table = authoritative source of truth for topic metadata.
#   topic_header in plan_state.db = cache copy for offline coherence (backup/restore).
#   On conflict between topic_header and outputs.db, outputs.db governs.
#   topic_header updated by Phase 5A and Reconciliation Boot Check.
#
# Cross-database FK note: plan_state_blocks.focus_run_id references
#   focus_runs.id in outputs.db — cross-db FK by value only, application-enforced.
#   handoff_tokens.topic_id references topics.id in outputs.db — same.
#   SQLite cannot enforce FKs across separate database files.
#
# Block-level sensitivity model (ADR-013 Section 5.7):
#   Each block carries its own visibility_scope and sensitivity_preset.
#   A medical block does NOT elevate general research blocks.
#   Output blocks inherit highest sensitivity from dependency_refs lineage.
#   get_sensitivity_ceiling() computes the ceiling across referenced blocks.
#
# Connection lifecycle:
#   plan_state.db connections closed when topic becomes inactive.
#   QR_NETWORK_STORAGE=true uses journal_mode=DELETE via open_db() wrapper.
#   Never leave connections open idling over NAS.
#
# Soft ceiling notification (ADR-013 Section 3.3):
#   System surfaces a calm consolidation prompt when token estimate exceeds threshold.
#   NEVER automatically closes or consolidates — user controls response.
#
# Part of Phase B data model extension (D6-226+).

from __future__ import annotations

import json
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal

from providers.utils import now, open_db
from persistence.topic_store import (
    ensure_focus_dirs,
    get_plan_state_path,
    mark_storage_location_verified,
    mark_storage_location_orphaned,
)


# -- Tier ceiling constants ---------------------------------------------------

_ELIGIBLE_SCOPES_BY_TIER: dict[int, set[str]] = {
    1: {"tier_1_only", "anonymous_tier2", "tier2_permitted", "tier3_permitted"},
    2: {"anonymous_tier2", "tier2_permitted", "tier3_permitted"},
    3: {"tier3_permitted"},
}

DEFAULT_CEILING_THRESHOLD = 32000


# -- Dataclasses --------------------------------------------------------------

@dataclass
class PlanStateBlock:
    """Runtime representation of a plan_state_blocks row."""
    id: str
    block_type: str
    content: str
    visibility_scope: str
    transformation: str
    token_estimate: int
    inferred_by_system: bool
    focus_run_id: str
    created_at: str
    updated_at: str
    sensitivity_preset: str | None = None
    relevance_tags: list = field(default_factory=list)
    dependency_refs: list = field(default_factory=list)
    archived_at: str | None = None

    @classmethod
    def from_row(cls, row) -> PlanStateBlock:
        return cls(
            id=row["id"],
            block_type=row["block_type"],
            content=row["content"],
            visibility_scope=row["visibility_scope"],
            transformation=row["transformation"],
            sensitivity_preset=row["sensitivity_preset"],
            token_estimate=row["token_estimate"],
            inferred_by_system=bool(row["inferred_by_system"]),
            focus_run_id=row["focus_run_id"],
            created_at=row["created_at"],
            updated_at=row["updated_at"],
            relevance_tags=json.loads(row["relevance_tags"] or "[]"),
            dependency_refs=json.loads(row["dependency_refs"] or "[]"),
            archived_at=row["archived_at"],
        )


@dataclass
class TopicHeader:
    """
    Cache copy of topic metadata from outputs.db.
    outputs.db topics table is authoritative — this mirrors it.
    On conflict, outputs.db governs.
    """
    topic_id: str
    focus_id: str
    life_id: str
    placeholder_name: str
    lifecycle_state: str
    session_count: int
    created_at: str
    updated_at: str
    name: str | None = None
    current_phase: str | None = None

    @property
    def display_name(self) -> str:
        return self.name or self.placeholder_name


@dataclass
class StateCeilingStatus:
    """Runtime representation of the state_ceiling_status row."""
    current_token_estimate: int
    ceiling_threshold: int
    notification_sent_at: str | None = None
    user_response: str | None = None


# -- DB opener ----------------------------------------------------------------

def _open_plan_state_db(db_path: Path):
    """Open plan_state.db using the standard open_db() wrapper."""
    return open_db(db_path)


# -- Migration ----------------------------------------------------------------

def ensure_plan_state_db(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
) -> Path:
    """
    Ensure plan_state.db exists and is migrated for this topic.
    Creates the directory structure lazily if needed.
    Returns the database path.
    """
    from persistence.migrations import migrate_plan_state_db
    ensure_focus_dirs(user_id, life_id, focus_id, topic_id)
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    migrate_plan_state_db(user_id, life_id, focus_id, topic_id, key_hex)
    return db_path


# -- Topic header -------------------------------------------------------------

def get_topic_header(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
) -> TopicHeader | None:
    """
    Read the topic header cache copy from plan_state.db.
    Returns None if plan_state.db does not exist.
    Source of truth is outputs.db — this is a cache copy only.
    """
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    if not db_path.exists():
        return None

    with _open_plan_state_db(db_path) as db:
        row = db.execute(
            "SELECT topic_id, focus_id, life_id, name, placeholder_name, "
            "lifecycle_state, current_phase, session_count, created_at, updated_at "
            "FROM topic_header WHERE id = 1"
        ).fetchone()

    if not row:
        return None
    return TopicHeader(
        topic_id=row["topic_id"],
        focus_id=row["focus_id"],
        life_id=row["life_id"],
        name=row["name"],
        placeholder_name=row["placeholder_name"],
        lifecycle_state=row["lifecycle_state"],
        current_phase=row["current_phase"],
        session_count=row["session_count"],
        created_at=row["created_at"],
        updated_at=row["updated_at"],
    )


def initialise_topic_header(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
    name: str | None,
    placeholder_name: str,
) -> None:
    """
    Write the initial topic_header row on first plan_state.db creation.
    Uses INSERT OR IGNORE — safe to call multiple times.
    """
    timestamp = now()
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)

    with _open_plan_state_db(db_path) as db:
        db.execute(
            """INSERT OR IGNORE INTO topic_header
               (id, topic_id, focus_id, life_id, name, placeholder_name,
                lifecycle_state, session_count, created_at, updated_at)
               VALUES (1, ?, ?, ?, ?, ?, 'active', 0, ?, ?)""",
            [topic_id, focus_id, life_id, name, placeholder_name,
             timestamp, timestamp]
        )


def update_topic_header(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
    lifecycle_state: str | None = None,
    current_phase: str | None = None,
    name: str | None = None,
    increment_session: bool = False,
) -> None:
    """
    Update topic_header cache copy.
    Called by Phase 5A and Reconciliation Boot Check.
    Source of truth is outputs.db — this mirrors it.
    """
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    if not db_path.exists():
        return

    timestamp = now()
    with _open_plan_state_db(db_path) as db:
        if lifecycle_state is not None:
            db.execute(
                "UPDATE topic_header SET lifecycle_state = ?, updated_at = ? "
                "WHERE id = 1",
                [lifecycle_state, timestamp]
            )
        if current_phase is not None:
            db.execute(
                "UPDATE topic_header SET current_phase = ?, updated_at = ? "
                "WHERE id = 1",
                [current_phase, timestamp]
            )
        if name is not None:
            db.execute(
                "UPDATE topic_header SET name = ?, updated_at = ? WHERE id = 1",
                [name, timestamp]
            )
        if increment_session:
            db.execute(
                "UPDATE topic_header SET session_count = session_count + 1, "
                "updated_at = ? WHERE id = 1",
                [timestamp]
            )


# -- Block reads --------------------------------------------------------------

def get_eligible_blocks(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
    execution_tier: int,
    max_tokens: int | None = None,
    block_types: list[str] | None = None,
) -> list[PlanStateBlock]:
    """
    Retrieval Eligibility Check — pre-filter by visibility_scope vs tier ceiling.
    Returns non-archived blocks eligible for the given execution_tier,
    ordered by updated_at DESC (most recent first).
    Gate1 applies content-level abstraction policy separately.
    block_types: if provided, filters to specified types only.
    max_tokens: if provided, stops accumulating after budget is reached.
    """
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    if not db_path.exists():
        return []

    eligible_scopes = _ELIGIBLE_SCOPES_BY_TIER.get(execution_tier, set())
    if not eligible_scopes:
        return []

    placeholders = ",".join("?" * len(eligible_scopes))
    params: list = list(eligible_scopes)
    query = (
        f"SELECT id, block_type, content, visibility_scope, transformation, "
        f"sensitivity_preset, relevance_tags, token_estimate, dependency_refs, "
        f"inferred_by_system, focus_run_id, created_at, updated_at, archived_at "
        f"FROM plan_state_blocks "
        f"WHERE archived_at IS NULL "
        f"AND visibility_scope IN ({placeholders})"
    )

    if block_types:
        type_placeholders = ",".join("?" * len(block_types))
        query += f" AND block_type IN ({type_placeholders})"
        params.extend(block_types)

    query += " ORDER BY updated_at DESC"

    with _open_plan_state_db(db_path) as db:
        rows = db.execute(query, params).fetchall()

    blocks = []
    accumulated = 0
    for row in rows:
        block = PlanStateBlock.from_row(row)
        if max_tokens is not None:
            if accumulated + block.token_estimate > max_tokens:
                continue
            accumulated += block.token_estimate
        blocks.append(block)
    return blocks


def get_sensitivity_ceiling(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
    block_ids: list[str],
) -> str:
    """
    Compute the highest sensitivity_preset across a set of dependency block IDs.
    Used by output block sensitivity inheritance (ADR-013 Section 5.7).
    Sensitive data cannot be laundered through iterative summarisation.
    Returns 'standard' if no blocks found or no sensitivity set.
    preset ordering: standard < sensitive < private < locked.
    """
    if not block_ids:
        return "standard"

    preset_rank = {"standard": 0, "sensitive": 1, "private": 2, "locked": 3}
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    if not db_path.exists():
        return "standard"

    placeholders = ",".join("?" * len(block_ids))
    with _open_plan_state_db(db_path) as db:
        rows = db.execute(
            f"SELECT sensitivity_preset FROM plan_state_blocks "
            f"WHERE id IN ({placeholders}) AND sensitivity_preset IS NOT NULL",
            block_ids
        ).fetchall()

    if not rows:
        return "standard"

    max_rank = max(preset_rank.get(row["sensitivity_preset"], 0) for row in rows)
    rank_to_preset = {v: k for k, v in preset_rank.items()}
    return rank_to_preset.get(max_rank, "standard")


# -- Block writes -------------------------------------------------------------

def write_block(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
    block_type: Literal[
        "decision", "research", "action_output", "user_note", "system_observation"
    ],
    content: str,
    focus_run_id: str,
    visibility_scope: str = "tier2_permitted",
    transformation: str = "generalize_ok",
    sensitivity_preset: str | None = "standard",
    relevance_tags: list | None = None,
    token_estimate: int = 0,
    dependency_refs: list | None = None,
    inferred_by_system: bool = True,
) -> str:
    """
    Write a plan state block. Returns the block id.
    Updates state_ceiling_status.current_token_estimate after write.

    Sensitivity inheritance invariant (ADR-013 Section 5.7):
    If dependency_refs provided, sensitivity_preset is overridden by the
    ceiling of referenced blocks if that ceiling is higher.
    Sensitive data cannot be laundered through iterative summarisation.
    """
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    block_id = str(uuid.uuid4())
    timestamp = now()
    tags_json = json.dumps(relevance_tags or [])
    refs_json = json.dumps(dependency_refs or [])

    # Sensitivity inheritance from dependency_refs lineage.
    if dependency_refs:
        inherited = get_sensitivity_ceiling(
            user_id, life_id, focus_id, topic_id, key_hex, dependency_refs
        )
        preset_rank = {"standard": 0, "sensitive": 1, "private": 2, "locked": 3}
        current_rank = preset_rank.get(sensitivity_preset or "standard", 0)
        inherited_rank = preset_rank.get(inherited, 0)
        if inherited_rank > current_rank:
            sensitivity_preset = inherited

    with _open_plan_state_db(db_path) as db:
        db.execute(
            """INSERT INTO plan_state_blocks
               (id, block_type, content, visibility_scope, transformation,
                sensitivity_preset, relevance_tags, token_estimate, dependency_refs,
                inferred_by_system, focus_run_id, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
            [block_id, block_type, content, visibility_scope, transformation,
             sensitivity_preset, tags_json, token_estimate, refs_json,
             1 if inferred_by_system else 0,
             focus_run_id, timestamp, timestamp]
        )
        db.execute(
            "UPDATE state_ceiling_status "
            "SET current_token_estimate = current_token_estimate + ? WHERE id = 1",
            [token_estimate]
        )
    return block_id


def archive_all_blocks(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
) -> int:
    """
    Archive all active blocks when a topic closes.
    Blocks retained — not deleted. User preference determines archive vs discard.
    Returns count of archived blocks.
    """
    timestamp = now()
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    if not db_path.exists():
        return 0

    with _open_plan_state_db(db_path) as db:
        result = db.execute(
            "UPDATE plan_state_blocks SET archived_at = ? "
            "WHERE archived_at IS NULL",
            [timestamp]
        )
    return result.rowcount


# -- Handoff tokens -----------------------------------------------------------

def create_handoff_token(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
    focus_run_id: str,
    action_id: str,
    expiry_at: str,
    expected_return_schema: dict | None = None,
) -> str:
    """
    Create a handoff token for an Awaiting state dependency.
    Awaiting invariant: topic_id must be non-null — enforced by callers.
    topic_id stored on token for explicit ownership and Boot Check reconciliation.
    Cross-db FK: topic_id references topics.id in outputs.db — app-enforced.
    Returns the token id.
    """
    token_id = str(uuid.uuid4())
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    schema_json = json.dumps(expected_return_schema or {})

    with _open_plan_state_db(db_path) as db:
        db.execute(
            """INSERT INTO handoff_tokens
               (id, topic_id, focus_run_id, action_id,
                expected_return_schema, created_at, expiry_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)""",
            [token_id, topic_id, focus_run_id, action_id,
             schema_json, now(), expiry_at]
        )
    return token_id


def consume_handoff_token(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
    token_id: str,
) -> bool:
    """
    Mark a handoff token as consumed after a valid return result.
    Returns True if found and consumed, False if not found or already consumed.
    """
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    if not db_path.exists():
        return False

    with _open_plan_state_db(db_path) as db:
        result = db.execute(
            """UPDATE handoff_tokens SET consumed_at = ?
               WHERE id = ? AND consumed_at IS NULL AND expired_at IS NULL""",
            [now(), token_id]
        )
    return result.rowcount > 0


def expire_overdue_handoff_tokens(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
) -> int:
    """
    Called by Reconciliation Boot Check.
    Marks all overdue unconsumed tokens as expired.
    Returns count of expired tokens.
    Boot Check then transitions topic to paused, reason: dependency_timeout.
    Dashboard shows: "External update timed out — retry or resume manually."
    """
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    if not db_path.exists():
        return 0

    timestamp = now()
    with _open_plan_state_db(db_path) as db:
        result = db.execute(
            """UPDATE handoff_tokens SET expired_at = ?
               WHERE consumed_at IS NULL AND expired_at IS NULL
               AND expiry_at < ?""",
            [timestamp, timestamp]
        )
    return result.rowcount


# -- Soft ceiling -------------------------------------------------------------

def get_state_ceiling_status(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
) -> StateCeilingStatus | None:
    """
    Read the soft ceiling notification state.
    Returns None if plan_state.db does not exist.
    System NEVER automatically closes or consolidates — user controls response.
    """
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    if not db_path.exists():
        return None

    with _open_plan_state_db(db_path) as db:
        row = db.execute(
            "SELECT current_token_estimate, ceiling_threshold, "
            "notification_sent_at, user_response "
            "FROM state_ceiling_status WHERE id = 1"
        ).fetchone()

    if not row:
        return None
    return StateCeilingStatus(
        current_token_estimate=row["current_token_estimate"],
        ceiling_threshold=row["ceiling_threshold"],
        notification_sent_at=row["notification_sent_at"],
        user_response=row["user_response"],
    )


def record_ceiling_notification_sent(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
) -> None:
    """Record that the soft ceiling consolidation prompt was surfaced to the user."""
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    if not db_path.exists():
        return
    with _open_plan_state_db(db_path) as db:
        db.execute(
            "UPDATE state_ceiling_status SET notification_sent_at = ?, "
            "user_response = NULL WHERE id = 1",
            [now()]
        )


def record_ceiling_user_response(
    user_id: str,
    life_id: str,
    focus_id: str,
    topic_id: str,
    key_hex: str,
    response: Literal["consolidate", "continue"],
) -> None:
    """Record the user's response to the soft ceiling consolidation prompt."""
    db_path = get_plan_state_path(user_id, life_id, focus_id, topic_id)
    if not db_path.exists():
        return
    with _open_plan_state_db(db_path) as db:
        db.execute(
            "UPDATE state_ceiling_status SET user_response = ? WHERE id = 1",
            [response]
        )
