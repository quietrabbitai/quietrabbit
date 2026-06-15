# providers/utils.py
# Shared utilities — imported by all other modules.
#
# DATABASE ACCESS CONTRACTS:
# open_db()                  — UNENCRYPTED databases only (shared.db, scores.db)
#                              Sets journal mode. No PRAGMA key.
# open_personal_db()         — Encrypted per-user databases
# open_outputs_db()          — Encrypted per-user databases
# open_integration_keys_db() — Encrypted per-user databases
#
# For encrypted databases, PRAGMA key MUST be the first operation after
# connection open (SQLCipher requirement). Journal mode is set AFTER key.
# Never use open_db() for encrypted databases.
#
# Path construction: /users/{user_id}/personas/{persona_id}/
# Updated as part of Phase A codebase rename (D6-224, D6-225).
# Updated as part of Phase C Persona model migration (D6-298):
#   open_personal_db and open_outputs_db: lives/{life_id} -> personas/{persona_id}

from __future__ import annotations

import os
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path

from sqlcipher3 import dbapi2 as sqlite3


def now() -> str:
    """UTC timestamp in ISO 8601 format. Used throughout QR."""
    return datetime.now(timezone.utc).isoformat()


def get_data_root() -> Path:
    """
    Returns QR_DATA_ROOT as a Path.
    Falls back to ./quietrabbit-data for Topology A (zero-config).
    """
    return Path(os.environ.get("QR_DATA_ROOT", "./quietrabbit-data"))


def _apply_journal_mode(conn) -> None:
    """
    Set journal mode based on QR_NETWORK_STORAGE.
    Must be called AFTER PRAGMA key for encrypted databases.
    """
    network_storage = (
        os.environ.get("QR_NETWORK_STORAGE", "false").lower() == "true"
    )
    if network_storage:
        conn.execute("PRAGMA journal_mode=DELETE")   # rollback journal — NAS safe
    else:
        conn.execute("PRAGMA journal_mode=WAL")


@contextmanager
def open_db(path: Path | str):
    """
    Open an UNENCRYPTED SQLCipher database with explicit lifecycle management.
    Use ONLY for shared.db and scores.db — not for per-user encrypted databases.
    Sets journal mode before yielding. No PRAGMA key applied.
    """
    conn = sqlite3.connect(str(path))
    conn.row_factory = sqlite3.Row
    _apply_journal_mode(conn)   # safe — no key needed for unencrypted
    try:
        yield conn
        conn.commit()
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.close()


@contextmanager
def open_instance_db():
    """
    Context manager for instance/shared.db (unencrypted).
    Must be readable before any user logs in — no encryption key required.
    """
    path = get_data_root() / "instance" / "shared.db"
    with open_db(path) as db:
        yield db


@contextmanager
def open_integration_keys_db(user_id: str, key_hex: str):
    """
    Context manager for a user's integration_keys.db (encrypted).
    PRAGMA key applied first (SQLCipher requirement), then journal mode.
    key_hex: master key hex string from InMemoryKeyRegistry.
    """
    path = get_data_root() / "users" / user_id / "integration_keys.db"
    path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(str(path))
    conn.row_factory = sqlite3.Row
    conn.execute(f"PRAGMA key = \"x'{key_hex}'\"")   # key FIRST
    _apply_journal_mode(conn)                          # journal mode AFTER key
    try:
        yield conn
        conn.commit()
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.close()


@contextmanager
def open_personal_db(user_id: str, persona_id: str, key_hex: str):
    """
    Context manager for a user's personal.db (encrypted).
    PRAGMA key applied first, then journal mode.
    Path: /users/{user_id}/personas/{persona_id}/personal.db
    """
    path = (
        get_data_root() / "users" / user_id / "personas" / persona_id / "personal.db"
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(str(path))
    conn.row_factory = sqlite3.Row
    conn.execute(f"PRAGMA key = \"x'{key_hex}'\"")
    _apply_journal_mode(conn)
    try:
        yield conn
        conn.commit()
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.close()


@contextmanager
def open_outputs_db(user_id: str, persona_id: str, key_hex: str):
    """
    Context manager for a user's outputs.db (encrypted).
    PRAGMA key applied first, then journal mode.
    Path: /users/{user_id}/personas/{persona_id}/outputs.db
    """
    path = (
        get_data_root() / "users" / user_id / "personas" / persona_id / "outputs.db"
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(str(path))
    conn.row_factory = sqlite3.Row
    conn.execute(f"PRAGMA key = \"x'{key_hex}'\"")
    _apply_journal_mode(conn)
    try:
        yield conn
        conn.commit()
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.close()
