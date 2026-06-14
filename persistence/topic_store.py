# persistence/topic_store.py
# Topic CRUD, run history, classification preferences, and topic storage
# location registry. All backed by outputs.db (per-user, per-life, encrypted).
#
# Responsibility boundary:
#   topic_store    — topic lifecycle state, run history, classification prefs,
#                    storage location registry, topic_index mirror writes
#   lifecycle.py   — focus_run state transitions (create, promote, status)
#   domain_context_store.py — domain_context.db reads and writes
#   plan_state_store.py     — plan_state.db reads and writes
#
# topic_index in shared.db (unencrypted) mirrors topic metadata for the
# Life dashboard. topic_store writes to both outputs.db and shared.db
# where possible. On conflict, outputs.db is authoritative.
#
# topic_storage_locations is a DISCOVERY INDEX and dashboard accelerator —
# NOT the authoritative source of truth for filesystem state.
# On backup restore or manual file recovery, filesystem existence takes
# precedence. Boot Check uses the registry as primary lookup, with a
# filesystem scan as fallback for orphan detection only.
#
# Cross-database FK note: references to topics(id) from child databases
# (plan_state.db) are cross-db FKs by value only — application-enforced,
# not SQLite-enforced. SQLite cannot enforce FKs across separate files.
#
# Scale assumption: Release 1 target <100 active topics per life.
# Revisit directory structure assumptions if this grows significantly.
#
# Part of Phase B data model extension (D6-226+).
#
# Updated as part of Phase C Persona model migration (D6-298):
#   life_id → persona_id throughout
#   Path: lives/{life_id} → personas/{persona_id} in all path functions
#   SQL: life_id column → persona_id in topics, run_history,
#        classification_preferences, topic_index
#   Topic/ClassificationPreference dataclasses: life_id → persona_id field

from __future__ import annotations

import json
import os
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone, timedelta
from pathlib import Path
from typing import Literal

from providers.utils import get_data_root, now, open_outputs_db, open_instance_db


# -- Constants ----------------------------------------------------------------

RUN_HISTORY_RETENTION_DAYS: int = int(
    os.environ.get("QR_RUN_HISTORY_RETENTION_DAYS", "90")
)

LIFECYCLE_STATES = Literal["active", "paused", "awaiting", "complete", "closed"]


# -- Directory helpers --------------------------------------------------------

def ensure_focus_dirs(
    user_id: str,
    persona_id: str,
    focus_id: str,
    topic_id: str | None = None,
) -> tuple[Path, Path | None]:
    """
    Lazily create the directory structure for a focus and optionally a topic.
    Called at Phase 3 INITIALIZE before opening domain_context.db or plan_state.db.
    Never called at init_db time — only on first use.

    Returns:
        (focus_dir, topic_dir)
        topic_dir is None if topic_id is not provided.

    Directory structure:
        users/{user_id}/lives/{persona_id}/focuses/{focus_id}/
        users/{user_id}/lives/{persona_id}/focuses/{focus_id}/topics/{topic_id}/
    """
    data_root = get_data_root()
    focus_dir = (
        data_root / "users" / user_id / "personas" / persona_id
        / "focuses" / focus_id
    )
    focus_dir.mkdir(parents=True, exist_ok=True)

    topic_dir: Path | None = None
    if topic_id is not None:
        topic_dir = focus_dir / "topics" / topic_id
        topic_dir.mkdir(parents=True, exist_ok=True)

    return focus_dir, topic_dir


def get_domain_context_path(
    user_id: str, persona_id: str, focus_id: str
) -> Path:
    """Return the expected path for a focus's domain_context.db."""
    data_root = get_data_root()
    return (
        data_root / "users" / user_id / "personas" / persona_id
        / "focuses" / focus_id / "domain_context.db"
    )


def get_plan_state_path(
    user_id: str, persona_id: str, focus_id: str, topic_id: str
) -> Path:
    """Return the expected path for a topic's plan_state.db."""
    data_root = get_data_root()
    return (
        data_root / "users" / user_id / "personas" / persona_id
        / "focuses" / focus_id / "topics" / topic_id / "plan_state.db"
    )


# -- Dataclasses --------------------------------------------------------------

@dataclass
class Topic:
    """Runtime representation of a topics row from outputs.db."""
    id: str
    focus_id: str
    user_id: str
    persona_id: str
    lifecycle_state: str
    placeholder_name: str
    created_at: str
    updated_at: str
    name: str | None = None
    dormant_since: str | None = None
    closed_at: str | None = None
    extra_metadata: dict = field(default_factory=dict)

    @property
    def display_name(self) -> str:
        """Resolved display name — user name if set, otherwise placeholder."""
        return self.name or self.placeholder_name

    @classmethod
    def from_row(cls, row) -> Topic:
        metadata = {}
        raw = row["extra_metadata"]
        if raw:
            try:
                metadata = json.loads(raw)
            except (json.JSONDecodeError, TypeError):
                metadata = {}
        return cls(
            id=row["id"],
            focus_id=row["focus_id"],
            user_id=row["user_id"],
            persona_id=row["persona_id"],
            lifecycle_state=row["lifecycle_state"],
            placeholder_name=row["placeholder_name"],
            created_at=row["created_at"],
            updated_at=row["updated_at"],
            name=row["name"],
            dormant_since=row["dormant_since"],
            closed_at=row["closed_at"],
            extra_metadata=metadata,
        )


@dataclass
class ClassificationPreference:
    """Runtime representation of a classification_preferences row."""
    id: str
    focus_id: str
    persona_id: str
    content_type: str
    visibility_scope: str
    transformation: str
    user_calibrated: bool
    confidence: float
    created_at: str
    updated_at: str
    sensitivity_preset: str | None = None
    last_applied_at: str | None = None

    @classmethod
    def from_row(cls, row) -> ClassificationPreference:
        return cls(
            id=row["id"],
            focus_id=row["focus_id"],
            persona_id=row["persona_id"],
            content_type=row["content_type"],
            visibility_scope=row["visibility_scope"],
            transformation=row["transformation"],
            sensitivity_preset=row["sensitivity_preset"],
            user_calibrated=bool(row["user_calibrated"]),
            confidence=row["confidence"],
            last_applied_at=row["last_applied_at"],
            created_at=row["created_at"],
            updated_at=row["updated_at"],
        )


# -- Topic CRUD ---------------------------------------------------------------

def create_topic(
    user_id: str,
    persona_id: str,
    key_hex: str,
    focus_id: str,
    name: str | None = None,
    placeholder_name: str | None = None,
) -> Topic:
    """
    Create a new topic in outputs.db and register it in shared.db topic_index.
    Also registers plan_state.db path in topic_storage_locations.
    placeholder_name generated from focus_id + timestamp if not provided.
    name: user-assigned. None = unnamed (naming offered on resume).
    """
    topic_id = str(uuid.uuid4())
    timestamp = now()
    ph_name = placeholder_name or f"{focus_id} — {timestamp[:10]} {timestamp[11:16]}"
    plan_state_path = str(get_plan_state_path(user_id, persona_id, focus_id, topic_id))

    with open_outputs_db(user_id, persona_id, key_hex) as db:
        db.execute(
            """INSERT INTO topics
               (id, focus_id, user_id, persona_id, name, placeholder_name,
                lifecycle_state, created_at, updated_at, extra_metadata)
               VALUES (?, ?, ?, ?, ?, ?, 'active', ?, ?, '{}')""",
            [topic_id, focus_id, user_id, persona_id, name, ph_name,
             timestamp, timestamp]
        )
        # Register plan_state.db path in discovery index.
        # topic_storage_locations is a discovery index — not authoritative source of truth.
        # Filesystem existence takes precedence on restore/conflict.
        db.execute(
            """INSERT INTO topic_storage_locations
               (topic_id, db_path, created_at)
               VALUES (?, ?, ?)""",
            [topic_id, plan_state_path, timestamp]
        )

    _mirror_topic_index(
        topic_id=topic_id,
        persona_id=persona_id,
        focus_id=focus_id,
        display_name=name or ph_name,
        lifecycle_state="active",
        last_active_at=timestamp,
        session_count=0,
        created_at=timestamp,
    )

    return Topic(
        id=topic_id,
        focus_id=focus_id,
        user_id=user_id,
        persona_id=persona_id,
        name=name,
        placeholder_name=ph_name,
        lifecycle_state="active",
        created_at=timestamp,
        updated_at=timestamp,
    )


def get_topic(
    user_id: str,
    persona_id: str,
    key_hex: str,
    topic_id: str,
) -> Topic | None:
    """Fetch a topic by id. Returns None if not found."""
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        row = db.execute(
            """SELECT id, focus_id, user_id, persona_id, name, placeholder_name,
                      lifecycle_state, dormant_since, created_at, updated_at,
                      closed_at, extra_metadata
               FROM topics WHERE id = ?""",
            [topic_id]
        ).fetchone()
    return Topic.from_row(row) if row else None


def list_topics(
    user_id: str,
    persona_id: str,
    key_hex: str,
    focus_id: str | None = None,
    lifecycle_state: str | None = None,
) -> list[Topic]:
    """
    List topics for a user/life with optional filters.
    Ordered by updated_at DESC (most recently active first).
    """
    query = (
        "SELECT id, focus_id, user_id, persona_id, name, placeholder_name, "
        "lifecycle_state, dormant_since, created_at, updated_at, "
        "closed_at, extra_metadata FROM topics WHERE 1=1"
    )
    params: list = []
    if focus_id is not None:
        query += " AND focus_id = ?"
        params.append(focus_id)
    if lifecycle_state is not None:
        query += " AND lifecycle_state = ?"
        params.append(lifecycle_state)
    query += " ORDER BY updated_at DESC"

    with open_outputs_db(user_id, persona_id, key_hex) as db:
        rows = db.execute(query, params).fetchall()
    return [Topic.from_row(row) for row in rows]


def update_topic_state(
    user_id: str,
    persona_id: str,
    key_hex: str,
    topic_id: str,
    lifecycle_state: str,
    dormant_since: str | None = None,
) -> bool:
    """
    Update topic lifecycle state. Returns True if found and updated.
    Automatically sets closed_at when transitioning to complete or closed.
    Also updates topic_index mirror in shared.db.

    Completion authority invariant (ADR-013 Section 8.9):
    topics.lifecycle_state NEVER set to 'complete' by system autonomously.
    This function does not enforce that — callers must not pass 'complete'
    from system code. Only user-initiated calls may pass 'complete'.
    """
    timestamp = now()
    closed_at = timestamp if lifecycle_state in ("complete", "closed") else None

    with open_outputs_db(user_id, persona_id, key_hex) as db:
        result = db.execute(
            """UPDATE topics SET lifecycle_state = ?, dormant_since = ?,
               closed_at = ?, updated_at = ? WHERE id = ?""",
            [lifecycle_state, dormant_since, closed_at, timestamp, topic_id]
        )
        updated = result.rowcount > 0

    if updated:
        _update_topic_index_state(topic_id, lifecycle_state, timestamp)
    return updated


def name_topic(
    user_id: str,
    persona_id: str,
    key_hex: str,
    topic_id: str,
    name: str,
) -> bool:
    """
    Set or update the user-assigned name for a topic.
    Also updates the topic_index display_name mirror.
    Returns True if found and updated.
    """
    timestamp = now()
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        result = db.execute(
            "UPDATE topics SET name = ?, updated_at = ? WHERE id = ?",
            [name, timestamp, topic_id]
        )
        updated = result.rowcount > 0
    if updated:
        _update_topic_index_display_name(topic_id, name, timestamp)
    return updated


def increment_topic_session_count(
    user_id: str,
    persona_id: str,
    key_hex: str,
    topic_id: str,
) -> int:
    """
    Increment session_count on topic_index in shared.db.
    Called at Phase 3 INITIALIZE.
    Returns new session_count. 0 if topic not found in index.
    open_instance_db() is the shared.db opener in this codebase.
    """
    timestamp = now()
    with open_instance_db() as db:
        db.execute(
            """UPDATE topic_index SET session_count = session_count + 1,
               last_active_at = ?, updated_at = ? WHERE topic_id = ?""",
            [timestamp, timestamp, topic_id]
        )
        row = db.execute(
            "SELECT session_count FROM topic_index WHERE topic_id = ?",
            [topic_id]
        ).fetchone()
    return row["session_count"] if row else 0


# -- Topic storage location registry ------------------------------------------

def get_plan_state_db_path(
    user_id: str,
    persona_id: str,
    key_hex: str,
    topic_id: str,
) -> str | None:
    """
    Retrieve the registered plan_state.db path for a topic from the discovery index.
    Returns None if not registered.
    Boot Check uses this as primary lookup — filesystem scan is orphan fallback only.
    topic_storage_locations is a discovery index, not authoritative source of truth.
    """
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        row = db.execute(
            "SELECT db_path FROM topic_storage_locations WHERE topic_id = ?",
            [topic_id]
        ).fetchone()
    return row["db_path"] if row else None


def mark_storage_location_verified(
    user_id: str,
    persona_id: str,
    key_hex: str,
    topic_id: str,
) -> None:
    """Mark a topic's plan_state.db as verified at current time."""
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        db.execute(
            "UPDATE topic_storage_locations SET verified_at = ?, orphaned = 0 "
            "WHERE topic_id = ?",
            [now(), topic_id]
        )


def mark_storage_location_orphaned(
    user_id: str,
    persona_id: str,
    key_hex: str,
    topic_id: str,
) -> None:
    """
    Mark a topic's plan_state.db as orphaned (file missing at Boot Check).
    Boot Check never auto-deletes — surfaces as Life dashboard notification.
    User action required to resolve.
    """
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        db.execute(
            "UPDATE topic_storage_locations SET orphaned = 1, verified_at = ? "
            "WHERE topic_id = ?",
            [now(), topic_id]
        )


# -- Run history --------------------------------------------------------------

def create_run_history_entry(
    user_id: str,
    persona_id: str,
    key_hex: str,
    focus_run_id: str,
    focus_id: str,
    is_quick_ask: bool,
    topic_id: str | None = None,
    output_id: str | None = None,
    output_type: str | None = None,
) -> str:
    """
    Create a run_history entry for a focus run.

    Quick Ask invariant: promote_window_expires_at is always NULL for Quick Ask runs.
    Quick Ask runs can never be promoted to a topic — enforced here.
    Named runs (topic_id non-null) also have no promote window.
    90-day promote window applies only to unnamed non-Quick Ask runs.

    Returns the run_history entry id.
    """
    entry_id = str(uuid.uuid4())
    timestamp = now()

    promote_expires: str | None = None
    if not is_quick_ask and topic_id is None:
        expiry = datetime.now(timezone.utc) + timedelta(days=RUN_HISTORY_RETENTION_DAYS)
        promote_expires = expiry.isoformat()

    with open_outputs_db(user_id, persona_id, key_hex) as db:
        db.execute(
            """INSERT INTO run_history
               (id, focus_run_id, focus_id, persona_id, topic_id,
                output_id, output_type, is_quick_ask,
                promote_window_expires_at, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
            [entry_id, focus_run_id, focus_id, persona_id, topic_id,
             output_id, output_type, 1 if is_quick_ask else 0,
             promote_expires, timestamp]
        )
    return entry_id


def nullify_run_history_output(
    user_id: str,
    persona_id: str,
    key_hex: str,
    output_id: str,
) -> None:
    """
    Set output_id to NULL in run_history when a Library output is deleted.
    Entry retained for audit unless user explicitly purges.
    """
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        db.execute(
            "UPDATE run_history SET output_id = NULL WHERE output_id = ?",
            [output_id]
        )


def list_promotable_runs(
    user_id: str,
    persona_id: str,
    key_hex: str,
    focus_id: str | None = None,
) -> list[dict]:
    """
    List unnamed non-Quick Ask runs within their promote window.
    Used for "Promote to topic" UI (90-day window).
    """
    query = (
        "SELECT id, focus_run_id, focus_id, output_id, output_type, "
        "created_at, promote_window_expires_at "
        "FROM run_history "
        "WHERE topic_id IS NULL AND is_quick_ask = 0 "
        "AND promote_window_expires_at > ? "
    )
    params: list = [now()]
    if focus_id is not None:
        query += "AND focus_id = ? "
        params.append(focus_id)
    query += "ORDER BY created_at DESC"

    with open_outputs_db(user_id, persona_id, key_hex) as db:
        rows = db.execute(query, params).fetchall()
    return [dict(row) for row in rows]


# -- Classification preferences -----------------------------------------------

def get_classification_preference(
    user_id: str,
    persona_id: str,
    key_hex: str,
    focus_id: str,
    content_type: str,
) -> ClassificationPreference | None:
    """
    Fetch the classification preference for a content type within a focus.
    Returns None if no preference established — Mode 2 should fire.
    Mode 1 reads from this table.
    """
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        row = db.execute(
            """SELECT id, focus_id, persona_id, content_type, visibility_scope,
                      transformation, sensitivity_preset, user_calibrated,
                      confidence, last_applied_at, created_at, updated_at
               FROM classification_preferences
               WHERE focus_id = ? AND persona_id = ? AND content_type = ?""",
            [focus_id, persona_id, content_type]
        ).fetchone()
    return ClassificationPreference.from_row(row) if row else None


def upsert_classification_preference(
    user_id: str,
    persona_id: str,
    key_hex: str,
    focus_id: str,
    content_type: str,
    visibility_scope: str,
    transformation: str,
    sensitivity_preset: str | None = None,
    user_calibrated: bool = False,
    confidence: float = 1.0,
) -> str:
    """
    Insert or update a classification preference.
    Mode 2 response: user_calibrated=True.
    Mode 1 inference: user_calibrated=False.
    Returns the preference id.

    Preset-to-dimensions mapping (convenience reference):
        standard  = tier2_permitted  + generalize_ok
        sensitive = anonymous_tier2  + anonymize_ok
        private   = tier_1_only      + generalize_ok
        locked    = tier_1_only      + no_generalize
    """
    timestamp = now()
    pref_id = str(uuid.uuid4())

    with open_outputs_db(user_id, persona_id, key_hex) as db:
        existing = db.execute(
            "SELECT id FROM classification_preferences "
            "WHERE focus_id = ? AND persona_id = ? AND content_type = ?",
            [focus_id, persona_id, content_type]
        ).fetchone()

        if existing:
            pref_id = existing["id"]
            db.execute(
                """UPDATE classification_preferences SET
                   visibility_scope = ?, transformation = ?,
                   sensitivity_preset = ?, user_calibrated = ?,
                   confidence = ?, updated_at = ?
                   WHERE id = ?""",
                [visibility_scope, transformation, sensitivity_preset,
                 1 if user_calibrated else 0, confidence, timestamp, pref_id]
            )
        else:
            db.execute(
                """INSERT INTO classification_preferences
                   (id, focus_id, persona_id, content_type, visibility_scope,
                    transformation, sensitivity_preset, user_calibrated,
                    confidence, created_at, updated_at)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                [pref_id, focus_id, persona_id, content_type, visibility_scope,
                 transformation, sensitivity_preset,
                 1 if user_calibrated else 0, confidence, timestamp, timestamp]
            )
    return pref_id


def record_preference_applied(
    user_id: str,
    persona_id: str,
    key_hex: str,
    focus_id: str,
    content_type: str,
) -> None:
    """Update last_applied_at timestamp when a Mode 1 preference is used."""
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        db.execute(
            """UPDATE classification_preferences SET last_applied_at = ?
               WHERE focus_id = ? AND persona_id = ? AND content_type = ?""",
            [now(), focus_id, persona_id, content_type]
        )


# -- topic_index mirror (shared.db) -------------------------------------------

def _mirror_topic_index(
    topic_id: str,
    persona_id: str,
    focus_id: str,
    display_name: str,
    lifecycle_state: str,
    last_active_at: str,
    session_count: int,
    created_at: str,
) -> None:
    """
    Write or update the topic_index row in shared.db.
    Non-fatal if shared.db write fails — outputs.db is authoritative.
    topic_index is a cache copy for the Life dashboard only.
    open_instance_db() is the shared.db opener in this codebase.
    """
    try:
        with open_instance_db() as db:
            db.execute(
                """INSERT OR REPLACE INTO topic_index
                   (topic_id, persona_id, focus_id, display_name, lifecycle_state,
                    last_active_at, session_count, content_summary,
                    created_at, updated_at)
                   VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)""",
                [topic_id, persona_id, focus_id, display_name, lifecycle_state,
                 last_active_at, session_count, created_at, created_at]
            )
    except Exception:
        pass  # Non-fatal — outputs.db is authoritative


def _update_topic_index_state(
    topic_id: str,
    lifecycle_state: str,
    timestamp: str,
) -> None:
    """Update lifecycle_state and last_active_at in topic_index. Non-fatal."""
    try:
        with open_instance_db() as db:
            db.execute(
                """UPDATE topic_index SET lifecycle_state = ?,
                   last_active_at = ?, updated_at = ? WHERE topic_id = ?""",
                [lifecycle_state, timestamp, timestamp, topic_id]
            )
    except Exception:
        pass


def _update_topic_index_display_name(
    topic_id: str,
    display_name: str,
    timestamp: str,
) -> None:
    """Update display_name in topic_index when user names a topic. Non-fatal."""
    try:
        with open_instance_db() as db:
            db.execute(
                """UPDATE topic_index SET display_name = ?, updated_at = ?
                   WHERE topic_id = ?""",
                [display_name, timestamp, topic_id]
            )
    except Exception:
        pass
