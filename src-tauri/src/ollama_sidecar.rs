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

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Client;
use tokio::process::{Child, Command};

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

/// Lifecycle manager for the bundled Ollama sidecar.
///
/// One instance lives in `AppState` for the duration of the process,
/// wrapped in `tokio::sync::Mutex` (required because `tokio::process::Child`
/// is not `Sync`). The mutex is held only during startup and shutdown —
/// normal health polling never acquires it.
pub struct OllamaSidecar {
    child: Option<Child>,
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

    /// Warn-only contention check, then unconditionally start QR's own
    /// sidecar on its dedicated port.
    ///
    /// Single public entry point for startup.
    ///
    /// # Order of operations (decisions.id=840)
    /// 1. Probe `http://127.0.0.1:11434/api/tags` with a 2 s timeout —
    ///    purely to set `system_ollama_contention`; never gates step 2.
    /// 2. Start QR's own bundled sidecar from `resource_dir`, always, on
    ///    `127.0.0.1:21434` (`QR_OLLAMA_PORT`).
    /// 3. Poll `127.0.0.1:21434` every 500 ms for up to 5 s →
    ///    `OllamaSource::Sidecar`.
    /// 4. If the sidecar fails to start or become ready →
    ///    `OllamaSource::Unavailable`.
    ///
    /// Must be called from `tauri::Builder::setup()` so the result is
    /// written before any IPC handler can fire.
    pub async fn ensure_available(&mut self, resource_dir: &Path) -> SidecarStartup {
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

        let source = if self.start_sidecar(resource_dir).await {
            log::info!("ollama_sidecar: sidecar ready at 127.0.0.1:{QR_OLLAMA_PORT}");
            OllamaSource::Sidecar
        } else {
            log::warn!("ollama_sidecar: sidecar failed to start or become ready");
            OllamaSource::Unavailable
        };

        SidecarStartup {
            source,
            system_ollama_contention,
        }
    }

    /// Stop the sidecar process if one was started by this manager.
    ///
    /// No-op if the source was `Unavailable` (no child held).
    /// Called on `CloseRequested` from the main window event handler.
    ///
    /// # TODO
    /// Tie to `RunEvent::Exit` for headless or multi-window support.
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            log::info!(
                "ollama_sidecar: stopping bundled sidecar (PID {:?})",
                child.id()
            );
            if let Err(e) = child.kill().await {
                log::warn!("ollama_sidecar: kill failed: {e}");
            }
            let _ = child.wait().await;
            log::info!("ollama_sidecar: sidecar stopped");
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
    async fn start_sidecar(&mut self, resource_dir: &Path) -> bool {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let candidates = candidate_paths(exe_dir.as_deref(), resource_dir, cfg!(debug_assertions));
        let models_dir = crate::providers::utils::get_data_root().join("ollama_models");

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
            if self.try_start(&program, &models_dir).await {
                return true;
            }
        }

        log::warn!("ollama_sidecar: no candidate binary started successfully");
        false
    }

    /// Spawn `program serve` on the dedicated port and wait for readiness.
    /// On success the child is stored for `stop()`. On failure it is killed
    /// and released so the next candidate can bind the port.
    ///
    /// `kill_on_drop(true)` ensures the child is terminated if QR exits
    /// unexpectedly (panic, crash) before `stop()` is called.
    async fn try_start(&mut self, program: &OsStr, models_dir: &Path) -> bool {
        let mut child = match Command::new(program)
            .arg("serve")
            .env("OLLAMA_HOST", format!("127.0.0.1:{QR_OLLAMA_PORT}"))
            .env("OLLAMA_MODELS", models_dir)
            .kill_on_drop(true)
            .spawn()
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
            "ollama_sidecar: {} spawned (PID {:?}) — polling for ready",
            program.to_string_lossy(),
            child.id()
        );

        if Self::wait_for_ready(&mut child).await {
            self.child = Some(child);
            return true;
        }

        // Started but never became ready, or exited early — release it.
        if let Err(e) = child.kill().await {
            log::debug!("ollama_sidecar: cleanup kill failed: {e}");
        }
        let _ = child.wait().await;
        false
    }

    /// Poll 127.0.0.1:{QR_OLLAMA_PORT}/api/tags every 500 ms for up to 5 s
    /// (10 attempts). This is the sidecar's own readiness check — separate
    /// from `detect()`'s 11434 contention probe. Bails out immediately if
    /// the child has already exited (e.g. the placeholder binary).
    async fn wait_for_ready(child: &mut Child) -> bool {
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
}
