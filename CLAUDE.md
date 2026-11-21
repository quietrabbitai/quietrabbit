# Quiet Rabbit — Claude Code Context

## How this file works
Area knowledge lives in path-scoped rule files under `.claude/rules/`, which load only when a session
reads a file their `paths:` match (index below). Subsystem knowledge is updated IN PLACE in its rule
file; never add a new CLAUDE.md section per item. A new area gets a new rule file with `paths:` and one
index line here. Keep this file under 200 lines.

## How to work
The starter governs each task. Work in stages at the starter's checkpoints, run the local gate after
each unit and the full gate (below) before any push.

## Project
Self-hosted privacy-first AI platform. Engine: Conductor. Version: 0.1.0 (src-tauri/tauri.conf.json and Cargo.toml).
Tagline: "Your personal AI. Built to grow, always yours."

## Architecture Reference
No document is guaranteed current. Prose architecture docs (03_ProjectDocs/Architecture/*.md, readable
from here) explain design rationale, but any "is X built" claim in them can be stale: check this repo's
live source first, or the items/decisions tables in qr_docs.db for project status. Read
QUIET_RABBIT_ARCHITECTURE.md and AUTH_MULTIUSER_ARCHITECTURE.md for why things are built as they are.
Still unclear after source and database: say so in the handoff ("Ending a Session") for Chat-PM.

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

Rust dev runs locally on Garuda via cargo.
Docker and the LXC container are retired — do not reference them.

Key commands:
  cargo build                                          — compile check
  cargo fmt --check                                    — formatting (CI gate; `cargo fmt` reformats the whole crate and cannot be scoped to a file; to fix one file run `rustfmt` on that path; after any `cargo fmt` run `git status` and stage by explicit path (project guide entry cargo_fmt_does_not_scope_to_files))
  cargo clippy --tests -- -D warnings                  — lint (CI gate)
  cargo deny fetch && cargo deny check                 — advisories/licenses/bans/sources (CI gate; run `fetch` first: CI uses the advisory database of the day and a local copy that is days old passes advisories CI then fails, items.id=927)
  cargo test 2>&1 | grep -E "^error|test result"      — test summary (PLAIN, never --lib: --lib skips the src-tauri/tests/ targets)
  git branch --show-current                            — verify branch (must show main)
  git log --oneline main | head -10                    — commit verification

The cargo commands above run from src-tauri/. CI (.github/workflows/tauri-ci.yml) splits the gate over parallel jobs: `lint` (fmt, deny), `clippy`, `check` (the tests) and `frontend` (items.id=787). A change is not verified until build, fmt, clippy, deny and plain test all pass locally. Build plus test alone has let both a compile break and a fmt failure reach unpushed main undetected (items.id=532).

Frontend gate (items.id=588): CI also runs a separate `frontend` job in
tauri-ci.yml, in parallel with `check` (Node 24: npm ci, npx tsc -b, npm test,
npm run lint, all run from frontend/). The same four commands are the local
frontend gate -- a frontend change is not verified until all pass. oxlint
warnings (30 as of 2026-10-04) do not fail the gate; errors do. npm test needs
Node >= 22.6 (it runs the plain-node test files with --experimental-strip-types).

## Index of rule files
Plain links; each file loads automatically when a matching file is read.
- [ollama](.claude/rules/ollama.md) — Bundled Ollama sidecar: ports, ownership proof, no-cloud env, single instance
- [privacy-model](.claude/rules/privacy-model.md) — Privacy Filter model: pin, install, marker, runtime, release asset
- [model-blocks](.claude/rules/model-blocks.md) — Model blocks (cb-12/13/14), block registry, draft_check, calibration harness
- [focus-dev](.claude/rules/focus-dev.md) — Focus development tool: spec, validate, reseed, emit-migration, presets, dev-root guard
- [model-resolver](.claude/rules/model-resolver.md) — Model resolver: requirement, capability cells and bars, readiness, network predicate
- [step-privacy](.claude/rules/step-privacy.md) — Privacy treatment of a step attempt (step_treatment::derive, what reads the ceiling)
- [pass-artifacts](.claude/rules/pass-artifacts.md) — Per-pass artifact store (pass_artifacts, reserved bound names, writers, seeding)
- [manual-handoff](.claude/rules/manual-handoff.md) — Manual hand-off: cb-15 starter, handoff_ingest return, taint, panel state
- [launch-questions](.claude/rules/launch-questions.md) — Launch questions, mode profiles, run-start settings, answer store, launch IPC
- [run-plan](.claude/rules/run-plan.md) — Run plan, digest, plan approvals and memory home, plan gate, park reasons, run-plan IPC
- [external-steps](.claude/rules/external-steps.md) — External steps and chat history: user-turns-only external_input, API Chat rule C
- [run-lifecycle](.claude/rules/run-lifecycle.md) — Parked-run retention, resume staleness, fail_run, guarded claim, post-output sweep, resume pin, archive hook, store sweep
- [collection-consent](.claude/rules/collection-consent.md) — Collection consent for focus_level Focuses: state, gate, commands, audit, frontend confirm
- [device-library](.claude/rules/device-library.md) — Focus-owned entities, device catalogue, entity writers, device seeding, hard purge, device library
- [personal-fields](.claude/rules/personal-fields.md) — Personal fact bridge, single-pass render, local render of fields, carry scan, prompt authoring
- [output-scan](.claude/rules/output-scan.md) — Persona context, output scan, external-step scoring, long-prompt review, Gate 3 riders
- [allowed-answers](.claude/rules/allowed-answers.md) — Device facts in external steps, per-span allowed answers, device spans, device-seed resume pin
- [build-and-ci](.claude/rules/build-and-ci.md) — Dev run quirks, CI shards, release builds, toolchain pin, dev-only paths, project structure
- [naming](.claude/rules/naming.md) — Retired-to-canonical term map (Persona, Focus, Action, Topic...)
- [rust-architecture](.claude/rules/rust-architecture.md) — Async runtime, actor model, golden-vector requirement
- [database](.claude/rules/database.md) — SQLCipher + sqlx connection pattern, schema authoring rule
- [entity-model](.claude/rules/entity-model.md) — Entity model (entities, entity_facts) and the fact-model decisions
- [auth](.claude/rules/auth.md) — Account creation order and the recovery key
- [ipc](.claude/rules/ipc.md) — Tauri IPC command conventions
In-repo docs: [README](README.md), [SETUP_GUIDE](SETUP_GUIDE.md), [CONTRIBUTING](CONTRIBUTING.md), [frontend/README](frontend/README.md).

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
- Tier 3 is a terminal boundary — execution loop breaks, status=awaiting_user.
- focus_run created as status=initializing, promoted to running after Phase 3 only.
- Release 1/Release 2 = product roadmap. Phase 1-7 = Conductor lifecycle phases.
- SYSTEM_TOKENS is a const array in conductor/tokens.rs (5 reserved names:
  user_input, persona_context, voice_profile, previous_output, focus_context).
- ContextCompactor is not built: no such type exists in source. decisions.id=87
  stays active. Chat history is bounded by message count only
  (HISTORY_WINDOW = 10, commands/messages.rs).
- is_fast_lane boolean on focus_runs — Phase 1 capture only.

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

## Rust/Tauri Architecture

### Branch rule (standing)
ALL commits → main. Only branch: main. rust-migration branch is retired.
Verify before every commit: `git branch --show-current` must show main.

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
