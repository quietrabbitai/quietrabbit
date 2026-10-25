# Quiet Rabbit — Claude Code Context
# Last updated: October 25, 2026 (items.id=653 docs pass -- Ollama and Dev
# Environment per decisions.id=840 and 863, version, R1 Focuses, tier
# vocabulary, key-hierarchy wording, Rust names for Python-era rules, cites.
# Sections not touched this pass were not re-verified.)

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
Self-hosted privacy-first AI platform. Engine: Conductor. Version: 0.1.0 (src-tauri/tauri.conf.json and Cargo.toml).
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
- Data key: never written in the clear to the app's databases or files, and never
  in IPC responses. It lives in Rust AppState/KeyRegistry for the duration of the
  session. At rest it exists only as the password wrap and the recovery seal in
  shared.db (decisions.id=893, AUTH_MULTIUSER_ARCHITECTURE.md Section 4.0); if the
  optional OS-keychain convenience entry is ever enabled, Section 2.2 governs it
  and it is replaced at every rotation.
- Tier vocabulary (decisions.id=811, 818): code and decisions say qr_local,
  qr_hosted (the old Tier 1.5), cloud_anonymous (old Tier 2) and cloud_frontier
  (old Tier 3); users see "Cloud Chat". qr_hosted = user opt-in to a hosted
  provider running the same class of model as qr_local, for speed;
  non-anonymous. Hosted providers are curated rows in the providers table with
  disclosed attributes (training, retention, anonymity, jurisdiction, cost);
  warn and allow, never block; no provider or model is named in code and none
  is a default or silently routed to (decisions.id=872, 875). cloud_anonymous:
  Duck.ai is the only entry (items.id=194; Brave Leo has no standalone launch
  URL, so it is not currently possible -- src-tauri/schema/shared_001.sql).
- Silent operator: personal context informs output, never narrated.
  Never "Since you mentioned..." or "Based on your preference..."
- Human in the loop: all auto-improvements require explicit approval.
  No silent changes.

## Dev Environment
Repo root:
  /home/kulaga/QuietRabbit/06_GitRepos/quietrabbit-core/

NAS path (Garuda only; the share is served by the Proxmox host over NFS and
mounted at /mnt/NAS -- the LXC and Docker stack are retired):
  /mnt/NAS/QuietRabbitMirror/

Services:
  Ollama: QR starts its own sidecar on 127.0.0.1:21434 (see "Ollama" below).
  Never point QR at another Ollama. Use localhost, never a LAN address
  (DHCP-assigned, changes). Manual experiments against some other Ollama are
  run by hand and are never configured into QR.

Rust dev runs locally on Garuda via cargo.
Docker and the LXC container are retired — do not reference them.

`cargo tauri dev` always runs the app with CWD=src-tauri regardless of the
directory you invoke it from (items.id=316). find_focus_file() in
src-tauri/src/conductor/lifecycle.rs resolves the dev-workflow Focus-file
path relative to CARGO_MANIFEST_DIR (compile-time, fixed to src-tauri),
not CWD, so this no longer breaks quick-ask/chat under `cargo tauri dev` —
but keep it in mind if you add other CWD-relative paths in this codebase.

`cargo tauri dev` does not enforce the CSP (tauri-2.11.2 source), so verify CSP changes with `cargo tauri build --debug --no-bundle` and the devtools console filtered on "Refused to".

Key commands:
  cargo build                                          — compile check
  cargo fmt --check                                    — formatting (CI gate; fix with plain `cargo fmt` over the whole crate, never scoped to one file)
  cargo clippy -- -D warnings                          — lint (CI gate)
  cargo deny check                                     — advisories/licenses/bans/sources (CI gate)
  cargo test 2>&1 | grep -E "^error|test result"      — test summary (PLAIN, never --lib: --lib skips the src-tauri/tests/ targets)
  git branch --show-current                            — verify branch (must show main)
  git log --oneline main | head -10                    — commit verification

The cargo commands above run from src-tauri/. CI's check job (.github/workflows/tauri-ci.yml) runs fmt, clippy, deny and plain test; a change is not verified until build, fmt, clippy, deny and plain test all pass locally. Build plus test alone has let both a compile break and a fmt failure reach unpushed main undetected (items.id=532).

Frontend gate (items.id=588): CI also runs a separate `frontend` job in
tauri-ci.yml, in parallel with `check` (Node 24: npm ci, npx tsc -b, npm test,
npm run lint, all run from frontend/). The same four commands are the local
frontend gate -- a frontend change is not verified until all pass. oxlint
warnings (30 as of 2026-10-04) do not fail the gate; errors do. npm test needs
Node >= 22.6 (it runs the plain-node test files with --experimental-strip-types).

The Rust toolchain is pinned (rust-toolchain.toml at repo root, version
matched in the CI workflow's Install Rust step) rather than left on
`stable` -- an unpinned `dtolnay/rust-toolchain@stable` install can pick
up a newer clippy than the one a local gate run just passed against and
fail CI on lints that never fired locally (commit dbae287, 2026-10-03,
closed under items.id=586: a
`double_must_use` lint against `#[async_trait]`-generated methods, new
or tightened on a newer stable, failed CI on pre-existing code the same
day a local `cargo clippy -- -D warnings` run passed clean). Bump the
version in both places together, deliberately, and re-run the full local
gate above (including clippy -D warnings) before pushing a bump.

Dev-only paths must be flagged in a comment where added and tracked for
removal before release: find_focus_file's compile-time CARGO_MANIFEST_DIR path
(items.id=617), the privacy-filter model at
~/.local/share/quietrabbit/models/privacy-filter-q8.gguf (items.id=678, the
first-login download), and the debug-only PATH lookup of `ollama`
(items.id=583).

## Ollama (decisions.id=840, 863)
QR always runs its own bundled Ollama sidecar and never uses a user-installed
Ollama (decisions.id=840). No detect-and-reuse: a user's own Ollama is invisible
to QR's model registry, Focus eligibility and Cloud Chat, and the sidecar uses
its own models directory (QR_DATA_ROOT/ollama_models). Port 11434 is probed
only to log a GPU/RAM contention warning; it never gates or redirects the
sidecar.

Built today (ollama_sidecar.rs): the sidecar binds 127.0.0.1:21434
(QR_OLLAMA_PORT), fixed, no fallback. Startup reclaims leftover QR orphans
(items.id=586), identified by environment variable and command-line match under
ollama_models.

Decided in decisions.id=863, NOT built yet -- do not assume any of it in code:
- single instance: a second launch focuses the first window and exits
  (items.id=667, ready);
- the sidecar binary is named qr-ollama and QR signals only processes it can
  prove are its own (recorded process group, not command-line match)
  (items.id=668, ready, depends on items.id=616);
- port preferred 21434, fallback through 21435-21443, failing only if all ten
  are busy; no manual port setting in R1 (items.id=668).
Related and still open: OllamaClient::base_url() still honours OLLAMA_HOST and
OLLAMA_PORT (items.id=616, ready); the bundled binary is a stub (items.id=583,
ready).

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
| life_context         | persona_context     | SYSTEM_TOKENS in tokens.rs                   |
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
R1 core (ships installed) is Writing Assistant and Tech Support only; every
other Focus is available from the gallery, default off (decisions.id=600, a
Phase-1 output that Phase 5 re-applies, items.id=130). FOCUS_ROADMAP.md is
superseded (tracked_files.id=43, tracker items.id=130): do not read its
13-Focus R1 list as current.
Built (app/core_artifacts/focuses/): writing-assistant, quick-ask,
research-and-buy, role-assessment, qr-admin, api-chat. Tech Support is not built.
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
- Recovery key: BIP39 mnemonic of 256-bit recovery entropy, independent of the data key. The app stores only the derived recovery public key and the data key sealed to it; every password change, restore and new recovery phrase rotates the data key. See AUTH_MULTIUSER_ARCHITECTURE.md Sections 4.0 and 5.1.
- Release 1/Release 2 = product roadmap. Phase 1-7 = Conductor lifecycle phases.
- SYSTEM_TOKENS is a const array in conductor/tokens.rs (5 reserved names:
  user_input, persona_context, voice_profile, previous_output, focus_context).
- ContextCompactor is not built: no such type exists in source. decisions.id=87
  stays active. Chat history is bounded by message count only
  (HISTORY_WINDOW = 10, commands/messages.rs).
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
- Migrations (migrations.rs) run one SAVEPOINT per schema version on an
  autocommit connection, executing parse_statements() output one statement at
  a time. No BEGIN/COMMIT wrappers: a SAVEPOINT outside a BEGIN is the
  outermost transaction and RELEASE commits it, so an explicit COMMIT breaks
  atomicity.
- bootstrap_lock_table() before acquire_lock() in run_migrations() (migrations.rs).
- Voice profile VALUES must be validated at write time — reject + warn if PII detected.
- Floor consent preference must be scoped: abstraction_tier + consent_timestamp.
- not_permitted enforces at Tier 2+ only. Raw values permitted at Tier 1.

## Schema Authoring Rule
Do not use semicolons inside string literals in .sql files.
parse_statements() (migrations.rs) is not a general-purpose SQL parser.
A migration that drops a table another table still references via FOREIGN
KEY must be listed in FK_OFF_MIGRATIONS (migrations.rs) -- DROP TABLE
triggers ON DELETE CASCADE the same as an explicit DELETE would, not just
the latter; the every_drop_table_is_fk_off_or_audited test enforces this
(items.id=585/586).

## Prompt Authoring Rule
render_template() (conductor/executor.rs) substitutes {token} with
String::replace and strips unresolved {token} patterns. Templates must
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
one exception — genuinely unencrypted, pooled via items.id=483.
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
