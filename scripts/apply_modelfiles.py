#!/usr/bin/env python3
# scripts/apply_modelfiles.py
# Apply QR Modelfiles to Ollama.
# Run at startup if Modelfile versions are stale.
# Can also be run manually during development.
#
# Modelfiles live in app/modelfiles/ in the Docker image.
# Each Modelfile must contain: # QR-MODELFILE-VERSION: {version}
# Filename convention: {model-name}-v{version}.Modelfile
# Colons in model names become hyphens: llama3.2:3b -> llama3.2-3b-v1.0.Modelfile
#
# Usage: python scripts/apply_modelfiles.py

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from providers.ollama_client import (
    apply_modelfile,
    check_modelfile_version,
    check_ollama_health,
)

# Expected Modelfile versions — update when Modelfiles change
EXPECTED_VERSIONS = {
    "llama3.2:3b": "1.0",
    "llama3.1:8b": "1.0",
    "qwen2.5:7b":  "1.0",
}


def _safe_name(model_id: str) -> str:
    """Convert model_id to filesystem-safe name. llama3.2:3b -> llama3.2-3b"""
    return model_id.replace(":", "-")


def find_modelfile(
    model_id: str,
    version: str,
    modelfiles_dir: Path,
) -> Path | None:
    """
    Find Modelfile for a given model and version.
    Primary: {safe_name}-v{version}.Modelfile
    Fallback: any file matching {safe_name}*.Modelfile in directory.
    Returns None if not found.
    """
    safe = _safe_name(model_id)

    # Primary — exact version match
    exact = modelfiles_dir / f"{safe}-v{version}.Modelfile"
    if exact.exists():
        return exact

    # Fallback — any Modelfile for this model
    candidates = sorted(modelfiles_dir.glob(f"{safe}*.Modelfile"))
    if candidates:
        return candidates[-1]   # newest by name sort

    return None


def main():
    print("Quiet Rabbit — Apply Modelfiles")
    print("=" * 40)

    # Verify Ollama is reachable
    health = check_ollama_health()
    if health.status == "unavailable":
        print(
            f"ERROR: Ollama is not reachable ({health.error})",
            file=sys.stderr
        )
        print("Start Ollama and try again.", file=sys.stderr)
        sys.exit(1)

    print(f"Ollama: {health.status} ({len(health.available_models)} models)")

    # Locate modelfiles directory
    repo_root = Path(__file__).parent.parent
    modelfiles_dir = repo_root / "app" / "modelfiles"

    if not modelfiles_dir.exists():
        print(
            f"ERROR: Modelfiles directory not found: {modelfiles_dir}",
            file=sys.stderr
        )
        sys.exit(1)

    applied = 0
    skipped = 0
    failed = 0

    for model_id, expected_version in EXPECTED_VERSIONS.items():
        version_info = check_modelfile_version(model_id, expected_version)

        if version_info.is_current:
            print(f"  {model_id}: current (v{expected_version})")
            skipped += 1
            continue

        mf_path = find_modelfile(model_id, expected_version, modelfiles_dir)

        if not mf_path:
            print(
                f"  {model_id}: SKIP — no Modelfile found in {modelfiles_dir}",
                file=sys.stderr
            )
            skipped += 1
            continue

        print(
            f"  {model_id}: applying v{expected_version} "
            f"(was: {version_info.applied_version or 'not applied'}) "
            f"from {mf_path.name}..."
        )

        success = apply_modelfile(model_id, mf_path)

        if success:
            # Re-verify after apply to confirm success
            recheck = check_modelfile_version(model_id, expected_version)
            if recheck.is_current:
                print(f"  {model_id}: OK (verified)")
                applied += 1
            else:
                print(
                    f"  {model_id}: FAILED — version mismatch after apply",
                    file=sys.stderr
                )
                failed += 1
        else:
            print(f"  {model_id}: FAILED", file=sys.stderr)
            failed += 1

    print(f"\nDone. Applied: {applied}  Skipped: {skipped}  Failed: {failed}")

    if failed:
        sys.exit(1)


if __name__ == "__main__":
    main()
