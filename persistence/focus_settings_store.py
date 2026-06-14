# persistence/focus_settings_store.py
# Focus-level settings CRUD operations.
# Reads from and writes to instance/shared.db via open_instance_db().
# New in Phase C Persona model migration (D6-299, D6-302).
#
# focus_settings lives in shared.db (unencrypted) so Privacy Guardian
# can read Focus settings before opening encrypted per-user stores.
# Settings are behavioral configuration, not personal data.
#
# Three independent Focus settings per D6-291:
#   context_flow:       bidirectional | receive_only | isolated
#   library_visibility: shared | persona_visible | persona_hidden
#   privacy_tier:       1 (red) | 2 (yellow) | 3 (green)
# max_permitted_tier: hard Tier ceiling for this Focus (D6-297).
# focus_profile:      convenience label for the three settings (D6-294).
# voice_override:     Focus-level voice JSON or None (D6-302).
#
# PK is (persona_id, focus_id) -- Option B decision: full PK required
# for all reads. lifecycle.py calls get_focus_settings(self.persona_id,
# self.focus_id) after task 8. No ambiguity for multi-persona installs.
#
# Primary read path: get_focus_settings(persona_id, focus_id).
# Called by conductor/lifecycle.py AUTHORIZE and _get_focus_tier_ceiling()
# after task 8. Conductor asserts non-None at AUTHORIZE -- missing row
# is a hard error, not a fallback.

from __future__ import annotations

import json
from dataclasses import dataclass

from providers.utils import now, open_instance_db


VALID_CONTEXT_FLOWS = frozenset({"bidirectional", "receive_only", "isolated"})
VALID_LIBRARY_VISIBILITY = frozenset({"shared", "persona_visible", "persona_hidden"})
VALID_FOCUS_PROFILES = frozenset({"open", "organized", "protected"})
TIER_MIN = 1
TIER_MAX = 3

# Sentinel for update_focus_settings: distinguishes "no change" from None.
_UNSET = object()


# -- FocusSettings dataclass --------------------------------------------------

@dataclass
class FocusSettings:
    """
    Runtime representation of a focus_settings row from shared.db.
    voice_override is a dict at runtime (None if no override set).
    Serialized to JSON at DB boundary.
    Schema verified against shared_004.sql Step 10.
    """
    persona_id: str
    focus_id: str
    context_flow: str
    library_visibility: str
    privacy_tier: int
    max_permitted_tier: int
    focus_profile: str
    voice_override: dict | None
    created_at: str
    updated_at: str

    @classmethod
    def from_row(cls, row) -> FocusSettings:
        voice = None
        raw = row["voice_override"]
        if raw:
            try:
                voice = json.loads(raw)
            except (json.JSONDecodeError, TypeError):
                voice = None
        return cls(
            persona_id=row["persona_id"],
            focus_id=row["focus_id"],
            context_flow=row["context_flow"],
            library_visibility=row["library_visibility"],
            privacy_tier=row["privacy_tier"],
            max_permitted_tier=row["max_permitted_tier"],
            focus_profile=row["focus_profile"],
            voice_override=voice,
            created_at=row["created_at"],
            updated_at=row["updated_at"],
        )


# -- Validation ---------------------------------------------------------------

def _validate_settings(
    context_flow: str,
    library_visibility: str,
    privacy_tier: int,
    max_permitted_tier: int,
    focus_profile: str,
) -> None:
    if context_flow not in VALID_CONTEXT_FLOWS:
        raise ValueError(
            f"context_flow must be one of {sorted(VALID_CONTEXT_FLOWS)}, "
            f"got '{context_flow}'."
        )
    if library_visibility not in VALID_LIBRARY_VISIBILITY:
        raise ValueError(
            f"library_visibility must be one of "
            f"{sorted(VALID_LIBRARY_VISIBILITY)}, got '{library_visibility}'."
        )
    if not (TIER_MIN <= privacy_tier <= TIER_MAX):
        raise ValueError(
            f"privacy_tier must be between {TIER_MIN} and {TIER_MAX}, "
            f"got {privacy_tier}."
        )
    if not (TIER_MIN <= max_permitted_tier <= TIER_MAX):
        raise ValueError(
            f"max_permitted_tier must be between {TIER_MIN} and {TIER_MAX}, "
            f"got {max_permitted_tier}."
        )
    if focus_profile not in VALID_FOCUS_PROFILES:
        raise ValueError(
            f"focus_profile must be one of {sorted(VALID_FOCUS_PROFILES)}, "
            f"got '{focus_profile}'."
        )


# -- Read operations ----------------------------------------------------------

def get_focus_settings(persona_id: str, focus_id: str) -> FocusSettings | None:
    """
    Fetch focus settings by full PK (persona_id, focus_id).
    Returns None if not found.
    Primary read path -- called by lifecycle.py AUTHORIZE and
    _get_focus_tier_ceiling() after task 8.
    Conductor asserts non-None at AUTHORIZE -- missing row is a hard error.
    Full PK required: focus_id alone is not unique across personas (D6-303).
    """
    with open_instance_db() as db:
        row = db.execute(
            "SELECT persona_id, focus_id, context_flow, library_visibility, "
            "privacy_tier, max_permitted_tier, focus_profile, voice_override, "
            "created_at, updated_at "
            "FROM focus_settings WHERE persona_id = ? AND focus_id = ?",
            [persona_id, focus_id]
        ).fetchone()
    return FocusSettings.from_row(row) if row else None


def list_focus_settings_for_persona(persona_id: str) -> list[FocusSettings]:
    """Return all focus settings rows for a persona, ordered by focus_id."""
    with open_instance_db() as db:
        rows = db.execute(
            "SELECT persona_id, focus_id, context_flow, library_visibility, "
            "privacy_tier, max_permitted_tier, focus_profile, voice_override, "
            "created_at, updated_at "
            "FROM focus_settings WHERE persona_id = ? "
            "ORDER BY focus_id",
            [persona_id]
        ).fetchall()
    return [FocusSettings.from_row(row) for row in rows]


# -- Write operations ---------------------------------------------------------

def create_focus_settings(
    persona_id: str,
    focus_id: str,
    context_flow: str = "bidirectional",
    library_visibility: str = "shared",
    privacy_tier: int = 2,
    max_permitted_tier: int = 2,
    focus_profile: str = "open",
    voice_override: dict | None = None,
) -> FocusSettings:
    """
    Create a focus_settings row for the given persona + focus.
    Raises ValueError on invalid field values.
    IntegrityError on duplicate PK propagates uncaught -- duplicate is an
    application logic error (AUTHORIZE assertion catches missing rows first).
    """
    _validate_settings(
        context_flow, library_visibility,
        privacy_tier, max_permitted_tier, focus_profile,
    )
    created_at = now()
    voice_json = json.dumps(voice_override) if voice_override is not None else None

    with open_instance_db() as db:
        db.execute(
            "INSERT INTO focus_settings "
            "(persona_id, focus_id, context_flow, library_visibility, "
            "privacy_tier, max_permitted_tier, focus_profile, voice_override, "
            "created_at, updated_at) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            [persona_id, focus_id, context_flow, library_visibility,
             privacy_tier, max_permitted_tier, focus_profile,
             voice_json, created_at, created_at]
        )

    return FocusSettings(
        persona_id=persona_id,
        focus_id=focus_id,
        context_flow=context_flow,
        library_visibility=library_visibility,
        privacy_tier=privacy_tier,
        max_permitted_tier=max_permitted_tier,
        focus_profile=focus_profile,
        voice_override=voice_override,
        created_at=created_at,
        updated_at=created_at,
    )


def update_focus_settings(
    persona_id: str,
    focus_id: str,
    context_flow: str | None = None,
    library_visibility: str | None = None,
    privacy_tier: int | None = None,
    max_permitted_tier: int | None = None,
    focus_profile: str | None = None,
    voice_override=_UNSET,
) -> FocusSettings:
    """
    Update one or more fields on an existing focus_settings row.
    Only provided (non-None) fields are updated.
    voice_override sentinel: omit argument = no change, None = clear override.
    Raises LookupError if row not found.
    """
    existing = get_focus_settings(persona_id, focus_id)
    if not existing:
        raise LookupError(
            f"focus_settings not found for persona='{persona_id}' "
            f"focus='{focus_id}'."
        )

    new_flow = context_flow if context_flow is not None else existing.context_flow
    new_vis = library_visibility if library_visibility is not None else existing.library_visibility
    new_ptier = privacy_tier if privacy_tier is not None else existing.privacy_tier
    new_mtier = max_permitted_tier if max_permitted_tier is not None else existing.max_permitted_tier
    new_profile = focus_profile if focus_profile is not None else existing.focus_profile

    _validate_settings(new_flow, new_vis, new_ptier, new_mtier, new_profile)

    if voice_override is _UNSET:
        new_voice = existing.voice_override
    else:
        new_voice = voice_override
    new_voice_json = json.dumps(new_voice) if new_voice is not None else None

    updated_at = now()
    with open_instance_db() as db:
        db.execute(
            "UPDATE focus_settings SET "
            "context_flow = ?, library_visibility = ?, privacy_tier = ?, "
            "max_permitted_tier = ?, focus_profile = ?, voice_override = ?, "
            "updated_at = ? "
            "WHERE persona_id = ? AND focus_id = ?",
            [new_flow, new_vis, new_ptier, new_mtier, new_profile,
             new_voice_json, updated_at, persona_id, focus_id]
        )

    existing.context_flow = new_flow
    existing.library_visibility = new_vis
    existing.privacy_tier = new_ptier
    existing.max_permitted_tier = new_mtier
    existing.focus_profile = new_profile
    existing.voice_override = new_voice
    existing.updated_at = updated_at
    return existing
