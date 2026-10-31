//! Ollama sidecar lifecycle manager.
//!
//! # Dedicated-port trust boundary (decisions.id=840, decisions.id=863)
//! QR's bundled sidecar always runs on its own dedicated port, never on
//! 11434 and never shared with any user-installed Ollama. It prefers 21434
//! and falls back through 21435-21443 in order ([`PORT_CANDIDATES`]); the
//! fallback list is fixed and sits outside both Linux's ephemeral range and
//! Windows' dynamic range, and there is no manual port setting in R1.
//! `ensure_available()` unconditionally starts the sidecar; it no longer
//! branches on whether a system instance is detected. decisions.id=380's
//! original "detect-first, reuse if found" design is retired: items.id=436's
//! security review found QR must never load a model it did not itself pull
//! from its own curated list (Ollama has real, exploitable GGUF/Modelfile-
//! parsing vulnerabilities -- see decisions.id=840 for the CVE list), so
//! reusing a detected instance's models is no longer safe regardless of
//! convenience.
//!
//! `ensure_available()` still probes 127.0.0.1:11434 (decisions.id=380's
//! original detection mechanism, reused verbatim) -- but purely to warn
//! the user of possible GPU/RAM contention if their own Ollama is also
//! running, via `SidecarStartup::system_ollama_contention`. That probe
//! never gates whether the sidecar starts and never causes QR to read or
//! trust the detected instance's models.
//!
//! No caller outside this module ever invokes `tokio::process::Command`
//! directly -- all process management is encapsulated here.
//!
//! # Port fallback (items.id=668)
//! Each attempt spawns the sidecar with `OLLAMA_HOST=127.0.0.1:{port}` and
//! polls `/api/tags` on that same port only while its own child is alive. A
//! child that exits means that port is not ours (Ollama exits 1 with "bind:
//! address already in use"), so the next port is tried. A 2xx that arrives
//! after our child died came from whatever holds the port and is rejected.
//! The bind address, the readiness URL and the log lines all take the one
//! `port` argument, so they cannot disagree. If every port fails the sidecar
//! is `Unavailable`, the ports tried are logged, and **nothing is killed**:
//! QR never signals the holder of a port. The port that won lives in
//! [`port()`], which `OllamaClient` reads.
//!
//! # Binary bundling (build pipeline note -- D6-353)
//! `tauri.conf.json` `externalBin` lists `binaries/qr-ollama`; the source
//! file is `src-tauri/binaries/qr-ollama-{target-triple}` (on Garuda:
//! `qr-ollama-x86_64-unknown-linux-gnu`). Tauri copies it next to the app
//! executable with the triple stripped, i.e. `<exe dir>/qr-ollama` (verified
//! in dev: `target/debug/qr-ollama`). It is NOT placed in the resource
//! directory. The packaged-build layout has not been verified, so
//! `candidate_paths()` also tries the legacy `<resource_dir>/qr-ollama-{triple}`
//! name from D6-353. The name `qr-ollama` is also how QR recognises its own
//! sidecar when it reclaims one (`ollama_ownership::member_is_provable`).
//!
//! The checked-in binary is currently a placeholder script that exits 1
//! (real binary comes in the packaging pass, items.id=583). In debug builds
//! only, `candidate_paths()` therefore ends with a PATH lookup of `ollama` so
//! `cargo tauri dev` still gets a QR-owned instance: same dedicated ports,
//! same QR-private model directory, never the user's 11434 instance.
//! Release builds never take that fallback.
//!
//! # Leak prevention and client trust (items.id=586, items.id=668)
//! `ensure_available()` reclaims any leftover sidecar group a dead QR
//! recorded (`ollama_ownership::reclaim_orphans`) before starting fresh --
//! never adopting a running process, and never signalling anything outside a
//! group QR itself recorded at spawn (see that module for the proof; a live
//! sibling QR instance's own sidecar is left alone, and there is no single-
//! instance guard on QR itself until items.id=667). The spawned child is
//! given its own process group (`ollama_spawn_thread::spawn_with_pdeathsig`,
//! which also arms Linux's `PR_SET_PDEATHSIG`) so `stop()` and the startup
//! reclaim path can both tear down a runner the `serve` process spawned, not
//! just `serve` itself -- confirmed empirically that a runner shares
//! `serve`'s process group rather than starting its own.
//!
//! [`is_trusted`] is the single process-wide gate `OllamaClient` checks
//! before making any network call (see that module): fail-closed by
//! default, set true only once this module has itself verified the
//! sidecar came up, and set false again the moment the held child exits
//! (`liveness_tick`) -- never left stuck true against a port some
//! unidentified later process happens to be answering on.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::Duration;

use reqwest::Client;

use crate::ollama_ownership;
use crate::ollama_spawn_thread::{self, SpawnedChild};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The ports QR's own sidecar may bind, in the order they are tried, via
/// `OLLAMA_HOST=127.0.0.1:{port}`. Never 11434 (decisions.id=840). The list
/// is fixed (decisions.id=863) and sits outside both Linux's default
/// ephemeral port range (32768-60999) and Windows' dynamic port range
/// (49152-65535) -- decisions.id=840's original pick, 35973, was inside the
/// Linux range and was moved for that reason; OS-assigned ports were
/// rejected.
pub const PORT_CANDIDATES: [u16; 10] = [
    21434, 21435, 21436, 21437, 21438, 21439, 21440, 21441, 21442, 21443,
];

/// The port the sidecar that came up is bound to; 0 until one has. Written
/// (Release) only by [`publish_ready`], before [`SIDECAR_TRUSTED`] goes true,
/// and cleared by [`withdraw_trust`].
static ACTIVE_PORT: AtomicU16 = AtomicU16::new(0);

/// The port `OllamaClient` must contact: the one QR's own sidecar is bound
/// to. The client takes its port from here and never names a literal.
/// Before a sidecar is up it reports the preferred port, which is harmless:
/// the client is gated by [`is_trusted`] and makes no call until it is true.
pub fn port() -> u16 {
    #[cfg(test)]
    {
        let forced = TEST_PORT_OVERRIDE.load(Ordering::Relaxed);
        if forced != 0 {
            return forced;
        }
    }
    match ACTIVE_PORT.load(Ordering::Acquire) {
        0 => PORT_CANDIDATES[0],
        active => active,
    }
}

/// 0 = no override. Test-only seam so a mock listener on an ephemeral port
/// can stand in for the sidecar now that `OllamaClient` no longer reads
/// `OLLAMA_HOST`/`OLLAMA_PORT` (items.id=616).
#[cfg(test)]
static TEST_PORT_OVERRIDE: AtomicU16 = AtomicU16::new(0);

/// Test-only. Callers must hold `test_support::ENV_MUTEX` for as long as the
/// override is set, and should reset it with [`ForcedPortGuard`] so an assert
/// failure cannot leak it into the next test.
#[cfg(test)]
pub fn force_port_for_test(port: Option<u16>) {
    TEST_PORT_OVERRIDE.store(port.unwrap_or(0), Ordering::Relaxed);
}

/// Sets the test port override and clears it on drop (including on panic).
#[cfg(test)]
pub struct ForcedPortGuard;

#[cfg(test)]
impl ForcedPortGuard {
    pub fn set(port: u16) -> Self {
        force_port_for_test(Some(port));
        Self
    }
}

#[cfg(test)]
impl Drop for ForcedPortGuard {
    fn drop(&mut self) {
        force_port_for_test(None);
    }
}

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
    /// QR's own sidecar, on one of its dedicated ports, started successfully.
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

/// Why `start_sidecar` found no sidecar to run.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StartFailure {
    /// No candidate binary exists, or none could be spawned.
    NoBinary,
    /// A candidate spawned but did not come up on any of these ports: each
    /// was busy (its child exited on the bind failure) or the binary exits
    /// at start. Nothing was signalled.
    PortsExhausted { tried: Vec<u16> },
    /// A candidate spawned and stayed alive on `port` but never answered
    /// within the readiness window (a slow start, e.g. GPU discovery). It
    /// was released and no further port was tried for that binary.
    TimedOut { port: u16 },
}

/// How the wait for a freshly spawned sidecar ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadyOutcome {
    Ready,
    /// The child exited within the window (its port was busy, or the binary
    /// failed). Also the outcome of a 2xx that arrived after the child died.
    Exited,
    /// The child stayed alive the whole window without answering.
    TimedOut,
}

/// How long to wait for readiness: probes, 500 ms apart (5 s in production).
const READY_PROBES: u32 = 10;

/// Outcome of one spawn-and-wait on one port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    Ready,
    /// Spawned and then exited: that port is not ours (a busy port makes
    /// Ollama exit 1 on the bind), so the next port is tried.
    Exited,
    /// Spawned, still alive, but never answered within the window. The port
    /// may well be ours; a slow start is not a reason to restart the binary
    /// on every other port, so the loop moves to the next binary instead.
    TimedOut,
    /// The binary could not be spawned at all; no port will change that.
    SpawnFailed,
}

// ---------------------------------------------------------------------------
// Sidecar manager
// ---------------------------------------------------------------------------

/// Single process-wide gate `OllamaClient` checks before any network call
/// (items.id=586). Fail-closed: `false` until `ensure_available` itself
/// confirms the sidecar it just started is ready, and `false` again the
/// moment `liveness_tick` observes the held child has exited. Never set
/// from anywhere that only *thinks* a request to the sidecar port would
/// succeed -- only from this module's own confirmed process state.
///
/// Stored with Release and loaded with Acquire, so a reader that sees `true`
/// also sees the [`ACTIVE_PORT`] written before it.
static SIDECAR_TRUSTED: AtomicBool = AtomicBool::new(false);

/// Whether `OllamaClient` may contact [`port()`] right now.
pub fn is_trusted() -> bool {
    SIDECAR_TRUSTED.load(Ordering::Acquire)
}

/// The port first, then the trust flag, so [`is_trusted`] never reads true
/// while [`port()`] still names a different port.
fn publish_ready(port: u16) {
    ACTIVE_PORT.store(port, Ordering::Release);
    SIDECAR_TRUSTED.store(true, Ordering::Release);
}

/// Trust first, then the port.
fn withdraw_trust() {
    SIDECAR_TRUSTED.store(false, Ordering::Release);
    ACTIVE_PORT.store(0, Ordering::Release);
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
    SIDECAR_TRUSTED.store(value, Ordering::Release);
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
    /// The ports tried, in order. [`PORT_CANDIDATES`] in production; tests
    /// inject their own so none of them ever binds or names 21434-21443.
    ports: Vec<u16>,
    /// Readiness probes per attempt ([`READY_PROBES`] in production; tests
    /// shorten it so a never-answering stub does not make CI wait 5 s).
    ready_probes: u32,
}

impl Default for OllamaSidecar {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaSidecar {
    pub fn new() -> Self {
        Self {
            child: None,
            ports: PORT_CANDIDATES.to_vec(),
            ready_probes: READY_PROBES,
        }
    }

    /// Test-only: a manager that tries `ports` instead of [`PORT_CANDIDATES`].
    #[cfg(test)]
    fn with_ports(ports: Vec<u16>) -> Self {
        Self {
            child: None,
            ports,
            ready_probes: READY_PROBES,
        }
    }

    /// Test-only: shorten the readiness window to `probes` x 500 ms.
    #[cfg(test)]
    fn with_ready_probes(mut self, probes: u32) -> Self {
        self.ready_probes = probes;
        self
    }

    /// Reclaims any leftover recorded sidecar group first, then does the
    /// warn-only contention check, then unconditionally starts QR's own
    /// fresh sidecar on the first free port of its dedicated list.
    ///
    /// Single public entry point for startup.
    ///
    /// # Order of operations (decisions.id=840, decisions.id=863, items.id=586)
    /// 0. Reclaim: signal the provable members of any sidecar group a dead QR
    ///    recorded (`ollama_ownership::reclaim_orphans`). Never adopts a
    ///    survivor; always starts fresh afterward regardless of what reclaim
    ///    found.
    /// 1. Probe `http://127.0.0.1:11434/api/tags` with a 2 s timeout --
    ///    purely to set `system_ollama_contention`; never gates step 3.
    /// 2. If reclaim signalled anything, wait (bounded, ~3 s) for the first
    ///    port to actually be rebindable, so QR comes back on 21434 rather
    ///    than the next port -- confirmed empirically that the OS can take a
    ///    moment after a kill before the port is free again.
    /// 3. Start QR's own bundled sidecar from `resource_dir`, always, on the
    ///    first of [`PORT_CANDIDATES`] it can bind (`127.0.0.1:{port}`).
    /// 4. Poll that port every 500 ms for up to 5 s -> `OllamaSource::
    ///    Sidecar`, and publish the port and set [`is_trusted`] true.
    /// 5. If no port works -> `OllamaSource::Unavailable`, the ports tried
    ///    are logged, nothing is killed, and [`is_trusted`] stays false.
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
                     starts its own sidecar on its own port ({} to {}) regardless \
                     (decisions.id=840); running both may contend for GPU/RAM",
                    PORT_CANDIDATES[0],
                    PORT_CANDIDATES[PORT_CANDIDATES.len() - 1]
                );
                true
            }
            DetectionResult::NotFound => false,
        };

        if reclaimed {
            if let Some(&first) = self.ports.first() {
                if !wait_for_port_free(first, Duration::from_secs(3)).await {
                    log::warn!(
                        "ollama_sidecar: {first} still not rebindable 3s after reclaiming an \
                         orphan -- trying to start anyway (the next port is used if it is \
                         still held)"
                    );
                }
            }
        }

        let source = match self.start_sidecar(resource_dir, &models_dir).await {
            Ok(port) => {
                log::info!("ollama_sidecar: sidecar ready at 127.0.0.1:{port}");
                publish_ready(port);
                OllamaSource::Sidecar
            }
            Err(StartFailure::PortsExhausted { tried }) => {
                log::error!(
                    "ollama_sidecar: the sidecar did not come up on any of the ports tried \
                     {tried:?} (each busy, or the binary exits at start). The local tier is \
                     unavailable. Nothing was killed: QR never signals the holder of a port."
                );
                withdraw_trust();
                OllamaSource::Unavailable
            }
            Err(StartFailure::TimedOut { port }) => {
                log::error!(
                    "ollama_sidecar: the sidecar stayed alive on port {port} but never \
                     answered within the readiness window, and was stopped. No other port \
                     was tried for it (a slow start is not a busy port). The local tier is \
                     unavailable."
                );
                withdraw_trust();
                OllamaSource::Unavailable
            }
            Err(StartFailure::NoBinary) => {
                log::warn!("ollama_sidecar: no candidate binary started");
                withdraw_trust();
                OllamaSource::Unavailable
            }
        };

        SidecarStartup {
            source,
            system_ollama_contention,
        }
    }

    /// Stop the sidecar process if one was started by this manager, and
    /// anything in its process group (a runner it loaded) along with it --
    /// `kill_process_group` itself signals only members it can prove are
    /// QR's own (see its own doc comment). The record is dropped only when
    /// no provable member survived, so the next start can retry otherwise.
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
            withdraw_trust();
            let (pid, pgid) = (child.pid, child.pgid);
            let outcome = ollama_ownership::kill_process_group(pgid).await;
            let reaped = reap_bounded(child).await;
            if outcome.survivors == 0 && reaped {
                ollama_ownership::remove_record(pid);
            } else {
                log::warn!(
                    "ollama_sidecar: {} sidecar process(es) survived stop (leader reaped: \
                     {reaped}) -- keeping the record so the next start can reclaim them",
                    outcome.survivors
                );
            }
            log::info!("ollama_sidecar: sidecar stopped");
        }
    }

    /// Non-blocking liveness check for the background watcher `main.rs`
    /// spawns right after a successful `ensure_available` (items.id=586,
    /// amendment 2 -- a directly-callable tick function rather than only a
    /// sleep-wrapped loop, so it's unit-testable). Returns `true` while the
    /// caller should keep polling; `false` once there is nothing left to
    /// watch (no child held, or the child has just been observed to exit --
    /// either way `is_trusted()` is left/set `false` before returning).
    ///
    /// When `serve` has exited on its own, a runner it loaded is still alive
    /// and holding memory, and with the broad sweep gone the record is the
    /// only way back to it (items.id=668). So the held child's group is
    /// killed (provable members only) before the record is dropped, and the
    /// record is kept if a provable member survives.
    pub async fn liveness_tick(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            withdraw_trust();
            return false;
        };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                let (pid, pgid) = (child.pid, child.pgid);
                log::warn!(
                    "ollama_sidecar: sidecar exited unexpectedly ({status}) -- no longer \
                     trusted"
                );
                self.child = None;
                withdraw_trust();
                let outcome = ollama_ownership::kill_process_group(pgid).await;
                if outcome.survivors == 0 {
                    ollama_ownership::remove_record(pid);
                } else {
                    log::warn!(
                        "ollama_sidecar: {} runner process(es) of the exited sidecar survived \
                         -- keeping the record so the next start can reclaim them",
                        outcome.survivors
                    );
                }
                false
            }
            Err(e) => {
                log::warn!("ollama_sidecar: try_wait failed during liveness check: {e}");
                true
            }
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

    /// Start QR's own Ollama, trying each candidate from `candidate_paths()`
    /// in order until one becomes ready. Returns the port it is bound to.
    ///
    /// `OLLAMA_MODELS` is set to a QR-owned directory
    /// (`<QR_DATA_ROOT>/ollama_models`), fully separate from any system
    /// Ollama's `~/.ollama/models` (decisions.id=840's implementation
    /// choice, made during items.id=436: QR never trusts/reads a BYO
    /// instance's models regardless of directory sharing, so the
    /// digest-dedup disk savings a shared directory would offer don't
    /// offset the risk of a user's independent `ollama rm` silently
    /// invalidating QR's own `providers.installed` bookkeeping).
    async fn start_sidecar(
        &mut self,
        resource_dir: &Path,
        models_dir: &Path,
    ) -> Result<u16, StartFailure> {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let candidates = candidate_paths(exe_dir.as_deref(), resource_dir, cfg!(debug_assertions));
        self.start_with_candidates(candidates, models_dir).await
    }

    /// The candidate × port loop. Separate from `start_sidecar` so a test can
    /// supply its own candidates and never reach the debug PATH fallback
    /// (which would launch the machine's real `ollama`).
    async fn start_with_candidates(
        &mut self,
        candidates: Vec<SidecarCandidate>,
        models_dir: &Path,
    ) -> Result<u16, StartFailure> {
        let ports = self.ports.clone();
        let mut exhausted = false;
        let mut timed_out = None;

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
                         being launched on 127.0.0.1 (ports {ports:?}) with QR's own model \
                         directory. Packaging must replace the placeholder binary; release \
                         builds never take this path."
                    );
                    OsString::from(system_binary_name())
                }
            };

            let mut spawned = true;
            for &port in &ports {
                match self.try_start(&program, models_dir, port).await {
                    Attempt::Ready => return Ok(port),
                    Attempt::Exited => {}
                    Attempt::TimedOut => {
                        // Alive but silent: stop trying ports for this
                        // binary and go on to the next one.
                        log::error!(
                            "ollama_sidecar: {} stayed alive on port {port} for the whole \
                             readiness window without answering -- stopped it; not trying \
                             the remaining ports for it",
                            program.to_string_lossy()
                        );
                        timed_out = Some(port);
                        spawned = false;
                        break;
                    }
                    Attempt::SpawnFailed => {
                        spawned = false;
                        break;
                    }
                }
            }
            if spawned && !ports.is_empty() {
                exhausted = true;
            }
        }

        log::warn!("ollama_sidecar: no candidate binary started successfully");
        if exhausted {
            Err(StartFailure::PortsExhausted { tried: ports })
        } else if let Some(port) = timed_out {
            Err(StartFailure::TimedOut { port })
        } else {
            Err(StartFailure::NoBinary)
        }
    }

    /// Spawn `program serve` on `port` and wait for readiness.
    ///
    /// The group is recorded the moment it is spawned, before readiness
    /// (`ollama_ownership::record_spawn`), so a `serve` that dies during the
    /// readiness window and leaves a runner behind is still reclaimable. On
    /// success the child is stored for `stop()`. On failure its group is
    /// killed (provable members only, in case it got far enough to load a
    /// runner before failing readiness) and the record dropped, so the next
    /// port can be tried.
    ///
    /// Spawned via `ollama_spawn_thread::spawn_with_pdeathsig` (items.id
    /// =586), not a bare `Command::spawn()` -- see that module's doc
    /// comment for the Linux parent-death-signal and process-group
    /// reasoning. There is no `kill_on_drop` safety net here the way the
    /// old implementation had: losing an unreaped `SpawnedChild` on a panic
    /// is now covered by the parent-death signal plus next-startup reclaim
    /// instead, which (unlike `kill_on_drop`) also covers `_exit()`/
    /// `SIGKILL` of this process itself, not just a normal Rust unwind.
    async fn try_start(&mut self, program: &OsStr, models_dir: &Path, port: u16) -> Attempt {
        let envs = vec![
            (
                OsString::from("OLLAMA_HOST"),
                OsString::from(format!("127.0.0.1:{port}")),
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
                    return Attempt::SpawnFailed;
                }
            };
        ollama_ownership::record_spawn(child.pid, child.pgid);

        log::info!(
            "ollama_sidecar: {} spawned (pid {}) on port {port} — polling for ready",
            program.to_string_lossy(),
            child.pid
        );

        let outcome = Self::wait_for_ready(&mut child, port, self.ready_probes).await;
        if outcome == ReadyOutcome::Ready {
            self.child = Some(child);
            return Attempt::Ready;
        }

        // Exited early (a busy port), or alive but silent for the whole
        // window -- release it (provable members only) and drop its record.
        let pid = child.pid;
        let killed = ollama_ownership::kill_process_group(child.pgid).await;
        let reaped = reap_bounded(child).await;
        if killed.survivors == 0 && reaped {
            ollama_ownership::remove_record(pid);
        }
        match outcome {
            ReadyOutcome::TimedOut => Attempt::TimedOut,
            _ => Attempt::Exited,
        }
    }

    /// Poll 127.0.0.1:{port}/api/tags every 500 ms for up to `probes` probes
    /// (5 s in production), checking that the child is still running every 100 ms in
    /// between. This is the sidecar's own readiness check — separate from
    /// `detect()`'s 11434 contention probe. Bails out immediately if the
    /// child has exited (a busy port, or the placeholder binary) -- that is
    /// [`ReadyOutcome::Exited`], which moves on to the next port. A child
    /// still alive when the window ends is [`ReadyOutcome::TimedOut`], which
    /// does not: our rule is that a child that EXITS means the port is not
    /// ours.
    ///
    /// A 2xx only counts if the child is still running when it arrives
    /// (items.id=616): if our child lost the bind to something already on
    /// the port and exited, the answer came from that other process, and
    /// reporting ready would make `OllamaClient` trust it.
    async fn wait_for_ready(child: &mut SpawnedChild, port: u16, probes: u32) -> ReadyOutcome {
        const TICKS_PER_PROBE: u32 = 5;

        let client = match Client::builder().timeout(Duration::from_secs(2)).build() {
            Ok(c) => c,
            Err(_) => return ReadyOutcome::TimedOut,
        };
        let url = format!("http://127.0.0.1:{port}/api/tags");

        for attempt in 1..=probes {
            for _ in 0..TICKS_PER_PROBE {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Ok(Some(status)) = child.try_wait() {
                    log::warn!(
                        "ollama_sidecar: process exited early ({status}) — port {port} is not \
                         ours or the binary failed; not ready"
                    );
                    return ReadyOutcome::Exited;
                }
            }
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(Some(status)) = child.try_wait() {
                        log::warn!(
                            "ollama_sidecar: port {port} answered but the sidecar process had \
                             already exited ({status}) -- the answer came from another \
                             process, not ready"
                        );
                        return ReadyOutcome::Exited;
                    }
                    log::info!("ollama_sidecar: ready after {} poll(s)", attempt);
                    return ReadyOutcome::Ready;
                }
                _ => log::debug!("ollama_sidecar: poll {attempt}/{probes} — not yet ready"),
            }
        }
        ReadyOutcome::TimedOut
    }
}

/// Waits up to 2 s for a child to have exited, then reaps it. Returns whether
/// it did. Never blocks indefinitely: a child that `kill_process_group` did
/// not signal because it is not provably QR's own (an unreadable `exe()`, a
/// binary name outside the allowlist) is still running, and an unbounded
/// `wait()` on it would hang startup or shutdown. It is left alone and its
/// record is kept, so the next start can look at it again.
async fn reap_bounded(mut child: SpawnedChild) -> bool {
    for _ in 0..20 {
        if let Ok(Some(_)) = child.try_wait() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    log::warn!(
        "ollama_sidecar: pid {} is still running and was not signalled (not provably QR's \
         own) -- leaving it",
        child.pid
    );
    false
}

/// Polls, by actually attempting to bind `port` on `127.0.0.1` (and
/// immediately dropping the listener on success -- this only ever checks,
/// it never holds the port), until it's free or `timeout` elapses. Used
/// only right after a reclaim signalled something (items.id=586 fix-up):
/// confirmed empirically that the OS can take a moment after a process
/// dies before its listening port is actually rebindable. Returns `false`
/// (not an error) if `timeout` elapses with the port still held -- the
/// caller starts anyway and the port fallback absorbs a port that is still
/// busy.
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
        format!("qr-ollama-{arch}-unknown-linux-musl")
    } else if cfg!(target_os = "linux") {
        format!("qr-ollama-{arch}-unknown-linux-gnu")
    } else if cfg!(target_os = "macos") {
        format!("qr-ollama-{arch}-apple-darwin")
    } else if cfg!(target_os = "windows") {
        format!("qr-ollama-{arch}-pc-windows-msvc.exe")
    } else {
        format!("qr-ollama-{arch}")
    }
}

/// Filename Tauri gives an `externalBin` next to the app executable: the
/// configured name with the target triple stripped.
fn bundled_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "qr-ollama.exe"
    } else {
        "qr-ollama"
    }
}

/// Name resolved through PATH for the debug-only fallback: the machine's own
/// `ollama`, not QR's bundled binary.
fn system_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "ollama.exe"
    } else {
        "ollama"
    }
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
    use crate::test_support::procs::{is_effectively_dead, named_copy, spawn_sleep_stub};
    use crate::test_support::{DataRoot, KillGroupOnDrop};
    use std::net::TcpListener;

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
    fn the_bundled_binary_is_named_qr_ollama() {
        assert!(bundled_binary_name().starts_with("qr-ollama"));
        assert!(sidecar_binary_name().starts_with("qr-ollama-"));
        assert_eq!(system_binary_name().trim_end_matches(".exe"), "ollama");
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

    #[test]
    fn port_candidates_are_the_ten_decided_ports() {
        assert_eq!(PORT_CANDIDATES.len(), 10);
        assert_eq!(PORT_CANDIDATES[0], 21434);
        assert_eq!(PORT_CANDIDATES[9], 21443);
        assert!(PORT_CANDIDATES.windows(2).all(|w| w[1] == w[0] + 1));
        assert!(!PORT_CANDIDATES.contains(&11434));
    }

    /// A free port from the OS's ephemeral range -- never one of
    /// [`PORT_CANDIDATES`], so no test ever binds or names 21434-21443.
    fn free_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!PORT_CANDIDATES.contains(&port));
        port
    }

    /// Holds a port by listening on it; the test's own listener.
    fn hold_port() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!PORT_CANDIDATES.contains(&port));
        (listener, port)
    }

    /// Publishes a sidecar port for the test and withdraws it on drop, so a
    /// failed assertion cannot leave trust or a port behind.
    struct WithdrawOnDrop;

    impl Drop for WithdrawOnDrop {
        fn drop(&mut self) {
            withdraw_trust();
        }
    }

    // -- port() / publish_ready ------------------------------------------------

    #[tokio::test]
    async fn publish_ready_makes_port_and_trust_agree_and_withdraw_clears_both() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _withdraw = WithdrawOnDrop;
        withdraw_trust();
        assert!(!is_trusted());
        assert_eq!(port(), PORT_CANDIDATES[0], "no sidecar yet: preferred port");

        publish_ready(PORT_CANDIDATES[3]);
        assert!(is_trusted());
        assert_eq!(port(), PORT_CANDIDATES[3]);

        withdraw_trust();
        assert!(!is_trusted());
        assert_eq!(port(), PORT_CANDIDATES[0]);
    }

    // -- try_start --------------------------------------------------------------

    /// A binary that exits immediately (like the checked-in placeholder)
    /// must fail fast, well inside the 5 s readiness window.
    #[cfg(unix)]
    #[tokio::test]
    async fn early_exiting_binary_fails_fast() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let root = DataRoot::isolated();
        let dir = root.path();
        let script = dir.join("qr-ollama");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut sidecar = OllamaSidecar::with_ports(vec![free_port()]);
        let started = std::time::Instant::now();
        let attempt = sidecar
            .try_start(script.as_os_str(), dir, sidecar.ports[0])
            .await;
        let elapsed = started.elapsed();

        assert_eq!(attempt, Attempt::Exited);
        assert!(sidecar.child.is_none());
        assert!(
            crate::ollama_ownership::recorded_groups_for_test().is_empty(),
            "a failed attempt leaves no record"
        );
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    }

    #[tokio::test]
    async fn spawn_failure_stops_the_port_loop_for_that_candidate() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let root = DataRoot::isolated();
        let mut sidecar = OllamaSidecar::with_ports(vec![free_port(), free_port()]);

        let got = sidecar
            .start_with_candidates(
                vec![SidecarCandidate::File(root.path().join("does-not-exist"))],
                root.path(),
            )
            .await;
        assert_eq!(got, Err(StartFailure::NoBinary));

        let not_executable = root.path().join("not-executable");
        std::fs::write(&not_executable, "plain text").unwrap();
        let got = sidecar
            .start_with_candidates(vec![SidecarCandidate::File(not_executable)], root.path())
            .await;
        assert_eq!(got, Err(StartFailure::NoBinary), "spawn error, not a port");
    }

    /// items.id=668: the pidfile record exists while the sidecar is still
    /// starting, not only once it is ready.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_group_is_recorded_at_spawn_before_it_is_ready() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let root = DataRoot::isolated();
        let dir = root.path();
        // Alive for a moment, never ready (no listener), then gone.
        let script = dir.join("qr-ollama");
        std::fs::write(&script, "#!/bin/sh\nsleep 1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut sidecar = OllamaSidecar::with_ports(vec![free_port()]);
        let port = sidecar.ports[0];
        let (attempt, seen) =
            tokio::join!(sidecar.try_start(script.as_os_str(), dir, port), async {
                for _ in 0..40 {
                    let groups = crate::ollama_ownership::recorded_groups_for_test();
                    if !groups.is_empty() {
                        return groups;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Vec::new()
            });

        assert_eq!(seen.len(), 1, "a record must exist while still starting");
        assert_eq!(seen[0].0 as libc::pid_t, seen[0].1, "pid == pgid");
        assert_eq!(attempt, Attempt::Exited);
        let _cleanup = KillGroupOnDrop(seen[0].1);
        assert!(crate::ollama_ownership::recorded_groups_for_test().is_empty());
    }

    // -- liveness_tick ----------------------------------------------------------

    /// items.id=586 plan amendment 2 (Jason): `liveness_tick` must flip
    /// `is_trusted()` false the moment the held child exits, not just once
    /// at startup. Deliberately does not go through `ensure_available`/
    /// `try_start` at all. `liveness_tick` itself doesn't care what the
    /// child is, so a plain `sleep` standing in for it exercises the same
    /// logic.
    #[tokio::test]
    async fn liveness_tick_flips_trust_false_when_child_exits() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        // items.id=663: a child that exited reaches the record cleanup,
        // whose get_data_root() no longer falls back to a relative path.
        let _root = DataRoot::isolated();
        let _withdraw = WithdrawOnDrop;
        let spawned = crate::ollama_spawn_thread::spawn_with_pdeathsig(
            "/bin/sh",
            vec!["-c".into(), "sleep 0.3".into()],
            vec![],
        )
        .await
        .expect("spawn should succeed");
        let _kill = KillGroupOnDrop(spawned.pgid);
        let mut sidecar = OllamaSidecar::with_ports(vec![]);
        sidecar.child = Some(spawned);
        publish_ready(free_port());

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
        assert_eq!(
            port(),
            PORT_CANDIDATES[0],
            "the active port is withdrawn too"
        );
    }

    #[tokio::test]
    async fn liveness_tick_false_immediately_with_no_child_held() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _withdraw = WithdrawOnDrop;
        let mut sidecar = OllamaSidecar::new();
        publish_ready(free_port());

        assert!(!sidecar.liveness_tick().await);
        assert!(!is_trusted());
    }

    /// items.id=668 (Jason's scope decision): `serve` crashes mid-session and
    /// leaves a runner behind. With the broad sweep gone the record is the
    /// only way back to that runner, so `liveness_tick` kills the held
    /// child's group (provable members) before it drops the record.
    #[tokio::test]
    async fn liveness_tick_kills_a_runner_left_behind_when_serve_dies() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let root = DataRoot::isolated();
        let _withdraw = WithdrawOnDrop;
        let runner_prog = named_copy(root.path(), "/bin/sleep", "llama-server");

        // The `serve` stand-in: a group leader that exits on its own.
        let serve = crate::ollama_spawn_thread::spawn_with_pdeathsig(
            "/bin/sh",
            vec!["-c".into(), "sleep 1.5".into()],
            vec![],
        )
        .await
        .expect("spawn should succeed");
        let (pid, pgid) = (serve.pid, serve.pgid);
        let _kill = KillGroupOnDrop(pgid);
        crate::ollama_ownership::record_spawn(pid, pgid);
        // Its runner: a provable member of the same group that outlives it.
        let runner = spawn_sleep_stub(&runner_prog, pgid, &[]);

        let mut sidecar = OllamaSidecar::with_ports(vec![]);
        sidecar.child = Some(serve);
        publish_ready(free_port());
        assert!(sidecar.liveness_tick().await);
        assert!(!is_effectively_dead(runner.id()));

        tokio::time::sleep(Duration::from_millis(1900)).await;
        assert!(!sidecar.liveness_tick().await, "serve has exited");

        assert!(
            is_effectively_dead(runner.id()),
            "the runner of the exited serve must be killed"
        );
        assert!(!is_trusted());
        assert!(
            crate::ollama_ownership::recorded_groups_for_test().is_empty(),
            "nothing provable survived, so the record is dropped"
        );
    }

    // -- slow start: alive but silent (items.id=668) --------------------------

    /// A stand-in for a sidecar that is slow to start (ROCm discovery, say):
    /// it appends its pid to `launches` each time it is started, then stays
    /// alive without ever listening. Its pid is also its process group id.
    #[cfg(unix)]
    fn write_silent_stub(path: &Path, launches: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            path,
            format!("#!/bin/sh\necho $$ >> '{}'\nsleep 30\n", launches.display()),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Kill guards for every group the silent stub started (read back from
    /// its launch log), so a failed assertion cannot leave `sleep 30` behind.
    #[cfg(unix)]
    fn kill_launched(launches: &Path) -> (Vec<KillGroupOnDrop>, usize) {
        let pids: Vec<libc::pid_t> = std::fs::read_to_string(launches)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        let n = pids.len();
        (pids.into_iter().map(KillGroupOnDrop).collect(), n)
    }

    /// A child that stays alive without answering for the whole window is
    /// stopped, and no further port is tried for that binary: only a child
    /// that EXITS means the port is not ours. Exactly one launch, the
    /// remaining ports untouched, and a bystander the test spawned (provable
    /// by name, its own group) is not signalled.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_live_but_silent_sidecar_is_stopped_and_no_other_port_is_tried() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let root = DataRoot::isolated();
        let launches = root.path().join("launches");
        let script = root.path().join("qr-ollama");
        write_silent_stub(&script, &launches);
        let bystander_prog = named_copy(root.path(), "/bin/sleep", "llama-server");
        let bystander = spawn_sleep_stub(&bystander_prog, 0, &[]);
        let ports = vec![free_port(), free_port(), free_port()];
        let mut sidecar = OllamaSidecar::with_ports(ports.clone()).with_ready_probes(1);

        let started = std::time::Instant::now();
        let got = sidecar
            .start_with_candidates(vec![SidecarCandidate::File(script)], root.path())
            .await;
        let (_kill, launched) = kill_launched(&launches);

        assert_eq!(got, Err(StartFailure::TimedOut { port: ports[0] }));
        assert_eq!(launched, 1, "one attempt, the other ports are not tried");
        assert!(sidecar.child.is_none());
        // This stub is a shell script, so its exe is `sh`: not provable, never
        // signalled. It is therefore still running, and its record is kept
        // for the next start to look at (a provable child's is dropped; see
        // the failed-attempt tests above).
        assert_eq!(crate::ollama_ownership::recorded_groups_for_test().len(), 1);
        assert!(!is_effectively_dead(bystander.id()));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the shortened window must keep this test fast: {:?}",
            started.elapsed()
        );
    }

    /// After a timeout the loop goes on to the next candidate binary (on the
    /// same first port, which the silent one never held).
    #[cfg(unix)]
    #[tokio::test]
    async fn after_a_timeout_the_next_binary_is_tried_on_the_first_port() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let root = DataRoot::isolated();
        let launches = root.path().join("launches");
        let silent_dir = root.path().join("silent");
        std::fs::create_dir_all(&silent_dir).unwrap();
        let silent = silent_dir.join("qr-ollama");
        write_silent_stub(&silent, &launches);
        let working = write_stub_server(root.path());
        let ports = vec![free_port(), free_port()];
        let mut sidecar = OllamaSidecar::with_ports(ports.clone()).with_ready_probes(1);

        let got = sidecar
            .start_with_candidates(
                vec![
                    SidecarCandidate::File(silent),
                    SidecarCandidate::File(working),
                ],
                root.path(),
            )
            .await;
        let (_kill_silent, launched) = kill_launched(&launches);
        let _kill_ready = sidecar.child.as_ref().map(|c| KillGroupOnDrop(c.pgid));

        assert_eq!(got, Ok(ports[0]));
        assert_eq!(launched, 1);
    }

    // -- wait_for_ready re-check (items.id=616) ------------------------------

    /// Serves exactly one HTTP request on a fresh loopback port: reads it,
    /// runs `before_reply`, then answers `200 {}`. Returns the port.
    fn one_shot_ok_server(before_reply: impl FnOnce() + Send + 'static) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                before_reply();
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
            }
        });
        port
    }

    /// Something other than our child answers on the port, and our child is
    /// still alive: ready.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn wait_for_ready_accepts_a_2xx_while_the_child_is_alive() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let mut child = crate::ollama_spawn_thread::spawn_with_pdeathsig(
            "/bin/sh",
            vec!["-c".into(), "sleep 30".into()],
            vec![],
        )
        .await
        .expect("spawn should succeed");
        let _kill = KillGroupOnDrop(child.pgid);
        let port = one_shot_ok_server(|| {});

        assert_eq!(
            OllamaSidecar::wait_for_ready(&mut child, port, READY_PROBES).await,
            ReadyOutcome::Ready
        );
    }

    /// The child is alive at the first poll, then dies while the request is
    /// in flight, and only then does the (foreign) listener answer 200. No
    /// clock is involved: the server handler kills the child and blocks in
    /// `waitid(WNOWAIT)` until it has exited (leaving it for `try_wait` to
    /// reap), and only then replies, so the reply always arrives after the
    /// exit. `wait_for_ready` must say not ready.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn wait_for_ready_rejects_a_2xx_that_arrives_after_the_child_exited() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let mut child = crate::ollama_spawn_thread::spawn_with_pdeathsig(
            "/bin/sh",
            vec!["-c".into(), "sleep 30".into()],
            vec![],
        )
        .await
        .expect("spawn should succeed");
        let _kill = KillGroupOnDrop(child.pgid);
        let pid = child.pid;
        // The handler reports its kill/waitid results back here. A panic in
        // the handler thread drops the sender, so `recv` below errors and
        // the test fails instead of passing vacuously on the connection
        // that never got a reply.
        let (tx, rx) = std::sync::mpsc::channel::<(i32, i32)>();
        let port = one_shot_ok_server(move || unsafe {
            let kill_rc = libc::kill(pid as libc::pid_t, libc::SIGKILL);
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let waitid_rc = libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            );
            let _ = tx.send((kill_rc, waitid_rc));
        });

        let ready = OllamaSidecar::wait_for_ready(&mut child, port, READY_PROBES).await;
        let (kill_rc, waitid_rc) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the server handler must have run to completion");
        assert_eq!(kill_rc, 0, "kill failed");
        assert_eq!(waitid_rc, 0, "waitid failed");
        assert_eq!(
            ready,
            ReadyOutcome::Exited,
            "a 2xx from the port must not count once our own child has exited"
        );
    }

    // -- port fallback (items.id=668) ------------------------------------------

    /// A stand-in for `qr-ollama serve`: binds `OLLAMA_HOST`, answers
    /// `/api/tags`, and exits 1 if it cannot bind -- like Ollama does.
    /// Needs `python3`; the test fails loudly if it is missing, never skips.
    #[cfg(unix)]
    fn write_stub_server(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let probe = std::process::Command::new("python3")
            .arg("--version")
            .output();
        assert!(
            probe.is_ok_and(|o| o.status.success()),
            "python3 is required by the sidecar port-fallback tests (a stand-in for \
             `qr-ollama serve` that really binds OLLAMA_HOST); install python3"
        );
        let script = dir.join("qr-ollama");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import os, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

host, port = os.environ["OLLAMA_HOST"].rsplit(":", 1)

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'{"models":[]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass

try:
    server = HTTPServer((host, int(port)), Handler)
except OSError as e:
    print("Error: listen tcp %s:%s: %s" % (host, port, e), file=sys.stderr)
    sys.exit(1)
server.serve_forever()
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// With a listener on the first port, the sidecar comes up on the next.
    #[cfg(unix)]
    #[tokio::test]
    async fn comes_up_on_the_next_port_when_the_first_is_busy() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let root = DataRoot::isolated();
        let stub = write_stub_server(root.path());
        let (_held, busy) = hold_port();
        let free = free_port();
        let mut sidecar = OllamaSidecar::with_ports(vec![busy, free]);

        let got = sidecar
            .start_with_candidates(vec![SidecarCandidate::File(stub)], root.path())
            .await;
        let _kill = sidecar.child.as_ref().map(|c| KillGroupOnDrop(c.pgid));

        assert_eq!(got, Ok(free));
        assert!(sidecar.child.is_some());
    }

    /// Every port busy: an error naming the ports tried, no child held, no
    /// signal sent. A bystander the test itself spawned (provable by name,
    /// in a group of its own) stays alive.
    #[cfg(unix)]
    #[tokio::test]
    async fn all_ports_busy_is_an_error_and_nothing_is_signalled() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let root = DataRoot::isolated();
        let stub = write_stub_server(root.path());
        let bystander_prog = named_copy(root.path(), "/bin/sleep", "llama-server");
        let bystander = spawn_sleep_stub(&bystander_prog, 0, &[]);
        let (_h1, p1) = hold_port();
        let (_h2, p2) = hold_port();
        let (_h3, p3) = hold_port();
        let mut sidecar = OllamaSidecar::with_ports(vec![p1, p2, p3]);

        let got = sidecar
            .start_with_candidates(vec![SidecarCandidate::File(stub)], root.path())
            .await;

        assert_eq!(
            got,
            Err(StartFailure::PortsExhausted {
                tried: vec![p1, p2, p3]
            })
        );
        assert!(sidecar.child.is_none());
        assert!(!is_effectively_dead(bystander.id()));
        assert!(crate::ollama_ownership::recorded_groups_for_test().is_empty());
    }

    /// Answers `200 {}` to every request on a fresh loopback port until the
    /// returned guard drops: a foreign process holding a candidate port.
    struct ForeignOkServer {
        port: u16,
        stop: std::sync::Arc<AtomicBool>,
    }

    impl ForeignOkServer {
        fn start() -> Self {
            use std::io::{Read, Write};
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            std::thread::spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let mut buf = [0u8; 1024];
                            let _ = stream.read(&mut buf);
                            let _ = stream.write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                            );
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            });
            Self { port, stop }
        }
    }

    impl Drop for ForeignOkServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// A foreign listener that answers `/api/tags` with 200 on a candidate
    /// port while our child died on the bind failure is not the sidecar: the
    /// port is rejected and the next one used.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_foreign_listener_answering_while_our_child_died_is_rejected() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let root = DataRoot::isolated();
        let stub = write_stub_server(root.path());
        let foreign = ForeignOkServer::start();
        let free = free_port();
        let mut sidecar = OllamaSidecar::with_ports(vec![foreign.port, free]);

        let got = sidecar
            .start_with_candidates(vec![SidecarCandidate::File(stub)], root.path())
            .await;
        let _kill = sidecar.child.as_ref().map(|c| KillGroupOnDrop(c.pgid));

        assert_eq!(
            got,
            Ok(free),
            "the foreign answer on the first port is not ours"
        );
    }

    // -- wait_for_port_free (items.id=586 fix-up) ----------------------------

    #[tokio::test]
    async fn wait_for_port_free_returns_true_immediately_when_already_free() {
        // Port 0 asks the OS to assign an ephemeral port, bind it, then
        // immediately drop the listener -- the exact port number is then
        // free (vanishingly unlikely to be grabbed by anything else in
        // this test's own brief lifetime).
        let port = free_port();

        let started = std::time::Instant::now();
        assert!(wait_for_port_free(port, Duration::from_secs(3)).await);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "should return almost immediately when the port is already free"
        );
    }

    #[tokio::test]
    async fn wait_for_port_free_detects_release_within_the_timeout() {
        let (listener, port) = hold_port();

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
        let (listener, port) = hold_port();

        let started = std::time::Instant::now();
        assert!(!wait_for_port_free(port, Duration::from_millis(500)).await);
        assert!(started.elapsed() >= Duration::from_millis(500));
        drop(listener);
    }
}
