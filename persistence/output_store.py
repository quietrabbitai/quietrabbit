# persistence/output_store.py
# CRUD helpers for outputs.db — output records and read-only run status lookup.
#
# All database access via open_outputs_db() — never raw sqlite3.
# open_outputs_db() commits on clean context exit and sets row_factory=sqlite3.Row.
# FTS5 sync is handled by schema triggers — writes to outputs trigger index update.
#
# Responsibility boundary:
#   output_store  — output record persistence + read-only run status for UI polling
#   lifecycle.py  — all focus_run state transitions (create, promote, status updates)
#
# get_focus_run_status() is a documented exception to the output-only boundary:
# the UI polling endpoint needs run status without importing lifecycle machinery.
# Revisit when a proper service layer is introduced in Layer 8+.
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   space_id → life_id
#   path_run_id → focus_run_id
#   path_runs table → focus_runs table
#
# Updated as part of Phase C Persona model migration (D6-298):
#   life_id → persona_id in all function signatures and open_outputs_db calls

from __future__ import annotations

import uuid
from dataclasses import dataclass

from providers.utils import now, open_outputs_db


# Canonical sensitivity values — must match sensitivity_levels.yaml and
# lifecycle._output_sensitivity(). Reject anything outside this set at write time.
_VALID_SENSITIVITY = {"general", "personal", "medical", "financial"}


@dataclass
class OutputRecord:
    id: str
    focus_run_id: str
    output_type: str
    content: str
    sensitivity: str
    status: str
    created_at: str
    updated_at: str


def _row_to_output_record(row) -> OutputRecord:
    """Map a sqlite3.Row from the outputs table to an OutputRecord."""
    return OutputRecord(
        id=row["id"],
        focus_run_id=row["focus_run_id"],
        output_type=row["output_type"],
        content=row["content"],
        sensitivity=row["sensitivity"],
        status=row["status"],
        created_at=row["created_at"],
        updated_at=row["updated_at"],
    )


def save_output(
    user_id: str,
    persona_id: str,
    key_hex: str,
    focus_run_id: str,
    output_type: str,
    content: str,
    sensitivity: str = "general",
    output_id: str | None = None,
) -> str:
    """
    Write a completed output to outputs.db. Returns the output id.
    FTS5 index updated automatically via schema trigger on insert.
    """
    if sensitivity not in _VALID_SENSITIVITY:
        raise ValueError(
            f"Invalid sensitivity '{sensitivity}'. "
            f"Must be one of: {sorted(_VALID_SENSITIVITY)}"
        )
    oid = output_id or str(uuid.uuid4())
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        db.execute(
            """INSERT INTO outputs
               (id, focus_run_id, output_type, content, sensitivity,
                status, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, 'active', ?, ?)""",
            [oid, focus_run_id, output_type, content, sensitivity, now(), now()],
        )
    return oid


def get_output(
    user_id: str,
    persona_id: str,
    key_hex: str,
    output_id: str,
) -> OutputRecord | None:
    """
    Fetch a single active output by id.
    Returns None if not found or not active.
    """
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        row = db.execute(
            """SELECT id, focus_run_id, output_type, content,
                      sensitivity, status, created_at, updated_at
               FROM outputs
               WHERE id = ? AND status = 'active'""",
            [output_id],
        ).fetchone()
    return _row_to_output_record(row) if row else None


def get_output_for_run(
    user_id: str,
    persona_id: str,
    key_hex: str,
    focus_run_id: str,
) -> OutputRecord | None:
    """
    Fetch the most recent active output for a focus run.
    Returns None if no active output exists.
    Used by UI output display endpoint (/output/<focus_run_id>).
    """
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        row = db.execute(
            """SELECT id, focus_run_id, output_type, content,
                      sensitivity, status, created_at, updated_at
               FROM outputs
               WHERE focus_run_id = ? AND status = 'active'
               ORDER BY created_at DESC
               LIMIT 1""",
            [focus_run_id],
        ).fetchone()
    return _row_to_output_record(row) if row else None


def get_focus_run_status(
    user_id: str,
    persona_id: str,
    key_hex: str,
    focus_run_id: str,
) -> str | None:
    """
    Fetch the current status of a focus run.
    Returns status string or None if focus_run_id not found.
    Used by UI polling endpoint (/status/<focus_run_id>).

    Note: reads focus_runs, which is lifecycle state. This is a documented
    exception — the UI polling endpoint needs run status without importing
    lifecycle machinery. Revisit when a service layer is introduced in Layer 8+.
    """
    with open_outputs_db(user_id, persona_id, key_hex) as db:
        row = db.execute(
            "SELECT status FROM focus_runs WHERE id = ?",
            [focus_run_id],
        ).fetchone()
    return row["status"] if row else None


def delete_output(
    user_id: str,
    persona_id: str,
    key_hex: str,
    output_id: str,
) -> None:
    """
    Delete an output. Full sequence implemented in Layer 5+.
    Correct deletion sequence (architecture Section 3.4):
      1. Zero content:  UPDATE outputs SET content = '' WHERE id = ?
      2. FTS5 update:   handled by COALESCE trigger in schema
      3. Mark deleted:  UPDATE outputs SET status = 'deleted', deleted_at = ? WHERE id = ?
    Row is never deleted — audit record preserved permanently.
    """
    raise NotImplementedError(
        "delete_output: full zero-then-delete sequence implemented in Layer 5+"
    )
