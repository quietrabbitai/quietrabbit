# persistence/migrations.py
# Database migration runner.
# Applies SQL migration files to a target database in version order.
# Uses migration_lock to prevent concurrent migrations on the same database.
# All databases (shared, personal, outputs, keys, scores) use this runner.
#
# SECURITY NOTE: migrate_personal_db, migrate_outputs_db, and migrate_keys_db
# accept key_hex as an argument. This function must NEVER be called from a
# context where arguments are logged (e.g. debug decorators, tracing).
# QR_ENV=development does NOT log function arguments — this is enforced
# in the logging configuration, not here.
#
# ATOMICITY NOTE: executescript() issues an implicit COMMIT before execution,
# which invalidates any active SAVEPOINT. We execute statements individually
# within SAVEPOINT scope to preserve true rollback guarantees.
#
# SCHEMA AUTHORING RULE: do not use semicolons inside string literals in .sql
# files (e.g. DEFAULT 'foo;bar'). _parse_statements() strips SQL comments
# before splitting on semicolons, and handles CREATE TRIGGER blocks atomically,
# but is not a general-purpose SQL parser. String literals containing semicolons
# will be split incorrectly.
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   spaces/ → lives/ in path construction, space_id → life_id in signatures
# Updated as part of Phase C Persona model migration (D6-298):
#   lives/ → personas/ in path construction, life_id → persona_id in signatures
# Current convention: /users/{user_id}/personas/{persona_id}/

from __future__ import annotations

import os
import socket
from pathlib import Path

from providers.utils import get_data_root, now


SCHEMA_DIR = Path(__file__).parent / "schema"


def get_migration_files(prefix: str) -> list[tuple[int, Path]]:
    """
    Return sorted list of (version, path) for all migration files
    matching prefix_NNN.sql in the schema directory.
    """
    files = sorted(SCHEMA_DIR.glob(f"{prefix}_*.sql"))
    result = []
    for f in files:
        try:
            version = int(f.stem.split("_")[-1])
            result.append((version, f))
        except ValueError:
            continue
    return result


def get_applied_version(conn) -> int:
    """Return the highest migration version applied to this database."""
    try:
        row = conn.execute(
            "SELECT MAX(version) FROM schema_version"
        ).fetchone()
        return row[0] if row and row[0] is not None else 0
    except Exception:
        return 0


def schema_version_exists(db_path: Path, key_hex: str | None = None) -> bool:
    """
    Return True if the schema_version table exists in the database at db_path.

    Used by DB openers to detect fresh (uninitialized) databases before
    running auto-migration. Opens and closes its own short-lived connection
    so the caller connection is always opened against the final schema.

    Returns False if the file does not exist (fast path — no connection opened).
    Returns False on any connection or query error (treated as uninitialised).
    """
    if not db_path.exists():
        return False
    conn = _open_raw(db_path)
    try:
        if key_hex:
            conn.execute(f"PRAGMA key = \"x'{key_hex}'\"")
        row = conn.execute(
            "SELECT name FROM sqlite_master "
            "WHERE type='table' AND name='schema_version'"
        ).fetchone()
        return row is not None
    except Exception:
        return False
    finally:
        conn.close()


def _bootstrap_lock_table(conn) -> None:
    """
    Create migration_lock and seed row atomically.
    Wrapped in a SAVEPOINT so both operations succeed or fail together.
    Safe to call on already-migrated databases — IF NOT EXISTS and
    INSERT OR IGNORE are both no-ops when the table and row already exist.
    """
    conn.execute("SAVEPOINT bootstrap_lock")
    try:
        conn.execute("""
            CREATE TABLE IF NOT EXISTS migration_lock (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                locked_at TEXT,
                locked_by TEXT
            )
        """)
        conn.execute("INSERT OR IGNORE INTO migration_lock (id) VALUES (1)")
        conn.execute("RELEASE bootstrap_lock")
    except Exception:
        try:
            conn.execute("ROLLBACK TO bootstrap_lock")
        except Exception:
            pass
        raise
    conn.commit()


def acquire_lock(conn) -> bool:
    """
    Acquire migration_lock. SQLite write serialization ensures
    the UPDATE is atomic — no race condition possible.
    Returns True if lock acquired, False if already locked.
    """
    process_id = f"{socket.gethostname()}:{os.getpid()}"
    try:
        conn.execute(
            "UPDATE migration_lock SET locked_at = ?, locked_by = ? "
            "WHERE id = 1 AND locked_at IS NULL",
            [now(), process_id]
        )
        conn.commit()
        row = conn.execute(
            "SELECT locked_by FROM migration_lock WHERE id = 1"
        ).fetchone()
        return row and row[0] == process_id
    except Exception:
        return False


def release_lock(conn) -> None:
    """Release migration_lock unconditionally."""
    try:
        conn.execute(
            "UPDATE migration_lock "
            "SET locked_at = NULL, locked_by = NULL WHERE id = 1"
        )
        conn.commit()
    except Exception:
        pass


def _parse_statements(sql: str) -> list[str]:
    """
    Split SQL file into individual statements for execution.
    Strips comment lines before parsing to avoid false splits on
    semicolons inside SQL comments.

    Handles CREATE TRIGGER blocks correctly — BEGIN...END inside a trigger
    body contains semicolons that must NOT be treated as statement terminators.

    CONSTRAINT: do not use semicolons inside string literals in .sql files.
    """
    stripped_lines = [
        line for line in sql.splitlines()
        if line.strip() and not line.strip().startswith("--")
    ]
    stripped_sql = "\n".join(stripped_lines)

    statements = []
    current: list[str] = []
    in_trigger = False

    for line in stripped_sql.splitlines():
        upper = line.strip().upper()

        if upper.startswith("CREATE TRIGGER") or \
           upper.startswith("CREATE OR REPLACE TRIGGER"):
            in_trigger = True

        current.append(line)

        if in_trigger:
            if upper in ("END", "END;"):
                stmt = "\n".join(current).strip()
                if stmt:
                    statements.append(stmt)
                current = []
                in_trigger = False
        else:
            if line.rstrip().endswith(";"):
                stmt = "\n".join(current).rstrip().rstrip(";").strip()
                if stmt:
                    statements.append(stmt)
                current = []

    remainder = "\n".join(current).strip()
    if remainder:
        statements.append(remainder)

    return statements


def run_migrations(
    conn,
    prefix: str,
    key_hex: str | None = None,
) -> int:
    """
    Apply all pending migrations for the given prefix to conn.
    If key_hex is provided, applies PRAGMA key before any operations.
    Returns number of migrations applied.
    Raises DatabaseMigrationError on failure.
    """
    from providers.errors import DatabaseMigrationError

    if key_hex:
        conn.execute(f"PRAGMA key = \"x'{key_hex}'\"")

    network_storage = (
        os.environ.get("QR_NETWORK_STORAGE", "false").lower() == "true"
    )
    if network_storage:
        conn.execute("PRAGMA journal_mode=DELETE")
    else:
        conn.execute("PRAGMA journal_mode=WAL")

    conn.execute("PRAGMA busy_timeout=5000")

    _bootstrap_lock_table(conn)

    if not acquire_lock(conn):
        raise DatabaseMigrationError(
            db_path="unknown",
            plain_language=(
                "Quiet Rabbit is already starting up in another process. "
                "Please wait a moment and try again."
            )
        )

    try:
        current_version = get_applied_version(conn)
        migrations = get_migration_files(prefix)
        applied = 0

        for version, path in migrations:
            if version <= current_version:
                continue

            sql = path.read_text()
            savepoint = f"migration_v{version}"
            statements = _parse_statements(sql)

            try:
                conn.execute(f"SAVEPOINT {savepoint}")

                for stmt in statements:
                    conn.execute(stmt)

                conn.execute(f"RELEASE {savepoint}")
                applied += 1

            except Exception as e:
                try:
                    conn.execute(f"ROLLBACK TO {savepoint}")
                except Exception:
                    pass
                raise DatabaseMigrationError(
                    db_path=str(path),
                    plain_language=(
                        "Quiet Rabbit couldn't finish setting up. "
                        "Your data is safe. [Get help]"
                    )
                ) from e

        result = conn.execute("PRAGMA integrity_check").fetchone()
        if not result or result[0] != "ok":
            raise DatabaseMigrationError(
                db_path=prefix,
                plain_language=(
                    "Quiet Rabbit found a problem with its database. "
                    "Your data may need attention. [Get help]"
                )
            )

        return applied

    finally:
        release_lock(conn)


# -- Typed migration helpers --------------------------------------------------

def _open_raw(path: Path):
    """Open a SQLCipher connection without context manager (caller closes)."""
    from sqlcipher3 import dbapi2 as sqlite3
    return sqlite3.connect(str(path))


def migrate_shared_db() -> int:
    """Migrate instance/shared.db (unencrypted — readable before login)."""
    data_root = get_data_root()
    db_path = data_root / "instance" / "shared.db"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = _open_raw(db_path)
    try:
        return run_migrations(conn, prefix="shared")
    finally:
        conn.close()


def migrate_personal_db(user_id: str, persona_id: str, key_hex: str) -> int:
    """Migrate a user's personal.db (encrypted)."""
    data_root = get_data_root()
    db_path = (
        data_root / "users" / user_id / "personas" / persona_id / "personal.db"
    )
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = _open_raw(db_path)
    try:
        return run_migrations(conn, prefix="personal", key_hex=key_hex)
    finally:
        conn.close()


def migrate_outputs_db(user_id: str, persona_id: str, key_hex: str) -> int:
    """Migrate a user's outputs.db (encrypted)."""
    data_root = get_data_root()
    db_path = (
        data_root / "users" / user_id / "personas" / persona_id / "outputs.db"
    )
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = _open_raw(db_path)
    try:
        return run_migrations(conn, prefix="outputs", key_hex=key_hex)
    finally:
        conn.close()


def migrate_keys_db(user_id: str, key_hex: str) -> int:
    """Migrate a user's integration_keys.db (encrypted)."""
    data_root = get_data_root()
    db_path = data_root / "users" / user_id / "integration_keys.db"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = _open_raw(db_path)
    try:
        return run_migrations(conn, prefix="keys", key_hex=key_hex)
    finally:
        conn.close()


def migrate_scores_db() -> int:
    """Migrate models/scores.db (unencrypted, per-instance hardware metrics)."""
    data_root = get_data_root()
    db_path = data_root / "models" / "scores.db"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = _open_raw(db_path)
    try:
        return run_migrations(conn, prefix="scores")
    finally:
        conn.close()


def migrate_domain_context_db(
    user_id: str, persona_id: str, focus_id: str, key_hex: str
) -> int:
    """
    Migrate a focus's domain_context.db (encrypted).
    Path resolved via topic_store.get_domain_context_path() — single canonical source.
    Directory created by ensure_focus_dirs() before this is called —
    mkdir here as a safety net for direct calls.
    Part of Phase B data model extension (D6-226+).
    """
    from persistence.topic_store import get_domain_context_path
    db_path = get_domain_context_path(user_id, persona_id, focus_id)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = _open_raw(db_path)
    try:
        return run_migrations(conn, prefix="domain_context", key_hex=key_hex)
    finally:
        conn.close()


def migrate_plan_state_db(
    user_id: str, persona_id: str, focus_id: str, topic_id: str, key_hex: str
) -> int:
    """
    Migrate a topic's plan_state.db (encrypted).
    Path resolved via topic_store.get_plan_state_path() — single canonical source.
    Directory created by ensure_focus_dirs() before this is called —
    mkdir here as a safety net for direct calls.
    Part of Phase B data model extension (D6-226+).
    """
    from persistence.topic_store import get_plan_state_path
    db_path = get_plan_state_path(user_id, persona_id, focus_id, topic_id)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = _open_raw(db_path)
    try:
        return run_migrations(conn, prefix="plan_state", key_hex=key_hex)
    finally:
        conn.close()


def migrate_focus_storage(
    user_id: str, persona_id: str, focus_id: str, topic_id: str, key_hex: str
) -> tuple[int, int]:
    """
    Convenience helper — migrate both focus-level databases in one call.
    Runs domain_context_db migration then plan_state_db migration.
    Returns (domain_context_migrations_applied, plan_state_migrations_applied).
    Part of Phase B data model extension (D6-226+).
    """
    dc = migrate_domain_context_db(user_id, persona_id, focus_id, key_hex)
    ps = migrate_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex)
    return dc, ps
