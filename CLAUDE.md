# Quiet Rabbit — Claude Code Context
# Last updated: September 23, 2026 (Chat-PM staleness sweep -- Naming
# Architecture term map corrected: Plan->Topic and Task->Action rows added,
# "all migrations applied" framing fixed (Task->Action never propagated to
# code -- see below). Sections not touched this pass were not re-verified.)

## Session Discipline
- Respond with code only. No preamble, no recap, no explanation unless asked.
- Run /compact at natural phase boundaries: end of each file written, before
  integration review, before returning to Chat-PM (see "Ending a Session"
  near the end of this file for the actual handoff step). Do not wait for
  auto-compaction.
- When compacting, preserve: open files, current task, architectural decisions,
  unresolved errors, and the next action.
- One task at a time. Draft → Jason approves → write file. Never skip approval.

## Project
Self-hosted privacy-first AI platform. Engine: Conductor. Version: 0.2.
Tagline: "Your personal AI. Built to grow, always yours."

## Architecture Reference
There is no single document that's guaranteed current. Prose architecture docs
(03_ProjectDocs/Architecture/*.md, readable from here) describe design
rationale and mostly don't drift, but any specific "is X built" claim in them
can be stale -- verify against this repo's own live source first, or the
items/decisions tables in qr_docs.db (03_ProjectDocs/qr_docs.db) if the
question is about project status rather than code. Do not trust a doc's
current-state claim without one of those checks.
Read `Architecture/QUIET_RABBIT_ARCHITECTURE.md` and
`Architecture/AUTH_MULTIUSER_ARCHITECTURE.md` for design rationale and *why*
things are built the way they are -- that content doesn't need re-verifying
the way current-state claims do.
When genuinely unclear after checking source and the database: note it in
your end-of-session handoff (see "Ending a Session" below) for Chat-PM to
resolve, rather than guessing or inventing.

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
- Tier 1.5 = user choice: Groq (US, free tier) or Mistral (EU/GDPR, paid) --
  fast hosted inference, same capability class as local Tier 1, non-anonymous.
  Honest trade-off framing. No prescribed default. (decisions.id=665/727 --
  both providers moved off "Tier 2" once that label was clarified as
  split-screen-only; Tier 2 itself is Duck.ai/Brave Leo, anonymous,
  no-retention, not yet shipped.)
- Silent operator: personal context informs output, never narrated.
  Never "Since you mentioned..." or "Based on your preference..."
- Human in the loop: all auto-improvements require explicit approval.
  No silent changes.

## Dev Environment
Repo root:
  /home/kulaga/QuietRabbit/06_GitRepos/quietrabbit-core/

NAS path (Garuda only — Proxmox/LXC retired):
  /mnt/NAS/QuietRabbitMirror/

Services:
  Ollama API:  http://192.168.88.238:11434  (Garuda, ethernet)

Rust dev runs locally on Garuda via cargo.
Docker and the LXC container are retired — do not reference them.

`cargo tauri dev` always runs the app with CWD=src-tauri regardless of the
directory you invoke it from (items.id=316). find_focus_file() in
src-tauri/src/conductor/lifecycle.rs resolves the dev-workflow Focus-file
path relative to CARGO_MANIFEST_DIR (compile-time, fixed to src-tauri),
not CWD, so this no longer breaks quick-ask/chat under `cargo tauri dev` —
but keep it in mind if you add other CWD-relative paths in this codebase.

Key commands:
  cargo build                                          — compile check
  cargo fmt --check                                    — formatting (CI gate; fix with plain `cargo fmt` over the whole crate, never scoped to one file)
  cargo clippy -- -D warnings                          — lint (CI gate)
  cargo deny check                                     — advisories/licenses/bans/sources (CI gate)
  cargo test 2>&1 | grep -E "^error|test result"      — test summary (PLAIN, never --lib: --lib skips the src-tauri/tests/ targets)
  git branch --show-current                            — verify branch (must show main)
  git log --oneline main | head -10                    — commit verification

The cargo commands above run from src-tauri/. CI's check job (.github/workflows/tauri-ci.yml) runs fmt, clippy, deny and plain test; a change is not verified until build, fmt, clippy, deny and plain test all pass locally. Build plus test alone has let both a compile break and a fmt failure reach unpushed main undetected (items.id=532).

## Ollama (D6-353)
QR checks for a running Ollama instance at 127.0.0.1:11434 on startup.
If found, uses it. If not, starts the bundled Ollama sidecar.
No duplicate model downloads. No contention between instances.
Dev: Ollama runs on Garuda at http://192.168.88.238:11434 — already running, always detected.
127.0.0.1:11434 also works locally (system Ollama, always detected first).

## Naming Architecture — term map

Most legacy terms are retired; canonical terms are used in all new code.
Two exceptions below (Plan->Topic, Task->Action) are ADR-013-locked
canonical pairs whose code-level rename status differs -- check the Notes
column, don't assume every row here is fully propagated to code.

| Retired term         | Canonical term      | Notes                                        |
|----------------------|---------------------|----------------------------------------------|
| Space                | Persona             | space_id → life_id → persona_id              |
| space_id             | persona_id          | Column and parameter name                    |
| Life                 | Persona             | Intermediate migration term — fully retired  |
| life_id              | persona_id          | Migration complete (commit 8579f17)          |
| user_lives           | user_personas       | Join table in shared.db                      |
| lives/ (path)        | personas/ (path)    | Filesystem segment                           |
| life_affinity        | (removed)           | Dropped from FocusDefinition (D6-300)        |
| life_context         | persona_context     | SYSTEM_TOKENS in tokens.py                   |
| life_store.py        | persona_store.py    | Migration complete                           |
| Path                 | Focus               | File extension: .focus                       |
| path_id              | focus_id            | Column and parameter name                    |
| path_run             | focus_run           | DB table: focus_runs                         |
| path_run_id          | focus_run_id        | Column and parameter name                    |
| path_run_snapshot    | focus_run_snap      | DB table: focus_run_snapshots                |
| path_context         | focus_context       | SYSTEM_TOKENS                                |
| paths/ dir           | focuses/ dir        | core_artifacts/focuses/                      |
| Specialist           | Guide / Operator    | .guide (community), .operator (sys)          |
| specialist_id        | source_id           | personal_fields, voice_profiles              |
| specialists/ dir     | guides/ + ops/      | core_artifacts/guides/ + operators/          |
| quick_draft          | quick_ask           | output_type and focus_id                     |
| Personal Specialist  | (no named term)     | User sees: "What QR knows about you"         |
|                      |                     | personal-specialist.operator display_name    |
|                      |                     | needs update before Focus builds             |
| Plan                 | Topic               | plan_id → topic_id; migration complete,      |
|                      |                     | topic_id/topic_store.rs used throughout      |
| Task                 | Action              | ADR-013-locked (D6-214--D6-225) but NOT      |
|                      |                     | propagated to code (confirmed 2026-09-23):   |
|                      |                     | action_id appears only in                    |
|                      |                     | plan_state_store.rs as a bind parameter.     |
|                      |                     | Runtime code still uses step_id/             |
|                      |                     | StepDefinition throughout conductor/         |
|                      |                     | lifecycle.rs, executor.rs, and 8 other       |
|                      |                     | files. Rename tracked as items.id=552 --     |
|                      |                     | until that lands, use step_id/               |
|                      |                     | StepDefinition when writing/reading code,    |
|                      |                     | Action/action_id only in ADR-013/design docs.|

## R1 Focuses
Canonical R1 Focus list: FOCUS_ROADMAP.md → § R1 Confirmed scope
13 Focuses across 4 R1 Personas (Personal, Work, Student, Medical).
Wellness is post-R1 (wearable API required).
Finance is deferred (bank API required).
Do not build any Focus until Chat-PM directs the build session.
Design sessions must all complete before shared infrastructure build begins.

## Key Architectural Decisions
- execution_tier = min(focus_settings.max_permitted_tier, focus_def.max_routing_tier, step.routing_tier)
  focus_settings.max_permitted_tier is the hard ceiling.
  focus_settings.privacy_tier is user preference. Never conflate them.
  focus_settings row must exist before AUTHORIZE executes.
- PersonalTrack NEVER serialized to focus_run_snapshots — re-fetched fresh on resume
- Step 6: PG_GATE_1 writes approved/abstracted fields to step_disclosure_buffer
  Step 8: reads from step_disclosure_buffer for Tier 2+ (NEVER from PersonalTrack)
  Raw personal values never appear in external prompt strings.
- Tier 3 is a terminal boundary — execution loop breaks, status=awaiting_user.
- auth_enabled=1 ONLY if ALL databases migrate successfully. Partial = rollback.
- focus_run created as status=initializing, promoted to running after Phase 3 only.
- Recovery key: BIP39 mnemonic of full 32 bytes. Exact reconstruction, no wrapping.
- Release 1/Release 2 = product roadmap. Phase 1-7 = Conductor lifecycle phases.
- SYSTEM_TOKENS frozenset defined in conductor/tokens.py.
- ContextCompactor makes direct local model call. No Librarian, no routing.
- is_fast_lane boolean on focus_runs — Phase 1 capture only.
- Hierarchical fact model: scope tree (depth tiers) + knowledge graph (D6-459).
  Two distinct layers — do not conflate. Scope tree governs inheritance and access
  control. Knowledge graph governs semantic relationships.
- Source of truth framework (D6-460): source_registry, deduplication candidates,
  modification_state, soft-delete tombstone. First instantiated by Cooking.
- parent_entity_id: DROP in a future migration (not yet scheduled). Confirm no
  Rust code references this column first. entity_relationships is the canonical
  relationship store.

## Entity Model (D6-371–D6-375 — LOCKED)
personal_fields flat key-value model replaced by entities + entity_facts.
Migration: personal_002.sql (complete, entities + entity_facts). D6-459 additions
(source_registry, deduplication, modification_state, soft-delete tombstone) also
complete -- see personal_002.sql. CORRECTED 2026-09-12 (items.id=477's
schema-shape golden-snapshot test build surfaced this while iterating
SCHEMA_FILES): the personal_*.sql sequence was NOT consolidated to 2 files --
personal_005.sql through personal_008.sql are real, embedded in SCHEMA_FILES,
and exercised (an existing migrations.rs test asserts personal.db applies all
8 real versions on first open). The prior "consolidated from 7 planned files
to 2, do not search for those files" note here was accurate as of 2026-08-01
but has since gone stale as personal_005-008.sql were actually built. Current
count: 8 files, personal_001.sql through personal_008.sql, all live.

Key facts:
- entities: self-referential (parent_entity_id — PENDING DROP, not yet scheduled),
  open type vocabulary, aliases as JSON array, status (active/retired/archived)
- entity_facts: entity_id nullable (NULL = singleton), temporal validity,
  source field tracks provenance
- entity_relationships: stub table only R1 — no reads, no writes, no IPC
- Singletons (entity_id = NULL) work exactly as personal_fields did
- Full schema: D6-372. All fifteen invariants: D6-373.

## Layer Build Order (conceptual — Rust implementation)
0 Foundation → 1 Evaluation → 2 Skeleton → 3 Quick Ask → 4 Privacy Guardian
→ 5 Personal Context → 6 Writing Assistant → 7 Remaining Focuses → 8 Auth
→ 9 Optimizer

## Validation Workflow
This chat generates the artifact AND reconciles external review findings.
Gemini and ChatGPT are adversarial reviewers only — no project context shared.
Classify every finding: Accepted / Rejected / Deferred / Requires empirical validation.

## Critical Bug Patterns (do not repeat)
- PRAGMA key must be set BEFORE journal_mode in encrypted openers.
- Never use executescript() in migrations — implicit COMMIT breaks SAVEPOINT atomicity.
- _bootstrap_lock_table() before acquire_lock() in run_migrations().
- Voice profile VALUES must be validated at write time — reject + warn if PII detected.
- Floor consent preference must be scoped: abstraction_tier + consent_timestamp.
- not_permitted enforces at Tier 2+ only. Raw values permitted at Tier 1.

## Schema Authoring Rule
Do not use semicolons inside string literals in .sql files.
_parse_statements() is not a general-purpose SQL parser.

## Prompt Authoring Rule
_render_prompt() uses str.replace() for {token} substitution. Templates must
not use {token_name} syntax for any string not intended as a Conductor token.

## Rust/Tauri Architecture

### Branch rule (standing)
ALL commits → main. Only branch: main. rust-migration branch is retired.
Verify before every commit: `git branch --show-current` must show main.

### Project structure
src-tauri/ lives at repo root.
  src-tauri/Cargo.toml       — package manifest + dependencies
  src-tauri/build.rs         — required Tauri build script
  src-tauri/tauri.conf.json  — Tauri v2 app config
  src-tauri/src/main.rs      — async entry point (#[tokio::main])
  src-tauri/src/lib.rs       — library root; mod declarations go here
Rust dev runs locally on Garuda via cargo. Python backend retired (commit a27a2b1).

### Async runtime (D6-341)
Tokio async runtime. All Conductor modules are async.
Entry point: #[tokio::main] in main.rs.
IPC command handlers: #[tauri::command] async fn.
Use tokio::task::spawn_blocking for any synchronous I/O that cannot be async.

### Actor model (D6-342)
FocusRun owns its tracks (PersonalTrack, TaskTrack, SharedStateTrack).
Communicates by message passing. No Arc<Mutex<…>> for track ownership.
Tauri app handle wired into the Conductor actor at startup for push events.

### SQLCipher + sqlx connection pattern
sqlx with SQLCipher-linked libsqlite3-sys. Not bundled vanilla SQLite.
PRAGMA key MUST be set before PRAGMA journal_mode on every connection.
Enforce via sqlx after_connect hook.
Connection topology: open single connections on demand per DB file.
Do not use a keyed pool — many small per-scope encrypted DBs. Reaffirmed
with full reasoning by items.id=475 (2026-09-11): PRAGMA key uses a raw
hex-blob literal, not a passphrase, so SQLCipher's KDF is bypassed and
re-keying per connection is already cheap — the usual argument for pooling
encrypted connections doesn't apply here. The real reason not to pool is
security-boundary lifetime: a pooled connection stays unlocked past
KeyRegistry's clear() at logout/idle-timeout, silently extending decrypted
access beyond the window that boundary exists to enforce. shared.db is the
one exception — genuinely unencrypted, pooled via items.id=481.
SQLCipher linkage: libsqlite3-sys sqlcipher feature (D6-346).

### Tauri IPC command conventions
IPC surface defined in HANDOFF_IPC_SURFACE.md — read before building any IPC layer.
Command count grows regularly (20 groups as of 2026-08-29) -- do not rely on a
stated number here; count `commands::` entries in src-tauri/src/ipc.rs's
specta_builder() for the current live surface.
All command structs derive Serialize, Deserialize, specta::Type.
TypeScript types via tauri-specta (2.0.0-rc.25).
Run type export after any command struct change.
Raw PersonalTrack values never cross into IPC response layer.
Tauri event listeners must be explicitly detached on SPA view unmount.

### Golden-vector verification requirement
Privacy gates (Gate1–4) verified against Python oracle via golden vectors in
src-tauri/tests/golden/. Rust output must match bit-identically.
Port gates FIRST before anything else.

### Cargo conventions
Edition: 2021. One Cargo.toml at src-tauri/.
thiserror for all error types.
indexmap (not HashMap) wherever gate policy dispatch requires insertion-order determinism.

### Secrets (relocated from Chat-PM standing rule 50, D6-529)
API keys and credentials are NEVER hardcoded in source or committed files.
Values live in .env only (gitignored). This applies to every session touching
this repo, human or AI.

## Ending a Session (write the handoff yourself)
This project runs a multi-chat coordination system outside this repo (a
SQLite database, not something Claude Code needs to understand fully). The
one piece that matters here: before you finish, record what you did so
Chat-PM (a separate Claude session that reviews all work) can pick it up
without Jason re-explaining it.

Run exactly one command, from anywhere (it's not path-sensitive):

```
python3 /home/kulaga/QuietRabbit/03_ProjectDocs/scripts/db_utilities/handoff_write.py \
  --chat "Code" \
  --summary "What you did and found -- assume the reader has zero context on this session." \
  --decisions "Anything Chat-PM should weigh in on, or the literal word none." \
  --blocked "Anything unresolved/flagged, or omit this flag entirely if nothing." \
  --files "Every file you touched, comma-separated, or the literal word none."
```

Rules, enforced by the script itself, not just convention:
- Always `--chat "Code"` for this repo — hardcoded, don't change it. (This
  is a distinct chats row from "Chat-DEV" so Chat-PM can tell Claude Code
  sessions apart from terminal Chat-DEV sessions -- both do the same kind
  of work, this is attribution only.)
- `--decisions` and `--files` are required and cannot be blank — pass the
  literal word `none` if there's nothing to report, never omit them.
- You do NOT mark this reviewed, and there is no flag to try — the script
  always inserts `reviewed=0`. Only Chat-PM closes the loop on your work.
- This is the ONLY database write available to you. Don't look for or run
  any other Chat-PM/Chat-DEV workflow, starter script, or process doc —
  they exist for a different, much longer-running session type and are not
  meant for a Claude Code session. If Jason wants something beyond this,
  he'll tell you directly.
- After running it, tell Jason the command ran and stop. He'll bring it to
  Chat-PM himself.
