#!/usr/bin/env python3
# scripts/generate_manifest.py
# Regenerate app/taxonomy/manifest.yaml from current taxonomy file checksums.
#
# Run after any taxonomy file edit during development.
# Runs automatically during Docker image build.
# Never needed in production — manifest is baked into the image.
#
# Usage: python scripts/generate_manifest.py

from __future__ import annotations

import hashlib
import os
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

import yaml

QR_VERSION = "0.1"

TAXONOMY_FILES = [
    "task_types.yaml",
    "sensitivity_levels.yaml",
    "routing_table.yaml",
    "output_types.yaml",
    "signal_taxonomy.yaml",
]


def find_taxonomy_path() -> Path:
    """
    Resolve taxonomy directory regardless of working directory.
    Supports running from repo root, scripts/, or inside Docker (/app).
    """
    script_dir = Path(__file__).parent
    repo_root = script_dir.parent
    candidate = repo_root / "app" / "taxonomy"
    if candidate.exists():
        return candidate

    # Fallback: check environment override
    env_path = os.environ.get("QR_TAXONOMY_PATH")
    if env_path:
        p = Path(env_path)
        if p.exists():
            return p

    print(
        f"ERROR: Taxonomy directory not found. Tried: {candidate}",
        file=sys.stderr,
    )
    sys.exit(1)


def generate_manifest(taxonomy_path: Path) -> dict:
    manifest = {
        "version": "1.0",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "qr_version": QR_VERSION,
        "files": {},
    }

    missing = []
    for filename in TAXONOMY_FILES:
        path = taxonomy_path / filename
        if not path.exists():
            missing.append(filename)
            continue
        content = path.read_bytes()
        manifest["files"][filename] = {
            "sha256": hashlib.sha256(content).hexdigest(),
            "size_bytes": len(content),
        }

    if missing:
        print(
            f"ERROR: Missing taxonomy files: {', '.join(missing)}",
            file=sys.stderr,
        )
        sys.exit(1)

    return manifest


def main():
    taxonomy_path = find_taxonomy_path()
    manifest = generate_manifest(taxonomy_path)
    output_path = taxonomy_path / "manifest.yaml"

    # Atomic write — prevents corrupt manifest if interrupted
    tmp = output_path.with_suffix(".yaml.tmp")
    try:
        tmp.write_text(yaml.dump(manifest, default_flow_style=False))
        tmp.replace(output_path)
    except Exception as e:
        tmp.unlink(missing_ok=True)
        print(f"ERROR: Failed to write manifest: {e}", file=sys.stderr)
        sys.exit(1)

    print(f"Manifest written: {output_path}")
    for filename, info in manifest["files"].items():
        print(f"  {filename}: {info['sha256'][:16]}...")


if __name__ == "__main__":
    main()
