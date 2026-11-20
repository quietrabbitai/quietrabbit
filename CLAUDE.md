# Quiet Rabbit — Claude Code Context
# Last updated: November 20, 2026 (items.id=888 pass 1: new "Run failure, resume claim and post-output
# runs" section; under "Parked-run retention" the opening "failed delete them at once" claim (false for
# the error path until 888 stage 1), the HANDLING line that said resume_execution writes `running` and a
# pointer to the post-output sweep; the user_input bullet under "Model blocks"
# (awaiting_extract_confirm keeps no input); the single-device purge under "Focus-owned entities"
# (cancels parked runs too); nothing else. Pass 2 of 888 (context_key, seed pin, clear_chat, sweeps)
# is not built.).
# Before that, November 20, 2026 (items.id=922: new "Persona context, the output scan and external-step
# scoring" section; the external-model-step gap note under "Manual hand-off", the persona_context and
# scan_output item (2) under "Privacy treatment of a step attempt" (KNOWN BEHAVIOURS) and the Prompt
# Authoring Rule; nothing else.).
# Before that, November 20, 2026 (items.id=681: new "Parked-run retention and resume staleness"
# section; the PersonalTrack line under "Key Architectural Decisions" and the user_input bullet under
# "Model blocks"; nothing else.).
# Before that, November 19, 2026 (items.id=932: new "Single-pass render substitution" section; the VALUE
# and NOT BUILT bullets under "Personal fact bridge", the Prompt Authoring Rule and the examples line under
# "Calibration harness" (CI does build examples); nothing else.).
# Before that, November 18, 2026 (items.id=931: new "Personal fact bridge" section; nothing else.
# Items.id=930 built the catalogue and the policy store it uses; the older sections were not re-verified.).
# Before that, November 17, 2026 (items.id=883: new "External steps and chat history" section; the
# focus_runs.user_input bullet under "Model blocks", the starter's {user_input} line under "Manual
# hand-off" and the RESUME paragraph under "Launch questions"; nothing else.).
# Before that, November 17, 2026 (items.id=898: new "Focus development tool" section; the dev-only
# paths paragraph under "Dev Environment" (one more compile-time path); nothing else. The
# stale "document_types seed comes from focus_dev" comment in document_type.rs is corrected in code.).
# Before that, November 17, 2026 (items.id=927: the cargo deny line under "Key commands" only -- fetch the
# advisory database first; nothing else.).
# Before that, November 17, 2026 (items.id=899: new "Calibration harness" section; the
# unsupported-additions line under "Model blocks" (the measures live in conductor/draft_check.rs)
# and the PROVISIONAL BARS bullet under "Model resolver" (now the 21 read cells; both UNRATED
# lists empty); nothing else.).
# Before that, November 16, 2026 (items.id=901: new "Run plan and strategy approval" section;
# four older lines that named the run plan as future work -- the quality_floor.max line and the
# GATE 3 line under "Model resolver" and "Privacy treatment of a step attempt", and the
# ManualHandoff range line under "Manual hand-off"; nothing else. Hosted enforcement is
# tested only in fixtures until items.id=913, ordinary external steps are not plan-gated.).
# Before that, November 16, 2026 (items.id=897: new "Launch questions and run-start settings"
# section; the DOCUMENT PROFILE bullet and KNOWN BEHAVIOURS item 2 under "Model resolver", which
# said items.id=897 would register the document-type name; the two pending pointers -- the
# device-names note now says its closed-list refusal is temporary and names items.id=924, and the
# external-model-step gap note names items.id=922; nothing else.).
# Before that, November 16, 2026 (items.id=884: STORE_NAMES_EXTERNAL_ALLOWED is true and
# the two device names stay out by a closed list -- the artifact-store, Manual hand-off and
# Focus-owned entities bullets that stated the flag, the render row's inheritance rule and the
# user_input taint note; nothing else. The outbound review itself has no section here yet.).
# Before that, November 15, 2026 (items.id=921: new "Privacy treatment of a step attempt"
# section, the render sentence in the NETWORK PREDICATE bullet and KNOWN BEHAVIOURS item 1
# under "Model resolver", nothing else).
# Before that, November 15, 2026 (items.id=387: new "Manual hand-off" section, the fourth
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
  cargo deny fetch && cargo deny check                 — advisories/licenses/bans/sources (CI gate; run `fetch` first: CI uses the advisory database of the day and a local copy that is days old passes advisories CI then fails, items.id=927)
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
debug-only PATH lookup of `ollama` (items.id=583), and focus_dev's schema-directory path
(examples/focus_dev.rs and its use of CARGO_MANIFEST_DIR for emit-migration, items.id=898).

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
  names neither {user_input} nor {persona_context}. B10 flips the constant (the outbound
  review of items.id=884 has landed).
- tighten() (model_block/config.rs) is the one function that narrows an envelope
  (quality floor, highest tier, destinations, declared inputs); only those four are
  tighten-only. Latitude and the voice settings are the user's to move both ways
  (document_type::resolve_settings). flag_unsupplied_references is tighten-only
  against based_on (decisions.id=548). A run re-expands every model block when it starts, with
  the user's launch answers and the three setting layers, and applies a mode profile through
  tighten() (see "Launch questions and run-start settings").
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
- focus_runs.user_input holds the blended chat window so a parked run can resume (and
  focus_runs.external_input, items.id=883, its user-turns-only copy: see "External steps and
  chat history"; both are written together and cleared together). It
  is NULLed when the status becomes complete, cancelled, failed or awaiting_extract_confirm
  (output_store::USER_INPUT_CLEARED_STATUSES; the last since items.id=888, F8) and is not
  written for a latched one-time chat. A run
  parked and never finished keeps it until the retention sweep expires it (items.id=681,
  see "Parked-run retention and resume staleness": it clears both input columns) or
  items.id=888 clears it.
- Resuming a requires_user_handoff step re-parks it (pinned by a characterization
  test in lifecycle.rs). items.id=387 does NOT change this: the manual hand-off runs on
  chat passes and parks nothing (see "Manual hand-off"). rehydrate_focus_run restores
  neither context_key nor the turn text, so a resumed run cannot run a store writer
  until items.id=888 persists context_key.
- The unsupported-additions measures live in ONE shipped module, conductor/draft_check.rs
  (items.id=899, moved unchanged from examples/focus_calibration.rs; do not add a second
  copy). It is pure (no I/O, no model or provider id, scans pin both): check(text, &CheckInput)
  returns a CheckReport (placeholders, opening and closing chatter, unsupported specifics,
  coverage) and decide(report, latitude, attempt) returns Accept, Regenerate, Mark or Skipped
  through CheckPolicy::for_latitude (strict: regenerate once then mark, framed: mark only,
  free: off). Coverage is keyed and used by the harness and tests only; production passes no
  points. NOTHING in the executor calls it yet: items.id=903 consumes decide() (the guarded
  second draft row of a composition is B7a and B7d), so no executor wiring lives in 899.
  The check cannot catch soft embellishment, an invented cause, a reversed fact that keeps the
  keywords, tone or readability.

## Calibration harness (items.id=899, decisions.id=950)
src-tauri/examples/calibration_harness.rs (with examples/support/, examples/harness/ and
examples/calibration_data/: synthetic briefs, voice cases, rubrics and bars.json, never
shipped) generates drafts through the SHIPPED cb-12 and cb-13 blocks on QR's own sidecar,
scores them with draft_check, has the OTHER local models cross-judge (temperature 0, a closed
JSON verdict, unparseable = no verdict), and writes read.html: a static page of at most 100
drafts (at least 3 reads per cell) that Jason reads and exports as ratings.json.
Flags: --approve-long-run (required for a real run, about 2 hours), --resume, --summarize,
--judge, --read-page, --apply-ratings <run> <file>, --emit-cells <run>, --dry-run.
- The judge only CHOOSES which drafts are read. A cell's bar comes only from Jason's reads
  (provenance jason_read, best bar that at least 66% of the reads reach, fewer than 3 reads =
  no cell); no harness_local cell is written, and the judge's agreement with Jason is reported
  (agreement.md), not trusted. Network is local Ollama only (local_url() accepts only http to
  127.0.0.1, localhost or ::1 with no credentials).
- Run 20261116-213009 (300 drafts, 600 judge calls) produced the 21 baseline cells. They
  REPLACE the first 12 seed rows in place in shared_045.sql and catalog/provider-catalog.json
  (decisions.id=857: no new schema file; the row keys collide, so INSERT OR IGNORE alone would
  have dropped them). A development database already at schema 45 does NOT get them (the seed
  only runs at migration): reseed it or apply a catalog.
- EXAMPLES ARE NOT LINTED IN CI (the CI clippy runs `--tests` only); they ARE compiled there
  (`cargo test --no-run` in .github/scripts/ci-test-shards.sh builds every target plain `cargo test`
  does, examples included, items.id=932 checked the script). Locally, from src-tauri/: `cargo clippy --tests --examples -- -D warnings` and
  `cargo test --examples`.

## Focus development tool (items.id=898, decisions.id=951)
Seeds, validates and re-seeds a Focus in a DEVELOPMENT database, emits a migration and expands a
preset. Never part of the app: a lib module with a thin example, debug and test builds only.
- LAYOUT. conductor/focus_dev/: spec.rs (the Focus spec, serde deny_unknown_fields), preset.rs,
  shape.rs, render.rs (SQL text) and drift.rs are PURE (no sqlx, no file, no network; a scan pins it)
  and always compiled. devroot.rs and tool.rs (and the Focus writers write_focus, write_launch and
  delete_focus in persistence/focus_composition_store.rs, and migrate_shared_db_at and
  shared_schema_versions in migrations.rs) are compiled ONLY under cfg(any(test, debug_assertions)):
  `test` is there because `cargo test --release --lib -- release_` compiles the test modules. A
  release build of the app contains no code that opens, writes or deletes a Focus row and no marker
  writer (a test pins the attributes, the release build compiles; symbols were not inspected).
  examples/focus_dev.rs only parses arguments and prints (a stub in release). Nothing outside
  focus_dev, its tests and the example names it (scan test); tauri.conf.json is pinned unchanged.
- THE SPEC is the source of truth, a checked-in JSON file (src-tauri/focus_specs/<focus_id>.json,
  none exists yet: the first arrives with items.id=903 or 915). It states the focuses row, ordered
  rows (an ordinary step, or a block id plus its configuration), typed guards (decisions.id=938: the
  columns, no expression), store-writer columns, a schedule trigger, launch questions and a
  one-liner. The DB is derived from it. ONE WRITER: write_focus is used by the admission tests, by
  validate and reseed; emit-migration renders the same row structs as SQL, and a test applies that
  text to a scratch database and compares every seeded table with the writer's.
- COMMANDS (cargo run --example focus_dev --, from src-tauri/): validate <spec> (scratch database, the
  REAL loader: it is the guard-path, placement, declared-inputs and launch lint), reseed <spec>
  --data-root <dir> [--apply] (dry run unless --apply), reseed --mark-dev-root <dir>, emit-migration
  <spec> [--number N] [--baseline] [--out file] and expand-preset <id> --as <prefix> --param k=v.
  emit-migration prints the SchemaFile line to add to migrations.rs by hand and never edits it.
- DEV-ROOT GUARD. reseed writes only <dir>/instance/shared.db and only of a directory that is not,
  does not contain and is not inside the real app data dirs ($XDG_DATA_HOME and ~/.local/share, then
  ai.quietrabbit.app or quietrabbit), that holds the marker .qr_dev_root written by
  --mark-dev-root (it names the canonical path, so a copied or moved directory loses it), and that
  has instance/shared.db or is empty. instance/ and shared.db must not be symlinks and must resolve
  inside the root: DevRoot::shared_db_path re-verifies on every use, immediately before the database
  is opened or migrated. Linux only (items.id=700, 701). If XDG_DATA_HOME in the shell points at the
  dev root the tool refuses it: unset it. The scratch directories it makes are mode 0700. Encrypted
  databases and users/ are never opened.
- RESEED. validate on a scratch database first, then the loader runs on a VACUUM INTO copy of the dev
  database (the loader takes a pool), then ONE transaction on the dev database (delete the Focus,
  write it, read it back, commit; any failure rolls back). A dry run never migrates.
- EMIT. A data-only shared_NNN.sql: DELETE then plain INSERT (never INSERT OR IGNORE, which cannot
  repair a stale row), header line `-- focus_dev: <focus_id> spec-sha256 <hex>`, then the
  schema_version row. --baseline omits that row: a fragment for the one-baseline file (items.id=633,
  decisions.id=857). A text value with a semicolon is REFUSED; multi-line values whose lines
  parse_statements would drop (blank, "--", "create trigger", "end") are written with char(10) joins.
  The text is parsed back and must equal what was rendered. The next number comes from SCHEMA_FILES
  and the schema directory, which must agree. items.id=633 folds the emitted files in.
- DRIFT. A test requires every focus_specs/*.json to have an emitted file (a schema/shared_*.sql or
  focus_specs/<id>.baseline.sql) whose header hash is the spec's current SHA-256. Vacuous until the
  first spec.
- PRESETS (decisions.id=942 Q7) are shipped parameterised fragments of rows in
  block_data/focus_presets.json (EMPTY until items.id=918), typed params (slug, enum, int), %name%
  slots, brace-free values, locals renamed with the use's prefix, expanded at seed time into ordinary
  rows. The runtime never sees one. A preset admission test expands every preset for every param
  combination (vacuous today, a fixture proves it bites).
- LOADER RULE (the rider). build_action_definition fails at LOAD on a present unparsable step_type
  and on a present task_type that is not one of ModelTaskType::ALL or "general" (exact match); an
  absent value keeps its default. A test pins the list equal to providers/evaluation.rs's tasks.
  RawStep and RawStepFromDb still have no deny_unknown_fields: shape.rs reports an unknown key in an
  ordinary step (ORDINARY_STEP_KEYS, pinned to the struct), and the check over every shipped Focus
  (the shared.db seeds and the six .focus files) found 0 unknown keys in 16 steps.
- NOT BUILT: a strict loader, preset launch rows, the first real spec and preset, Windows and macOS.

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
  is not read by the resolver. The run plan (items.id=901) uses it as a HINT only: an offer
  already above it is flagged and sorted last, never filtered.
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
- BARS (Jason's read of the items.id=899 calibration run, 100 reads over run 20261116-213009;
  21 cells, every one jason_read, 12 drafts each, dated 2026-11-17; many rest on only 3 reads, so
  a cell with 2 reads at one bar and 1 below takes the higher bar: the cell's note says the
  split). Writing cells (creative_writing, per shipped type): rma_letter: llama3.2:3b
  below_basic, llama3.1:8b standard, qwen2.5:7b standard; complaint_email: 3b standard, 8b high,
  qwen standard; personal_note: 3b high, 8b high, qwen standard; cover_letter: 3b basic, 8b high,
  qwen high; essay_section: 3b high, 8b standard, qwen basic; creative_story: 3b high, 8b high,
  qwen basic. Voice (quick_response, task-type-wide, the lower of apply and refine): 3b standard,
  8b basic, qwen basic. Only Jason's reads set a bar; the judge never does.
  Consequences: llama3.2:3b is NEVER chosen for an rma_letter step (below_basic), so the next
  qualifier is; a floor of high still parks for rma_letter and several others; no shipped type
  or block task type is unrated any more (UNRATED_SHIPPED_TYPES and UNRATED_BLOCK_TASK_TYPES in
  model_block/admission_tests.rs are EMPTY and a new type or block forces a decision there
  again). Preferences live in `note` only. New measurements go through the harness
  ("Calibration harness"), never by hand-editing a bar.
- DOCUMENT PROFILE. The profile is the id of a SHIPPED document type and is read ONLY from
  the pass's seeds under the bound name `document_type` (DOCUMENT_TYPE_NAME in
  model_resolver_input.rs), never from the block's own configuration. A custom type
  resolves through `based_on` to its shipped type (profile_for_document_type; custom types
  are not built yet, items.id=905, so the glue passes none). The name is registered in
  bound_spec (DOCUMENT_TYPE, Source::Launch, "Launch questions and run-start settings"
  below) and a Focus with a model block always has a value: LOAD refuses one that declares no
  document_type question, and the question's default is one of its options. An absent or
  unmappable name (a caller that supplied no launch resolution fails the pass closed before
  this; an id that maps to no shipped profile) means no profile, the resolver reads only
  the task-type-wide cell, the shipped data has none, and the step parks with
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
  why the generation branch's `unreachable!` stays unreachable. A hosted choice under a
  LocalOnly ceiling is refused before any gate by step_treatment::derive (items.id=921
  moved the refusal there from select_model_for_block, wording unchanged). Since items.id=921
  the render branch also follows the destination (`treatment.hosted`, see "Privacy
  treatment of a step attempt" below). executor.rs has a source-scan test pinning the
  three sites.
- CONTEXT FALLBACK. If a resolved local step overflows its window after the render, it
  moves to the next ranked local candidate that fits. It does NOT re-render and does NOT
  re-run Gate 1 (every fallback is local, as is the model it replaces, so the provider
  Gate 1 recorded, none, is unchanged). The disclosure log records a provider and fields,
  not a model tag, so it is the same with or without a fallback; the switch is logged only
  as an info line with the two local tags. A step that sets its own num_ctx never falls back.
- KNOWN BEHAVIOURS LATER BUILDS MUST RESPECT:
  1. (items.id=921, landed) Gate 1, the floor consent gate, the render, the voice scan,
     Gate 2, the sensitivity score, the recorded tier and Gate 3 key off the SELECTED
     destination for a resolved step, not the ceiling. See "Privacy treatment of a step
     attempt" below. The ceiling still decides what a step MAY reach (AUTHORIZE, Step 3,
     the resolver's candidate set); it no longer decides how a chosen local model is gated.
  2. No shipped Focus declares a launch question yet (items.id=903 ships Writing
     Assistant's, with its rebuild), and no shipped Focus uses a model block, so nothing runs
     model blocks through the app today. A Focus that does must declare a document_type question,
     and its run resolves the value once at LOAD ("Launch questions and run-start settings").
  3. PlanIncluded is never produced (nothing records which plan a user holds): a hosted
     model is Free or Metered. A hosted model is never chosen until it has a measured cell
     (items.id=788, 726), and EXTERNAL_MODEL_BLOCKS_ALLOWED is still false (items.id=913).
     The hosted call through execute_once is not tested end to end (the real client is
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

## Privacy treatment of a step attempt (items.id=921, decisions.id=942 addendum 5 (e), decisions.id=943 as amended)
A model-block step is gated by where it GOES, not by what it may reach. conductor/step_treatment.rs
is the one pure function (`derive`, no I/O); execute_once calls it once per attempt, right after
selection and before Gate 1, through `derive_treatment(ctx, resolver_req, &selected_model)`.
- INPUT: the ceiling values lifecycle computed into StepContext (effective_access, execution_tier,
  abstraction_tier with the floor clamp, raw_abstraction) and the basis: Ordinary (no requirement,
  or a bound API Chat run) or Resolved (the requirement's highest tier and whether the pick is
  local or hosted).
- RULE: Ordinary passes the ceiling values through UNCHANGED (hosted = access != LocalOnly). A
  local pick under any ceiling is the LOCAL treatment: LocalOnly, tier 1, raw and abstraction 1,
  nothing clamped. A hosted pick is EXTERNAL at the reach, min(ceiling, requirement highest
  tier), with each tier the max of the derived and the ceiling value (never looser than an
  ordinary external step). Disagreements are refused, never skipped: a hosted pick under a
  local-only ceiling (the wording items.id=896 shipped), a hosted pick from a range that reaches
  only the device, a selection that is half local and half hosted, a broken floor invariant. The
  refusal goes through the failure handler as a TierBoundaryViolation before any gate.
- WHAT READS THE TREATMENT: Gate 1 (its arguments, and so its fatal-versus-swallowed disclosure
  log split), the floor consent block (the prompt, the floor_consent_auto row, the prompt
  metadata), scan_voice_profile (a local pick strips a contaminated attribute and continues, a
  hosted call halts), the render branch, hosted_step, reply_gate2, compute_step_sensitivity,
  completed_step (routing_tier_used) and Gate 3. `treatment.hosted` is the ONE predicate for the
  network branch, the render and, for items.id=884, the outbound review: 884 triggers on
  `treatment.hosted` and takes its ceiling argument from `treatment.access`, not
  ctx.effective_access. The disagreement 884's amendment names is already an error before its hook.
- WHAT STILL READS THE CEILING (pinned by the_ceiling_values_are_read_only_where_reviewed): the
  bound-run stop, Step 4.5 and select_model (before selection), the resolver's candidate set, the
  floor-invariant check on the ceiling values, derive_treatment's own input, step_streams.
  A new read of ctx.effective_access, execution_tier, abstraction_tier or raw_abstraction fails
  that test and needs a decision.
- DISCLOSURE LOG (open question a): a local pick writes its gate1_pass row like any local step
  (tier 1, no provider, fields shared, non-fatal on failure), so every executed step keeps a row
  and the row says nothing left the device. The log still records no model tag.
- FLOOR CONSENT (open question b): the prompt exists because the clamp changed what leaves. A local
  pick has no clamp, so it never prompts and the saved "modified" preference is not consulted.
- TIER OF A HOSTED PICK (open question c): the lower of the ceiling and the requirement's highest
  tier. In production this equals the step's own override (model-block expansion sets
  routing_tier and external_access_override to the clamped highest tier), so the choice only fixes
  the pure function's contract.
- NOT STORED: the treatment is a local of one execute_once attempt (a scan pins that nothing in
  lifecycle.rs, types.rs, tokens.rs or StepContext holds it), so each retry and each resumed run
  selects and derives again; the disclosure buffer is overwritten each attempt. Characterized and
  pinned: a resumed hosted pick parks on floor consent exactly like an ordinary external step, and
  proceeds exactly like one when the saved preference is present.
- GATE 3: a local pick records tier 1, so Gate 3 reviews its output when the next step may go out
  (next_execution_tier > 1). next_execution_tier is the static ceiling look-ahead and cannot know
  the next step's own pick, so Gate 3 may also fire when that step resolves local. Refined by
  items.id=901 (see "Run plan and strategy approval"): under a verified approval a following
  model-block step with no PAID pin counts as tier 1, because the executor then stops any hosted
  pick of it. With no approval, a rehydrated run or a bound API Chat run it is still the
  ceiling look-ahead (over-prompt, never under-prompt).
- STREAMING: a resolved step never streams (step_streams has `!is_resolved(ctx)`, because
  retry_loop asks it without a selection). R1 streams only bound API Chat runs, which are never
  resolved. Streaming a resolved step needs a decision.
- THREE BEHAVIOURS LATER BUILDS MUST RESPECT:
  1. A one-time floor-consent "proceed" is written to consent_decisions but nothing reads it on
     resume: only the saved preference in personal.db is read (lifecycle.rs, "modified" when
     stored_tier <= abstraction_tier). A resumed external step, resolved or ordinary, therefore
     parks on floor consent again until the user saves the preference. Existing behaviour,
     left alone; the hosted pick matches it.
  2. Two run-level reads did key off the Focus ceiling, not the pick (items.id=922 re-checked
     both: neither leaks). scan_output at output creation now logs the highest tier a step used.
     The persona_context memory slice is still assembled at the Focus's max tier (a LOWER tier
     gets MORE scopes, so the ceiling slice is strict, and its only reader is the local render),
     but only when a step names {persona_context}. See "Persona context, the output scan and
     external-step scoring".
  3. A floor-consent "use local" answer cannot re-route a resolved step: the resolver never reads
     that preference. A hosted pick that parks on floor consent stays a hosted pick on resume
     unless readiness changes.

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
  tainting a step that names one (so it stays out of extraction), and, since items.id=884
  flipped STORE_NAMES_EXTERNAL_ALLOWED to true (declared_inputs.rs, pin test
  the_store_name_flag_ships_true_and_the_device_list_does_not_depend_on_it), allowed in an
  external step that DECLARES it (the declaration rule decides, and the outbound review sees
  the text). A stored text that fails its validator fails the pass closed.
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
  items.id=883: its {user_input} is the user's own turns only (FocusRun.external_input, never the
  window that holds model turns), because a render row is an external step to the lint.
  - It is a block row to the registry and an EXTERNAL step to the declared-inputs lint
    (ActionDefinition::is_external_for_lint): declared inputs only, no persona_context,
    voice_profile, previous_output or focus_context (refused at parse, declared or not),
    only {user_input} among the system tokens, no field_requirements. STORE_NAMES_EXTERNAL_ALLOWED
    is true (items.id=884 flipped it), so a starter may declare a store-backed name, content_brief
    included, and {user_input} even in a Focus that also has a handoff_ingest row (taint below).
    It may NOT name or declare the two device names (subject_context, device_library_brief): the
    closed list DEVICE_NAMES_NEVER_EXTERNAL refuses them whatever the flag says (see "Focus-owned
    entities"). The real starter naming content_brief is therefore no longer blocked by the flag;
    it is built with the Writing Assistant (still not built).
  - Its record (handoff_render::inherit_from_inputs), over each DECLARED input: (1) takes the
    highest sensitivity_severity and ORs personal_flagged and audit_unavailable of the step that
    produced it (the last producer of the variable); (2) a STORE-BACKED declared input counts as
    PERSONAL: the record's sensitivity_severity is at least 2 (Sensitivity::Personal) and
    personal_flagged is true, whether or not a producer exists and whatever it recorded (a floor,
    it never lowers rule 1; audit_unavailable still comes only from producers). Store-backed =
    a registered bound name, a variable a step the taint closure marks as tainted produced, or
    user_input in a Focus that has a handoff_ingest row (handoff_render::store_backed_inputs);
    (3) any other declared name (the {user_input} token with no return row, a field name)
    contributes nothing. Items.id=884's rider; only declared inputs count. routing_tier_used
    stays 1: the render itself calls nothing. (An EXTERNAL MODEL step's record used to score only
    Gate 1 projected fields, so a store-backed input it names was not marked personal: items.id=922
    closed this gap, see "Persona context, the output scan and external-step scoring".)
  - handoff_pending (Session bound name, `h_` plus 12 lowercase hex of SHA-256 over run and
    row) is written by the render through the typed session store: latch-gated (a one-time
    chat still shows the starter and records nothing), NOT collection-consent gated (no user
    content). A run with no chat renders the starter and records nothing.
  - EXTERNAL_MODEL_BLOCKS_ALLOWED does not govern this: it is one boolean for a MODEL block's
    external destinations (hosted_api, manual_handoff) and still waits for B10. A ManualHandoff
    range of a model block is still refused. items.id=901 does not change that: the run plan
    LISTS manual hand-off as a strategy only when every reachable path is a cb-15 starter with no
    model call, and it never selects one.
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
    out of extraction (and so is a step naming its output). Since items.id=884 flipped
    STORE_NAMES_EXTERNAL_ALLOWED an external step of that Focus MAY name and declare it, behind
    the outbound review; a render row that declares it counts as personal (the rule above). A
    pasted reply is not device content: the device closure leaves user_input out.
  - handoff_pending and handoff_received are reserved: never a session_put target, never a
    writer's argument, never an output_var. Both are seeded at pass start and are ordinary
    store-backed names otherwise (restricted to the step whose template names them, tainting
    it, and allowed in an external step only when declared, since the flag is true). Their
    normal use is a guard.
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
- NOT BUILT: the real Writing Assistant starter (the flag flip it waited for has landed), a
  Derived handoff_draft name, multiple hand-offs inside one pass (the return pass's row order is
  the Focus's), file or document replies beyond a plain paste, any model step after the return
  row (B7c delta validation reads the reply by handle).

## Launch questions and run-start settings (items.id=897, decisions.id=942 point 8 and addendum 3 (g), decisions.id=948)
A Focus may ask up to five closed-choice questions before a run, or none. The answers feed
guards, the model resolver's document profile and the settings of every model block. No real
question or mode ships for any Focus yet (items.id=903 ships Writing Assistant's); the
mechanism is exercised by fixture Focuses in tests.
- DATA (schema/shared_047.sql, shipped data only, identical for every install, no foreign keys,
  classified GlobalCatalog in shared_db_residual.rs; items.id=633 must absorb it): focus_launch_
  questions (slot, kind mode or choice, asked, default_option, prompt), focus_launch_options
  (label, hint), focus_mode_profiles (envelope JSON) and focus_launch_display (ui_one_liner, at
  most 80 characters). Prompt, label, hint and ui_one_liner are placeholder wording owned by
  Chat-BRAND (items.id=506). ui_one_liner rows exist for writing-assistant, research-and-buy and
  role-assessment only. `asked` = 0 is a preset: the default is the value and nothing is submitted.
  The mode list of a Focus is the options of its one question of kind `mode` (slot launch_mode).
- DISPLAY DATA IS NOT COPIED. A Focus's display name and description stay in their one existing
  home and conductor/focus_display.rs read() is the single reader (load_focus_definition: the
  focuses row, else the .focus file; plus the one-liner). Writing Assistant's name and description
  are in both shared_038.sql and its .focus file and are pinned equal by a drift test; neither copy is
  removed. FocusInfo is unchanged.
- THE ONLY READER (conductor/launch_config.rs): validate() is the one rule set (at most 5
  questions, one question per slot, only the mode question on launch_mode, every question has at
  least one option and a default that is one of them, an asked question has two options, a
  document_type question offers shipped document types only, exactly one profile per mode option
  and none elsewhere, no empty destination set, no brace or {token} in wording). The reader of the
  rows is private (read_unvalidated); read_validated() runs validate() and is the only way out,
  pinned by a source scan (nothing else names the private reader or reads the three tables, LOAD
  calls read_validated and lint). A row this build cannot read is an error, never skipped.
- SIX BOUND NAMES (bound_spec.rs, Source::Launch): document_type, launch_mode, launch_answer_1 to 3
  (Presence::Always, Literals::Authored, a shape-only validator of 1 to 40 characters) and run_change
  (Presence::Optional, parses only as a SettingsPatch, at most 2048 bytes, so an envelope key, a
  tier, a destination or a gate does not parse). Rules: answers are stored as session variables of
  the chat but a run reads them ONCE at LOAD and every pass seeds that resolution; a Focus row
  never writes one (refused as a session_put target, a writer argument and an output_var); each
  is a registered store-backed name for an external step (it must declare one it names, a step that
  names one is tainted and so is its variable and every step downstream, so it stays out of
  extraction, and a cb-15 starter that declares one counts as personal); none is in
  DEVICE_NAMES_NEVER_EXTERNAL. DOCUMENT_TYPE_NAME (model_resolver_input.rs) equals DOCUMENT_TYPE.
- LOAD LINT (launch_config::lint, both load paths): a step that names a slot needs a question on it, a
  guard literal on a slot is one of that question's options, and a step with a `requirement` needs a
  document_type question (asked or fixed). run_change needs no question. On the DB path
  run_settings::trial_expand also expands every model row against every document-type option and
  every mode profile, so a combination that cannot be applied refuses the Focus.
- GUARD ANALYSIS. A slot is Authored, so a guard on it supplies its literals plus one implicit
  OTHER, exactly as for a model-produced router: a Focus guarded on a slot needs a fallback row (an
  `ne` or `present` guard), even though the run's value is always one of the options, and every
  guarded slot multiplies the enumerated paths, which count toward the 256-path cap
  (guard::MAX_PATHS).
- RUN-START HOOK (FocusRun::load() calls apply_launch_settings(), before authorize() and
  resolve_tier_config(), so the tier check sees the expanded steps; rehydrate_focus_run reaches it
  through load()). It resolves, in conductor/run_settings.rs: every question's value (the stored
  answer, else the default; always the default for a question that is not asked), the mode and its
  envelope override, the selected document type and the settings. It then re-expands every
  ModelOrigin (kept on FocusDefinition.model_origins) with the selected type (ExpandInput::
  document_type replaces the type the row names), the settings and the mode's envelope override,
  keeping each row's guards and schedule trigger. The mode profile is applied through tighten()
  inside the block's own finish(), after the Focus-ceiling clamp, and applies to EVERY model row of
  its Focus; a profile that removes a declared input or leaves no destination is refused (at LOAD by
  the trial expansion). The resolution lives on the FocusRun in memory only, so a retry and a
  resumed run resolve again.
- THREE SETTING LAYERS (resolve_settings, field by field, either direction): (1) the shipped default
  of the selected type, (2) the user's saved adjustments, (3) run_change. Layer 2 is one function,
  run_settings::saved_settings_patch(), which returns nothing until items.id=905. items.id=905
  provides: a per-persona ENCRYPTED store (never shared.db), validation through SettingsPatch and
  Scales::check_settings, custom document types fed into resolve_type and run_profile (executor.rs
  passes none today), the persona's custom ids in the submit command's allowed document_type set,
  and this function.
- SEEDING. seed_pass takes the run's LaunchResolution and seeds the launch names from it, opening no
  database for them (braces in run_change become parentheses). A caller that supplies none for a name a
  step needs fails the pass closed (SeedError::LaunchNotResolved), and so does the store reader if asked
  for a launch name. required_names adds document_type for any step with a `requirement`.
- STALE OR UNREADABLE. A stored answer that is not one of the question's options, a stored run_change
  that does not parse, an unreadable store or a config that does not validate fails the run
  (LifecycleError::LaunchAnswers), naming the question and never the value; no default replaces it.
- RESUME. rehydrate_focus_run sets `rehydrated` and has no context_key, so it cannot read the
  chat's answers, and the defaults could be looser than what the user chose. A resumed run of a Focus
  with an ASKED launch question therefore fails with LifecycleError::LaunchAnswersUnavailableOnResume
  until items.id=888 carries context_key on rehydrate (items.id=913 depends on it: a review park
  resumes through rehydrate). A Focus whose questions are all fixed, and a run with no chat
  (submit_focus_run), use the defaults. A run_change is lost on a resumed run.
  items.id=883 adds the same shape for the external text: focus_runs.external_input is stored beside
  user_input and a NULL resumes empty (see "External steps and chat history").
- ANSWER STORE AND THE ONE-TIME LATCH (persistence/launch_answer_store.rs; no new table: rows of
  session_variables under the six names). The only writer is put_answers(), which accepts only the
  ValidatedAnswers that launch_config::validate_submission() builds (every answer one of its
  question's options, a question that is not asked refused, a run_change parsed as a SettingsPatch
  whose named steps exist) and writes all of a submit in one BEGIN IMMEDIATE transaction. The
  latch is obeyed with no exemption: a latched chat refuses the whole submit (PutError::OneTimeChat,
  code one_time_chat), a latch that cannot be read fails it closed, and the run uses the Focus
  defaults. Answers stored BEFORE the latch was set stay (setting it deletes nothing), reads are not
  latch-gated, and so they keep seeding later passes of that chat. No clear_chat or retention sweep
  exists yet, so they stay until items.id=888 (session_variables rows are covered by it).
- RETRYING OPENER. The run-start read (read_answers_for_run) and the launch commands open outputs.db
  with output_store::open_outputs_db_retrying (commit 8d8d66f measured lock failures on the plain
  opener under contention, and a failed read here fails the run start closed). The scan test
  only_the_review_paths_open_with_the_retry lists persistence/launch_answer_store.rs.
- NOTHING APPROVES A HOSTED OR METERED STEP (decisions.id=942 Q3). A mode profile is an
  EnvelopeOverride (four keys, pinned) and only narrows (property-tested); a source scan pins that
  spend, review, treatment, provider-selection and executor code never name a launch name or the
  run-start module (the resolver names document_type only). EXTERNAL_MODEL_BLOCKS_ALLOWED is
  unchanged.
- IPC (commands/launch.rs, Group 38 of HANDOFF_IPC_SURFACE.md; ipc.rs comments number it as the
  next group after the outbound review): get_focus_launch_config(focus_id) (shipped data only, never a
  mode profile), get_launch_state(persona_id, context_key, focus_id) (read only, works in a latched
  chat: per question the stored answer, the effective value, from_default and stale, plus run_change,
  one_time_chat and defaults_apply) and submit_launch_answers(persona_id, context_key, focus_id,
  answers [{question_id, value}], run_change string or null) (returns the new state). Error codes,
  each with a placeholder key launch.error.<code> in en.json that a test requires: persona_not_found,
  invalid_focus_id, invalid_context_key, no_launch_config, unknown_question, question_not_asked,
  invalid_answer, invalid_run_change, one_time_chat, launch_unavailable. The keys are in the orphan
  allowlist of styleAndI18nConsistency.test.ts until items.id=902 shows them. No screen is built.
- NOT BUILT: saved adjustments and custom document types (items.id=905), carrying context_key on
  rehydrate (items.id=888), a per-row mode selector, any real Focus question (items.id=903), the
  generic launch screen (items.id=902).

## Run plan and strategy approval (items.id=901, decisions.id=942 Q2 and Q3)
Before a run that has model-block steps starts, the user can see what it could do and approve
what goes beyond the resolver's local picks. Built: the plan, the approval stores and the two
homes, enforcement at selection and at the spend gate, the park reason, five IPC commands. NOT
built: any screen (items.id=902 or 904).
- SCOPE. Only steps that carry a `requirement` (expanded from a model block, kind model_block).
  A Focus with none has NO plan: run_plan::has_planned_step is false and nothing is read or
  written for it. Ordinary external steps and API Chat keep today's spend gate and outbound review
  and are NOT plan-gated (see the pending pointers). A bound API Chat run skips the gate.
- THE PLAN (conductor/run_plan.rs, computed, never stored). Reachable paths are the LOAD guard
  enumeration (MAX_PATHS and the analysis untouched) filtered by the run's launch values; a guard
  on any other name keeps both branches. Per step: the resolver's answer, and the OFFERS (one
  resolver run per candidate: an unmeasured local model or a hosted model is offered, never picked
  by the resolver; only an explicit pin selects it). Strategies local, paid_api and manual_handoff
  each carry a reason id (manual hand-off is listed only when every reachable path is a cb-15
  starter with no model call). Cost is the spend gate's own worst case (spend::estimate::
  worst_case), the dearest reachable path; PlanIncluded is never produced. More than MAX_PATHS is
  plan_too_many_paths and no plan is offered. The module names no model or provider id and holds
  ids, closed words and numbers only (scans pin both).
- THE DIGEST (tag rp1, SHA-256) is STRUCTURE ONLY: the Focus, the document profile, per reachable
  planned step its destinations, highest tier, quality range and hosted_possible, whether manual
  hand-off is listed, and any pins. Not prices, readiness, installed models, the resolver's answer
  or any text. get_run_plan shows the PIN-FREE digest (`RunPlan.digest`); an approval stores
  `digest_with(&pins)`. approve() compares the digest the user was shown with the plan the server
  just recomputed (mismatch: plan_changed, nothing approved); verify() at run time recomputes
  `digest_with(stored pins)` from the run's own plan. A digest the CLIENT sends is never used by a
  run. A narrower mode changes the digest and re-asks (over-asking, never under-asking).
- THE APPROVAL (conductor/plan_approval.rs): the digest, a per-run ceiling in micro-USD (at least
  the plan's estimate under the pins, at most PLAN_CEILING_MAX_MICROS = 25,000,000) and the pins
  (only an unmeasured local model or a paid model, one per step, each one of the plan's own
  selectable offers). A launch answer, mode or run_change never counts as an approval.
- TABLES (schema/outputs_017.sql, the persona's encrypted outputs.db, no foreign keys or
  triggers, no text columns; items.id=633 must absorb it with the other outputs files, and
  tests/golden/schema_shape.txt already carries it): plan_approvals (PK context_key + focus_id:
  digest, ceiling_micros, choices_json of the pins, version) and run_parks (run_id: action_id, a
  registered code, params_json at most 4096 bytes). Stores: persistence/plan_approval_store.rs
  (put_approval is the only writer, latch checked first inside one BEGIN IMMEDIATE; delete is NOT
  latch-gated) and persistence/run_park_store.rs.
- TWO HOMES OF AN APPROVAL (conductor::plan_approval::save_chat_approval is the ONLY caller of
  put_approval). An unlatched chat: persisted. A latched chat, a latch that cannot be read, or a
  database that cannot be opened: NOTHING is written and the approval is held in memory
  (PlanMemory). A run with no chat: only the run-scoped offer in memory, by an opaque 128-bit
  token, single use, bound to user, Persona and Focus, consumed with the run (a token is spent by
  the run it was presented to, even if verification then fails). Reads prefer memory (it can only
  be newer). Revocation removes BOTH homes and is not latch-gated. An approval persisted before a
  latch still reads.
- MEMORY HOME (conductor/plan_memory.rs, managed state, never serialized, logged or shown in
  Debug): approvals 64 entries and 2 hours (absolute from approval, not sliding), chatless offers
  16 and 10 minutes, duplicate-turn markers 64 and 120 seconds (a hash only), run parks 64 and 2
  hours; the oldest are evicted on overflow and expired entries are dropped on every access.
  CLEARED: at logout or lock (main.rs, KeyRegistry on_key_leave -> clear_user, everything of the
  user), when a chat is archived (archive_chat -> clear_chat: that chat's approvals and marker; a
  park or an offer has no chat key and goes by TTL, logout or a later row write) and by TTL.
- plan.exceeded (spend::codes::Code::PlanExceeded; the IPC string for a start that needs an
  approval is the literal `plan_required`, a different thing). Parks the step (F10, AwaitUser)
  with nothing sent. In the executor, at selection and before derive_treatment and every gate:
  `no_approval` (a hosted pick and the run holds no approval: a rehydrated or swept run, or a run
  that needed none), `model_not_planned` (an approval that does not name this model for this step:
  a zero-pin approval permits resolver LOCAL picks only) and `pin_unavailable` (the pinned model is
  not a ready candidate of its kind any more, or measured below the bar; no silent fallback). At
  the spend gate, check 5b (SpendCall.plan_cap_micros, Some only for a resolved step and zero when
  it has no approval, after every existing check and before the reserve, inside the same
  transaction): the run's ledger total (held plus settled) plus this call's worst case over the
  ceiling is `over_ceiling`, a cap with no run id is `no_run_id`. It can only REFUSE.
- PLAN GATE (conductor/plan_gate.rs). load_and_authorize_run runs load(), authorize_checks(),
  then plan_gate(), then open_record(), so a run that needs an approval it lacks writes no
  focus_runs row (authorize() is exactly authorize_checks() then open_record(), pinned). It
  returns LoadOutcome::PlanRequired and every caller maps it to Err("plan_required"). A plan that
  needs no approval proceeds with none (only local resolver picks can then run).
- PARK REASON (conductor/run_park.rs, items.id=901 stage 7). A run that parks leaves its reason in
  run_parks, written by lifecycle::write_run_row in the same function and on the same connection
  as the focus_runs row: every status write stores the park it is given or removes any earlier
  one, so a park exists exactly when the last status written for the run carried one. Which
  failures: a code in Code::ALL of the resolver, spend_block, plan or step_review family (by Code
  variant). What it carries: sanitize_params keeps keys of [a-z0-9_] and values that are numbers,
  bools, null or identifier-shaped words (no spaces), at most three deep, and DROPS anything else,
  so prose cannot ride in a park; a TooWeak or NoCandidate park carries the resolver's ranked
  options, and when they would exceed 4096 bytes whole options are dropped from the worst-ranked
  end (options_total and options_omitted say how many) so the JSON is always complete. If the row
  or the park cannot be stored it is held in memory instead. A failed park write or clear never
  changes the failure or fails a cancel or status write. Cleared by any later status write,
  cancel_focus_run and set_focus_run_status (best effort). get_run_park's `params_json` is clean
  BY CONSTRUCTION at write time (sanitize_params); the DTO shape test in commands/
  run_plan_commands_tests.rs checks a fixture park and does not prove real park content.
- WHO READS AND WRITES WHAT (a new reader fails the allow-list scan
  the_plan_the_approval_and_the_memory_home_are_read_only_where_reviewed in
  conductor/plan_gate_tests.rs and needs review):
  - the plan gate (plan_gate.rs) builds the plan (the one run_plan::build call, shared with
    plan_offer), reads a chat's approval or takes an offer, and sets `FocusRun.plan`;
  - the executor reads ctx.plan at exactly three sites: plan_pin, check_plan_envelope and
    plan_cap_micros (executor tests pin it);
  - lifecycle reads the approval for the Gate 3 look-ahead (has_paid_pin, one predicate, with a
    coupling test against the executor's own rule);
  - send_message (commands/messages.rs) runs a plan preflight BEFORE the user's turn is saved,
    through execution::plan_probe_run, the one constructor of the probe run that get_run_plan uses
    too, so the digest a screen is shown is the digest the run recomputes; commands/turn_marker.rs
    sets and takes the duplicate-turn marker; archive_chat and main.rs clear;
  - IPC Group 39 of HANDOFF_IPC_SURFACE.md (Group 36 in ipc.rs's own list), commands/run_plan.rs:
    get_run_plan(persona_id, context_key?, focus_id) (read only; with no chat it registers the
    offer and returns the plan_token), approve_run_plan(request {persona_id, context_key?,
    plan_token?, focus_id, digest, ceiling_micros, choices}) (exactly one of context_key and
    plan_token), get_run_plan_approval(persona_id, context_key, focus_id) (read only),
    revoke_run_plan_approval(persona_id, context_key?, plan_token?, focus_id) and
    get_run_park(persona_id, run_id) (memory first, then run_parks). Micro-USD travel as decimal
    strings. Each takes writer_gate::command first and the user from the session (no user_id
    argument); the Persona id is a canonical UUID owned by the user (persona_not_found for malformed
    and foreign alike). Codes (ERROR_CODES, each with a placeholder plan.error.<code> key in en.json
    held in the orphan allowlist of styleAndI18nConsistency.test.ts until items.id=902 or 904
    shows them): persona_not_found, invalid_focus_id, invalid_context_key, invalid_plan_token,
    invalid_run_id, no_plan, plan_changed, invalid_choice, invalid_ceiling, plan_too_many_paths,
    plan_unavailable. The wording is placeholder, Chat-BRAND's. A step's label is not in the plan:
    the screen maps an action id to the Focus's own data.
- KNOWN BEHAVIOURS LATER BUILDS MUST RESPECT:
  1. The preflight of a chat send costs ONE indexed shared.db read (focus_has_model_block) for a
     Focus with no model block, and nothing else: no Focus loaded, no persona database opened.
  2. An in-run `plan_required` can still occur AFTER the user's turn was saved (the plan changed
     between the preflight and the load). The memory marker (persona, chat, saved message id, a
     hash of the content, 120 s) narrows the duplicate-turn gap: the next send skips the save
     only if the hash matches, the marked message is still the newest and a user turn. The marker
     is never persisted and is lost at an app restart, when the turn saves once more.
  3. A rehydrated or swept run has no plan memory and no context_key, so it reads no approval: a
     hosted pick stops with plan.exceeded `no_approval` until items.id=888 carries context_key on
     rehydrate. The user's recovery today is to approve and send again, not to resume.
  4. The memory TTLs use a monotonic clock that may not advance while the machine is suspended, so
     an entry can outlive its nominal time across a suspend. Bounded by logout and the entry caps.
  5. The ceiling bounds the WHOLE run ledger (every metered step of the run, ordinary steps'
     spend included), not only planned steps.
  6. A restart re-asks every approval held in memory (a latched chat's, a chat whose latch or
     database could not be read, a chatless run's); a persisted approval of an unlatched chat
     survives it.
  7. Persisted approvals have no retention sweep until items.id=888.
  8. PlanIncluded is never produced: a hosted model is Free or Metered. (The open design question
     of recording which plan a user holds is not built.)
  9. EXTERNAL_MODEL_BLOCKS_ALLOWED is false, so no loadable Focus has a hosted range and the hosted
     enforcement runs ONLY in fixtures: the executor pin tests (hosted_pick_ctx with an approved
     pin), the spend gate's call_capped tests and the plan gate tests against the loopback mock
     listener at zero connections. Those are the live test items.id=913 must keep green when it
     turns hosted blocks on. The real hosted client is HTTPS only.
  10. An approval and an offer never widen a gate: the executor, the spend gate and the outbound
      review still run for every approved step.
- PENDING POINTERS:
  - items.id=888: clear_chat and the retention sweep MUST call PlanMemory::clear_chat, delete
    plan_approvals by context_key and by updated_at, and delete run_parks of the runs they remove;
    it must also carry context_key on rehydrate (so a resumed run reads its approval).
  - items.id=902 or 904 (the screens): show the plan, the approval and the park reason from the
    five commands, and the plan.error keys and the plan.stop.exceeded key (orphan-allowlisted
    today). items.id=906: named for this pass; its scope is not recorded in this repo and nothing
    here depends on it.
  - items.id=913 (hosted blocks on): see known behaviour 9; depends on 888.
  - ORDINARY EXTERNAL STEPS ARE NOT PLAN-GATED: Q3's "always" is open for them. A new item,
    opened by Chat-PM (number not named here), adds a second step kind to the plan list (the
    envelope: kind, action id, the provider the user prefers, the ceiling tier), the same
    ceiling accounting and the same executor check keyed by (action id, kind). A bump of the
    digest tag re-asks and never allows.
  - items.id=633: must absorb outputs_017.sql with the other outputs files.

## External steps and chat history (items.id=883, decisions.id=937 point 3 and decisions.id=952)
An external destination never receives a model turn that a chat folded into {user_input}: a model
turn can carry persona-derived text from an earlier local run. The local render is unchanged.
- TWO FORMS, ONE FOLD. commands/messages.rs fold_window() builds both: build_conversation_prompt
  (`User: ...` and `Assistant: ...` lines, the LOCAL form, FocusRun.user_input) and
  build_user_turns_prompt (the same last HISTORY_WINDOW messages, the same `User: ` label and skips,
  user rows only, FocusRun.external_input). The second is a strict subset of the first by construction
  (a test pins it). A user turn is kept verbatim, including one the user pasted from a model: it cannot
  be told apart from user text, and the outbound review (items.id=884) is its control.
- WHAT READS WHICH. render_prompt (local) inserts ctx.user_input; render_prompt_with_disclosure (every
  step with `treatment.hosted`) and the cb-15 starter (lifecycle.rs execute_handoff_render) insert
  ctx.external_input and never name the full window (scan tests in executor.rs). new() defaults
  external_input to user_input, which is right for a run with no history (submit_focus_run's typed
  text). send_message passes Some(build_user_turns_prompt) and send_api_chat_message Some(its API
  history) as the new load_and_authorize_run parameter `external_input` (server-side, never a DTO
  field): it must be set BEFORE the focus_runs row is written, because that row stores it.
- RESUME. focus_runs.external_input (schema/outputs_018.sql, items.id=633 must absorb it) is written beside
  user_input and cleared at EVERY site that clears user_input (the lifecycle writer, output_store::
  set_focus_run_status and cancel_focus_run; the same keep_input rule: a terminal status, the one-time
  latch, focus_level consent). rehydrate_focus_run reads it and NEVER derives it from user_input (a model
  reply can contain a line that starts with `User: `, so parsing the window would pass model text off as a
  user turn; a scan pins it). A NULL (a row from before outputs_018, or one written without a copy)
  resumes with an EMPTY external input: an external step renders {user_input} empty while a local step
  still sees the stored window. Resume and the scheduled sweep both go through rehydrate_focus_run.
- API CHAT, RULE C (amends decisions.id=888 point 8). build_api_history keeps an assistant turn ONLY
  when its provider_id is the bound destination's; a turn from another provider or with no recorded
  producer is left out, flagged or not (the earlier rule left out only a flagged one). With an immutable
  binding this never fires: it governs transfers and fan-out. A flagged one still counts in
  omitted_flagged, the rest in ApiHistory.omitted_other_producer (not an IPC field).
- NOT CHANGED: output variables and {previous_output} that carry local-step output into an external
  prompt (items.id=922's carry, not this item), and the declared-inputs lint (user_input is declared as
  before).

## Parked-run retention and resume staleness (items.id=681, decisions.id=865)
A parked run's snapshot holds step outputs and disclosure buffers with the values approved for each
step (raw at tier 1, abstracted from tier 2 up), encrypted at rest in the persona's outputs.db.
A run keeps snapshots while it runs (the periodic checkpoints, until output() purges them) and while it is
parked in awaiting_user, awaiting_schedule or paused. complete, cancelled and
failed delete them when the status is written (cleanup, fail_run, the cancel writers; best effort, and no
sweep selects a finished run). That was FALSE for a run failing through execute_full's error arms until
items.id=888 stage 1 (see "Run failure, resume claim and post-output runs"). This item bounds how long a
parked run keeps them, and refuses a resume on changed data. Runs whose reply was saved and that wait for
nothing (awaiting_extract_confirm, awaiting_feedback) are not parked: items.id=888's post-output sweep,
in the same tick and login pass, settles them (same section).
- PURGE_AFTER (conductor/run_retention.rs, CheckpointPark). Written when a run parks, by
  FocusRun::write_checkpoint_for (the one writer; write_checkpoint is the periodic one and stores NULL):
  awaiting_user = checkpoint time + AWAITING_USER_RETENTION_DAYS (30; each new checkpoint is a new row, so
  it refreshes), awaiting_schedule = the step's scheduled_for + AWAITING_SCHEDULE_RETENTION_DAYS (30). One
  fixed text format (whole seconds, UTC, Z). A scan pins the four parking sites.
- THE SWEEP. PARKED_STATUSES = awaiting_user, awaiting_schedule, paused. output_store::expire_parked_run
  runs per run in ONE BEGIN IMMEDIATE transaction (decide, then cancel_parked_in_tx): the run's latest
  snapshot is expired when its purge_after has passed; a snapshot with NO purge_after (the periodic one a
  crash left, a run demote_interrupted_runs turned into paused) expires at created_at + 30 days; a parked run
  with NO snapshot at all expires at started_at + 30 days (it cannot resume and still holds user_input); an
  unparseable time is never expired. The cancel is UPDATE focus_runs SET status='cancelled', user_input and
  external_input NULL, notes.cancel_reason WHERE id=? AND status IN (the parked set), and only if exactly one
  row changed are the snapshots, the park reason and a pending step review removed: a run that resumed
  meanwhile is left alone. This is the FOURTH site that clears focus_runs.user_input (beside the lifecycle
  writer, set_focus_run_status and cancel_focus_run). The message is "expired while waiting"; focus_runs has
  no message column, so it is in notes and in the event's step_display_name.
- WHEN IT RUNS. Hourly supervised task parked_run_expiry (main.rs, pinned in writer_gate MAIN_TASKS) and one
  pass per persona inside the login recovery task right after the demote. Only while the owner's key is
  resident (decisions.id=847): the tick does nothing without one. Lease kind Sweep (short, not visible work: it
  neither defers the demote nor counts as a run for a rotation to wait for; no FocusRun lease because it
  cancels, never resumes). The injected clock is `now`; only the supervised tick reads Utc::now. The login pass
  has no AppHandle and emits no event (the UI reads run state after login); the hourly tick emits
  run-status-update.
- RESUME STALENESS (conductor/resume_staleness.rs, pure `check`). rehydrate_focus_run reads the snapshot's
  personal_context_manifest and compares field_hashes ONLY with the fresh PersonalTrack. A resume is STALE when a
  field that a COMPLETED step used has changed or is gone. "Used" = a name in the disclosure buffer of a step in
  the restored task track, whether or not the step still declares it; the step re-run at resume (awaiting_user
  and awaiting_schedule resume AT the checkpointed step) is excluded. A field only in the fresh track (added) is
  not stale. A NULL, blank or hash-less manifest means no check; an unparseable non-empty manifest (or one that is
  not a JSON object) fails CLOSED. PersonalContextManifest::matches() is NOT used (it also compares
  source_versions, which a .focus edit or app update would change) and has no caller outside its own tests.
  Re-running Gate 1 is not the remedy: execute_once already runs Gate 1 on the fresh track before every step
  attempt and rewrites that step's buffer, and Gate 1 needs no AppHandle. What goes stale is the OUTPUT text of
  completed steps, which Gate 1 cannot change.
- HANDLING. rehydrate_focus_run returns LifecycleError::ResumeStale and writes NOTHING (it runs before the run
  is promoted: since items.id=888 the guarded claim writes `running`). The one helper resume_staleness::cancel_stale_resume is called
  by resume_run, reenter_step (commands/execution.rs) and scheduled_sweep: it cancels with "personal information
  changed while waiting" through the same guarded UPDATE (only if the run is still parked) and emits the event.
  A scan test pins the three callers.
- STATE PLAINLY (limits, accepted):
  (a) A confirmed cross-Persona fact released raw to a local step makes a resume cancel that run: both
      resume paths (execution.rs and scheduled_sweep) pass an empty confirmed-id set, so the fact is absent on
      resume and reads as deleted.
  (b) A policy tightened while a run is parked is NOT detected for completed output: the manifest holds no
      policy. The executor still re-reads fact_disclosure_policy before each later hosted step (decisions.id=953).
  (c) Entity (device), group and persona_context facts are NOT covered: only the four catalogued singleton
      facts of items.id=931 are in the manifest.
- NOT BUILT: a rewind-and-rerun of the affected steps (items.id=574's reenter_step could do it), a "continue
  anyway" resume, the Library or clear_chat hooks (items.id=888), a banner for a stale cancel (the user sees
  the cancelled run and its message).

## Run failure, resume claim and post-output runs (items.id=888 pass 1, privacy review 2026-11-20 F5 to F8 and F15)
Pass 1 of items.id=888 (stages 1 to 3). Pass 2 (context_key on rehydrated runs, the seed version pin and
published ids, clear_chat on archive, the 30-day store sweeps) is NOT built.
- FAIL_RUN (lifecycle.rs, F5 and F6). The one way an Err ends a run: drop the tracks, purge the run's
  snapshots BEFORE writing `failed` (write_run_row clears both inputs), emit. Both error arms of
  execute_full (the F_SYSTEM one too) and resume_claimed_run use it; a source scan pins that neither
  function writes `failed` itself. A resumed run that errors is therefore `failed`, not left `running`,
  and loses its parked state (before, the next login demoted it to paused and it could be retried).
- THE GUARDED CLAIM (F7). rehydrate_focus_run records a ResumeBasis (the status it read and the id of the
  latest snapshot, ordered created_at DESC, rowid DESC). output_store::claim_run is ONE UPDATE:
  status := to WHERE id AND status = basis.status AND status IN (allowed) AND latest snapshot id =
  basis.snapshot_id ('' for none); true only if one row changed. It changes the status and nothing else.
  - resume_execution = claim_resume()? (OUTSIDE any error arm) then resume_claimed_run(). A loser gets
    LifecycleError::ResumeRaced and has written nothing: no status, no purge, no event. resume_claimed_run
    refuses a FocusRun that did not win a claim. allowed = run_retention::RESUMABLE_STATUSES
    (awaiting_user, awaiting_schedule, paused).
  - resume_run (commands/execution.rs) refuses `running` and `initializing` (run_live:<status>), claims
    BEFORE it spawns (a lost claim: run_not_resumable) and spawns resume_claimed_run. reenter_step refuses
    the two live statuses and claims to awaiting_user (REENTERABLE_STATUSES adds awaiting_feedback) after
    plan_reentry and before any change; after a re-entry it carries its new checkpoint as its basis. The
    scheduled sweep skips a lost claim before finalize_chat_reply. A scan pins the two callers outside
    lifecycle.rs.
  - write_checkpoint_for returns the new snapshot id.
  - Checked with the claim: the step review's needs_resume auto-resume (at most 2, stepReview.ts; a repeat
    loses and takes resume_failed), Try again (ChatPane), lateReplyRecovery (reads status only).
- THE EXTRACTION FLAG (F8). conductor/extract.rs EXTRACT_CONFIRM_SCREEN_BUILT = false, pinned by a test:
  execute_and_finish skips the WHOLE pass after output() (no personal.db read, no model call, no
  candidate, no park) and completes the run. items.id=38 flips it with its confirm screen. resume_run's
  replay branch and submit_extract_confirm are unchanged. awaiting_extract_confirm keeps no input
  (USER_INPUT_CLEARED_STATUSES).
- THE POST-OUTPUT SWEEP (F8). run_retention::POST_OUTPUT_STATUSES = awaiting_extract_confirm,
  awaiting_feedback (disjoint from RESUMABLE_STATUSES and PARKED_STATUSES, pinned).
  output_store::sweep_post_output_runs, in the hourly tick and the login pass, key resident only:
  every pass NULLs both inputs of every awaiting_extract_confirm row, any age; at started_at +
  POST_OUTPUT_RETENTION_DAYS (30) settle_post_output_in_tx sets `complete`, both inputs NULL and
  notes.cancel_reason "expired while waiting" WHERE id AND status IN (the set), and only if one row changed
  deletes EVERY candidate row of the run (pending, declined, confirmed), any snapshot, the park and a
  pending review. Settle and claim cannot both act: both are single UPDATEs guarded on disjoint status sets.
  The tick emits run-status-update `complete` for a settled run (Swept::Settled).
- F15. purge_device cancels the Focus's parked runs, as purge_all_devices does.
- KNOWN LIMITS, accepted:
  (a) A re-entry that fails after its claim (a database error mid-way) leaves the run at awaiting_user with
      its EARLIER checkpoint, so a resume of a run that was paused re-runs the checkpointed step.
  (b) A crashed `running` run cannot be resumed in-session: it becomes paused at the next login
      (demote_interrupted_runs).
  (c) The raw codes run_not_resumable and run_live:<status> reach the user (Try again notice, review modal
      error), like run_already_finished:. Wording is Chat-BRAND's.
  (d) Extraction is off everywhere (chat, Board, resume, scheduled sweep) until items.id=38.
  (e) Existing rows: the first login after the upgrade clears the inputs of every awaiting_extract_confirm
      run and settles every post-output run older than 30 days, deleting its candidates.
  (f) The purge confirmations do not say that parked runs are cancelled (Chat-BRAND).

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
  subject_context renders EVERY catalogued fact, serial number and address included, as LOCAL text.
  CLOSED LIST (items.id=884, declared_inputs.rs DEVICE_NAMES_NEVER_EXTERNAL = subject_context and
  device_library_brief, pinned equal to the two bound_spec names): LOAD refuses an external step that
  names or declares either name, a variable derived from one, or a step that follows one through
  {previous_output}, with its own error (DeviceNameInExternalStep), checked first on every Focus and
  NOT relaxed by STORE_NAMES_EXTERNAL_ALLOWED (true since items.id=884). Why: the catalogue marks
  serial number, warranty contact and access location not_permitted at tiers 2 and 3 (never, even
  with consent), the brief carries the user's device names and two free-text fields with no secret
  detector, the outbound review detects and asks (it does not enforce the catalogue), and
  decisions.id=513's redaction needs flags decisions.id=945 defers. The closed-list refusal is
  TEMPORARY: items.id=924 (a design, not built) replaces it with a per-field policy (never, ask,
  pass) and a tier-aware render of the catalogue's policies, and removes the refusal for the fields
  it allows. Opening the names is that design, not a flag. They stay tainted like every store
  name. Seeds are read once per pass, so a device's facts appear one pass
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
  (RunsStillRunning). Both purge_device (since items.id=888, F15) and purge_all_devices cancel the
  Focus's parked runs (every one: the code cannot tell which run read which device). NOT reached: chat
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

## Personal fact bridge (items.id=931, decisions.id=953; the catalogue and store are items.id=930)
Before items.id=931 PersonalTrack.fields was empty in every real run, so a declared
field_requirements token rendered empty. Now a singleton entity fact in the closed catalogue
(block_data/personal_fact_fields.json: location_city, job_title, employer, industry, all personal,
default OMIT at tiers 2 and 3) reaches Gate 1 at the user's disclosure policy.
- ONE CONSTRUCTOR (conductor/personal_fact_bridge.rs, PURE, no I/O). lifecycle.rs
  bridge_personal_facts is its only caller and the only production add_field on a conductor
  PersonalTrack (executor.rs's gt.add_field is the separate privacy gate track): both are pinned by scans
  in the bridge's tests (T6). Input: facts that PASSED apply_entity_fact_provenance_check, so an
  unconfirmed cross-Persona fact never gets here; a confirmed one is bridged as not_permitted (v1).
  Entity-scoped and uncatalogued facts are skipped, a duplicate name is dropped whole, an unreadable
  catalogue bridges nothing. A bridged field is marked by source_id BRIDGE_SOURCE_ID, nothing else writes it.
- POLICY. abstraction_tier2/3 on the field carry the effective policy word (personal_fact_fields::
  effective_policy over fact_disclosure_policy, personal.db). The bridge NEVER reads the entity_facts
  abstraction columns (EntityFact does not carry them, the loader does not select them, a scan pins both).
  The default is omit, only an explicit user pass releases the raw value and only at tier 2 (tier 3 pass is
  not selectable for any v1 field), medical and financial (and an unknown sensitivity) are not_permitted
  whatever the user says. ask behaves as omit until items.id=924.
- RE-READ. A run uses the INITIALIZE snapshot and execute_once reads fact_disclosure_policy AGAIN before
  Gate 1 for a hosted step that holds a bridged field, taking the STRICTER word of the two (not_permitted >
  omit, with ask and unknown as omit > summarize > pass; equal words keep the snapshot, so an explicit pass
  survives a new value of the same field). An unreadable table or database is omit for every bridged field.
  A resumed run builds its track again, so a policy tightened while it was parked is honoured (T9).
- GATE 1 report_scope. gate1 takes an optional set of declared names (execute_once passes the step's
  field_requirements keys): fields_shared, fields_abstracted, fields_withheld and floor_clamped_fields
  (the log row and the floor-consent prompt) cover declared fields only. approved_fields stays unfiltered and
  the declared-field filter in execute_once stays the live control (T3 proves it, step built bypassing the lint).
  A floor-consent prompt therefore fires only for a field the step declares.
- FAIL CLOSED MAPPING (executor.rs to_gate_track_with, bridged fields only): an unknown sensitivity maps to
  financial; a field outside the catalogue or outside general/personal is not_permitted at both tiers. An
  ordinary field keeps the old mapping (unknown is general).
- VALUE. A bridged value has `{` and `}` turned into `(` and `)`, because render_template WAS not single
  pass (it replaced token by token over a HashMap, so a value naming {user_input} could be expanded by a
  later substitution). items.id=932 made it single-pass; the neutralization STAYS as defence in depth (see
  "Single-pass render substitution"). A value that fails the field
  validator (length, content) keeps its CLEANED text in PersonalTrack.fields, which Gate 2, the voice scan and
  step sensitivity still read, but is not_permitted at both tiers so Gate 1 never releases it.
- LOAD LINT (declared_inputs::lint_field_requirements, both loaders): in an external step every
  field_requirements name must be a catalogue name, be named as a {token} in the template and equal no
  output_var, bound name or system token.
- ACCEPTED SIDE EFFECTS (decisions.id=953 ruling 11), for any persona holding a catalogued fact even with
  no policy: the first external run may park once on floor consent, voice-profile contamination can strip an
  attribute (local) or halt an external step, Gate 2 can flag a reply echoing a value (flag only), step and
  output sensitivity rise to personal, and checkpoint manifests carry name:value hashes. Local steps are
  otherwise unchanged.
- NOT BUILT: UI and copy, group facts and entity-scoped (device) facts (items.id=924), releasing medical,
  financial or cross-Persona facts, an end-to-end hosted call (the
  loopback client is not HTTPS). Hand checks for the live app are listed in the items.id=931 handoff.
  items.id=633 absorbs personal_012.sql (items.id=930).

## Single-pass render substitution (items.id=932, decisions.id=953 amendment of 2026-11-17)
render_template (conductor/executor.rs) is ONE call to substitute_once: a left-to-right pass over the
TEMPLATE. A substituted value is never scanned again, so a value holding {user_input}, {previous_output}
or any token text appears literally, and the result does not depend on HashMap order. Before this,
render_template did String::replace per token on the RUNNING result (so later tokens re-expanded earlier
values, in HashMap order) and then stripped every leftover {x} from the whole result, values included.
- SCANNING is find_tokens() exactly: at `{` look for `}`; a nested `{` or the end means the `{` is literal.
  A closed span whose text is a KEY of the map is replaced by its value (ANY brace-free key is looked up:
  cb-15 declared names and an ordinary output_var are not required to be snake_case, valid_name and
  valid_var_name allow uppercase, and an ordinary output_var is not shape-checked at LOAD). Else a span shaped
  [a-z_][a-z0-9_]* is an unresolved TEMPLATE token: removed, logged at DEBUG. Else the span is kept.
- EVERY RENDER PATH goes through it: render_prompt (local), render_prompt_with_disclosure (every step with
  treatment.hosted), fixed_tokens_of_step (empty map: strips every template token) and the cb-15 starter
  (handoff_render::render_starter, which also still cleans values: braces become parentheses). No other
  production code substitutes tokens (scan: token_substitution_has_one_site_and_it_is_single_pass).
  examples/support/mod.rs keeps its own replace loop (the engine is pub(crate)); its comment says so.
- BEHAVIOUR CHANGE, intended: braces in a VALUE (a user turn, a model output, a disclosure value) are no
  longer stripped or expanded, they reach the prompt as typed. The outbound review sees the final text as
  before. Gate 1, the disclosure buffer, report_scope and the treatment are untouched.
- PARITY. The retired Python oracle (_render_template, git a27a2b1^) did sequential replace and so DID
  re-expand; Rust deliberately differs on this one point. tests/golden_vectors.rs covers Gates 1 to 4 only
  and has no render vector, so no vector changed.
- NO SHIPPED TEMPLATE RELIES ON RE-EXPANSION: lifecycle.rs no_shipped_template_relies_on_re_expansion
  renders every template of the six .focus files and every Focus seeded in shared.db (Writing Assistant
  included) with brace-free canary values and requires the output equal to the old algorithm's in both key
  orders. A community .focus file that relied on it is unknowable.
- KEPT AS DEFENCE IN DEPTH: seeding::clean (braces to parentheses on seeded store names), the
  personal_fact_bridge brace neutralization, StoreTokens::apply (which decides which seeds a step is given).
- TESTS: render_template_never_re_expands_a_value, the_local_render_..., the_external_render_...,
  a_value_arrives_cleaned_and_never_expands_in_the_starter (the starter test cannot fail from the engine alone
  because clean() runs first), single_pass_against_the_pre_932_algorithm_by_class (every class of difference
  is a row with its explanation: value naming another key, value wrapped in template braces, value with
  unknown or malformed braces; nested {{a}}, unclosed braces, invalid shapes are identical). Mutations M1 to
  M5 (old loop, post-pass strip, per-path post-render replace, fixpoint, unresolved token left) each fail a test.

## Persona context, the output scan and external-step scoring (items.id=922, decisions.id=947, 952, 953)
Two reads that keyed off the run ceiling, and three gaps found with them, built in five stages.
- DIRECTION CHECK (live source, neither leaks). (1) persona_context: the tier only picks visibility
  scopes (domain_context_store::get_eligible_scopes, plan_state_store::eligible_scopes_for_tier) and a
  HIGHER tier gets FEWER scopes, so the ceiling slice is a subset of a tier-1 slice; its only reader is
  the local render (the external one inserts an empty string, items.id=882). The tier was left as the
  ceiling on purpose (a tier-1 slice would widen what a local step sees, ruled out). (2) scan_output:
  execution_tier reaches only the disclosure row; the block decision is the Privacy Filter result, or
  without it the severity (output_scan.rs legacy path), never the tier. A test pins that.
- TOKEN SHAPE AT LOAD (declared_inputs::lint_with, OutputVarNotTokenShaped, both loaders). Every
  recorded variable (ordinary output_var, block handler, render row, store writer's published variable) must
  be [a-z_][a-z0-9_]*. Before this the renderer resolved `{Draft}` (substitute_once looks up ANY
  brace-free key) while find_tokens, which the declared-inputs rule and both taint closures read, did not
  see it: an external step could take a local step's output undeclared. Shipped Focuses had no hits.
  NOT SHAPE-CHECKED: the field_requirements names of a LOCAL step (an external step's are checked against
  the catalogue by items.id=931); a local name with other characters is not looked up by find_tokens.
- PERSONA CARRY REFUSED AT LOAD (declared_inputs::lint_persona_carry, PersonaContextCarriedToExternalStep).
  An external step may not name or declare a variable derived from a LOCAL step that names
  {persona_context}, nor read {previous_output} after such a row: taint_closure_of over ["persona_context"],
  transitive, with the {previous_output} over-approximation (it can refuse a clean Focus, never admit a
  carrying one). Refused, not tainted: persona_context is a system token, not a bound name, and tainting
  would restrict it and drop steps from extraction. A local step may read such a variable. NO runtime
  backstop for this carry. items.id=903's record names no step that carries it.
- STORE-BACKED FLOOR ON EXTERNAL STEPS (executor.rs recorded_step, apply_store_backed_floor). A step that
  went to a hosted destination (`treatment.hosted`) and DECLARES a store-backed input (a bound name, a
  variable a tainted step produced, or user_input in a Focus that ingests a pasted reply:
  StoreTokens::is_store_backed, pinned equal to handoff_render::store_backed_inputs) records severity at
  least 2 and personal_flagged true. A floor only: it never lowers a severity and never sets
  audit_unavailable; a local step is unchanged; undeclared names do not count. StoreTokens carries
  user_input_backed beside restricted, both from seeding::store_state, set by every pass in execute(), so a
  resumed run scores the same as a fresh one (a real rehydrate_focus_run is tested).
  `personal_flagged` ON A FLOORED STEP MEANS "STORE-BACKED INPUT", NOT "GATE 2 FOUND A VALUE". Its only
  consumer is the status payload field, which nothing renders yet; a screen must not read it as a Gate 2 hit.
  An omitted bridged field (items.id=931 default) is not projected, so a hosted step that sent nothing still
  records 1.
- KNOWN GAP, SEVERITY LOST THROUGH A LOCAL STEP (characterized, not fixed). Gate 3 reads the record of the
  step it JUST completed, never output_sensitivity() or the track's sensitivity_ceiling (a scan pins that
  executor.rs never names the ceiling); output_sensitivity() is read only at output creation (the legacy
  output scan and the output's label). For A (local, severity 3) -> C (local, reads A's variable) -> B
  (external): no Gate 3 at A (next tier 1 is not above 1); at C Gate 3 runs on C's reply with C's own
  severity, 1, because a step's severity comes from the PersonalTrack fields its template names, not from the
  variables it consumed, so the legacy gate approves what it would block at severity 3; the outbound review
  at B screens the prompt with a placeholder severity (REVIEW_SEVERITY 1, not authoritative), so a prompt with
  no Privacy Filter span passes. The path is not ungated, but the severity-3 rule never sees A's label.
  PRODUCER-SEVERITY INHERITANCE (rule 1 of handoff_render::inherit_from_inputs, for external model steps) is
  DEFERRED: an inherited 3 or 4 would block at Gate 3 (legacy) or force High review (Privacy Filter), label
  the output medical or financial and let the legacy output scan block export, so it needs its own decision.
- SCAN TIER. output() passes lifecycle::highest_tier_used (the highest routing_tier_used over the track, 1
  when no step ran, clamped to 1..3) as scan_output's execution_tier; it was the Focus ceiling.
- PERSONA SLICE ONLY WHEN NAMED. assemble_persona_context (both call sites: run start and
  rehydrate_focus_run) returns an empty string unless steps_name_persona_context finds {persona_context} in a
  step's own template; no shipped Focus names it, so no shipped run reads the persona's memory stores for it.
  domain_context_store::test_set_standing_summary is test-only.
- UNCHANGED, stated: a one-time floor-consent "proceed" is still not read on resume, and a "use local"
  answer still cannot re-route a resolved step (items.id=921's notes). Not verified: any hosted call end to
  end (blocks stay off, items.id=913; the floor is tested through the pure function), the rehydrate call
  site of the persona guard under a real resume, user-installed Focuses against the new LOAD refusals, and
  Windows or macOS.

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
- PersonalTrack is never serialized: only a manifest of field names and hashes goes into a
  snapshot, and it is re-fetched fresh on resume. This does NOT mean snapshots hold no
  personal values: step outputs and disclosure buffers hold the values approved for each step
  (raw at tier 1, abstracted from tier 2 up), encrypted at rest in the persona's outputs.db
  (decisions.id=865). Parked runs' snapshots are swept (items.id=681).
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
render_template() (conductor/executor.rs) substitutes {token} in ONE pass
(substitute_once, items.id=932) and strips unresolved {token} patterns of the
TEMPLATE. Templates must not use {token_name} syntax for any string not intended
as a Conductor token. A substituted value is never re-scanned: a value that
holds {x} text appears literally. A step's output_var must be token-shaped,
[a-z_][a-z0-9_]*: LOAD refuses any other (items.id=922), because the renderer
resolves any brace-free span but find_tokens, which every lint reads, sees only
that shape.

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
