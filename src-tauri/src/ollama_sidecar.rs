//! Ollama sidecar lifecycle manager.
//!
//! # Dedicated-port trust boundary (decisions.id=840, supersedes part of
//! # decisions.id=380)
//! QR's bundled sidecar always runs on its own dedicated port, 21434 —
//! never negotiated, never falls back to 11434, never shares a process
//! with any user-installed Ollama. `ensure_available()` unconditionally
//! starts the sidecar; it no longer branches on whether a system instance
//! is detected. decisions.id=380's original "detect-first, reuse if
//! found" design is retired: items.id=436's security review found QR must
//! never load a model it did not itself pull from its own curated list
//! (Ollama has real, exploitable GGUF/Modelfile-parsing vulnerabilities —
//! see decisions.id=840 for the CVE list), so reusing a detected
//! instance's models is no longer safe regardless of convenience.
//!
//! `ensure_available()` still probes 127.0.0.1:11434 (decisions.id=380's
//! original detection mechanism, reused verbatim) — but purely to warn
//! the user of possible GPU/RAM contention if their own Ollama is also
//! running, via `SidecarStartup::system_ollama_contention`. That probe
//! never gates whether the sidecar starts and never causes QR to read or
//! trust the detected instance's models.
//!
//! No caller outside this module ever invokes `tokio::process::Command`
//! directly — all process management is encapsulated here.
//!
//! # Binary bundling (build pipeline note — D6-353)
//! `tauri.conf.json` `externalBin` lists `binaries/ollama`; the source file
//! is `src-tauri/binaries/ollama-{target-triple}` (on Garuda:
//! `ollama-x86_64-unknown-linux-gnu`). Tauri copies it next to the app
//! executable with the triple stripped, i.e. `<exe dir>/ollama` (verified
//! in dev: `target/debug/ollama`). It is NOT placed in the resource
//! directory. The packaged-build layout has not been verified, so
//! `candidate_paths()` also tries the legacy `<resource_dir>/ollama-{triple}`
//! name from D6-353.
//!
//! The checked-in binary is currently a placeholder script that exits 1
//! (real binary comes in the packaging pass). In debug builds only,
//! `candidate_paths()` therefore ends with a PATH lookup of `ollama` so
//! `cargo tauri dev` still gets a QR-owned instance: same dedicated port,
//! same QR-private model directory, never the user's 11434 instance.
//! Release builds never take that fallback.
//!
//! # Leak prevention and client trust (items.id=586)
//! `ensure_available()` reclaims any provably-QR, orphaned `serve`/runner
//! left over from a prior run (`ollama_ownership::reclaim_orphans`) before
//! starting fresh -- never adopting a running process, and never touching
//! one it cannot prove is both QR's own and actually orphaned (a live
//! sibling QR instance's own sidecar is left alone; there is no single-
//! instance guard on QR itself). The spawned child is given its own
//! process group (`ollama_spawn_thread::spawn_with_pdeathsig`, which also
//! arms Linux's `PR_SET_PDEATHSIG`) so `stop()` and the startup reclaim
//! path can both tear down a runner the `serve` process spawned, not just
//! `serve` itself -- confirmed empirically this session that a runner
//! shares `serve`'s process group rather than starting its own.
//!
//! [`is_trusted`] is the single process-wide gate `OllamaClient` checks
//! before making any network call (see that module): fail-closed by
//! default, set true only once this module has itself verified the
//! sidecar came up, and set false again the moment the held child exits
//! (`liveness_tick`) -- never left stuck true against a port some
//! unidentified later process happens to be answering on.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::Client;

use crate::ollama_ownership;
use crate::ollama_spawn_thread::{self, SpawnedChild};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The dedicated port QR's own sidecar always binds, via
/// `OLLAMA_HOST=127.0.0.1:21434`. Never negotiated, never 11434
/// (decisions.id=840). Chosen outside both Linux's default ephemeral port
/// range (32768–60999) and Windows' dynamic port range (49152–65535) —
/// decisions.id=840's original pick, 35973, was inside the Linux range and
/// was moved here after that was caught during this item's implementation.
pub const QR_OLLAMA_PORT: u16 = 21434;

/// Whether QR's own sidecar started, or is unavailable.
///
/// Written once in `tauri::Builder::setup()`, read frequently by
/// `get_health()`. Serialized to IPC strings only in `HealthResponse`.
/// No `System` variant: per decisions.id=840, QR always starts its own
/// sidecar regardless of what's detected on 11434 — a detected system
/// Ollama is surfaced separately, as a contention warning, never as a
/// routing choice. See `SidecarStartup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OllamaSource {
    /// QR's own sidecar, on its dedicated port, started successfully.
    Sidecar,
    /// The sidecar failed to start or become ready.
    Unavailable,
    // TODO (post-Release 1): add Detecting variant to distinguish
    // "detection in progress" from "detection complete, nothing found".
    // Requires frontend handling of the transient state.
}

impl OllamaSource {
    /// IPC-safe string for `HealthResponse.ollama_source`.
    pub fn as_str(&self) -> &'static str {
        match self {
            OllamaSource::Sidecar => "sidecar",
            OllamaSource::Unavailable => "unavailable",
        }
    }
}

/// Result of `ensure_available()`: QR's own sidecar outcome, plus whether
/// a separate, untouched user Ollama was also seen on 11434.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarStartup {
    pub source: OllamaSource,
    /// True iff 127.0.0.1:11434 answered during startup. A warning signal
    /// only (possible shared GPU/RAM contention from two Ollama processes)
    /// — never read or trusted for models, never affects `source`.
    pub system_ollama_contention: bool,
}

/// Internal result of the contention probe. Not exposed to callers.
enum DetectionResult {
    SystemOllama,
    NotFound,
}

// ---------------------------------------------------------------------------
// Sidecar manager
// ---------------------------------------------------------------------------

/// Single process-wide gate `OllamaClient` checks before any network call
/// (items.id=586). Fail-closed: `false` until `ensure_available` itself
/// confirms the sidecar it just started is ready, and `false` again the
/// moment `liveness_tick` observes the held child has exited. Never set
/// from anywhere that only *thinks* a request to port
/// `QR_OLLAMA_PORT` would succeed -- only from this module's own confirmed
/// process state.
static SIDECAR_TRUSTED: AtomicBool = AtomicBool::new(false);

/// Whether `OllamaClient` may contact `QR_OLLAMA_PORT` right now.
pub fn is_trusted() -> bool {
    SIDECAR_TRUSTED.load(Ordering::Relaxed)
}

/// Test-only escape hatch for `OllamaClient` tests that need to exercise
/// the gated-request path without a real sidecar having started (this
/// process never calls `ensure_available`, e.g. `ocr_eval.rs`'s standalone
/// `#[ignore]`d eval tests, or a mock-server test of `OllamaClient` itself).
/// Never callable from production code -- `cfg(test)` only. Callers must
/// serialize through `test_support::ENV_MUTEX` the same way other tests
/// that touch process-global state already do, since this flag is shared
/// by the whole test binary.
#[cfg(test)]
pub fn force_trust_for_test(value: bool) {
    SIDECAR_TRUSTED.store(value, Ordering::Relaxed);
}

/// Lifecycle manager for the bundled Ollama sidecar.
///
/// One instance lives in `AppState` for the duration of the process,
/// wrapped in `tokio::sync::Mutex` to sequence startup/shutdown/liveness-
/// check operations against each other. `SpawnedChild` itself is a plain
/// `pid`/`pgid` plus a `std::process::Child` handle -- see
/// `ollama_spawn_thread`'s own module doc comment for why that, and not
/// `tokio::process::Child`, is what this holds.
pub struct OllamaSidecar {
    child: Option<SpawnedChild>,
}

impl Default for OllamaSidecar {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaSidecar {
    pub fn new() -> Self {
        Self { child: None }
    }

    /// Reclaims any provably-QR, orphaned leftover first, then does the
    /// warn-only contention check, then unconditionally starts QR's own
    /// fresh sidecar on its dedicated port.
    ///
    /// Single public entry point for startup.
    ///
    /// # Order of operations (decisions.id=840, items.id=586)
    /// 0. Reclaim: kill any process group that is both provably QR's own
    ///    (`ollama_ownership::is_qr_owned`) and orphaned
    ///    (`ollama_ownership::is_orphaned`) -- a pidfile-targeted pass,
    ///    then a broad sweep that also catches a runner left behind with
    ///    no live `serve` parent at all. Never adopts a survivor; always
    ///    starts fresh afterward regardless of what reclaim found.
    /// 1. Probe `http://127.0.0.1:11434/api/tags` with a 2 s timeout —
    ///    purely to set `system_ollama_contention`; never gates step 2.
    /// 2. If reclaim killed anything, wait (bounded, ~3 s) for
    ///    `QR_OLLAMA_PORT` to actually be rebindable before trying to
    ///    start -- confirmed empirically this session that the OS can take
    ///    a moment after a kill before the port is free again ("bind:
    ///    address already in use" on the very next attempt otherwise), and
    ///    retry the start a few times with a short backoff if the first
    ///    attempt still fails right after a reclaim.
    /// 3. Start QR's own bundled sidecar from `resource_dir`, always, on
    ///    `127.0.0.1:21434` (`QR_OLLAMA_PORT`).
    /// 4. Poll `127.0.0.1:21434` every 500 ms for up to 5 s →
    ///    `OllamaSource::Sidecar`, and set [`is_trusted`] true.
    /// 5. If the sidecar fails to start or become ready →
    ///    `OllamaSource::Unavailable`, and [`is_trusted`] stays false.
    ///
    /// Must be called from `tauri::Builder::setup()` so the result is
    /// written before any IPC handler can fire.
    pub async fn ensure_available(&mut self, resource_dir: &Path) -> SidecarStartup {
        let models_dir = crate::providers::utils::get_data_root().join("ollama_models");

        let qr_exe_path = std::env::current_exe();
        let reclaimed = match &qr_exe_path {
            Ok(qr_exe_path) => ollama_ownership::reclaim_orphans(&models_dir, qr_exe_path).await,
            Err(e) => {
                log::warn!(
                    "ollama_sidecar: could not resolve current_exe() -- skipping orphan \
                     reclaim this startup: {e}"
                );
                false
            }
        };

        let system_ollama_contention = match self.detect().await {
            DetectionResult::SystemOllama => {
                log::warn!(
                    "ollama_sidecar: a system Ollama is running at 127.0.0.1:11434 — QR \
                     starts its own sidecar on 127.0.0.1:{QR_OLLAMA_PORT} regardless \
                     (decisions.id=840); running both may contend for GPU/RAM"
                );
                true
            }
            DetectionResult::NotFound => false,
        };

        let started = if reclaimed {
            self.start_sidecar_after_reclaim(resource_dir, &models_dir)
                .await
        } else {
            self.start_sidecar(resource_dir, &models_dir).await
        };

        let source = if started {
            log::info!("ollama_sidecar: sidecar ready at 127.0.0.1:{QR_OLLAMA_PORT}");
            SIDECAR_TRUSTED.store(true, Ordering::Relaxed);
            OllamaSource::Sidecar
        } else {
            log::warn!("ollama_sidecar: sidecar failed to start or become ready");
            SIDECAR_TRUSTED.store(false, Ordering::Relaxed);
            OllamaSource::Unavailable
        };

        SidecarStartup {
            source,
            system_ollama_contention,
        }
    }

    /// Stop the sidecar process if one was started by this manager, and
    /// anything in its process group (a runner it loaded) along with it --
    /// `kill_process_group` itself refuses to signal anything in that
    /// group it can't prove is QR's own (see its own doc comment), though
    /// for a child this manager spawned itself that should never actually
    /// matter in practice.
    ///
    /// No-op if the source was `Unavailable` (no child held).
    /// Called on `CloseRequested` from the main window event handler, and
    /// synchronously from `RunEvent::Exit` in `main.rs` before `_exit(0)`.
    pub async fn stop(&mut self) {
        if let Some(child) = self.child.take() {
            log::info!(
                "ollama_sidecar: stopping bundled sidecar (pid {})",
                child.pid
            );
            let models_dir = crate::providers::utils::get_data_root().join("ollama_models");
            SIDECAR_TRUSTED.store(false, Ordering::Relaxed);
            ollama_ownership::kill_process_group(child.pgid, &models_dir).await;
            if let Err(e) = child.wait().await {
                log::debug!("ollama_sidecar: wait() after stop failed: {e}");
            }
            ollama_ownership::remove_pidfile();
            log::info!("ollama_sidecar: sidecar stopped");
        }
    }

    /// Wrapper around `start_sidecar` used only when this call's own
    /// reclaim pass just killed something: waits for `QR_OLLAMA_PORT` to
    /// actually be rebindable (bounded, ~3 s), then retries the start a
    /// few times with a short backoff if the first attempt still fails --
    /// confirmed empirically that a fresh start attempted immediately
    /// after a reclaim can otherwise fail with "address already in use"
    /// even though the reclaimed process is already gone. Deliberately not
    /// applied to every start -- only the reclaim-just-happened case has
    /// this specific, confirmed race; a normal failed start (no binary, no
    /// candidate, genuinely occupied port) should still report
    /// `Unavailable` promptly rather than retrying blind.
    async fn start_sidecar_after_reclaim(
        &mut self,
        resource_dir: &Path,
        models_dir: &Path,
    ) -> bool {
        const ATTEMPTS: u32 = 3;

        if !wait_for_port_free(QR_OLLAMA_PORT, Duration::from_secs(3)).await {
            log::warn!(
                "ollama_sidecar: {QR_OLLAMA_PORT} still not rebindable 3s after reclaiming an \
                 orphan -- trying to start anyway"
            );
        }

        for attempt in 1..=ATTEMPTS {
            if self.start_sidecar(resource_dir, models_dir).await {
                return true;
            }
            if attempt < ATTEMPTS {
                log::warn!(
                    "ollama_sidecar: start attempt {attempt}/{ATTEMPTS} failed right after a \
                     reclaim -- retrying after a short backoff"
                );
                tokio::time::sleep(Duration::from_millis(400 * u64::from(attempt))).await;
            }
        }
        false
    }

    /// Non-blocking liveness check for the background watcher `main.rs`
    /// spawns right after a successful `ensure_available` (items.id=586,
    /// amendment 2 -- a directly-callable tick function rather than only a
    /// sleep-wrapped loop, so it's unit-testable). Returns `true` while the
    /// caller should keep polling; `false` once there is nothing left to
    /// watch (no child held, or the child has just been observed to exit --
    /// either way `is_trusted()` is left/set `false` before returning).
    pub async fn liveness_tick(&mut self) -> bool {
        match self.child.as_mut() {
            None => {
                SIDECAR_TRUSTED.store(false, Ordering::Relaxed);
                false
            }
            Some(child) => match child.try_wait() {
                Ok(None) => true,
                Ok(Some(status)) => {
                    log::warn!(
                        "ollama_sidecar: sidecar exited unexpectedly ({status}) -- no longer \
                         trusted"
                    );
                    self.child = None;
                    SIDECAR_TRUSTED.store(false, Ordering::Relaxed);
                    ollama_ownership::remove_pidfile();
                    false
                }
                Err(e) => {
                    log::warn!("ollama_sidecar: try_wait failed during liveness check: {e}");
                    true
                }
            },
        }
    }

    // -----------------------------------------------------------------------
    // Private
    // -----------------------------------------------------------------------

    /// Probe 127.0.0.1:11434/api/tags with a 2 s timeout.
    ///
    /// A 2xx response means Ollama is running. Any error or timeout → `NotFound`.
    /// Intentionally separate from `OllamaClient::check_health()`:
    ///   - different timeout (2 s vs 5 s)
    ///   - binary question only (running or not)
    ///   - startup-only, not used for runtime monitoring
    async fn detect(&self) -> DetectionResult {
        let client = match Client::builder().timeout(Duration::from_secs(2)).build() {
            Ok(c) => c,
            Err(_) => return DetectionResult::NotFound,
        };

        match client.get("http://127.0.0.1:11434/api/tags").send().await {
            Ok(resp) if resp.status().is_success() => DetectionResult::SystemOllama,
            _ => DetectionResult::NotFound,
        }
    }

    /// Start QR's own Ollama on the dedicated port, trying each candidate
    /// from `candidate_paths()` in order until one becomes ready.
    ///
    /// Returns `true` if a sidecar spawned and became ready.
    ///
    /// `OLLAMA_MODELS` is set to a QR-owned directory
    /// (`<QR_DATA_ROOT>/ollama_models`), fully separate from any system
    /// Ollama's `~/.ollama/models` (decisions.id=840's implementation
    /// choice, made during items.id=436: QR never trusts/reads a BYO
    /// instance's models regardless of directory sharing, so the
    /// digest-dedup disk savings a shared directory would offer don't
    /// offset the risk of a user's independent `ollama rm` silently
    /// invalidating QR's own `providers.installed` bookkeeping).
    async fn start_sidecar(&mut self, resource_dir: &Path, models_dir: &Path) -> bool {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let candidates = candidate_paths(exe_dir.as_deref(), resource_dir, cfg!(debug_assertions));

        for candidate in candidates {
            let program: OsString = match &candidate {
                SidecarCandidate::File(path) => {
                    if !path.exists() {
                        log::debug!(
                            "ollama_sidecar: no binary at {} — trying next candidate",
                            path.display()
                        );
                        continue;
                    }
                    path.clone().into_os_string()
                }
                SidecarCandidate::SystemPath => {
                    log::warn!(
                        "ollama_sidecar: DEV-ONLY fallback in use — QR's bundled Ollama \
                         binary was not found or did not start, so `ollama` from PATH is \
                         being launched on 127.0.0.1:{QR_OLLAMA_PORT} with QR's own model \
                         directory. Packaging must replace the placeholder binary; release \
                         builds never take this path."
                    );
                    OsString::from(system_binary_name())
                }
            };
            if self.try_start(&program, models_dir).await {
                return true;
            }
        }

        log::warn!("ollama_sidecar: no candidate binary started successfully");
        false
    }

    /// Spawn `program serve` on the dedicated port and wait for readiness.
    /// On success the child is stored for `stop()` and a pidfile is
    /// written for the next startup's reclaim pass. On failure it is
    /// killed (its whole process group, in case it got far enough to load
    /// a runner before failing readiness) and released so the next
    /// candidate can bind the port.
    ///
    /// Spawned via `ollama_spawn_thread::spawn_with_pdeathsig` (items.id
    /// =586), not a bare `Command::spawn()` -- see that module's doc
    /// comment for the Linux parent-death-signal and process-group
    /// reasoning. There is no `kill_on_drop` safety net here the way the
    /// old implementation had: losing an unreaped `SpawnedChild` on a panic
    /// is now covered by the parent-death signal plus next-startup reclaim
    /// instead, which (unlike `kill_on_drop`) also covers `_exit()`/
    /// `SIGKILL` of this process itself, not just a normal Rust unwind.
    async fn try_start(&mut self, program: &OsStr, models_dir: &Path) -> bool {
        let envs = vec![
            (
                OsString::from("OLLAMA_HOST"),
                OsString::from(format!("127.0.0.1:{QR_OLLAMA_PORT}")),
            ),
            (
                OsString::from("OLLAMA_MODELS"),
                models_dir.as_os_str().to_owned(),
            ),
        ];
        let mut child =
            match ollama_spawn_thread::spawn_with_pdeathsig(program, vec!["serve".into()], envs)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    log::warn!(
                        "ollama_sidecar: failed to spawn {}: {e}",
                        program.to_string_lossy()
                    );
                    return false;
                }
            };

        log::info!(
            "ollama_sidecar: {} spawned (pid {}) — polling for ready",
            program.to_string_lossy(),
            child.pid
        );

        if Self::wait_for_ready(&mut child).await {
            ollama_ownership::write_pidfile(child.pid);
            self.child = Some(child);
            return true;
        }

        // Started but never became ready, or exited early — release it.
        ollama_ownership::kill_process_group(child.pgid, models_dir).await;
        let _ = child.wait().await;
        false
    }

    /// Poll 127.0.0.1:{QR_OLLAMA_PORT}/api/tags every 500 ms for up to 5 s
    /// (10 attempts). This is the sidecar's own readiness check — separate
    /// from `detect()`'s 11434 contention probe. Bails out immediately if
    /// the child has already exited (e.g. the placeholder binary).
    async fn wait_for_ready(child: &mut SpawnedChild) -> bool {
        let client = match Client::builder().timeout(Duration::from_secs(2)).build() {
            Ok(c) => c,
            Err(_) => return false,
        };
        let url = format!("http://127.0.0.1:{QR_OLLAMA_PORT}/api/tags");

        for attempt in 1u8..=10 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(Some(status)) = child.try_wait() {
                log::warn!("ollama_sidecar: process exited early ({status}) — not ready");
                return false;
            }
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    log::info!("ollama_sidecar: ready after {} poll(s)", attempt);
                    return true;
                }
                _ => log::debug!("ollama_sidecar: poll {attempt}/10 — not yet ready"),
            }
        }
        false
    }
}

/// Polls, by actually attempting to bind `port` on `127.0.0.1` (and
/// immediately dropping the listener on success -- this only ever checks,
/// it never holds the port), until it's free or `timeout` elapses. Used
/// only right after a reclaim killed something (items.id=586 fix-up):
/// confirmed empirically that the OS can take a moment after a process
/// dies before its listening port is actually rebindable. Returns `false`
/// (not an error) if `timeout` elapses with the port still held -- the
/// caller tries to start anyway and lets the normal failure/retry path
/// handle it.
async fn wait_for_port_free(port: u16, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------
// Platform helpers
// ---------------------------------------------------------------------------

/// Construct the Tauri-bundled binary filename for the current platform.
///
/// `cfg!()` is evaluated at compile time — each build produces exactly
/// the right filename for its target triple. `target_env` distinguishes
/// glibc (`gnu`) from musl on Linux.
fn sidecar_binary_name() -> String {
    let arch = std::env::consts::ARCH;
    if cfg!(all(target_os = "linux", target_env = "musl")) {
        format!("ollama-{arch}-unknown-linux-musl")
    } else if cfg!(target_os = "linux") {
        format!("ollama-{arch}-unknown-linux-gnu")
    } else if cfg!(target_os = "macos") {
        format!("ollama-{arch}-apple-darwin")
    } else if cfg!(target_os = "windows") {
        format!("ollama-{arch}-pc-windows-msvc.exe")
    } else {
        format!("ollama-{arch}")
    }
}

/// Filename Tauri gives an `externalBin` next to the app executable: the
/// configured name with the target triple stripped.
fn bundled_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "ollama.exe"
    } else {
        "ollama"
    }
}

/// Name resolved through PATH for the debug-only fallback.
fn system_binary_name() -> &'static str {
    bundled_binary_name()
}

/// Where the sidecar binary may be launched from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SidecarCandidate {
    /// A file on disk; skipped if it does not exist.
    File(PathBuf),
    /// `ollama` resolved through PATH. Debug builds only.
    SystemPath,
}

/// Ordered launch candidates. Pure so the release/debug difference is
/// unit-testable: `SystemPath` appears only when `allow_dev_fallback`.
fn candidate_paths(
    exe_dir: Option<&Path>,
    resource_dir: &Path,
    allow_dev_fallback: bool,
) -> Vec<SidecarCandidate> {
    let mut candidates = Vec::new();
    if let Some(dir) = exe_dir {
        candidates.push(SidecarCandidate::File(dir.join(bundled_binary_name())));
    }
    candidates.push(SidecarCandidate::File(
        resource_dir.join(sidecar_binary_name()),
    ));
    if allow_dev_fallback {
        candidates.push(SidecarCandidate::SystemPath);
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_prefer_exe_dir_then_legacy_resource_name() {
        let got = candidate_paths(Some(Path::new("/app/bin")), Path::new("/app/res"), false);
        assert_eq!(
            got,
            vec![
                SidecarCandidate::File(Path::new("/app/bin").join(bundled_binary_name())),
                SidecarCandidate::File(Path::new("/app/res").join(sidecar_binary_name())),
            ]
        );
    }

    #[test]
    fn release_candidates_never_include_path_fallback() {
        for exe_dir in [Some(Path::new("/app/bin")), None] {
            let got = candidate_paths(exe_dir, Path::new("/app/res"), false);
            assert!(!got.contains(&SidecarCandidate::SystemPath));
        }
    }

    #[test]
    fn dev_candidates_end_with_path_fallback() {
        let got = candidate_paths(Some(Path::new("/app/bin")), Path::new("/app/res"), true);
        assert_eq!(got.last(), Some(&SidecarCandidate::SystemPath));
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn missing_exe_dir_still_yields_legacy_candidate() {
        let got = candidate_paths(None, Path::new("/app/res"), false);
        assert_eq!(got.len(), 1);
    }

    /// A binary that exits immediately (like the checked-in placeholder)
    /// must fail fast, well inside the 5 s readiness window.
    #[cfg(unix)]
    #[tokio::test]
    async fn early_exiting_binary_fails_fast() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let script = dir.join("ollama");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut sidecar = OllamaSidecar::new();
        let started = std::time::Instant::now();
        let ok = sidecar.try_start(script.as_os_str(), dir).await;
        let elapsed = started.elapsed();

        assert!(!ok);
        assert!(sidecar.child.is_none());
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    }

    /// items.id=586 plan amendment 2 (Jason): `liveness_tick` must flip
    /// `is_trusted()` false the moment the held child exits, not just once
    /// at startup. Deliberately does not go through `ensure_available`/
    /// `try_start` at all -- those bind the real, hardcoded
    /// `QR_OLLAMA_PORT`, which this test has no safe way to fake a
    /// responder for without risking a real port collision with another
    /// test or a real dev session. `liveness_tick` itself doesn't care
    /// what the child is, so a plain `sleep` standing in for it exercises
    /// the same logic without that risk.
    #[tokio::test]
    async fn liveness_tick_flips_trust_false_when_child_exits() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let spawned = crate::ollama_spawn_thread::spawn_with_pdeathsig(
            "/bin/sh",
            vec!["-c".into(), "sleep 0.3".into()],
            vec![],
        )
        .await
        .expect("spawn should succeed");
        let mut sidecar = OllamaSidecar {
            child: Some(spawned),
        };
        SIDECAR_TRUSTED.store(true, Ordering::Relaxed);

        assert!(
            sidecar.liveness_tick().await,
            "child is still alive right after spawn"
        );
        assert!(is_trusted());

        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            !sidecar.liveness_tick().await,
            "child has exited by the time this tick runs"
        );
        assert!(sidecar.child.is_none());
        assert!(!is_trusted());
    }

    #[tokio::test]
    async fn liveness_tick_false_immediately_with_no_child_held() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let mut sidecar = OllamaSidecar::new();
        SIDECAR_TRUSTED.store(true, Ordering::Relaxed);

        assert!(!sidecar.liveness_tick().await);
        assert!(!is_trusted());
    }

    // -- wait_for_port_free (items.id=586 fix-up) ----------------------------

    #[tokio::test]
    async fn wait_for_port_free_returns_true_immediately_when_already_free() {
        // Port 0 asks the OS to assign an ephemeral port, bind it, then
        // immediately drop the listener -- the exact port number is then
        // free (vanishingly unlikely to be grabbed by anything else in
        // this test's own brief lifetime).
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };

        let started = std::time::Instant::now();
        assert!(wait_for_port_free(port, Duration::from_secs(3)).await);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "should return almost immediately when the port is already free"
        );
    }

    #[tokio::test]
    async fn wait_for_port_free_detects_release_within_the_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(listener);
        });

        let started = std::time::Instant::now();
        assert!(wait_for_port_free(port, Duration::from_secs(3)).await);
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "should not report free before the listener was actually dropped"
        );
    }

    #[tokio::test]
    async fn wait_for_port_free_times_out_when_held_throughout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let started = std::time::Instant::now();
        assert!(!wait_for_port_free(port, Duration::from_millis(500)).await);
        assert!(started.elapsed() >= Duration::from_millis(500));
        drop(listener);
    }

    // -- start_sidecar_after_reclaim (items.id=586 fix-up) -------------------

    /// Can't prove a *recovered* start through this path without binding
    /// the real, hardcoded `QR_OLLAMA_PORT` (same constraint noted on
    /// `liveness_tick_flips_trust_false_when_child_exits` above) -- so this
    /// proves the retry/backoff *shape* instead: a candidate that always
    /// fails fast is tried `ATTEMPTS` times, with increasing backoff
    /// between tries, and the whole call still returns promptly rather
    /// than hanging.
    #[cfg(unix)]
    #[tokio::test]
    async fn start_sidecar_after_reclaim_retries_with_backoff_then_gives_up() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = crate::test_support::ENV_MUTEX.lock().await;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Unlike try_start (a bare program path, no search), start_sidecar
        // -- and so start_sidecar_after_reclaim -- always runs the real
        // candidate_paths() search, which unconditionally includes the
        // debug-only PATH fallback (cfg!(debug_assertions) is baked in at
        // compile time, not something a test can turn off). Install the
        // always-fails fake at BOTH the resource_dir name AND as `ollama`
        // on PATH (temporarily prepended, restored after, serialized via
        // ENV_MUTEX like other process-global-env tests in this codebase)
        // so every reachable candidate resolves to it -- otherwise the
        // PATH-fallback candidate would resolve to the real system
        // `ollama` and actually try to bind the real, hardcoded
        // QR_OLLAMA_PORT, which this test (like
        // liveness_tick_flips_trust_false_when_child_exits above) must
        // never risk.
        std::fs::write(dir.join(sidecar_binary_name()), "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(
            dir.join(sidecar_binary_name()),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(dir.join("ollama"), "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(dir.join("ollama"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let saved_path = std::env::var("PATH").ok();
        let new_path = match &saved_path {
            Some(p) => format!("{}:{p}", dir.display()),
            None => dir.display().to_string(),
        };
        std::env::set_var("PATH", &new_path);

        let mut sidecar = OllamaSidecar::new();
        let started = std::time::Instant::now();
        let ok = sidecar.start_sidecar_after_reclaim(dir, dir).await;
        let elapsed = started.elapsed();

        match saved_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }

        assert!(!ok);
        assert!(sidecar.child.is_none());
        // 3 attempts, backoff after the first two (400ms + 800ms = 1.2s
        // minimum) -- in practice comfortably exceeded anyway since each
        // attempt's own candidate search costs wait_for_ready's first
        // 500ms poll per reachable candidate, but the floor is asserted
        // directly rather than relied on incidentally.
        assert!(
            elapsed >= Duration::from_millis(1200),
            "expected backoff between attempts, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(15),
            "retries should not hang, took {elapsed:?}"
        );
    }
}
