# persistence/domain_context_store.py
# Domain Context CRUD for domain_context.db.
# Per-user, per-life, per-focus encrypted database.
# Path: /users/{user_id}/personas/{persona_id}/focuses/{focus_id}/domain_context.db
#
# Extraction authority invariant (ADR-013 Section 6.6):
#   The generalisation pass is the ONLY mechanism that writes Domain Context.
#   Users review and approve extraction cards — they do not author blocks directly.
#   All writes: Phase 5B → pending_extractions → user review
#   → write_approved_extraction() → domain_context_blocks.
#
# Circular FK insertion order (see domain_context_001.sql):
#   1. INSERT provenance_log (approved_block_id = NULL)
#   2. INSERT domain_context_blocks (extraction_event_id = provenance_log.id)
#   3. UPDATE provenance_log SET approved_block_id = block.id
#   write_approved_extraction() enforces this atomically in one connection.
#
# Cross-database FK note: domain_context_blocks.source_topic_id references
#   topics.id in outputs.db — cross-db FK by value only, application-enforced.
#   SQLite cannot enforce FKs across separate database files.
#
# Retrieval Eligibility Check:
#   Pre-filter on visibility_scope vs tier ceiling before Gate1.
#   get_eligible_blocks() applies this filter.
#   Gate1 remains the single abstraction authority for content-level policy.
#
# Connection lifecycle:
#   Close domain_context.db connections when topic becomes inactive.
#   QR_NETWORK_STORAGE=true uses journal_mode=DELETE via open_db() wrapper.
#   Never leave connections open idling over NAS.
#
# Part of Phase B data model extension (D6-226+).
#
# Updated as part of Phase C Persona model migration (D6-298):
#   life_id → persona_id in all function signatures
#   Path: lives/{life_id} → personas/{persona_id}
#   No SQL column changes -- domain_context schema has no life_id column

from __future__ import annotations

import json
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal

from providers.utils import now, open_db
from persistence.topic_store import ensure_focus_dirs, get_domain_context_path


# -- Tier ceiling constants ---------------------------------------------------

# Visibility scopes eligible for each execution tier.
# Retrieval Eligibility Check applies this mapping — structural access control only.
# Gate1 applies content-level abstraction policy separately.
_ELIGIBLE_SCOPES_BY_TIER: dict[int, set[str]] = {
    1: {"tier_1_only", "anonymous_tier2", "tier2_permitted", "tier3_permitted"},
    2: {"anonymous_tier2", "tier2_permitted", "tier3_permitted"},
    3: {"tier3_permitted"},
}


# -- Dataclasses --------------------------------------------------------------

@dataclass
class DomainContextBlock:
    """Runtime representation of a domain_context_blocks row."""
    id: str
    content: str
    visibility_scope: str
    transformation: str
    sensitivity_preset: str
    source_topic_id: str
    extraction_event_id: str
    token_estimate: int
    inferred_by_system: bool
    standing_summary_eligible: bool
    created_at: str
    updated_at: str
    relevance_tags: list = field(default_factory=list)
    dependency_refs: list = field(default_factory=list)
    revoked_at: str | None = None
    revocation_reason: str | None = None

    @classmethod
    def from_row(cls, row) -> DomainContextBlock:
        return cls(
            id=row["id"],
            content=row["content"],
            visibility_scope=row["visibility_scope"],
            transformation=row["transformation"],
            sensitivity_preset=row["sensitivity_preset"],
            source_topic_id=row["source_topic_id"],
            extraction_event_id=row["extraction_event_id"],
            token_estimate=row["token_estimate"],
            inferred_by_system=bool(row["inferred_by_system"]),
            standing_summary_eligible=bool(row["standing_summary_eligible"]),
            created_at=row["created_at"],
            updated_at=row["updated_at"],
            relevance_tags=json.loads(row["relevance_tags"] or "[]"),
            dependency_refs=json.loads(row["dependency_refs"] or "[]"),
            revoked_at=row["revoked_at"],
            revocation_reason=row["revocation_reason"],
        )


@dataclass
class StandingSummary:
    """Runtime representation of the standing_summary row."""
    content: str
    token_count: int
    source_block_ids: list
    generated_at: str
    invalidated_at: str | None = None


@dataclass
class PendingExtraction:
    """Runtime representation of a pending_extractions row."""
    id: str
    source_topic_id: str
    source_focus_run_id: str
    proposed_content: str
    proposed_preset: str
    status: str
    created_at: str
    reviewed_at: str | None = None


# -- DB opener ----------------------------------------------------------------

def _open_domain_context_db(db_path: Path):
    """Open domain_context.db using the standard open_db() wrapper."""
    return open_db(db_path)


# -- Migration ----------------------------------------------------------------

def ensure_domain_context_db(
    user_id: str,
    persona_id: str,
    focus_id: str,
    key_hex: str,
) -> Path:
    """
    Ensure domain_context.db exists and is migrated.
    Creates the focus directory if needed (lazy initialisation).
    Returns the database path.
    """
    from persistence.migrations import migrate_domain_context_db
    ensure_focus_dirs(user_id, persona_id, focus_id)
    db_path = get_domain_context_path(user_id, persona_id, focus_id)
    migrate_domain_context_db(user_id, persona_id, focus_id, key_hex)
    return db_path


# -- Retrieval ----------------------------------------------------------------

def get_eligible_blocks(
    user_id: str,
    persona_id: str,
    focus_id: str,
    key_hex: str,
    execution_tier: int,
    max_tokens: int | None = None,
) -> list[DomainContextBlock]:
    """
    Retrieval Eligibility Check — pre-filter by visibility_scope vs tier ceiling.
    Returns non-revoked blocks eligible for the given execution_tier,
    ordered by token_estimate ASC (fits smallest first within budget).
    Gate1 applies content-level abstraction policy separately — this is
    structural access control only, not privacy policy.
    max_tokens: if provided, stops accumulating after budget is reached.
    """
    db_path = get_domain_context_path(user_id, persona_id, focus_id)
    if not db_path.exists():
        return []

    eligible_scopes = _ELIGIBLE_SCOPES_BY_TIER.get(execution_tier, set())
    if not eligible_scopes:
        return []

    placeholders = ",".join("?" * len(eligible_scopes))
    query = (
        f"SELECT id, content, visibility_scope, transformation, "
        f"sensitivity_preset, source_topic_id, extraction_event_id, "
        f"token_estimate, inferred_by_system, standing_summary_eligible, "
        f"relevance_tags, dependency_refs, revoked_at, revocation_reason, "
        f"created_at, updated_at "
        f"FROM domain_context_blocks "
        f"WHERE revoked_at IS NULL "
        f"AND visibility_scope IN ({placeholders}) "
        f"ORDER BY token_estimate ASC"
    )

    with _open_domain_context_db(db_path) as db:
        rows = db.execute(query, list(eligible_scopes)).fetchall()

    blocks = []
    accumulated = 0
    for row in rows:
        block = DomainContextBlock.from_row(row)
        if max_tokens is not None:
            if accumulated + block.token_estimate > max_tokens:
                continue
            accumulated += block.token_estimate
        blocks.append(block)
    return blocks


def get_standing_summary(
    user_id: str,
    persona_id: str,
    focus_id: str,
    key_hex: str,
) -> StandingSummary | None:
    """
    Fetch the current standing summary.
    Returns None if domain_context.db does not exist.
    invalidated_at non-null signals summary needs regeneration.
    Memory Broker reads this for Tier A always-loaded retrieval.
    Standing summary is a derived artifact — never authoritative.
    """
    db_path = get_domain_context_path(user_id, persona_id, focus_id)
    if not db_path.exists():
        return None

    with _open_domain_context_db(db_path) as db:
        row = db.execute(
            "SELECT content, token_count, source_block_ids, "
            "generated_at, invalidated_at FROM standing_summary WHERE id = 1"
        ).fetchone()

    if not row:
        return None
    return StandingSummary(
        content=row["content"],
        token_count=row["token_count"],
        source_block_ids=json.loads(row["source_block_ids"] or "[]"),
        generated_at=row["generated_at"],
        invalidated_at=row["invalidated_at"],
    )


def list_pending_extractions(
    user_id: str,
    persona_id: str,
    focus_id: str,
    key_hex: str,
) -> list[PendingExtraction]:
    """List all pending_review extraction cards for user review."""
    db_path = get_domain_context_path(user_id, persona_id, focus_id)
    if not db_path.exists():
        return []

    with _open_domain_context_db(db_path) as db:
        rows = db.execute(
            "SELECT id, source_topic_id, source_focus_run_id, proposed_content, "
            "proposed_preset, status, created_at, reviewed_at "
            "FROM pending_extractions WHERE status = 'pending_review' "
            "ORDER BY created_at ASC"
        ).fetchall()

    return [
        PendingExtraction(
            id=row["id"],
            source_topic_id=row["source_topic_id"],
            source_focus_run_id=row["source_focus_run_id"],
            proposed_content=row["proposed_content"],
            proposed_preset=row["proposed_preset"],
            status=row["status"],
            created_at=row["created_at"],
            reviewed_at=row["reviewed_at"],
        )
        for row in rows
    ]


# -- Write operations ---------------------------------------------------------

def write_pending_extraction(
    user_id: str,
    persona_id: str,
    focus_id: str,
    key_hex: str,
    source_topic_id: str,
    source_focus_run_id: str,
    proposed_content: str,
    proposed_preset: str = "standard",
) -> str:
    """
    Write a Phase 5B generalisation pass output to pending_extractions staging.
    Returns the pending extraction id.
    Purge aggressively after review — pending_extractions is a temporary cache.
    provenance_log is retained permanently (audit trail).
    """
    entry_id = str(uuid.uuid4())
    db_path = get_domain_context_path(user_id, persona_id, focus_id)

    with _open_domain_context_db(db_path) as db:
        db.execute(
            """INSERT INTO pending_extractions
               (id, source_topic_id, source_focus_run_id, proposed_content,
                proposed_preset, status, created_at)
               VALUES (?, ?, ?, ?, ?, 'pending_review', ?)""",
            [entry_id, source_topic_id, source_focus_run_id,
             proposed_content, proposed_preset, now()]
        )
    return entry_id


def write_approved_extraction(
    user_id: str,
    persona_id: str,
    focus_id: str,
    key_hex: str,
    pending_id: str,
    final_content: str,
    visibility_scope: str,
    transformation: str,
    sensitivity_preset: str,
    source_topic_id: str,
    source_focus_run_id: str,
    inferred_by_system: bool = True,
    relevance_tags: list | None = None,
    token_estimate: int = 0,
) -> str:
    """
    Write an approved extraction from pending_extractions to domain_context_blocks.

    Enforces circular FK insertion order (see file header):
      1. INSERT provenance_log (approved_block_id = NULL)
      2. INSERT domain_context_blocks (extraction_event_id = provenance_log.id)
      3. UPDATE provenance_log SET approved_block_id = block.id
    All three steps in one connection context — atomic.
    Invalidates standing summary after write.
    Returns the new domain_context_blocks.id.
    """
    timestamp = now()
    provenance_id = str(uuid.uuid4())
    block_id = str(uuid.uuid4())
    tags_json = json.dumps(relevance_tags or [])
    db_path = get_domain_context_path(user_id, persona_id, focus_id)

    with _open_domain_context_db(db_path) as db:
        # Step 1: insert provenance_log with approved_block_id = NULL
        db.execute(
            """INSERT INTO provenance_log
               (id, source_topic_id, source_focus_run_id, proposed_content,
                approval_action, edited_content, approved_block_id, approved_at)
               VALUES (?, ?, ?, ?, 'approved', ?, NULL, ?)""",
            [provenance_id, source_topic_id, source_focus_run_id,
             final_content,
             final_content if final_content else None,
             timestamp]
        )

        # Step 2: insert domain_context_blocks
        db.execute(
            """INSERT INTO domain_context_blocks
               (id, content, visibility_scope, transformation, sensitivity_preset,
                relevance_tags, token_estimate, dependency_refs,
                source_topic_id, extraction_event_id, inferred_by_system,
                standing_summary_eligible, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, '[]', ?, ?, ?, 1, ?, ?)""",
            [block_id, final_content, visibility_scope, transformation,
             sensitivity_preset, tags_json, token_estimate,
             source_topic_id, provenance_id,
             1 if inferred_by_system else 0,
             timestamp, timestamp]
        )

        # Step 3: update provenance_log with the new block id
        db.execute(
            "UPDATE provenance_log SET approved_block_id = ? WHERE id = ?",
            [block_id, provenance_id]
        )

        # Mark pending extraction as approved
        db.execute(
            "UPDATE pending_extractions SET status = 'approved', reviewed_at = ? "
            "WHERE id = ?",
            [timestamp, pending_id]
        )

        # Invalidate standing summary — regenerate_standing_summary() called separately
        db.execute(
            "UPDATE standing_summary SET invalidated_at = ? WHERE id = 1",
            [timestamp]
        )

    return block_id


def discard_pending_extraction(
    user_id: str,
    persona_id: str,
    focus_id: str,
    key_hex: str,
    pending_id: str,
    source_topic_id: str,
    source_focus_run_id: str,
    proposed_content: str,
) -> None:
    """
    Mark a pending extraction as discarded and write provenance record.
    pending_extractions row marked discarded (purged later).
    provenance_log entry written and retained permanently.
    """
    timestamp = now()
    provenance_id = str(uuid.uuid4())
    db_path = get_domain_context_path(user_id, persona_id, focus_id)

    with _open_domain_context_db(db_path) as db:
        db.execute(
            """INSERT INTO provenance_log
               (id, source_topic_id, source_focus_run_id, proposed_content,
                approval_action, edited_content, approved_block_id, approved_at)
               VALUES (?, ?, ?, ?, 'discarded', NULL, NULL, ?)""",
            [provenance_id, source_topic_id, source_focus_run_id,
             proposed_content, timestamp]
        )
        db.execute(
            "UPDATE pending_extractions SET status = 'discarded', reviewed_at = ? "
            "WHERE id = ?",
            [timestamp, pending_id]
        )


def revoke_block(
    user_id: str,
    persona_id: str,
    focus_id: str,
    key_hex: str,
    block_id: str,
    reason: Literal[
        "user_deleted", "superseded", "privacy_request", "classification_error"
    ],
) -> bool:
    """
    Revoke a domain_context_blocks entry.
    Sets revoked_at and revocation_reason — row is NOT deleted (audit trail).
    Provenance log entry retained.
    Invalidates standing summary after revocation.
    Returns True if found and revoked, False if not found or already revoked.
    """
    timestamp = now()
    db_path = get_domain_context_path(user_id, persona_id, focus_id)
    if not db_path.exists():
        return False

    with _open_domain_context_db(db_path) as db:
        result = db.execute(
            """UPDATE domain_context_blocks
               SET revoked_at = ?, revocation_reason = ?, updated_at = ?
               WHERE id = ? AND revoked_at IS NULL""",
            [timestamp, reason, timestamp, block_id]
        )
        revoked = result.rowcount > 0
        if revoked:
            db.execute(
                "UPDATE standing_summary SET invalidated_at = ? WHERE id = 1",
                [timestamp]
            )
    return revoked


def regenerate_standing_summary(
    user_id: str,
    persona_id: str,
    focus_id: str,
    key_hex: str,
    max_tokens: int = 2048,
) -> StandingSummary:
    """
    Regenerate the standing summary from all eligible blocks.
    Called after any domain_context_blocks write or revocation.
    Enforces max_tokens ceiling — fits as many eligible blocks as possible.
    standing_summary_eligible=1 blocks only.
    Standing summary is a derived artifact — never authoritative.
    Returns the new StandingSummary.
    """
    db_path = get_domain_context_path(user_id, persona_id, focus_id)
    timestamp = now()

    with _open_domain_context_db(db_path) as db:
        rows = db.execute(
            "SELECT id, content, token_estimate FROM domain_context_blocks "
            "WHERE revoked_at IS NULL AND standing_summary_eligible = 1 "
            "ORDER BY token_estimate ASC"
        ).fetchall()

        assembled: list[str] = []
        source_ids: list[str] = []
        total_tokens = 0

        for row in rows:
            if total_tokens + row["token_estimate"] > max_tokens:
                continue
            assembled.append(row["content"])
            source_ids.append(row["id"])
            total_tokens += row["token_estimate"]

        summary_content = "\n\n".join(assembled)
        source_ids_json = json.dumps(source_ids)

        db.execute(
            """UPDATE standing_summary SET content = ?, token_count = ?,
               source_block_ids = ?, generated_at = ?, invalidated_at = NULL
               WHERE id = 1""",
            [summary_content, total_tokens, source_ids_json, timestamp]
        )

    return StandingSummary(
        content=summary_content,
        token_count=total_tokens,
        source_block_ids=source_ids,
        generated_at=timestamp,
        invalidated_at=None,
    )
