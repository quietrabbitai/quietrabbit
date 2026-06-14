# persistence/personal_store.py
# Personal field and voice profile data access layer.
# Standalone API — no Conductor lifecycle imports.
#
# Field encryption note:
#   Architecture Section 8.2 specifies HKDF field-level encryption.
#   Layer 5: field values stored as UTF-8 strings in the BLOB column.
#   The entire DB is SQLCipher-encrypted at file level — no plaintext on disk.
#   HKDF per-field encryption (additional layer) activates in Layer 8 when
#   key derivation infrastructure is live.
#   The store API is encryption-agnostic — callers pass field_value as str.
#
# Ownership scopes:
#   self:     written and read by this user only (default)
#   group:    shared with a context group (Release 2 UX)
#   instance: instance-wide; general/personal sensitivity only —
#             medical/financial blocked at application layer.
#             NOTE: this constraint is enforced here at write time only.
#             DB-level enforcement (trigger or CHECK) planned for Layer 8.
#
# Short-field warning:
#   Gate2 (PG_GATE_2) uses MIN_MATCH_LENGTH = 4 for substring scanning.
#   Fields shorter than 4 chars cannot be detected in model responses.
#   save_personal_field() warns when a medical/financial field has a
#   short value — these are the only cases where Gate2 detection failure
#   is a material privacy risk (Tier 2+ routes medical/financial is blocked
#   anyway, but the warning is belt-and-suspenders for future Tier config).
#
# Voice profile value validation (D5-151):
#   save_voice_profile_entry() validates values at write time.
#   Rejects values containing PII patterns (email, phone, URL) or exceeding
#   the word-count ceiling for behavioral descriptors.
#   Raises ValueError with plain-language message — not just a log entry.
#   Security Checker (Layer 8) handles broader PII validation; this is
#   a targeted write-time guard, not a substitute.
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   space_id → life_id throughout
#   specialist_id column → source_id throughout
#   path_run_id → focus_run_id in disclosure_log queries
#
# Updated as part of Phase C Persona model migration (D6-298):
#   persona_id → persona_id throughout (was life_id)
#   open_personal_db path: lives/{life_id} → personas/{persona_id}
#   SQL: life_id column → persona_id in voice_profiles queries and INSERT
#   stored_persona_id local variable (was stored_life_id) in voice profile writes
#   Cleanup flagged for Chat-PM: voice profile NULL matching semantics,
#   GLOBAL_VOICE_PRECEDENCE constant (ChatGPT review items 1-4)

from __future__ import annotations

import hashlib
import json
import logging
import re
import uuid
import warnings
from typing import Literal

from conductor.context import PersonalField, PersonalTrack
from providers.errors import (
    PersonalDBDecryptionError,
    PersonalDBNotFoundError,
)
from providers.utils import get_data_root, now, open_personal_db

log = logging.getLogger(__name__)


# -- Constants ----------------------------------------------------------------

# Gate2 minimum match length — fields shorter than this cannot be detected
# in model responses by PG_GATE_2 substring scan.
MIN_MATCH_LENGTH = 4

# Export sensitivity ceiling — fields above this severity are NEVER exported.
# general=1, personal=2, medical=3, financial=4.
# NEVER change without updating the operator file and export contract.
EXPORT_SENSITIVITY_CEILING = 2

# Export schema version — tracks the export contract version.
# Increment when export payload structure changes.
EXPORT_SCHEMA_VERSION = "1.0"

# Instance-scope sensitivity ceiling.
# Architecture Section 3.2: "Instance-scope restricted to general and personal."
INSTANCE_SCOPE_MAX_SEVERITY = 2

# Voice profile value validation (D5-151).
VOICE_VALUE_MAX_WORDS = 12

_VOICE_VALUE_PII_PATTERNS: list[tuple[re.Pattern, str]] = [
    (re.compile(r'\S+@\S+\.\S+'), "email address"),
    (re.compile(r'\+?\d[\d\s\-(). ]{7,}\d'), "phone number"),
    (re.compile(r'https?://|www\.'), "URL"),
]

_VOICE_VALUE_REJECTION_MSG = (
    "We couldn't save that voice preference — it looks like it contains "
    "personal details. Voice preferences describe how you communicate, "
    "not who you are. Try something like 'professional and direct' instead."
)


# -- Load (Phase 3 INITIALIZE) ------------------------------------------------

def load_personal_track(
    user_id: str,
    persona_id: str,
    key_hex: str,
) -> PersonalTrack:
    """
    Load all personal fields and voice profile for a user+life.
    Returns an UNSEALED PersonalTrack — caller (lifecycle.py) seals it.
    Called during Phase 3 INITIALIZE.

    Raises:
        PersonalDBNotFoundError: personal.db does not exist.
        PersonalDBDecryptionError: SQLCipher key rejected or file corrupt.
    """
    data_root = get_data_root()
    db_path = (
        data_root / "users" / user_id / "personas" / persona_id / "personal.db"
    )

    if not db_path.exists():
        raise PersonalDBNotFoundError(
            plain_language=(
                "Quiet Rabbit couldn't find your personal information. [Get help]"
            )
        )
    if not key_hex:
        raise PersonalDBDecryptionError(
            plain_language="Your session has expired. Please log in again."
        )

    track = PersonalTrack()

    try:
        with open_personal_db(user_id, persona_id, key_hex) as db:
            rows = db.execute(
                "SELECT field_name, field_value, sensitivity, "
                "sensitivity_severity, source_id, "
                "abstraction_tier2, abstraction_tier3 "
                "FROM personal_fields ORDER BY field_name"
            ).fetchall()

            for row in rows:
                track.add_field(PersonalField(
                    field_name=row["field_name"],
                    field_value=row["field_value"],
                    sensitivity=row["sensitivity"],
                    sensitivity_severity=row["sensitivity_severity"],
                    source_id=row["source_id"],
                    abstraction_tier2=row["abstraction_tier2"],
                    abstraction_tier3=row["abstraction_tier3"],
                ))

            profile = _resolve_voice_profile(db, persona_id)
            track.set_voice_profile(profile)
            track.set_life_context({})

    except (PersonalDBNotFoundError, PersonalDBDecryptionError):
        raise
    except Exception as e:
        if (
            "not a database" in str(e).lower()
            or "file is not a database" in str(e).lower()
        ):
            raise PersonalDBDecryptionError(
                plain_language=(
                    "Quiet Rabbit couldn't open your personal information. "
                    "Your session may have expired. Please log in again."
                )
            ) from e
        raise

    return track


# -- Voice profile (read) -----------------------------------------------------

def load_voice_profile(
    user_id: str,
    persona_id: str,
    key_hex: str,
) -> dict[str, str]:
    """
    Assemble voice profile for a life, resolving all five precedence levels.
    Returns {attribute: value} with highest-precedence values winning.
    """
    with open_personal_db(user_id, persona_id, key_hex) as db:
        return _resolve_voice_profile(db, persona_id)


def _resolve_voice_profile(db, persona_id: str) -> dict[str, str]:
    """
    Internal: resolve voice profile from an open personal.db connection.

    Resolution: overwrite_by_precedence.
    ORDER BY precedence ASC — lower-precedence rows processed first,
    higher-precedence rows overwrite them for the same attribute key.

    Global entries (precedence 3, persona_id IS NULL) are shared across all lives.
    Precedence 5 (writing_context) applied at Step 8 by StepExecutor —
    not loaded at Phase 3 INITIALIZE.
    """
    rows = db.execute(
        "SELECT attribute, value, precedence FROM voice_profiles "
        "WHERE persona_id = ? OR persona_id IS NULL "
        "ORDER BY precedence ASC",
        [persona_id]
    ).fetchall()

    profile: dict[str, str] = {}
    for row in rows:
        profile[row["attribute"]] = row["value"]
    return profile


# -- Personal fields (read) ---------------------------------------------------

def get_personal_field(
    user_id: str,
    persona_id: str,
    key_hex: str,
    field_name: str,
) -> PersonalField | None:
    """
    Load a single personal field by name.
    Returns None if not found.
    """
    with open_personal_db(user_id, persona_id, key_hex) as db:
        row = db.execute(
            "SELECT field_name, field_value, sensitivity, sensitivity_severity, "
            "source_id, abstraction_tier2, abstraction_tier3 "
            "FROM personal_fields WHERE field_name = ?",
            [field_name]
        ).fetchone()

    if not row:
        return None

    return PersonalField(
        field_name=row["field_name"],
        field_value=row["field_value"],
        sensitivity=row["sensitivity"],
        sensitivity_severity=row["sensitivity_severity"],
        source_id=row["source_id"],
        abstraction_tier2=row["abstraction_tier2"],
        abstraction_tier3=row["abstraction_tier3"],
    )


def list_personal_fields(
    user_id: str,
    persona_id: str,
    key_hex: str,
    source_id: str | None = None,
    sensitivity: str | None = None,
) -> list[PersonalField]:
    """
    List all personal fields for a user+life, with optional filters.
    source_id: if provided, returns only fields from that source.
    sensitivity: if provided, filters by sensitivity label.
    """
    query = (
        "SELECT field_name, field_value, sensitivity, sensitivity_severity, "
        "source_id, abstraction_tier2, abstraction_tier3 "
        "FROM personal_fields WHERE 1=1"
    )
    params: list = []

    if source_id is not None:
        query += " AND source_id = ?"
        params.append(source_id)
    if sensitivity is not None:
        query += " AND sensitivity = ?"
        params.append(sensitivity)

    query += " ORDER BY field_name"

    with open_personal_db(user_id, persona_id, key_hex) as db:
        rows = db.execute(query, params).fetchall()

    return [
        PersonalField(
            field_name=row["field_name"],
            field_value=row["field_value"],
            sensitivity=row["sensitivity"],
            sensitivity_severity=row["sensitivity_severity"],
            source_id=row["source_id"],
            abstraction_tier2=row["abstraction_tier2"],
            abstraction_tier3=row["abstraction_tier3"],
        )
        for row in rows
    ]


# -- Personal fields (write) --------------------------------------------------

def save_personal_field(
    user_id: str,
    persona_id: str,
    key_hex: str,
    field_name: str,
    field_value: str,
    sensitivity: Literal["general", "personal", "medical", "financial"],
    source_id: str = "personal-specialist",
    ownership_scope: Literal["self", "group", "instance"] = "self",
    abstraction_tier2: Literal[
        "pass", "omit", "summarize", "range_only", "not_permitted"
    ] = "pass",
    abstraction_tier3: Literal[
        "pass", "omit", "summarize", "range_only", "not_permitted"
    ] = "pass",
    source: str = "interview",
    extra_metadata: dict | None = None,
) -> str:
    """
    Insert or update a personal field in personal.db.
    Returns field id (UUID string — existing id if field_name already exists).

    sensitivity_severity is intentionally omitted from the UPDATE statement:
    it is a GENERATED ALWAYS column in SQLite (computed from sensitivity).
    Updating sensitivity is sufficient — the DB recomputes severity automatically.
    """
    sensitivity_severity = {
        "general": 1, "personal": 2, "medical": 3, "financial": 4
    }[sensitivity]

    # Enforce instance-scope sensitivity ceiling
    if ownership_scope == "instance" and sensitivity_severity > INSTANCE_SCOPE_MAX_SEVERITY:
        raise ValueError(
            f"Instance-scoped fields may not have sensitivity '{sensitivity}'. "
            f"Only 'general' or 'personal' are permitted at instance scope."
        )

    # Short-field warning for medical/financial fields only.
    if len(field_value) < MIN_MATCH_LENGTH and sensitivity_severity >= 3:
        msg = (
            f"Personal field '{field_name}' ({sensitivity}) has a value shorter "
            f"than {MIN_MATCH_LENGTH} chars. PG_GATE_2 cannot detect short values "
            f"in model responses. Consider abstraction_tier2='omit' or using a "
            f"longer representation."
        )
        warnings.warn(msg, UserWarning, stacklevel=2)
        log.warning("short-field write: %s", msg)

    metadata_json = json.dumps(extra_metadata or {})
    timestamp = now()

    with open_personal_db(user_id, persona_id, key_hex) as db:
        existing = db.execute(
            "SELECT id FROM personal_fields WHERE field_name = ?",
            [field_name]
        ).fetchone()

        if existing:
            field_id = existing["id"]
            # sensitivity_severity intentionally omitted — GENERATED ALWAYS column.
            db.execute(
                """UPDATE personal_fields SET
                   field_value = ?, sensitivity = ?,
                   source_id = ?, ownership_scope = ?,
                   abstraction_tier2 = ?, abstraction_tier3 = ?,
                   source = ?, updated_at = ?, extra_metadata = ?
                   WHERE id = ?""",
                [
                    field_value, sensitivity,
                    source_id, ownership_scope,
                    abstraction_tier2, abstraction_tier3,
                    source, timestamp, metadata_json,
                    field_id,
                ]
            )
        else:
            field_id = str(uuid.uuid4())
            db.execute(
                """INSERT INTO personal_fields
                   (id, source_id, field_name, field_value, sensitivity,
                    ownership_scope, abstraction_tier2, abstraction_tier3,
                    source, created_at, updated_at, extra_metadata)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                [
                    field_id, source_id, field_name, field_value,
                    sensitivity, ownership_scope,
                    abstraction_tier2, abstraction_tier3,
                    source, timestamp, timestamp, metadata_json,
                ]
            )

    return field_id


def delete_personal_field(
    user_id: str,
    persona_id: str,
    key_hex: str,
    field_name: str,
) -> bool:
    """
    Securely delete a personal field.
    Deletion sequence: zero field_value first -> then delete record.
    Returns True if found and deleted, False if field_name not found.
    """
    with open_personal_db(user_id, persona_id, key_hex) as db:
        existing = db.execute(
            "SELECT id FROM personal_fields WHERE field_name = ?",
            [field_name]
        ).fetchone()

        if not existing:
            return False

        # Step 1: zero the value before deletion.
        db.execute(
            "UPDATE personal_fields SET field_value = '', updated_at = ? "
            "WHERE field_name = ?",
            [now(), field_name]
        )

        # Step 2: delete the now-zeroed record.
        db.execute(
            "DELETE FROM personal_fields WHERE field_name = ?",
            [field_name]
        )

    return True


# -- Export -------------------------------------------------------------------

def export_personal_fields(
    user_id: str,
    persona_id: str,
    key_hex: str,
    source_id: str | None = None,
) -> list[dict]:
    """
    Export personal field metadata with sensitivity ceiling enforcement.
    Returns only fields with sensitivity_severity <= EXPORT_SENSITIVITY_CEILING (2).
    Medical and financial fields are NEVER exported — system invariant.

    Returns list of dicts (not PersonalField objects).
    field_value intentionally excluded — metadata only.
    """
    query = (
        "SELECT field_name, sensitivity, sensitivity_severity, "
        "source_id, abstraction_tier2, abstraction_tier3, "
        "ownership_scope, source, created_at, updated_at "
        "FROM personal_fields "
        "WHERE sensitivity_severity <= ? "
    )
    params: list = [EXPORT_SENSITIVITY_CEILING]

    if source_id is not None:
        query += "AND source_id = ? "
        params.append(source_id)

    query += "ORDER BY field_name"

    with open_personal_db(user_id, persona_id, key_hex) as db:
        rows = db.execute(query, params).fetchall()

    return [
        {
            "export_schema_version": EXPORT_SCHEMA_VERSION,
            "export_semantic": "metadata_only",
            "field_name": row["field_name"],
            "sensitivity": row["sensitivity"],
            "sensitivity_severity": row["sensitivity_severity"],
            "source_id": row["source_id"],
            "abstraction_tier2": row["abstraction_tier2"],
            "abstraction_tier3": row["abstraction_tier3"],
            "ownership_scope": row["ownership_scope"],
            "source": row["source"],
            "created_at": row["created_at"],
            "updated_at": row["updated_at"],
        }
        for row in rows
    ]


# -- Voice profile value validation -------------------------------------------

def _validate_voice_profile_value(attribute: str, value: str) -> None:
    """
    Validate a voice profile value at write time (D5-151).
    Raises ValueError with plain-language message on rejection.
    """
    normalized = value.strip()
    word_count = len(normalized.split())
    value_hash = hashlib.sha256(normalized.encode()).hexdigest()[:16]

    if word_count > VOICE_VALUE_MAX_WORDS:
        log.warning(
            "voice_profile write rejected: value too long "
            "attribute=%s word_count=%d hash=%s",
            attribute, word_count, value_hash,
        )
        raise ValueError(_VOICE_VALUE_REJECTION_MSG)

    for pattern, reason in _VOICE_VALUE_PII_PATTERNS:
        if pattern.search(normalized):
            log.warning(
                "voice_profile write rejected: %s detected "
                "attribute=%s hash=%s",
                reason, attribute, value_hash,
            )
            raise ValueError(_VOICE_VALUE_REJECTION_MSG)


# -- Voice profile (write) ----------------------------------------------------

def save_voice_profile_entry(
    user_id: str,
    persona_id: str,
    key_hex: str,
    attribute: str,
    value: str,
    precedence: int,
    source_id: str | None = None,
    extra_metadata: dict | None = None,
) -> str:
    """
    Write a voice profile entry at the specified precedence level.
    Precedence: 1=model_baseline (lowest) -> 5=writing_context (highest).

    space_id storage rule renamed to persona_id storage rule:
        precedence 3 (global): stored with persona_id=NULL.
        all other precedences: stored with persona_id=this life.

    Returns entry id (UUID). Upserts on composite key:
        (stored_persona_id, source_id, precedence, attribute).
    """
    _validate_voice_profile_value(attribute, value)

    if not 1 <= precedence <= 5:
        raise ValueError(
            f"Voice profile precedence must be 1-5, got {precedence}."
        )

    # Global entries (precedence 3) store NULL persona_id to match query
    # in _resolve_voice_profile: "WHERE persona_id = ? OR persona_id IS NULL"
    stored_persona_id = None if precedence == 3 else persona_id
    metadata_json = json.dumps(extra_metadata or {})
    timestamp = now()

    with open_personal_db(user_id, persona_id, key_hex) as db:
        existing = db.execute(
            "SELECT id FROM voice_profiles "
            "WHERE (persona_id = ? OR (persona_id IS NULL AND ? IS NULL)) "
            "AND (source_id = ? OR (source_id IS NULL AND ? IS NULL)) "
            "AND precedence = ? AND attribute = ?",
            [
                stored_persona_id, stored_persona_id,
                source_id, source_id,
                precedence, attribute,
            ]
        ).fetchone()

        if existing:
            entry_id = existing["id"]
            db.execute(
                "UPDATE voice_profiles SET value = ?, updated_at = ?, "
                "extra_metadata = ? WHERE id = ?",
                [value, timestamp, metadata_json, entry_id]
            )
        else:
            entry_id = str(uuid.uuid4())
            db.execute(
                """INSERT INTO voice_profiles
                   (id, persona_id, source_id, precedence,
                    attribute, value, created_at, updated_at, extra_metadata)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                [
                    entry_id, stored_persona_id, source_id,
                    precedence, attribute, value,
                    timestamp, timestamp, metadata_json,
                ]
            )

    return entry_id


def delete_voice_profile_entry(
    user_id: str,
    persona_id: str,
    key_hex: str,
    attribute: str,
    precedence: int,
    source_id: str | None = None,
) -> bool:
    """
    Delete a voice profile entry by attribute + precedence.
    Returns True if found and deleted, False if not found.
    """
    stored_persona_id = None if precedence == 3 else persona_id

    with open_personal_db(user_id, persona_id, key_hex) as db:
        existing = db.execute(
            "SELECT id FROM voice_profiles "
            "WHERE (persona_id = ? OR (persona_id IS NULL AND ? IS NULL)) "
            "AND (source_id = ? OR (source_id IS NULL AND ? IS NULL)) "
            "AND precedence = ? AND attribute = ?",
            [
                stored_persona_id, stored_persona_id,
                source_id, source_id,
                precedence, attribute,
            ]
        ).fetchone()

        if not existing:
            return False

        db.execute(
            "DELETE FROM voice_profiles WHERE id = ?",
            [existing["id"]]
        )

    return True
