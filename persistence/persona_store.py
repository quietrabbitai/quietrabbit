# persistence/persona_store.py
# Persona CRUD operations.
# Reads from and writes to instance/shared.db via open_instance_db().
# Replaces life_store.py as part of Phase C Persona model migration (D6-298).
#
# Persona is a personalization grouping only (D6-289, D6-291).
# No tier fields on the Persona record -- tier enforcement is Focus-level.
# get_persona_for_user() performs membership validation only.
# Tier ceiling and privacy settings are read from focus_settings via
# focus_settings_store.get_focus_settings(focus_id) (Task 7).
#
# floor_consent_preference is stored in personas.extra_metadata (D5-152).
# lifecycle.py and routes.py read/write it directly via open_instance_db()
# after tasks 8 and 18 update those call sites.

from __future__ import annotations

import json
import sqlite3
from dataclasses import dataclass, field

from providers.utils import now, open_instance_db


# -- Persona dataclass --------------------------------------------------------

@dataclass
class Persona:
    """
    Runtime representation of a persona record from shared.db.
    No tier fields -- Persona is a personalization grouping only (D6-297).
    extra_metadata is a dict at runtime -- serialized to JSON at DB boundary.
    Includes floor_consent_preference when set (D5-152).
    """
    id: str
    display_name: str
    persona_type: str
    created_at: str
    extra_metadata: dict = field(default_factory=dict)

    @classmethod
    def from_row(cls, row) -> Persona:
        metadata = {}
        raw = row["extra_metadata"]
        if raw:
            try:
                metadata = json.loads(raw)
            except (json.JSONDecodeError, TypeError):
                metadata = {}
        return cls(
            id=row["id"],
            display_name=row["display_name"],
            persona_type=row["persona_type"],
            created_at=row["created_at"],
            extra_metadata=metadata,
        )


# -- Read operations ----------------------------------------------------------

def get_persona(persona_id: str) -> Persona | None:
    """Fetch a persona by ID. Returns None if not found."""
    with open_instance_db() as db:
        row = db.execute(
            "SELECT id, display_name, persona_type, created_at, extra_metadata "
            "FROM personas WHERE id = ?",
            [persona_id]
        ).fetchone()
    return Persona.from_row(row) if row else None


def get_persona_for_user(user_id: str, persona_id: str) -> Persona | None:
    """
    Fetch a persona only if the user has membership.
    Returns None if not found or user is not a member.
    Membership validation only -- no tier data (D6-297).
    Used by lifecycle.py AUTHORIZE to enforce access control.
    """
    with open_instance_db() as db:
        row = db.execute(
            "SELECT p.id, p.display_name, p.persona_type, "
            "p.created_at, p.extra_metadata "
            "FROM personas p "
            "JOIN user_personas up ON up.persona_id = p.id "
            "WHERE up.user_id = ? AND p.id = ?",
            [user_id, persona_id]
        ).fetchone()
    return Persona.from_row(row) if row else None


def list_personas_for_user(user_id: str) -> list[Persona]:
    """Return all personas accessible to a user, ordered by display_name."""
    with open_instance_db() as db:
        rows = db.execute(
            "SELECT p.id, p.display_name, p.persona_type, "
            "p.created_at, p.extra_metadata "
            "FROM personas p "
            "JOIN user_personas up ON up.persona_id = p.id "
            "WHERE up.user_id = ? "
            "ORDER BY p.display_name",
            [user_id]
        ).fetchall()
    return [Persona.from_row(row) for row in rows]


# -- Write operations ---------------------------------------------------------

def create_persona(
    persona_id: str,
    display_name: str,
    persona_type: str,
    creator_user_id: str,
) -> Persona:
    """
    Create a new persona and atomically add the creator as a member.
    Raises ValueError if persona_id already exists.
    Uses INSERT + IntegrityError for race-safe uniqueness enforcement.
    No tier parameters -- tier settings belong to focus_settings (D6-297).
    Atomic: open_instance_db() commits on success, rolls back on any exception.
    """
    created_at = now()

    try:
        with open_instance_db() as db:
            db.execute(
                "INSERT INTO personas "
                "(id, display_name, persona_type, created_at, extra_metadata) "
                "VALUES (?, ?, ?, ?, ?)",
                [persona_id, display_name, persona_type, created_at, "{}"]
            )
            db.execute(
                "INSERT INTO user_personas (user_id, persona_id, joined_at) "
                "VALUES (?, ?, ?)",
                [creator_user_id, persona_id, created_at]
            )
    except sqlite3.IntegrityError:
        raise ValueError(f"Persona '{persona_id}' already exists.")

    return Persona(
        id=persona_id,
        display_name=display_name,
        persona_type=persona_type,
        created_at=created_at,
    )


def delete_persona(persona_id: str) -> bool:
    """
    Delete a persona and all user_persona memberships (CASCADE).
    Returns True if deleted, False if not found.
    user_personas.persona_id has ON DELETE CASCADE in shared_004.sql.
    Does NOT delete per-persona databases (personal.db, outputs.db) --
    those require explicit user confirmation and a separate cleanup operation.
    """
    with open_instance_db() as db:
        result = db.execute(
            "DELETE FROM personas WHERE id = ?", [persona_id]
        )
    return result.rowcount > 0


# -- User-persona membership --------------------------------------------------

def add_user_to_persona(user_id: str, persona_id: str) -> bool:
    """
    Add a user to a persona.
    Returns True if added, False if already a member.
    Raises LookupError if persona does not exist.
    """
    if not get_persona(persona_id):
        raise LookupError(f"Persona '{persona_id}' not found.")

    try:
        with open_instance_db() as db:
            db.execute(
                "INSERT INTO user_personas (user_id, persona_id, joined_at) "
                "VALUES (?, ?, ?)",
                [user_id, persona_id, now()]
            )
        return True
    except sqlite3.IntegrityError:
        return False  # Already a member


def remove_user_from_persona(user_id: str, persona_id: str) -> bool:
    """
    Remove a user from a persona.
    Returns True if removed, False if not a member.
    """
    with open_instance_db() as db:
        result = db.execute(
            "DELETE FROM user_personas WHERE user_id = ? AND persona_id = ?",
            [user_id, persona_id]
        )
    return result.rowcount > 0


def is_user_in_persona(user_id: str, persona_id: str) -> bool:
    """Check if a user is a member of a persona."""
    with open_instance_db() as db:
        row = db.execute(
            "SELECT 1 FROM user_personas WHERE user_id = ? AND persona_id = ?",
            [user_id, persona_id]
        ).fetchone()
    return row is not None
