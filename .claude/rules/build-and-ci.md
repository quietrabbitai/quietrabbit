---
paths:
  - ".github/**"
  - "rust-toolchain.toml"
  - "src-tauri/Cargo.toml"
  - "src-tauri/build.rs"
  - "src-tauri/tauri.conf.json"
  - "src-tauri/deny.toml"
  - "src-tauri/src/main.rs"
  - "src-tauri/src/lib.rs"
  - "src-tauri/src/test_support.rs"
  - "src-tauri/src/test_harness.rs"
  - "frontend/package.json"
---

## Dev Environment

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

## Rust/Tauri Architecture

### Project structure
src-tauri/ lives at repo root.
  src-tauri/Cargo.toml       — package manifest + dependencies
  src-tauri/build.rs         — required Tauri build script
  src-tauri/tauri.conf.json  — Tauri v2 app config
  src-tauri/src/main.rs      — async entry point (#[tokio::main])
  src-tauri/src/lib.rs       — library root; mod declarations go here
Rust dev runs locally on Garuda via cargo. Python backend retired (commit a27a2b1).
