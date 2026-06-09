# persistence/life_store.py
# Life and user-life CRUD operations.
# Reads from and writes to instance/shared.db via open_instance_db().
# Replaces space_store.py as part of Phase A codebase rename (D6-224, D6-225).

from __future__ import annotations

import json
import sqlite3
from dataclasses import dataclass, field
from typing import Literal

from providers.utils import now, open_instance_db


VALID_LIFE_TYPES = Literal[
    "personal", "work", "medical", "cooking",
    "homelab", "finance", "travel", "general"
]

TIER_MIN = 1
TIER_MAX = 3


# -- Life dataclass -----------------------------------------------------------

@dataclass
class Life:
    """
    Runtime representation of a life record from shared.db.
    max_permitted_tier is the hard ceiling — never exceeded.
    life_privacy_default_tier is the user's preference.
    extra_metadata is a dict at runtime — serialized to JSON at DB boundary.
    """
    id: str
    display_name: str
    life_type: str
    life_privacy_default_tier: int
    max_permitted_tier: int
    created_at: str
    extra_metadata: dict = field(default_factory=dict)

    @classmethod
    def from_row(cls, row) -> Life:
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
            life_type=row["life_type"],
            life_privacy_default_tier=row["life_privacy_default_tier"],
            max_permitted_tier=row["max_permitted_tier"],
            created_at=row["created_at"],
            extra_metadata=metadata,
        )


# -- Validation helpers -------------------------------------------------------

def _validate_tiers(life_privacy_default_tier: int, max_permitted_tier: int) -> None:
    for name, val in [
        ("life_privacy_default_tier", life_privacy_default_tier),
        ("max_permitted_tier", max_permitted_tier),
    ]:
        if not (TIER_MIN <= val <= TIER_MAX):
            raise ValueError(
                f"{name} must be between {TIER_MIN} and {TIER_MAX}, got {val}."
            )
    if max_permitted_tier < life_privacy_default_tier:
        raise ValueError(
            f"max_permitted_tier ({max_permitted_tier}) must be >= "
            f"life_privacy_default_tier ({life_privacy_default_tier})."
        )


# -- Read operations ----------------------------------------------------------

def get_life(life_id: str) -> Life | None:
    """Fetch a life by ID. Returns None if not found."""
    with open_instance_db() as db:
        row = db.execute(
            "SELECT id, display_name, life_type, life_privacy_default_tier, "
            "max_permitted_tier, created_at, extra_metadata "
            "FROM lives WHERE id = ?",
            [life_id]
        ).fetchone()
    return Life.from_row(row) if row else None


def get_life_for_user(user_id: str, life_id: str) -> Life | None:
    """
    Fetch a life only if the user has membership.
    Returns None if not found or user is not a member.
    Used by lifecycle.py AUTHORIZE to enforce access control.
    """
    with open_instance_db() as db:
        row = db.execute(
            "SELECT l.id, l.display_name, l.life_type, "
            "l.life_privacy_default_tier, l.max_permitted_tier, "
            "l.created_at, l.extra_metadata "
            "FROM lives l "
            "JOIN user_lives ul ON ul.life_id = l.id "
            "WHERE ul.user_id = ? AND l.id = ?",
            [user_id, life_id]
        ).fetchone()
    return Life.from_row(row) if row else None


def list_lives_for_user(user_id: str) -> list[Life]:
    """Return all lives accessible to a user, ordered by display_name."""
    with open_instance_db() as db:
        rows = db.execute(
            "SELECT l.id, l.display_name, l.life_type, "
            "l.life_privacy_default_tier, l.max_permitted_tier, "
            "l.created_at, l.extra_metadata "
            "FROM lives l "
            "JOIN user_lives ul ON ul.life_id = l.id "
            "WHERE ul.user_id = ? "
            "ORDER BY l.display_name",
            [user_id]
        ).fetchall()
    return [Life.from_row(row) for row in rows]


# -- Write operations ---------------------------------------------------------

def create_life(
    life_id: str,
    display_name: str,
    life_type: str,
    creator_user_id: str,
    life_privacy_default_tier: int = 1,
    max_permitted_tier: int = 1,
) -> Life:
    """
    Create a new life and atomically add the creator as a member.
    Raises ValueError if life_id already exists or tier values are invalid.
    Uses INSERT + IntegrityError for race-safe uniqueness enforcement.
    """
    _validate_tiers(life_privacy_default_tier, max_permitted_tier)
    created_at = now()

    try:
        with open_instance_db() as db:
            db.execute(
                "INSERT INTO lives "
                "(id, display_name, life_type, life_privacy_default_tier, "
                "max_permitted_tier, created_at, extra_metadata) "
                "VALUES (?, ?, ?, ?, ?, ?, ?)",
                [
                    life_id, display_name, life_type,
                    life_privacy_default_tier, max_permitted_tier,
                    created_at, "{}",
                ]
            )
            # Atomically add creator membership in same transaction
            db.execute(
                "INSERT INTO user_lives (user_id, life_id, joined_at) "
                "VALUES (?, ?, ?)",
                [creator_user_id, life_id, created_at]
            )
    except sqlite3.IntegrityError:
        raise ValueError(f"Life '{life_id}' already exists.")

    return Life(
        id=life_id,
        display_name=display_name,
        life_type=life_type,
        life_privacy_default_tier=life_privacy_default_tier,
        max_permitted_tier=max_permitted_tier,
        created_at=created_at,
    )


def update_life_tiers(
    life_id: str,
    life_privacy_default_tier: int | None = None,
    max_permitted_tier: int | None = None,
) -> Life:
    """
    Update tier settings for a life.
    Raises LookupError if life not found, ValueError on invalid tiers.
    """
    life = get_life(life_id)
    if not life:
        raise LookupError(f"Life '{life_id}' not found.")

    new_privacy = (
        life_privacy_default_tier if life_privacy_default_tier is not None
        else life.life_privacy_default_tier
    )
    new_max = (
        max_permitted_tier if max_permitted_tier is not None
        else life.max_permitted_tier
    )

    _validate_tiers(new_privacy, new_max)

    with open_instance_db() as db:
        db.execute(
            "UPDATE lives SET life_privacy_default_tier = ?, max_permitted_tier = ? "
            "WHERE id = ?",
            [new_privacy, new_max, life_id]
        )

    life.life_privacy_default_tier = new_privacy
    life.max_permitted_tier = new_max
    return life


def delete_life(life_id: str) -> bool:
    """
    Delete a life and all user_life memberships (CASCADE).
    Returns True if deleted, False if not found.
    Does NOT delete per-life databases (personal.db, outputs.db) —
    those require explicit user confirmation and a separate cleanup operation.
    """
    with open_instance_db() as db:
        result = db.execute(
            "DELETE FROM lives WHERE id = ?", [life_id]
        )
    return result.rowcount > 0


# -- User-life membership -----------------------------------------------------

def add_user_to_life(user_id: str, life_id: str) -> bool:
    """
    Add a user to a life.
    Returns True if added, False if already a member.
    Raises LookupError if life does not exist.
    Uses INSERT + IntegrityError for race-safe uniqueness enforcement.
    """
    if not get_life(life_id):
        raise LookupError(f"Life '{life_id}' not found.")

    try:
        with open_instance_db() as db:
            db.execute(
                "INSERT INTO user_lives (user_id, life_id, joined_at) "
                "VALUES (?, ?, ?)",
                [user_id, life_id, now()]
            )
        return True
    except sqlite3.IntegrityError:
        return False  # Already a member


def remove_user_from_life(user_id: str, life_id: str) -> bool:
    """
    Remove a user from a life.
    Returns True if removed, False if not a member.
    """
    with open_instance_db() as db:
        result = db.execute(
            "DELETE FROM user_lives WHERE user_id = ? AND life_id = ?",
            [user_id, life_id]
        )
    return result.rowcount > 0


def is_user_in_life(user_id: str, life_id: str) -> bool:
    """Check if a user is a member of a life."""
    with open_instance_db() as db:
        row = db.execute(
            "SELECT 1 FROM user_lives WHERE user_id = ? AND life_id = ?",
            [user_id, life_id]
        ).fetchone()
    return row is not None
