# Quiet Rabbit — Claude Code Context

## Session Discipline
- Respond with code only. No preamble, no recap, no explanation unless asked.
- Run /compact at natural phase boundaries: end of each file written, before
  integration review, before returning to Chat-PM. Do not wait for auto-compaction.
- When compacting, preserve: open files, current task, architectural decisions,
  unresolved errors, and the next action.
- One task at a time. Draft → Jason approves → write file. Never skip approval.

## Project
Self-hosted privacy-first AI platform. Engine: Conductor. Version: 0.1.
Tagline: "Your personal AI. Simple to start, built to grow, always yours."

## Architecture Reference
/docs/QUIET_RABBIT_ARCHITECTURE.md — authoritative for all decisions.
Read the relevant section before writing code.
This file wins over all other sources on conflicts.
When in doubt: stop and ask rather than invent.

## Core Tenets (non-negotiable)
- Privacy-first: no data leaves local without explicit user consent
- Self-hosted: runs entirely on user hardware
- No telemetry: QR never sends usage data anywhere
- SQLCipher required: system libsqlcipher.so at /usr/lib/ on Garuda.
  Linked via libsqlite3-sys feature flag. PRAGMA key fires before journal_mode
  via SqliteConnectOptions::pragma() insertion order — enforced in every opener.
  Never use sqlx::query!() macros — no static DATABASE_URL in many-small-encrypted-DB topology.
- Master key never persisted: keys live only in Rust AppState/KeyRegistry for the
  duration of the session. Never written to disk, never in IPC responses.
- Tier 2 = user choice: Mistral (EU/GDPR, paid) or Groq (US, free tier).
  Honest trade-off framing. No prescribed default.
- Silent operator: personal context informs output, never narrated.
  Never "Since you mentioned..." or "Based on your preference..."
- Human in the loop: all auto-improvements require explicit approval.
  No silent changes.

## Dev Environment
Repo root:
  /home/kulaga/QuietRabbit/06_GitRepos/quietrabbit-core/

NAS paths:
  Garuda:   /mnt/NAS/QuietRabbitMirror/
  Proxmox:  /mnt/pve/NAS/QuietRabbitMirror/
  LXC:      /mnt/NAS/QuietRabbitMirror/

Services:
  Ollama API:  http://192.168.88.26:11434  (Garuda, ethernet)

Rust dev runs locally on Garuda via cargo.
Docker and the LXC container are retired — do not reference them.

Key commands:
  cargo build                                          — compile check
  cargo test 2>&1 | grep -E "^error|test result"      — test summary
  git branch --show-current                            — verify branch before commit
  git log --oneline rust-migration | head -10          — commit verification

## Ollama (D6-353)
QR checks for a running Ollama instance at 127.0.0.1:11434 on startup.
If found, uses it. If not, starts the bundled Ollama sidecar.
No duplicate model downloads. No contention between instances.
Dev: Ollama runs on Garuda at http://192.168.88.26:11434 — already running, always detected.

## Naming Architecture (Phase A rename — D6-224, D6-225)
All legacy terms retired. Use only the new terms below.

| Old term         | New term         | Notes                              |
|------------------|------------------|------------------------------------|
| Space            | Life             | DB table: lives                    |
| space_id         | life_id          | Column and parameter name          |
| Path             | Focus            | File extension: .focus             |
| path_id          | focus_id         | Column and parameter name          |
| path_run         | focus_run        | DB table: focus_runs               |
| path_run_id      | focus_run_id     | Column and parameter name          |
| path_run_snapshot| focus_run_snap   | DB table: focus_run_snapshots      |
| Specialist       | Guide / Operator | .guide (community), .operator (sys)|
| specialist_id    | source_id        | personal_fields, voice_profiles    |
| quick_draft      | quick_ask        | output_type and focus_id           |
| paths/ dir       | focuses/ dir     | core_artifacts/focuses/            |
| specialists/ dir | guides/ + ops/   | core_artifacts/guides/ + operators/|
| PathRun class    | FocusRun class   | conductor/lifecycle.py             |
| PathDefinition   | FocusDefinition  | conductor/lifecycle.py             |
| path_context     | focus_context    | SYSTEM_TOKENS in tokens.py         |
| space_context    | life_context     | SYSTEM_TOKENS in tokens.py         |
| life_context     | persona_context  | SYSTEM_TOKENS in tokens.py (D6-323)|

Dev constants (routes.py, interview.py):
  _DEV_SPACE_ID = "dev-space"  →  _DEV_LIFE_ID = "dev-life"
  QR_INTERVIEW_SPACE_ID        →  QR_INTERVIEW_LIFE_ID
  SOURCE_ID = "personal-specialist"  (was SPECIALIST_ID)
## Naming Architecture (Phase C rename — D6-298)
All Life terms retired. Use only the new terms below.

| Old term (Phase A)      | New term (Phase C)          | Notes                                         |
|-------------------------|-----------------------------|-----------------------------------------------|
| Life                    | Persona                     | DB table: personas                            |
| life_id                 | persona_id                  | Column and parameter name                     |
| user_lives              | user_personas               | Join table in shared.db                       |
| lives/ (path)           | personas/ (path)            | Filesystem segment                            |
| _DEV_LIFE_ID            | _DEV_PERSONA_ID             | Dev constant (value stays "dev-life")         |
| QR_INTERVIEW_LIFE_ID    | QR_INTERVIEW_PERSONA_ID     | Env var in interview.py                       |
| life_affinity           | (removed)                   | Dropped from FocusDefinition (D6-300)         |
| life_store.py           | persona_store.py            | New file written (Task 6); life_store.py      |
|                         |                             | still on disk as dead code, not yet deleted   |
| (new)                   | focus_settings              | Per-Focus tier + behavior config (D6-299)     |

New stores (Phase C):
  persistence/persona_store.py         -- Persona CRUD (replaces life_store.py)
  persistence/focus_settings_store.py  -- Focus-level settings (new, D6-299)

Dev constants (routes.py, interview.py):
  _DEV_LIFE_ID = "dev-life"     ->  _DEV_PERSONA_ID (value stays "dev-life")
  QR_INTERVIEW_LIFE_ID          ->  QR_INTERVIEW_PERSONA_ID


## Phase 1 Focuses (8 confirmed)
1. Writing Assistant  2. Research & Buy  3. Job Match  4. Tech Support
5. Travel/Vacation Planning  6. Cooking  7. Personal Finance  8. Quick Ask

Quick Ask: full focus (Layer 3), output_type=quick_ask,
  suggest_in_focuses: [writing-assistant, job-match, research-and-buy]
Layer 7 build order: Travel after Job Match and Research & Buy.

## Key Architectural Decisions
- execution_tier = min(focus_settings.max_permitted_tier, focus_def.max_routing_tier, step.routing_tier)
  raw_abstraction = min(focus_settings.privacy_tier, execution_tier)
  abstraction_tier = max(2, raw_abstraction) if execution_tier > 1 else raw_abstraction
  focus_settings.max_permitted_tier is the hard ceiling (moved from Persona to Focus per D6-297).
  focus_settings.privacy_tier is user preference. Never conflate them.
  privacy_tier may reduce abstraction. It must never increase routing authority.
  focus_settings row must exist before AUTHORIZE executes. Creation is the
  responsibility of Focus creation flows. Missing row is a system error,
  not a recoverable condition (D6-303).
- PersonalTrack NEVER serialized to focus_run_snapshots — re-fetched fresh on resume
- Step 6: PG_GATE_1 writes approved/abstracted fields to step_disclosure_buffer
  Step 8: reads from step_disclosure_buffer for Tier 2+ (NEVER from PersonalTrack)
  Raw personal values never appear in external prompt strings.
- Tier 3 is a terminal boundary — execution loop breaks, status=awaiting_user,
  thread released. Never a synchronous inline call.
- auth_enabled=1 ONLY if ALL databases migrate successfully. Partial = rollback.
- focus_run created as status=initializing, promoted to running after Phase 3 only
- Recovery key: BIP39 mnemonic of full 32 bytes. mnemo.to_entropy() gives exact
  original key. No wrapping, no random suffix, no stored blob.
- Release 1/Release 2 = product roadmap. Phase 1-7 = Conductor lifecycle phases.
- SYSTEM_TOKENS frozenset defined in conductor/tokens.py
- open_db() wrapper in providers/utils.py — explicit close. Use everywhere.
  sqlite3 context manager handles transactions only, not connection lifecycle.
- ContextCompactor makes direct local model call. No Librarian, no routing.
  Avoids Conductor<->Librarian circular dependency.
- is_fast_lane boolean on focus_runs — Phase 1 capture only.
  Phase 2 uses for "Promote to focus" Focus Builder pre-population.
- Guide versions: _load_guide_versions() — artifact_type='guide', per focus steps.
  Operator versions: _load_operator_versions() — artifact_type='operator', always loaded.
  Two separate queries by design — guides and operators need different handling in future.

## Layer Build Order
0 Foundation -> 1 Evaluation -> 2 Skeleton -> 3 Quick Ask -> 4 Privacy Guardian
-> 5 Personal Specialist -> 6 Writing Assistant -> 7 Remaining focuses -> 8 Auth
-> 9 Focus Optimizer

## Validation Workflow (required before locking any layer)
This chat generates the artifact AND reconciles external review findings.
Gemini and ChatGPT are adversarial reviewers only — no project context shared.
Classify every finding: Accepted / Rejected / Deferred / Requires empirical validation.
Reviewer agreement != correctness. Shared training data = shared blind spots.
Save validation records to /mnt/NAS/QuietRabbitMirror/05_AI_Validation/

## Critical Bug Patterns (found in integration review — do not repeat)
- PRAGMA key must be set BEFORE journal_mode in encrypted openers. open_db() is for unencrypted only.
- Never use executescript() in migrations — implicit COMMIT breaks SAVEPOINT atomicity. Use individual execute() calls.
- _bootstrap_lock_table() before acquire_lock() in run_migrations(). Wrapped in SAVEPOINT. (D5-056, D5-062)
- Voice profile VALUES must be validated at write time — reject + warn if PII detected. (D5-151)
  ALLOWED_VOICE_ATTRIBUTES protects keys. Values are NOT protected — validate both.
  Plain-language warning required at point of input, not just a log entry.
- Floor consent preference must be scoped: abstraction_tier + consent_timestamp. (D5-152)
  Never store as Persona-wide blanket consent. Future focuses/providers require fresh consent.
  Stored in personas.extra_metadata in shared.db (open_instance_db) -- not outputs.db.
- not_permitted enforces at Tier 2+ only. Raw values permitted at Tier 1. (D5-093, ADR-012 Amendment 1)
  ARCHITECTURE.md not_permitted section is stale — ADR-012 is authoritative on this.

## Schema Authoring Rule
Do not use semicolons inside string literals in .sql files (e.g. DEFAULT 'foo;bar').
_parse_statements() is not a general-purpose SQL parser — semicolons in string literals
will be split incorrectly and cause migration failures.

## Prompt Authoring Rule
_render_prompt() uses str.replace() for {token} substitution. Prompt templates must
not use {token_name} syntax for any string not intended as a Conductor token.
JSON examples or code blocks with literal braces matching a token name will be mangled.

## NotebookLM Bugs (fix before any layer ships)
- Clipboard leakage: sensitivity > 2 -> Manual Copy UI, never system clipboard
- Token estimation: 20% safety buffer when task_type is 'code' or 'research'
- PG_GATE_2 latency: full response classified before any display to user
- Ollama NDJSON stream: parse final line for {"status":"success"} before confirming

## Rust/Tauri Architecture (rust-migration branch)

### Branch rule (standing)
ALL Rust work commits to `rust-migration` branch, never main.
Python-only fixes (oracle correctness bugs only) commit to main,
then cherry-pick to rust-migration if they affect gate behavior.
No Rust code on main until migration is complete and Python is deleted.
Verify branch before every commit: `git branch --show-current`
must show rust-migration for any Rust session.

### Python freeze rule (standing)
No Python source changes during migration except critical correctness bugs
(gate logic errors, data corruption, security issues). Cosmetic fixes,
cleanup, and non-critical improvements are deferred until after migration.
Any permitted Python fix requires re-extracting golden vectors for all
affected gate paths before Rust porting of those paths continues.
Chat-PM must approve any Python change during migration before it is made.

### Project structure
src-tauri/ lives at repo root (Option A — D6-339).
  src-tauri/Cargo.toml       — package manifest + dependencies
  src-tauri/build.rs         — required Tauri build script
  src-tauri/tauri.conf.json  — Tauri v2 app config
  src-tauri/src/main.rs      — async entry point (#[tokio::main])
  src-tauri/src/lib.rs       — library root; mod declarations go here
  src-tauri/icons/           — app icons (placeholder until branding pass)
Rust dev runs locally on Garuda via cargo — NOT in Docker.
Docker is Python/Flask only and will be retired when migration is complete.

### Async runtime (D6-341)
Tokio async runtime. All Conductor modules are async.
Entry point: #[tokio::main] in main.rs.
IPC command handlers: #[tauri::command] async fn.
Do not block the async executor — use tokio::task::spawn_blocking for
any synchronous I/O that cannot be made async.

### Actor model (D6-342)
Track ownership follows the actor model: FocusRun owns its tracks
(PersonalTrack, TaskTrack, SharedStateTrack) and communicates by message passing.
Do not share tracks across actors via Arc<Mutex<…>> — this was the Python
borrow-checker workaround and does not carry over.
Tauri app handle is wired into the Conductor actor at startup for push events.

### SQLCipher + sqlx connection pattern
sqlx with SQLCipher-linked libsqlite3-sys. Not the bundled vanilla SQLite.
PRAGMA key MUST be set before PRAGMA journal_mode on every connection.
Enforce via sqlx after_connect hook — not inline at call sites.
Connection topology: open single connections on demand per DB file.
Do not use a keyed pool — QR has many small per-scope encrypted DBs
(shared.db, per-persona personal.db + outputs.db, per-focus domain_context.db,
per-topic plan_state.db). Pool model does not fit this topology.
SQLCipher linkage: SQLX_SQLITE_USE_SYSTEM_LIBRARY=1 + system libsqlcipher.
Verify linkage before first persistence module is ported.

### Tauri IPC command conventions
37 typed IPC commands defined in HANDOFF_IPC_SURFACE.md.
4 are push events (tauri::Emitter::emit) — not request-response.
All command structs derive Serialize, Deserialize, specta::Type.
TypeScript types generated via tauri-specta (2.0.0-rc.25) + specta-typescript.
Run type export after any command struct change — do not allow frontend/backend drift.
get_personal_fields: enforce abstraction at the command boundary.
Raw PersonalTrack values never cross into IPC response layer.
Tauri event listeners must be explicitly detached on SPA view unmount.

### Golden-vector verification requirement
Privacy gates (Gate1–4) must be verified against the running Python oracle
before the Rust port is considered correct.
Extract (field, abstraction_tier, raw_abstraction, execution_tier) → output
tables across every policy × tier combo + edge cases.
Rust output must match Python oracle bit-identically.
Port gates FIRST and freeze as the verification anchor before porting anything else.
Same requirement applies to: tier math, voice-profile scanner, prompt token
renderer, migration statement parser.

### Python oracle note
Python backend has been retired (16l). The golden vectors in
src-tauri/tests/golden/ are the permanent behavioral reference for Gate1–4.

### Cargo conventions
Edition: 2021. One Cargo.toml at src-tauri/ — no workspace yet.
Feature flags: add features explicitly when a module first uses them.
Do not leave unused features enabled — they inflate compile time.
thiserror for all error types. No anyhow in library code (only in main.rs if needed).
indexmap (not HashMap) wherever gate policy dispatch requires insertion-order determinism.
