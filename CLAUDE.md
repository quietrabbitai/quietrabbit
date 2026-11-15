# Quiet Rabbit — Claude Code Context
# Last updated: November 15, 2026 (items.id=387: new "Manual hand-off" section, the fourth
# block kind in "Model blocks", and the one line under it that said B5 changes the re-park).
# Before that, November 15, 2026 (items.id=896: new "Model resolver" section, and the
# one line under "Model blocks" that said the executor did not read `requirement`).
# Before that, November 15, 2026 (items.id=895: new "Per-pass artifact store" section,
# and the catalog re-export line under "Model blocks" only). Before that, November 14, 2026
# (items.id=891: new "Focus-owned entities and the
# device library" section only). Before that, November 14, 2026 (items.id=920 stages 1 to 3: new
# "Collection consent" section only). Before that, November 14, 2026 (items.id=894: new "Model blocks"
# section only). Before that, November 8, 2026 (items.id=552: Naming Architecture Task->Action
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
Four block kinds share one registry (conductor/block_dispatch.rs,
registered_block()): Pure (a BlockHandler over the TaskTrack, cb-06), Model
(conductor/model_block/: cb-12 draft document, cb-13 voice apply or refine,
cb-14 revise), Render (cb-15, the manual hand-off starter: see "Manual hand-off"
below) and Seed (declared, none registered yet). A model block has no
handler: at LOAD a composition row citing it is parsed (typed, deny_unknown_fields,
configuration `{common, spec}`) and EXPANDED into an ordinary ActionDefinition with
block_stable_id None and a `requirement` (task need, range clamped to the Focus
ceiling, optional model-class preference, never a model id). Gate 1, the spend gate,
Gate 2, Gate 3 and the declared-inputs lint all apply on that one path. The executor
resolves `requirement` through the model resolver (see "Model resolver" below); a step
with no requirement still goes through select_model.
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
  and add its row to tests/fixtures/building_blocks_catalog.json. CODE re-exports that
  file (items.id=895 moved it from Chat-PM): once Chat-PM has written the building_blocks
  row in qr_docs.db, read it READ-ONLY with db_query.py (03_ProjectDocs/scripts/db_utilities,
  SELECT json_object(...) gives exact text), copy the columns unchanged, set the
  informational `kind` (pure, model, render or seed: the database has no such column and the
  admission test checks it against the registry), and update `exported` and the row's
  `source`. Never edit qr_docs.db or the vault from here. conductor/model_block/
  admission_tests.rs checks the rest, and PINNED_KEYS there fails on any new
  configuration key until it is reviewed.
- focus_runs.user_input holds the blended chat window so a parked run can resume. It
  is NULLed when the status becomes complete, cancelled or failed (output_store::
  USER_INPUT_CLEARED_STATUSES) and is not written for a latched one-time chat. A run
  parked and never finished keeps it until items.id=681 and 888 clear it.
- Resuming a requires_user_handoff step re-parks it (pinned by a characterization
  test in lifecycle.rs). items.id=387 does NOT change this: the manual hand-off runs on
  chat passes and parks nothing (see "Manual hand-off"). rehydrate_focus_run restores
  neither context_key nor the turn text, so a resumed run cannot run a store writer
  until items.id=888 persists context_key.
- The unsupported-additions measures live only in examples/focus_calibration.rs for
  now. They move into one shipped module at B6b; do not add a second copy.

## Model resolver (items.id=896, decisions.id=941 point 4, decisions.id=942 point 9)
A pure resolver chooses the model of a MODEL-BLOCK step: the lowest-cost model that
reaches the step's quality bar inside its ceiling. Rules are in the header of
conductor/model_resolver.rs; the gatherer is conductor/model_resolver_input.rs; the glue
is select_model_for_block in executor.rs.
- SCOPE. Only a step whose ActionDefinition carries `requirement`, on a run that is not
  a bound API Chat run (execute_once: resolver_req). A step with no requirement keeps
  Step 4.5 and select_model's hardcoded ids (decisions.id=870), and a bound API Chat run
  keeps its bound destination whatever the step carries. Both are pinned by tests in
  executor.rs.
- ANSWERS. Selected (with the other qualifying LOCAL models as ranked fallbacks), TooWeak
  (nothing reaches the bar: the best one below it, and every other candidate as below the
  bar or not measured) or NoCandidate (no destination, none ready, window too small).
  The last two park the step (F10, AwaitUser) with the codes resolver.too_weak and
  resolver.no_candidate in spend/codes.rs; en.json has placeholder keys under
  resolver.stop (Chat-BRAND owns the wording) and a Rust test requires a key per code.
  The required bar is quality_floor.min. Nothing below it is ever selected; quality_floor.max
  is not read by the resolver (B4, the run plan, uses it).
- RANK among qualifiers: local, then free, then plan-included, then metered; at
  anonymous_preferred an anonymous provider first inside a cost tier; then the smallest
  model class (or the class nearest the block's class_preference, which ranks and never
  filters); then estimated cost; then ids. A local model that qualifies always beats a
  hosted one. No model or provider id is named in the resolver (a scan test pins it).
- CAPABILITY DATA. shared.db model_capabilities (shared_045.sql): one measured cell per
  (subject local or hosted, provider, model, task type, document profile), holding the
  highest named bar reached (below_basic, basic, standard, high), the model class, the
  sample count and provenance. A cell with NO row is UNMEASURED, which is not below_basic:
  an unmeasured model, hosted or local, is NEVER chosen automatically and is listed as
  not measured in TooWeak's options (pinned in the resolver and in executor.rs tests).
  Rows come only from the signed catalog (its `capabilities` section, validated and
  undoable in provider_catalog/apply.rs, needing min_shared_schema 45) and from
  shared_045.sql's seed, which is the baseline catalog's own section; nothing at run time
  writes the table (persistence/model_capability_store.rs is read-only).
- PROVISIONAL BARS (Jason's read of the variant calibration run, decisions.id=942 addendum
  3 b): llama3.2:3b and llama3.1:8b are basic for rma_letter, complaint_email and
  personal_note; both are basic but MARGINAL for cover_letter; qwen2.5:7b is basic for
  personal_note only, and basic but MARGINAL for essay_section with the other two.
  qwen2.5:7b is unrated for the RMA letter, complaint and cover letter, creative_story
  was not run, and no model reaches standard or high anywhere. Preferences live in `note`
  only. A block whose floor is standard on a local-only ceiling therefore returns TooWeak
  until items.id=899 and voice tuning change the bars. The admission test
  (model_block/admission_tests.rs, check 10) lists the unrated shipped types
  (UNRATED_SHIPPED_TYPES: creative_story) and block task types (UNRATED_BLOCK_TASK_TYPES:
  quick_response, which cb-13's example uses, so a voice step cannot resolve until a
  catalog rates that task type); a new type or block forces a decision there.
- DOCUMENT PROFILE. The profile is the id of a SHIPPED document type and is read ONLY from
  the pass's seeds under the bound name `document_type` (DOCUMENT_TYPE_NAME in
  model_resolver_input.rs), never from the block's own configuration. A custom type
  resolves through `based_on` to its shipped type (profile_for_document_type; custom types
  are not built yet, items.id=905, so the glue passes none). items.id=897 registers the
  name in bound_spec and guarantees it a value (a Focus preset that fixes its type seeds
  the same name at seed time). An absent or unmappable name means no profile, the resolver
  reads only the task-type-wide cell, the shipped data has none, and the step parks with
  resolver.too_weak: fail closed on purpose. The exact profile's cell wins, then the
  task-type-wide cell, never another profile's.
- READINESS (the gatherer only marks Ready or not). Local: a curated focus-eligible row,
  its tag exactly one of CURATED_LOCAL_MODEL_TAGS, installed and not disabled by the user.
  Hosted: a cloud_inference_api provider the ceiling permits (providers_permitted_by_
  ceiling), with a stored key, a current consent and a readable price (the user's override
  first); a model of unknown price is never chosen automatically. The spend gate still
  decides at call time. Cloud Chat panes are never candidates (decisions.id=680).
- NETWORK PREDICATE (the privacy-critical diff, reviewed line by line by Chat-PM). For a
  resolved step, `hosted_step` and the hosted call's provider follow the CHOSEN model
  (selected_model.hosted_model, selected_model.provider_id), not the ceiling or the user's
  preference. SelectedModel::local and ::hosted are the only constructors the resolver glue
  uses, so a hosted selection always carries its provider and its row together, which is
  why the generation branch's `unreachable!` stays unreachable. select_model_for_block
  refuses a hosted choice under a LocalOnly ceiling before any gate. The render branch is
  unchanged and still keys on the ceiling. executor.rs has a source-scan test pinning the
  three sites.
- CONTEXT FALLBACK. If a resolved local step overflows its window after the render, it
  moves to the next ranked local candidate that fits. It does NOT re-render and does NOT
  re-run Gate 1 (every fallback is local, as is the model it replaces, so the provider
  Gate 1 recorded, none, is unchanged). The disclosure log records a provider and fields,
  not a model tag, so it is the same with or without a fallback; the switch is logged only
  as an info line with the two local tags. A step that sets its own num_ctx never falls back.
- KNOWN BEHAVIOURS LATER BUILDS MUST RESPECT:
  1. Gate 1, the abstraction floor and the render key off the run's CEILING
     (effective_access), not the chosen model (spec Parts 6e, 6g, 7b). A local model chosen
     under an external ceiling is therefore gated and rendered as if it were external, which
     is stricter, not weaker. Jason ruled that the privacy gate must change before R1; the
     work is items.id=921 (Plan Mode first, after items.id=896 lands). Do not change it in
     the resolver or the executor before then.
  2. Until items.id=897 registers the document-type name, every model-block step parks with
     resolver.too_weak (nothing runs model blocks today).
  3. PlanIncluded is never produced (nothing records which plan a user holds): a hosted
     model is Free or Metered. A hosted model is never chosen until it has a measured cell
     (items.id=788, 726), and EXTERNAL_MODEL_BLOCKS_ALLOWED is still false (items.id=884,
     913). The hosted call through execute_once is not tested end to end (the real client is
     HTTPS only); hosted selection and the call site are tested separately.
  4. Local context windows still come from get_context_window's hardcoded tag map in
     executor.rs, which names models in code; the gatherer and the fallback reuse it.
  5. Each retry resolves again, and a resumed run resolves from its seeds as they are then.
  6. The resolver's context filter is a lower bound (the template's fixed text), so it
     never drops a model the post-render check would accept; the post-render check stays
     the authority.
  7. The baseline catalog's catalog_version is unchanged (2026101100) though its content
     gained capability cells dated 2026-11-13; Chat-PM records the bump on items.id=730 and
     the first signed catalog carries its own version. The baseline agreement test applies
     the baseline at a fixed later date for that reason.
  8. items.id=633 must absorb shared_045.sql and regenerate tests/golden/schema_shape.txt.
- Spec: PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md (Chat-PM, rule 71) needs the resolver and
  the capability data, and Part 8h's per-user hosted ranking (verification status,
  qr_recommendation_rank, key-configured) is NOT used by the resolver, which ranks by cost
  tier, anonymity at anonymous_preferred, model class, estimated cost and ids.

## Per-pass artifact store (items.id=895, decisions.id=942 point 5, decisions.id=938 ruling 2)
Multi-turn work rides on chat-level passes, so text a later pass needs from an earlier one
(a brief, voice guidance, a validation report, delta items) that does not fit a 2048-byte
session variable lives in pass_artifacts, in the persona's encrypted outputs.db
(schema/outputs_015.sql, persistence/pass_artifact_store.rs). items.id=633 must absorb
outputs_015 with the other outputs files.
- Table: one row per (context_key, focus_id, kind), content 1 to 16384 bytes (the schema
  CHECK and ARTIFACT_MAX_BYTES), version compare-and-set, no foreign keys, no triggers, no
  revision history (a write replaces the text). kind is the closed Rust enum ArtifactKind
  (no SQL CHECK, a round-trip test pins it): content_brief, voice_guidance,
  validation_report, delta_items. session_intent is not a kind.
- Store module: put_artifact, get_artifact, clear_artifact ONLY, over session_store's
  BEGIN IMMEDIATE transaction and one-time latch (a latched chat refuses every write and the
  clear, an unreadable latch fails closed). Stored text is exact: nothing rewrites it.
- RESERVED BOUND NAMES: content_brief, voice_guidance, validation_report, delta_items
  (bound_spec.rs ARTIFACT_NAMES, Source::Artifact, Presence::Optional, Authored). A name
  equals its kind, a test pins it, and the name is the artifact's alone: LOAD refuses it as
  a row's output_var (declared-inputs lint), as a session_put target and as the argument of
  any store writer. A step that produces one of these texts uses another output_var (the
  cb-13 refine example uses refined_guidance) and an artifact_put row stores it under the
  name. cb-12's brief_var content_brief is the artifact, read in a later pass.
- Writers (store_op.rs, store_writer.rs; no second write path): artifact_put (store_target is
  the artifact name, one slot `value` from an earlier step's output variable) is gated by the
  collection consent like session_put (a per_field Focus is always Granted). artifact_clear
  (store_target only, no slots) removes one kind of the running Focus and chat and is not
  gated, like session_clear. Both are latch-gated and scoped to the running chat and Focus,
  never a row's choice. The writer reads the current version and writes with it, as
  put_session does.
- Reads: ONLY as bound names, seeded once at pass start (seeding.rs read_artifact). They are
  store-backed like every bound name: restricted to the step whose own template names them,
  tainting a step that names one (so it stays out of extraction), and refused in an external
  step, named or declared, while STORE_NAMES_EXTERNAL_ALLOWED is false (declared_inputs.rs
  pin test; items.id=884 flips it, and only then does the declaration rule decide). A stored
  text that fails its validator fails the pass closed.
- TWO BEHAVIOURS LATER BUILDS MUST RESPECT (items.id=903 and 907 carry the same note):
  1. Seeded artifact text has `{` and `}` turned into `(` and `)` (seeding::clean, as for the
     derived names, so stored text cannot become a prompt token). The stored text is exact,
     so a JSON delta_items renders with parentheses in a prompt. Read it through the store
     function, not a prompt, when the exact text matters.
  2. An absent artifact renders as NOTHING: Optional presence means no seed entry, so a
     guard on it is false and its token is stripped. A step that needs an artifact must be
     guarded on `present`, or it runs with an empty input. A value written in a pass is seen
     by the NEXT pass (seeds are read once at pass start), never by a later row of the same
     pass.
- Logging: artifact text never reaches a log or an error. The one log line is
  store_writer::log_artifact_write (kind, outcome, a Fingerprint of byte count and a short
  SHA-256 prefix), pass_artifact_store and seeding log nothing, Debug on PassArtifact and
  ArtifactWrite redacts the text, and source-scan tests pin all of it. Use fingerprint() if
  you must log one.
- RETENTION AND PURGE: items.id=888's clear_chat and retention sweep MUST ALSO cover
  pass_artifacts (delete by context_key, and by updated_at for the sweep), the same as
  session_variables. Neither exists yet and this item added neither: until they do, a
  chat's artifacts stay in the persona's outputs.db. clear_artifact is the only delete today.
  Not reached by the entity purge (an artifact names no entity id).

## Manual hand-off (items.id=387, decisions.id=942 point 7, decisions.id=846)
A person takes a starter to a provider by hand (a chat site, a model they hold an account
with) and pastes the answer back. It is TWO CHAT PASSES, not a parked step: nothing waits
in a run, so nothing depends on resume. The legacy requires_user_handoff pause is untouched.
- OUTBOUND (cb-15, BlockKind::Render, conductor/handoff_render.rs). A Focus row cites cb-15
  directly with `{display_name, output_var, template, declared_inputs}` (typed, every key
  required). It renders the template from its DECLARED inputs only (values brace-cleaned
  through seeding::clean, so one value cannot pull in another's token), records the starter
  as the pass's output and then sets handoff_pending. NO model call, no provider, no gate:
  the copy-time review (decisions.id=846: "Copy last response" and every copy path) is the
  gate, and there is no export command. The starter must be the pass's LAST output, because
  that button copies the newest assistant reply. A source scan pins that the render row never
  names execute_once, hosted_step_call, a provider, a gate or the spend gate, and the render
  dispatch sits before the tier computation in execute_step. Its only use of the executor is
  render_template (pub(crate) since this item) and find_tokens.
  - It is a block row to the registry and an EXTERNAL step to the declared-inputs lint
    (ActionDefinition::is_external_for_lint): declared inputs only, no persona_context,
    voice_profile, previous_output or focus_context (refused at parse, declared or not),
    only {user_input} among the system tokens, no field_requirements. While
    STORE_NAMES_EXTERNAL_ALLOWED is false it may not name or declare a store-backed name, so
    THE REAL STARTER NAMING content_brief WAITS FOR items.id=884's flag flip; until then it is
    built and tested with the fixture Focus (static text or untainted earlier variables). In a
    Focus that also has a handoff_ingest row it cannot name {user_input} either (taint below).
  - Its record inherits the highest sensitivity_severity, any personal_flagged and
    audit_unavailable of the steps that produced its DECLARED inputs (last producer of each
    variable); an input with no producing step (user_input, a seeded name) contributes
    nothing, a known gap to close when the flag flips and a store-backed name can be declared.
    routing_tier_used stays 1: the render itself calls nothing.
  - handoff_pending (Session bound name, `h_` plus 12 lowercase hex of SHA-256 over run and
    row) is written by the render through the typed session store: latch-gated (a one-time
    chat still shows the starter and records nothing), NOT collection-consent gated (no user
    content). A run with no chat renders the starter and records nothing.
  - EXTERNAL_MODEL_BLOCKS_ALLOWED does not govern this: it is one boolean for a MODEL block's
    external destinations (hosted_api, manual_handoff) and still waits for B10. A ManualHandoff
    range of a model block is refused until the run plan (items.id=901) selects it.
- RETURN (handoff_ingest, conductor/handoff_writer.rs, the 17th store op). The pasted reply
  is the NEXT chat turn. The row is guarded on handoff_pending `present` (LOAD refuses it
  otherwise), has NO slots and a required store_target (the pass variable the H- handle is
  published under), and is gated by the collection consent. Order: consent, the one-time
  latch (a latched chat stores no blob), a turn must exist, the paste caps, a hand-off must
  be pending, store_draft, then session_store::settle_handoff_return clears handoff_pending
  and sets handoff_received in ONE BEGIN IMMEDIATE transaction. A crash between the draft and
  that transaction leaves a stored reply the chat does not point at and the hand-off still
  pending, so the retry stores a second reply: accepted, both are the user's Library documents.
  - THE ONE EXCEPTION to "raw user text is never a store argument" (recorded in the store_op.rs
    header): handoff_ingest reads the raw turn from WriterContext.turn_text, because a
    variable would be a model's copy of the paste. The turn is FocusRun.current_turn (a
    CurrentTurn: not Clone, not Serialize, not Display, Debug redacts it), set only by
    send_message through set_current_turn, lent as TurnText whose accessor is private to
    handoff_writer.rs, NEVER persisted, checkpointed, snapshotted, serialized or logged
    (the one log line carries a byte count). rehydrate_focus_run never sets it, so a resumed
    run has none and the op fails closed. Scans in conductor/handoff_writer_tests.rs pin
    that no other file reads turn_text and that there is one sender, one field and one lender.
  - The reply is stored through persistence/ingested_document_store.rs (the three ingest writes
    moved out of commands/ingest.rs, behaviour unchanged) by persistence/handoff_draft_store.rs
    as an ingested output of type handoff_draft filed under the Focus: sensitivity personal,
    PASTE_MAX_BYTES 256 KiB, non-blank, no NUL, no lease (the run holds one), exact text. It
    is UNTRUSTED EXTERNAL CONTENT read ONLY by its H- handle (handles::Kind::Draft, resolved
    among the Focus's own non-deleted replies) through read_draft_by_handle; no template names
    a handle and no seeded name carries the text (a Derived handoff_draft name is deferred
    until items.id=907 states its size needs: derived text caps at 6000 bytes, a reply at 256
    KiB). The chat turn that carried the paste has no byte cap of its own (commands::messages).
  - TAINT (conductor/taint.rs): in a Focus that has a handoff_ingest row, `user_input` is a
    tainting name, because the blended chat window ({user_input}, focus_runs.user_input) carries
    the paste as its newest message for up to HISTORY_WINDOW messages. A step naming it is left
    out of extraction (and so is a step naming its output), and while
    STORE_NAMES_EXTERNAL_ALLOWED is false an external step of that Focus cannot name it.
  - handoff_pending and handoff_received are reserved: never a session_put target, never a
    writer's argument, never an output_var. Both are seeded at pass start and are ordinary
    store-backed names otherwise (restricted to the step whose template names them, tainting
    it, refused in an external step while the flag is false). Their normal use is a guard.
- CONSENT, FOR A FOCUS AUTHOR. In a focus_level Focus WITHOUT the current consent,
  handoff_ingest is SILENTLY SKIPPED by the collection-consent gate (declining is not a block:
  the Focus keeps working and saves nothing). A reply row such as an acknowledgement would
  still say "received" when nothing was saved, so a Focus author MUST GUARD THE REPLY ON
  CONSENT STATE, and the hand-off panel reads get_collection_consent_state next to
  get_handoff_state. A per_field Focus (the Writing Assistant) is always Granted.
- STATE FOR THE PANEL (commands/handoff.rs, IPC Group 33). get_handoff_state(persona_id,
  context_key, focus_id) is READ ONLY and never returns a reply's text: pending
  `{id, since}`, received `{handle, byte_count, received_at}` (None when the reply was deleted
  from the Library) and one_time_chat. Errors are the codes in handoff::ERROR_CODES
  (persona_not_found, invalid_focus_id, invalid_context_key, handoff_state_unavailable), each
  with a placeholder key handoff.error.<code> in frontend/src/i18n/en.json (Chat-BRAND owns the
  wording; a test requires a key per code). There is no write command, no export command and
  no paste command: the starter is copied with the existing copy review and the paste is sent
  through send_message. A cancel control for an abandoned hand-off is items.id=900's design and
  must be a pass (a guarded typed row), never a command that writes session_variables.
- WHAT items.id=912 EXPECTS: get_handoff_state, the two session names for guards, the fixture
  Focus pattern (a render row guarded on handoff_pending absent, a reply row and handoff_ingest
  guarded on present; the reply row sits BEFORE the ingest row in the fixture because the ingest
  row records nothing on the track and a real Focus follows it with model steps), and
  items.id=643 (below) landed before the panel offers any delete.
- RETENTION (for items.id=888, which must NOT repeat the pass_artifacts gap): the two session
  names are rows of session_variables, so clear_chat and the 30-day sweep cover them with no
  change. The pasted reply is NOT chat scratch: it is a durable Library "Imported" document the
  user owns, removed only through the Library, so clear_chat does NOT delete it (a deliberate
  ruling, items.id=387 plan Q1). The Library's soft delete zeroes `content` and sets deleted_at
  but leaves the encrypted blob file on disk: items.id=643 (it now blocks the panel). It is
  existing behaviour for every imported document and was left alone here.
- SCHEMA: shared_046.sql (data only, the cb-15 mirror row; the migration tests re-pinned to 46).
  No new table or column. items.id=633 must absorb shared_046.sql with the other shared files.
- NOT BUILT: the real Writing Assistant starter (waits for items.id=884's flag flip), a
  Derived handoff_draft name, multiple hand-offs inside one pass (the return pass's row order is
  the Focus's), file or document replies beyond a plain paste, any model step after the return
  row (B7c delta validation reads the reply by handle).

## Collection consent (items.id=920 stage 1, decisions.id=944, decisions.id=509)
A focus_level Focus (Tech Support) collects silently after ONE informed consent at
setup. Stage 1 is the store and the gate, stage 2 the commands and the consent text
data, stage 3 the frontend confirm.
- State: focus_settings.collection_consent (not_asked, granted, declined, withdrawn)
  and collection_consent_version (shared_044.sql). Default not_asked, so a row never
  asked never opens the gate. focuses.collection_posture (per_field default,
  focus_level) is DECLARED BY THE FOCUS (the focuses row or the .focus key
  collection_posture) and is never "not required". focus_level also means the generic
  extraction pass is skipped, wired by items.id=914, not here.
- ONLY focus_settings_store::set_collection_consent writes the state (stage 2's
  command is its only intended caller). The columns are NOT in
  UpdateFocusSettingsRequest, update_focus_settings does not touch them, so
  submit_friction_gate_decision cannot move them. No model path records consent:
  source scans in conductor/store_op_tests.rs pin that store_writer.rs, store_op.rs
  and lifecycle.rs never name the audit table or the setter, and that nothing outside
  tests calls either except commands/collection_consent.rs. The set_collection_consent
  version rule: Granted and Declined store the text version shown (1 or more), NotAsked
  and Withdrawn store 0.
- Text: block_data/collection_consent_text.json holds text_version and promise ids ONLY
  (conductor/collection_consent_text.rs; the closed PROMISES list is what QR can keep
  today, so editing reference sheets, per-case or per-output delete and cross-Focus use
  are not promisable). Prose is placeholder i18n copy in the frontend, owned by
  Chat-BRAND. Bumping text_version forces a new decision from every Persona.
- Commands (commands/collection_consent.rs, Group 31): get_collection_consent_state,
  submit_collection_consent_decision (granted or declined, with the version SHOWN; a
  version that is not current is consent_text_stale), withdraw_collection_consent,
  list_consent_audit_events. Each takes writer_gate::command first, derives the user and
  key from the session (no user_id argument), checks the Persona id is a UUID the user
  belongs to (private require_persona, for items.id=638 to lift) and has a *_for_session
  body. Write order is fail-safe both ways: a GRANT appends the audit event FIRST and the
  flag second (a crash leaves collection off, the retry reuses the event); a WITHDRAWAL
  clears the flag FIRST and appends the event second, and errors if the event fails so the
  frontend retries. A decline appends no event of its own, but on EVERY decline call, if the
  newest collection event is still a grant it appends collection_consent_withdrawn
  referencing it (retry semantics as withdraw). A withdrawal keeps what was collected and
  deletes nothing. needs_decision is true for not_asked, and for granted or declined at a
  version other than the current one; a withdrawn user is never re-asked.
- Audit: consent_audit_records in each Persona's encrypted personal.db
  (personal_010.sql, next to disclosure_log; never shared.db, decisions.id=862).
  persistence/consent_audit_store.rs has append, list and latest_for_focus ONLY: no
  UPDATE, no DELETE (source-scan test). Event types are the Rust enum
  ConsentEventType (no SQL CHECK, round-trip test). content_summary is a fixed abstract
  description, never user content. A purge is a separate user-initiated function.
- Gate: WriterContext.collection (Granted or NotGranted) is built in
  FocusRun::current_collection() from a FRESH read of focus_settings immediately before
  EACH writer op (not the AUTHORIZE snapshot). A per_field Focus is always Granted and
  never reads the row. A focus_level Focus is Granted only if collection_consent =
  granted AND the version equals the current text version; a missing row, a missing
  definition or an unreadable state is NotGranted. run_store_op checks it FIRST (before
  the context key and the latch) for every Op::requires_collection_consent() op and
  returns Outcome::RefusedNoConsent, writing nothing and calling no store function.
  NOT gated: Latch (it suppresses writes) and SessionClear (it removes data). Declining
  is not a block: the Focus keeps working and saves nothing.
- Latch and consent are independent: granted and open writes, granted and latched is
  RefusedOneTime, not granted is RefusedNoConsent. Granting mid-chat never lifts a latch.
- write_focus_run_record's keep_input is also consent-aware: the blended chat window is
  not persisted in focus_runs.user_input for a focus_level Focus without the current
  consent (an unreadable state counts as not granted).
- Frontend (frontend/src/chat/CollectionConsent*.tsx, useCollectionConsent.ts): ChatPane
  renders CollectionConsentSection for the Focus ids in collectionConsent.ts (tech-support
  only). It asks ONLY when get_collection_consent_state says needs_decision (the frontend
  never recomputes who is asked; a test scans for it), with fixed local i18n text (placeholder
  copy, Chat-BRAND) and one confirm ('Allow Tech Support to remember' / 'Use without saving')
  that sends the DISPLAYED version. The memory screen (stop, or allow again) says plainly that
  what QuietRabbit already knows stays until the user deletes it. Dev simulation: in a Vite dev
  build, localStorage['qr.devSimulateCollectionConsent'] = not_asked, stale_granted,
  stale_declined, declined, granted or withdrawn fakes the state (and the missing
  focus_settings_row of tech-support) in memory; the fake is tree-shaken from a production build.
- items.id=633 will fold personal_010 and shared_044 into the baselines; both files are
  self-explanatory in their headers (decisions.id=857).

## Focus-owned entities and the device library (items.id=891, decisions.id=507, 509, 940, 944)
Tech Support keeps DIRECT-SUBJECT devices only (relationships, entity_relationships and the
decisions.id=507/512/516 traversal stay deferred). Nothing here is enabled until items.id=915 seeds the
Focus and items.id=916 flips it; the hard purge ships before that.
- Ownership (personal_011.sql): entities.owner_focus (NULL = unowned, every pre-existing entity).
  entity_store::OwnerFilter (Any, Unowned, Focus) is a field of EntityFilter; a source scan
  (entity_store tests) pins that every non-test list_entities/search_entities caller outside the file
  names `owner:` and never uses the Any-by-default constructors. Persona share, sync, view-only sync,
  load_entity_facts_for_context (every Focus's PersonalTrack and the PF entity-fact resolution) and
  list_pending_cross_persona_exports exclude an owned entity and its facts. create_owned_entity_conn is
  the only writer of owner_focus. Entity serializes WITHOUT owner_focus (payload hashes unchanged).
- Facts (conductor/device_fields.rs, block_data/device_fields.json): a CLOSED catalogue, 7 categories
  and 20 fields, each with label, sensitivity, tier-2/3 abstraction policy, byte cap, and the
  access_credential_flag on serial_number. validate() refuses a field NAME containing a credential
  word (password, pin, token, secret, login...). known_issues (400 bytes) and hardware_specs (300) are
  free text in practice: nothing in code stops a typed secret, so the exclusion that covers them is
  prompt-level (the items.id=915 seed and its templates; decisions.id=507 credentials invariant) and
  no secret-shape detector exists (Jason's ruling). Subject class uses COLON separators. Facts are
  written through create_entity_fact_with_policy_conn (writes the abstraction policies; the older
  writer defaulted both to pass).
- Store layer: persistence/owned_entity_store.rs (find by replay key or exact normalized name or
  alias, create_subject, idempotent superseding put_owned_fact, rename_owned, list_active_facts).
  Everything is scoped by the owning Focus: another Focus's, unowned, archived and purge-marked
  entities are all NotFound. It has no delete.
- Writer ops (conductor/entity_writer.rs, store_op.rs): entity_find_or_create (replay key
  "{run}:{action}", then an E- handle if the Handle slot is handle-SHAPED (a handle-shaped value that
  resolves to nothing is an error; text like "none" is ignored), then the exact name, else create;
  PUBLISHES the entity id under a required store_target) and entity_fact_put (closed catalogue;
  an unknown field fails the pass). Both are in requires_collection_consent. They read the one-time
  latch (outputs.db) first and delete nothing; a latch set between the read and the write is an
  accepted window (no compensation). LOAD (store_op::check_composition(steps, row_vars, posture)):
  entity ops are refused in a per_field Focus; the entity slot of case_open, case_link,
  delta_changed, entity_fact_put and the entities slot of delta_record must name an id an EARLIER
  entity_find_or_create row published (a model never names an id; this tightened the 887/889
  contract); a Handle slot is model output only. At run time run_store_op takes Entities (Verified
  = the persona's personal.db, NotNeeded, Unchecked = tests only, a scan pins that production never
  names it) and verifies every entity id (owned by the running Focus, active) before the write.
  delta_record accepts ONE published id as a list of one. WriterContext carries persona_id.
- E- handles (persistence/handles.rs Kind::Entity): hash of (kind, focus, id), resolved among the
  Focus's own ACTIVE owned entities; the model and the UI see handles, never ids.
- Derived names (conductor/device_seeding.rs; bound_spec Derived, Always, text; read by seed_pass,
  which opens personal.db only for a pass that names one; read_seeds is outputs-only and fails closed
  if handed a device name): subject_context (the ACTIVE case's primary entity, only if owned by the
  running Focus and active, else ""; "Device: name (class)" then one "Label: value" line per
  catalogued fact in catalogue order, 200 bytes a value, 1800 total, whole lines dropped) and
  device_library_brief ("- E-XXXXXXXX: name (class) | Label: value ..." up to 8 devices newest first,
  up to 3 headline facts that the catalogue marks pass at tier 2, 60 bytes a value, "- and N more").
  subject_context renders EVERY catalogued fact, serial number and address included, as LOCAL text:
  STORE_NAMES_EXTERNAL_ALLOWED stays false (items.id=884), so both names are refused in external steps
  and tainted like every store name. Seeds are read once per pass, so a device's facts appear one pass
  late. The wording is provisional (Chat-PM reviewed; items.id=914 owns the other three texts and
  edits seeding.rs/bound_spec.rs after this item).
- HARD PURGE (persistence/entity_purge.rs, the ONLY module that deletes the entity, case and delta
  tables; a scan pins it, and the case_store/session_store no-delete scans still hold). Order:
  1 MARK (entities.status = user_deleted, the commit point; the entity is invisible to every active-
  filtered reader), 2 outputs.db in one BEGIN IMMEDIATE (active_case_id pointers to deleted cases;
  cases whose PRIMARY subject it is plus cases left with no link, cascading links and strategies;
  its remaining case_entities; its config_delta_entities; deltas left with no link; reads are
  unscoped by Focus), 3 personal.db in one BEGIN IMMEDIATE (all entity_facts versions,
  entity_relationships, dedup_candidates, the row; only an owned entity marked user_deleted is
  deleted). reconcile (no journal) finishes MARKED entities and sweeps ORPHAN ids (an id in
  case_entities/config_delta_entities with no entities row at all); it runs at login for every
  persona (supervised, Sweep lease, never awaited, creates no files), at the start of list_devices
  and in purge_all_devices. A purge stops the Focus's live runs first
  (writer_gate::request_stop_for_focus + wait_focus_runs_ended, bound by attach_lease to
  persona+Focus) and REFUSES with nothing changed if one ignores the stop for 60 s
  (RunsStillRunning). purge_all_devices also cancels the Focus's parked runs. NOT reached: chat
  messages, run outputs, focus_runs text of other runs, disclosure_log, fact-hash preference rows.
  Fail-injection hooks (PurgeStep) exist for tests only.
- Commands (commands/device_library.rs, Group 32, the consent-command pattern, none consent-gated):
  list_devices, get_device_sheet (read-only view; also addable_facts), preview_device_purge,
  purge_device, purge_all_devices (the separate withdraw-with-purge control), rename_device (device
  first, then refresh_subject_label of PRIMARY-subject cases; a retry completes it),
  set_device_fact, clear_device_fact. Devices are named by E- handle. Codes: persona_not_found,
  invalid_focus_id, device_not_found, invalid_device_name, name_in_use, unknown_field, invalid_value,
  runs_still_running, catalogue_unavailable. NOT in R1: archive, manual create, per-case/delta delete,
  editing the case and change parts of the sheet, ANY visibility setter or flag inheritance (the
  decisions.id=513 flags exist on entities and nothing in R1 reads or writes them for owned entities).
- Frontend (frontend/src/chat/DeviceLibrary.tsx, DeleteEverythingControl.tsx, useDeviceLibrary.ts,
  deviceLibraryApi.ts): a Devices entry beside the memory entry in CollectionConsentSection (not
  shown while the consent confirm is showing), the delete-everything control on the memory screen.
  The purge confirmations say what they delete (what QuietRabbit recorded: facts, tickets and
  configuration changes) and that chat history, earlier replies and the choice record are NOT
  deleted (tests pin it). Dev simulation: localStorage['qr.devSimulateDeviceLibrary'] = empty,
  devices or runs_blocked, behind import.meta.env.DEV, absent from the production bundle.
- Six-intent fixture (conductor/tech_support_draft.rs): 33 rows, 126 of 256 paths, 2..6 local calls
  per path; its device rows are entity_find_or_create writers and its templates use both device names.
- items.id=633 must absorb personal_011.sql (the owner_focus column and its index into the entities
  baseline CREATE); no shared or outputs file was added. items.id=888 keeps clear_chat, the retention
  sweep, context_key persistence, the resume pin and re-deriving pass-local published ids (an
  entity id is re-derivable from its replay key); it does NOT build any entity purge.

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
