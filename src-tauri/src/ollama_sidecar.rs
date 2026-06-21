//! Ollama sidecar lifecycle manager.
//!
//! At app startup, `ensure_available()` detects whether a system Ollama
//! instance is already running at 127.0.0.1:11434. If found, it returns
//! `OllamaSource::System` and no sidecar is started. If not found, it
//! starts the bundled Ollama binary and returns `OllamaSource::Sidecar`
//! (or `OllamaSource::Unavailable` if startup fails).
//!
//! No caller outside this module ever invokes `tokio::process::Command`
//! directly — all process management is encapsulated here.
//!
//! # Binary bundling (build pipeline note — D6-353)
//! The Tauri bundler packages the binary listed in `tauri.conf.json`
//! `externalBin` with the target triple appended:
//!   src-tauri/binaries/ollama-{target-triple}
//! Example on Garuda: `ollama-x86_64-unknown-linux-gnu`
//! At runtime the binary is resolved via the Tauri resource directory.
//! See `sidecar_binary_name()` for platform-specific naming.

use std::path::Path;
use std::time::Duration;

use reqwest::Client;
use tokio::process::{Child, Command};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Where Ollama is being served from during this session.
///
/// Written once in `tauri::Builder::setup()`, read frequently by
/// `get_health()`. Serialized to IPC strings only in `HealthResponse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OllamaSource {
    /// A pre-existing system Ollama was detected at 127.0.0.1:11434.
    System,
    /// No system Ollama found; the bundled sidecar was started.
    Sidecar,
    /// Neither system Ollama nor sidecar is available.
    Unavailable,
    // TODO (post-Release 1): add Detecting variant to distinguish
    // "detection in progress" from "detection complete, nothing found".
    // Requires frontend handling of the transient state.
}

impl OllamaSource {
    /// IPC-safe string for `HealthResponse.ollama_source`.
    pub fn as_str(&self) -> &'static str {
        match self {
            OllamaSource::System => "system",
            OllamaSource::Sidecar => "sidecar",
            OllamaSource::Unavailable => "unavailable",
        }
    }
}

/// Internal result of the detection probe. Not exposed to callers.
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

    /// Detect system Ollama or start the bundled sidecar.
    ///
    /// Single public entry point for startup. Returns the source that
    /// will serve Ollama requests for this session.
    ///
    /// # Order of operations
    /// 1. Probe `http://127.0.0.1:11434/api/tags` with a 2 s timeout.
    /// 2. If found → `OllamaSource::System` (no sidecar started).
    /// 3. If not found → start bundled sidecar from `resource_dir`.
    /// 4. Poll every 500 ms for up to 5 s → `OllamaSource::Sidecar`.
    /// 5. If sidecar fails to start or become ready → `OllamaSource::Unavailable`.
    ///
    /// Must be called from `tauri::Builder::setup()` so the source is
    /// written before any IPC handler can fire.
    pub async fn ensure_available(&mut self, resource_dir: &Path) -> OllamaSource {
        match self.detect().await {
            DetectionResult::SystemOllama => {
                log::info!("ollama_sidecar: system Ollama detected at 127.0.0.1:11434");
                OllamaSource::System
            }
            DetectionResult::NotFound => {
                log::info!("ollama_sidecar: no system Ollama found — starting bundled sidecar");
                if self.start_sidecar(resource_dir).await {
                    log::info!("ollama_sidecar: sidecar ready at 127.0.0.1:11434");
                    OllamaSource::Sidecar
                } else {
                    log::warn!("ollama_sidecar: sidecar failed to start or become ready");
                    OllamaSource::Unavailable
                }
            }
        }
    }

    /// Stop the sidecar process if one was started by this manager.
    ///
    /// No-op if the source was `System` or `Unavailable` (no child held).
    /// Called on `CloseRequested` from the main window event handler.
    ///
    /// # TODO
    /// Tie to `RunEvent::Exit` for headless or multi-window support.
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            log::info!("ollama_sidecar: stopping bundled sidecar (PID {:?})", child.id());
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
        let client = match Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
        {
            Ok(c) => c,
            Err(_) => return DetectionResult::NotFound,
        };

        match client
            .get("http://127.0.0.1:11434/api/tags")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => DetectionResult::SystemOllama,
            _ => DetectionResult::NotFound,
        }
    }

    /// Start the bundled Ollama binary from the Tauri resource directory.
    ///
    /// Returns `true` if the sidecar spawned and became ready within 5 s.
    ///
    /// `OLLAMA_MODELS` is set to `~/.ollama/models` so the sidecar shares
    /// model weights with any previous system Ollama install — no duplicate
    /// downloads. (D6-353)
    ///
    /// `kill_on_drop(true)` ensures the child is terminated if QR exits
    /// unexpectedly (panic, crash) before `stop()` is called.
    async fn start_sidecar(&mut self, resource_dir: &Path) -> bool {
        let binary = resource_dir.join(sidecar_binary_name());

        if !binary.exists() {
            log::warn!(
                "ollama_sidecar: bundled binary not found at {}",
                binary.display()
            );
            return false;
        }

        let models_dir = home_dir().join(".ollama").join("models");

        let child = match Command::new(&binary)
            .env("OLLAMA_HOST", "127.0.0.1:11434")
            .env("OLLAMA_MODELS", &models_dir)
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                log::warn!("ollama_sidecar: failed to spawn: {e}");
                return false;
            }
        };

        log::info!(
            "ollama_sidecar: sidecar spawned (PID {:?}) — polling for ready",
            child.id()
        );
        self.child = Some(child);

        if self.wait_for_ready().await {
            true
        } else {
            // Sidecar started but did not become ready — terminate and release.
            // TODO: add early-exit detection via Child::try_wait() (post-Release 1).
            if let Some(mut child) = self.child.take() {
                if let Err(e) = child.kill().await {
                    log::warn!("ollama_sidecar: cleanup kill failed: {e}");
                }
                let _ = child.wait().await;
            }
            false
        }
    }

    /// Poll 127.0.0.1:11434/api/tags every 500 ms for up to 5 s (10 attempts).
    async fn wait_for_ready(&self) -> bool {
        let client = match Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
        {
            Ok(c) => c,
            Err(_) => return false,
        };

        for attempt in 1u8..=10 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            match client
                .get("http://127.0.0.1:11434/api/tags")
                .send()
                .await
            {
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

/// Resolve the conventional user home directory used for locating
/// Ollama's default model store (`~/.ollama/models`).
///
/// Checks `HOME` (POSIX) then `USERPROFILE` (Windows). This intentionally
/// avoids a dependency on platform- or Tauri-specific path APIs, keeping
/// this module free of Tauri types and independently testable.
///
/// Falls back to `/tmp` if neither variable is set — should not occur
/// in practice on Linux, macOS, or Windows.
fn home_dir() -> std::path::PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            log::warn!("ollama_sidecar: HOME/USERPROFILE not set — using /tmp as fallback");
            std::path::PathBuf::from("/tmp")
        })
}
