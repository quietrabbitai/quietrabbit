# archive/

This directory contains files from the Python/Flask backend era of Quiet Rabbit,
retained for historical reference. They are not part of the Rust/Tauri build and
are not executed by any current code path.

The Python backend was retired in item 16l (D6-339). The Rust Conductor in
src-tauri/ is the sole backend going forward.

---

## Dockerfile

The Docker build file for the Python/Flask backend (qr-conductor container).
Built a python:3.12-slim image with SQLCipher dependencies, copied all Python
source directories, ran generate_manifest.py at build time, and launched the
app via `python -m ui`.

Replaced by: the Tauri binary, which runs natively on the user's machine.
No Docker container is used in the Rust/Tauri architecture.

## docker-compose.yml

Orchestrated the qr-conductor container with environment variable configuration
for Ollama host, data root, network storage mode, and dev/production flags.
Supported three deployment topologies: single-machine, separate inference machine,
and NAS/network storage.

Replaced by: Tauri app bundle. Configuration will move to a Tauri-managed config
file. The LXC/Docker deployment is retired.

## interview.py

CLI Personal Specialist install interview. Proved personal field and voice profile
storage end-to-end before the UI existed (Layer 5 dev tool). Collected name,
location, job, dietary, financial, and communication preference fields directly
into personal.db using QR_DEV_KEY_HEX.

Replaced by: the onboarding screens in the Tauri/SPA frontend (Screen 1–5 flow,
item 16l and beyond). The IPC command surface handles personal field writes via
`save_personal_field` and `save_voice_profile`.

## apply_modelfiles.py

Applied QR Modelfiles to Ollama at container startup or manually during development.
Read Modelfiles from app/modelfiles/, checked version headers, and called the
Ollama API to register custom model variants.

Replaced by: app/modelfiles/ are still present and Ollama still requires them, but
the application of Modelfiles will be handled by the Rust Ollama client
(src-tauri/src/providers/ollama_client.rs) or a startup script outside Docker.
No direct replacement exists yet in Release 1 — see apply_modelfiles.py for the
version-header convention and Modelfile naming scheme.

## extract_golden_vectors.py

Generated Gate1–4 golden test vectors by invoking the live Python PrivacyGateway
against a temporary SQLCipher database. Output JSON files to
src-tauri/tests/golden/gate{1,2,3,4}.json, which are the ground truth for the
Rust gate parity test suite (203 tests).

No direct replacement: this script's job is done. The golden vectors are committed
to the repo and the Rust gate implementation is verified against them. If gate
logic ever changes, this script (or a Rust equivalent) would need to be re-run
against the new oracle.

## taxonomy/ (directory)

Five YAML files defining the domain knowledge layer that underpins the Python
conductor's routing and capability decisions:

- **task_types.yaml** — enumeration of task type identifiers used in step
  routing decisions.
- **sensitivity_levels.yaml** — numeric sensitivity level definitions used by
  Gate2 and the signal taxonomy.
- **routing_table.yaml** — maps task types and sensitivity levels to execution
  tier recommendations.
- **output_types.yaml** — enumeration of valid output_type values (quick_ask,
  document, research, etc.).
- **signal_taxonomy.yaml** — PII signal categories and their associated
  sensitivity scores, consumed by Gate2 scanning.

These files were loaded by the Python conductor at runtime. The Rust Conductor
has no equivalent loader in Release 1 — routing logic is currently hardcoded in
the Rust evaluation and conductor layers.

Needed when: routing and capability profile are formally ported in Layer 7+.
These files are the authoritative domain definitions and should be the source of
truth when a Rust taxonomy loader is introduced.
