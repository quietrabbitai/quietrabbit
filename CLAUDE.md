# Quiet Rabbit — Claude Code Context
# Last updated: November 14, 2026 (items.id=894: new "Model blocks" section only).
# Before that, November 8, 2026 (items.id=552: Naming Architecture Task->Action
# row and intro only). Before that:
# Last updated: November 3, 2026 (items.id=848: one bullet in the "Ollama"
# section only). Before that, November 1, 2026 (items.id=678: new "Privacy model" section and
# the dev-only paths paragraph under "Dev Environment" only). Before that,
# November 1, 2026 (items.id=618: one paragraph under "Key commands"
# only -- release builds need PRIVACY_FILTER_LIB_DIR or the opt-out; plain
# `cargo test --release` is not a supported gate, release_ tests are run by
# name). Before that,
# November 1, 2026 (items.id=844: CI paragraph under "Key commands"
# only -- sharded Test step and the shared-vCPU test rule; also corrects the old
# "check job runs fmt, clippy, deny" claim, stale since items.id=787). Before
# that, October 31, 2026 (items.id=844 first version). Before that:
# Last updated: October 31, 2026 (items.id=667: Ollama section only --
# single-instance guard built, with its limits). Before that, October 31, 2026
# (items.id=668: Ollama section only -- sidecar
# name, recorded-group ownership proof, port fallback). Before that,
# October 30, 2026 (items.id=616: Ollama section only -- client
# port and sidecar environment). Before that, October 25, 2026 (items.id=653
# docs pass -- Ollama and Dev
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

On Linux, SIGINT, SIGTERM and SIGHUP exit the app through the window-close path
(glib unix signal sources installed in main.rs's RunEvent::Ready arm after
initialize_cef; items.id=842), so Ctrl-C under `cargo tauri dev` stops the app
and its sidecar. A kill of cargo-tauri alone never signals the app (accepted,
dev-only limit). Windows and macOS equivalents are not built (items.id=700).

`cargo tauri dev` does not enforce the CSP (tauri-2.11.2 source), so verify CSP changes with `cargo tauri build --debug --no-bundle` and the devtools console filtered on "Refused to".

Key commands:
  cargo build                                          — compile check
  cargo fmt --check                                    — formatting (CI gate; `cargo fmt` reformats the whole crate and cannot be scoped to a file; to fix one file run `rustfmt` on that path; after any `cargo fmt` run `git status` and stage by explicit path (project guide entry cargo_fmt_does_not_scope_to_files))
  cargo clippy --tests -- -D warnings                  — lint (CI gate)
  cargo deny check                                     — advisories/licenses/bans/sources (CI gate)
  cargo test 2>&1 | grep -E "^error|test result"      — test summary (PLAIN, never --lib: --lib skips the src-tauri/tests/ targets)
  git branch --show-current                            — verify branch (must show main)
  git log --oneline main | head -10                    — commit verification

The cargo commands above run from src-tauri/. CI (.github/workflows/tauri-ci.yml) splits the gate over parallel jobs: `lint` (fmt, deny), `clippy`, `check` (the tests) and `frontend` (items.id=787). A change is not verified until build, fmt, clippy, deny and plain test all pass locally. Build plus test alone has let both a compile break and a fmt failure reach unpushed main undetected (items.id=532).

CI's `check` Test step does not run plain `cargo test` (items.id=844): it runs
.github/scripts/ci-test-shards.sh, which builds once, runs the quietrabbit_lib
unit-test binary as 3 parallel shards (own process, own ENV_MUTEX, own
TMPDIR/XDG dirs; --test-threads 1/1/2), then runs the other targets (bins,
src-tauri/tests/*.rs, doc tests) once. The shard definitions are in that script:
A = ollama_ownership:: ollama_sidecar::, B = auth::key_rotation::
commands::key_rotation:: auth::writer_gate::, C = everything else (skips every A
and B filter, so a new test module lands in C). The trailing :: is mandatory.
A count check fails the job unless the shards are disjoint and their union
equals the full `--list`; a red shard, a signal exit or a missing target also
fail it, and every failure is reported. There is no retry: a crash is
investigated. Plain `cargo test` stays the local standard (decisions.id=776);
`.github/scripts/ci-test-shards.sh check` runs only the count check locally.
Shards share 4 vCPUs, so a test must not assert a pool size or a tight
wall-clock bound that a starved CPU can break: the idle_timeout leak test
detects a leak by pool exhaustion for that reason (items.id=844).

Release builds (`cargo build --release`, `cargo tauri build`, any profile inheriting release) fail in build.rs unless PRIVACY_FILTER_LIB_DIR is set; QR_ALLOW_RELEASE_WITHOUT_PRIVACY_FILTER=1 is the explicit local opt-out and is never for a shipped artifact (items.id=618). `cargo test --release` is not a supported gate: tests rely on debug-only fallbacks (the source-tree Focus path and the legacy privacy path). Release-only behaviour is covered by tests named release_*, run by name with `cargo test --release --lib -- release_` (a substring filter, so it also runs other tests that have release_ in their name; they pass in release).

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
(items.id=617), the privacy-filter dev paths (QR_PRIVACY_FILTER_MODEL and the
$HOME/.local/share/quietrabbit/models/ fallback used only when no data root is
set, privacy_model/paths.rs, items.id=678: compiled in but never read by a
release build, pinned by release_ tests; see "Privacy model" below), and the
debug-only PATH lookup of `ollama` (items.id=583).

## Ollama (decisions.id=840, 863)
QR always runs its own bundled Ollama sidecar and never uses a user-installed
Ollama (decisions.id=840). No detect-and-reuse: a user's own Ollama is invisible
to QR's model registry, Focus eligibility and Cloud Chat, and the sidecar uses
its own models directory (QR_DATA_ROOT/ollama_models). Port 11434 is probed
only to log a GPU/RAM contention warning; it never gates or redirects the
sidecar.

Built today (ollama_sidecar.rs, ollama_ownership.rs; items.id=668):
- Port: the sidecar prefers 127.0.0.1:21434 and falls back through 21435-21443
  in order (PORT_CANDIDATES), no manual port setting. Each attempt spawns with
  OLLAMA_HOST=127.0.0.1:{port} and polls /api/tags only while its own child is
  alive; a child that exits means the port is not ours, so the next is tried,
  and a 2xx that arrives after the child died is rejected. Only an EXIT moves
  to the next port: a child still alive after the 5 s window without answering
  (a slow start, e.g. GPU discovery) is stopped (provable members only), no
  other port is tried for that binary, and the next candidate binary is tried.
  A child that is alive but not provably QR's is never waited on indefinitely:
  it is left alone and its record is kept. All ten ports failing gives
  Unavailable and an error in the log naming the ports tried; nothing is killed
  and no notice is shown (a user-facing notice is a Chat-BRAND design item).
  The winning port is ACTIVE_PORT, published (Release) before the trust flag and
  read through ollama_sidecar::port(). The port list is a field on
  OllamaSidecar so tests inject their own; no test binds 21434-21443.
- Name: the bundled binary is qr-ollama (externalBin binaries/qr-ollama, stub
  src-tauri/binaries/qr-ollama-<triple>). The debug-only PATH fallback still
  launches the system `ollama` (items.id=583 retires it).
- Ownership proof: QR signals a process only if it is a live, non-zombie member
  of a process group QR recorded itself AND its exe file name is qr-ollama or
  llama-server (plus `ollama` in debug builds only); an unreadable exe() is not
  provable and is never signalled. Nothing matched by command-line argument or
  environment alone is ever signalled, and the broad sweep over all processes is
  gone. The group is recorded at spawn, before readiness, in
  QR_DATA_ROOT/ollama_sidecar.pid, a JSON array of {pid, start_time, pgid,
  owner_pid, owner_start_time} (the old single-object format is ignored). Reclaim
  skips a group whose owner QR is alive, drops a record whose leader pid was
  reused (start_time differs), and for a live leader also requires the exe name,
  OLLAMA_MODELS equal to models_dir (env is an extra check, never sufficient) and
  is_orphaned; a headless group (leader gone, runner left by an abrupt stop) is
  reclaimed by member name. stop() and liveness_tick() use the same
  kill_process_group and keep the record if a provable member survives. The
  residual pid-reuse risk is documented in ollama_ownership.rs.
- Startup reclaim (items.id=586) therefore touches only recorded groups.
- No cloud (items.id=848): every launch sets OLLAMA_NO_CLOUD=1 (sidecar_env() in
  ollama_sidecar.rs, with OLLAMA_HOST and OLLAMA_MODELS). Without it Ollama 0.40.13
  fetches its model-recommendations list from ollama.com at startup and every 4 h,
  which left a TCP connection to ollama.com open (about 90 s per refresh in the
  captures; items.id=848 first saw it last a whole 13 min run), with no user
  action, and read the user's ~/.ollama/id_ed25519 while doing so. With it:
  no non-loopback connection and no key read in idle, model load or chat. The
  variable is not on the inherited allowlist, so a user's own value cannot reach or
  override it; a user's ~/.ollama/server.json can only add to it (HOME is inherited).
  Verified on 0.40.13 only; the packaged binary's version is unknown, so
  items.id=583/619 must assert the variable and the capture on it.
OllamaClient::base_url() is always http://127.0.0.1:{ollama_sidecar::port()}
and never reads OLLAMA_HOST or OLLAMA_PORT (items.id=616); take the port from
ollama_sidecar::port(), never a literal. The sidecar does not inherit QR's
environment: ollama_spawn_thread.rs clears it and passes only the INHERITED_ENV
allowlist (PATH, HOME, locale/XDG, LD_LIBRARY_PATH, the CUDA/ROCm/HIP/HSA/Vulkan
device-selection variables, HTTPS_PROXY/NO_PROXY/SSL_CERT_*) plus QR's own
OLLAMA_HOST and OLLAMA_MODELS. No user OLLAMA_* variable reaches the sidecar in
R1, so a user's OLLAMA_VULKAN or OLLAMA_IGPU_ENABLE has no effect on it until
QR has its own setting. wait_for_ready counts a 2xx only if the sidecar child
is still running when it arrives. Tests aim OllamaClient at a mock listener with
ollama_sidecar::force_port_for_test / ForcedPortGuard, not the environment.

Single instance (decisions.id=863, items.id=667), built in main.rs: a second
launch raises the first window and exits. tauri-plugin-single-instance is pinned
=2.4.5 (2.5.x needs tauri 2.12) and registered first in the Tauri builder,
release builds only (cfg(not(debug_assertions)), no override in a shipped
build), after doc_parse::dispatch_parse_mode and dispatch_cef_subprocess; a
test in main.rs pins that order. It initialises inside `.build()`, so the
second process starts no CEF, data-directory work, sidecar, reclaim or pidfile
record. Callback: raise_main_window shows the main window, unminimizes it only if
minimized, focuses it, and logs and returns if the window does not exist yet.
Limits, accepted: the lock is a name on the D-Bus session bus (Linux), derived
from the identifier, so it is per login session, ignores HOME/XDG, and does
not cover two OS users or a separate bus; with no usable session bus it fails
open and a second instance starts, silently; a malformed
DBUS_SESSION_BUS_ADDRESS panics plugin 2.4.5 at launch. Wayland may only flag
the window as demanding attention. Windows and macOS are unverified
(items.id=700, 701). deny.toml ignores RUSTSEC-2026-0370/0371/0372 (zbus 5 pin).

Related and still open: the bundled binary is a stub and the runner layout of a
packaged build is unverified, so the member name list in ollama_ownership.rs
must be revisited there (items.id=583, ready). Where the guard does not apply
(debug builds, no session bus, another OS user or bus) a second QR instance on
the same data directory starts a second sidecar on the next free port and its
record is kept apart from the first's (neither is signalled).

## Privacy model (items.id=678, decisions.id=864)
The Privacy Filter model (OpenAI privacy-filter, LocalAI-io GGUF q8, Apache-2.0)
is a required component, not bundled in the installer. src-tauri/src/privacy_model/:
pin.rs (size 1,637,572,192, SHA-256 80efc180...98c, upstream revision 935d8688...,
release tag privacy-filter-q8-935d868), paths.rs, marker.rs, install.rs, runtime.rs.

- Where: <data_root>/models/privacy-filter-q8.gguf, with a `.verified` marker
  (size, mtime, hash) beside it. On Linux the data root is
  ~/.local/share/ai.quietrabbit.app (the Tauri identifier), NOT
  ~/.local/share/quietrabbit. The dir also holds scores.db: only those two exact
  names are touched, nothing is listed or scanned.
- When: finish_login calls privacy_model::runtime::ensure_started() at EVERY
  login (not awaited, single flight, no account state: it keeps running through a
  lock or logout; a source scan pins that). It starts only in a build with the
  Privacy Filter library linked (cfg privacy_filter_available) and only for the
  installed location. Order of a run: stat the marker (valid: just load), hash a
  file of the right size that has no valid marker (a wrong file is deleted),
  free-disk check, fetch (QR release asset first, upstream revision second, both
  checked against the same size and hash by pinned_asset, Range resume into
  `.part`), record the marker, then privacy_filter::reinitialize() (no restart).
  A file of the pinned size and hash placed at the exact path is accepted without
  any download (offline sideload); click retry or log in again afterwards.
- A release build loads the model only while the marker is valid
  (loadable_model_path); QR_PRIVACY_FILTER_MODEL and the $HOME legacy path exist
  in DEBUG builds only. A PF-enabled debug run (PRIVACY_FILTER_LIB_DIR set)
  WITHOUT QR_PRIVACY_FILTER_MODEL starts a 1.6 GB download at the first login,
  because the dev model is not under the data root. Set the variable (or put the
  file at the data-root path) before running `cargo tauri dev` with the library.
- pf_load returns a NON-NULL context carrying an error for a missing or corrupt
  model; load_decision treats that as failure and frees it. The instance is a
  ReloadableSlot: loaded once and never replaced; a failed load is latched until
  reinitialize(); "no verified model yet" is not latched.
- Status feed: PrivacyFilterStatus (ready, downloading{percent}, verifying,
  unavailable{reason}); reasons not_compiled_in, model_unavailable,
  download_failed, low_disk, verification_failed. Progress is on `task-progress`
  with kind `privacy_model` (total is the pinned size); retry is the IPC command
  retry_privacy_model_setup. Frontend copy is placeholders only (Chat-BRAND,
  items.id=839).
- Release asset: .github/workflows/privacy-model-release.yml, manual only. With
  publish off it downloads from the pinned revision, checks size and hash against
  pin.rs and keeps a 1-day artifact; with publish on it creates a DRAFT release
  (--latest=false) that Jason publishes by hand. Until it exists the first source
  answers 404 and the upstream revision serves the file.
- Measured: hashing the 1.6 GB file takes about 0.85 s warm (sha256sum); sha2 is
  built with opt-level 3 in the dev profile because it took 43 s at opt-level 0.
- Test the real library and model by hand (CI builds without the library):
  cd src-tauri && TMPDIR=$HOME/.cache/qr-678-tmp CARGO_TARGET_DIR=$HOME/.cache/qr-618-target-pf PRIVACY_FILTER_LIB_DIR=$HOME/privacy-filter.cpp/build/release-portable cargo test --lib privacy_filter::tests::pf_enabled
  (never build under /tmp, a 16 GB tmpfs).
- Unverified: the GitHub release redirect host until the asset is published; any
  Windows or macOS behaviour (app_data_dir values, FFI paths, mtime granularity,
  free-space probe; items.id=700, 701). Accepted: no automatic in-session retry
  (a retry is a login or the retry command); two processes on one data directory
  are not coordinated (the release single-instance guard covers shipped builds).

## Model blocks (items.id=894, decisions.id=941/942)
Three block kinds share one registry (conductor/block_dispatch.rs,
registered_block()): Pure (a BlockHandler over the TaskTrack, cb-06), Model
(conductor/model_block/: cb-12 draft document, cb-13 voice apply or refine,
cb-14 revise) and Seed (declared, none registered yet). A model block has no
handler: at LOAD a composition row citing it is parsed (typed, deny_unknown_fields,
configuration `{common, spec}`) and EXPANDED into an ordinary ActionDefinition with
block_stable_id None and a `requirement` (task need, range clamped to the Focus
ceiling, optional model-class preference, never a model id). Gate 1, the spend gate,
Gate 2, Gate 3 and the declared-inputs lint all apply on that one path. The executor
does not read `requirement` yet (select_model still decides, until the resolver B2).
- EXTERNAL_MODEL_BLOCKS_ALLOWED = false (model_block/mod.rs): LOAD refuses any
  destination but local and any tier above local_only. A block allowing hosted_api
  names neither {user_input} nor {persona_context}. B10 flips the constant after the
  outbound review (items.id=884).
- tighten() (model_block/config.rs) is the one function that narrows an envelope
  (quality floor, highest tier, destinations, declared inputs); only those four are
  tighten-only. Latitude and the voice settings are the user's to move both ways
  (document_type::resolve_settings). flag_unsupplied_references is tighten-only
  against based_on (decisions.id=548).
- Every sentence a block emits is DATA in src-tauri/block_data/document_types.json
  (phrases, scales, shipped document types), validated by the same function a custom
  type will use, with no {token} allowed. Rust holds only the template skeleton.
  Tuning wording is a data edit.
- Adding a block: register it, add its mirror row (a numbered data-only shared_*.sql),
  and add its row to tests/fixtures/building_blocks_catalog.json (Chat-PM re-exports
  it from qr_docs.db). conductor/model_block/admission_tests.rs checks the rest, and
  PINNED_KEYS there fails on any new configuration key until it is reviewed.
- focus_runs.user_input holds the blended chat window so a parked run can resume. It
  is NULLed when the status becomes complete, cancelled or failed (output_store::
  USER_INPUT_CLEARED_STATUSES) and is not written for a latched one-time chat. A run
  parked and never finished keeps it until items.id=681 and 888 clear it.
- Resuming a requires_user_handoff step re-parks it (pinned by a characterization
  test in lifecycle.rs; B5 changes it).
- The unsupported-additions measures live only in examples/focus_calibration.rs for
  now. They move into one shipped module at B6b; do not add a second copy.

## Naming Architecture — term map

Most legacy terms are retired; canonical terms are used in all new code.
Two rows below (Plan->Topic, Task->Action) are ADR-013-locked canonical
pairs; both are now propagated to code (Task->Action by items.id=552).
Check the Notes column for any remaining exception.

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
| Task                 | Action              | ADR-013-locked (D6-214--D6-225); migration   |
|                      |                     | complete (items.id=552): action_id and       |
|                      |                     | ActionDefinition used throughout code and    |
|                      |                     | schema. Retired: step_id, StepDefinition.    |
|                      |                     | Table focus_run_steps and reenter_step()     |
|                      |                     | keep their "step" names (not renamed).       |

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
- Account creation order (items.id=614, 630): an account row is written only after its first per-account databases (integration_keys.db, cloud_chat_cookies.db) exist, and a failed first-run bootstrap removes users/<user_id>/ and leaves no row. A later login recreates a missing first database under the same data key.
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
