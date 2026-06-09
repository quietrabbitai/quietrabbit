#!/usr/bin/env python3
# scripts/init_db.py
# First-run database initialization.
# Creates the directory structure and runs initial migrations for
# instance-level databases (shared.db and scores.db).
# Per-user databases (personal.db, outputs.db, keys.db) are initialized
# on first login, not here.
#
# Usage: python scripts/init_db.py
# Run once on first start, or after a clean data root wipe.
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   community_artifacts/paths/ → focuses/
#   community_artifacts/specialists/ → guides/ and operators/

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from providers.utils import get_data_root
from providers.errors import DatabaseMigrationError


def create_directory_structure(data_root: Path) -> None:
    """Create the QR data root directory structure."""
    dirs = [
        data_root / "instance",
        data_root / "users",
        data_root / "models",
        data_root / "logs",
        data_root / "cache" / "last_known_good",
        data_root / "config",
        data_root / "sessions",
        data_root / "community_artifacts" / "focuses",
        data_root / "community_artifacts" / "guides",
        data_root / "community_artifacts" / "operators",
        data_root / "community_artifacts" / "integrations",
    ]
    for d in dirs:
        d.mkdir(parents=True, exist_ok=True)
    print(f"  Directory structure: OK ({data_root})")


def verify_data_root_writable(data_root: Path) -> None:
    """Verify the data root is writable before proceeding."""
    sentinel = data_root / ".init_check"
    try:
        sentinel.write_text("ok")
        sentinel.unlink()
    except OSError as e:
        print(
            f"ERROR: Data root is not writable: {data_root}\n"
            f"  {e}\n"
            f"  Check permissions and QR_LOCAL_DATA setting.",
            file=sys.stderr
        )
        sys.exit(1)


def main():
    print("Quiet Rabbit — Database Initialization")
    print("=" * 40)

    data_root = get_data_root()
    print(f"Data root: {data_root}")

    if not data_root.exists():
        print(f"Creating data root: {data_root}")
        data_root.mkdir(parents=True, exist_ok=True)

    verify_data_root_writable(data_root)

    print("\nCreating directory structure...")
    create_directory_structure(data_root)

    print("\nInitializing instance databases...")
    from persistence.migrations import migrate_shared_db, migrate_scores_db

    try:
        n = migrate_shared_db()
        print(f"  shared.db: OK ({n} migration(s) applied)")
    except DatabaseMigrationError as e:
        print(f"ERROR: shared.db migration failed: {e}", file=sys.stderr)
        sys.exit(1)

    try:
        n = migrate_scores_db()
        print(f"  scores.db: OK ({n} migration(s) applied)")
    except DatabaseMigrationError as e:
        print(f"ERROR: scores.db migration failed: {e}", file=sys.stderr)
        sys.exit(1)

    print("\nVerifying SQLCipher...")
    try:
        from sqlcipher3 import dbapi2 as sqlite3
        conn = sqlite3.connect(":memory:")
        conn.execute("PRAGMA key='test'")
        version = conn.execute("PRAGMA cipher_version").fetchone()[0]
        conn.close()
        print(f"  SQLCipher: OK ({version})")
    except Exception as e:
        print(f"ERROR: SQLCipher verification failed: {e}", file=sys.stderr)
        sys.exit(1)

    print("\nInitialization complete.")
    print("Next: run scripts/generate_manifest.py if taxonomy files have changed.")


if __name__ == "__main__":
    main()
