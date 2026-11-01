// src-tauri/src/privacy_model/install.rs
//
// The Privacy Filter model setup task (items.id=678, decisions.id=864): check
// the file at the final path, fetch it when it is not the pinned model, record
// the verified marker, then tell the filter it can load.
//
// ONE INSTALLER, ONE TASK AT A TIME. `Installer::start` is single-flight: while
// a run is alive a second call is a no-op, so a second login or a retry click
// cannot start a second transfer over the same `.part` file.
//
// NO ACCOUNT IS INVOLVED. The model is shared by every account on the install
// and lives under the data root, not under a user. The task touches no account
// database and holds no session secret, so a lock, a logout or a second
// account signing in cannot stop it, and it needs no writer lease. Its entry
// points take no session state at all; a source scan in tests pins that.
//
// ORDER OF A RUN:
//   1. cheap check (stat only). A valid marker goes straight to step 5.
//   2. a file of the right size with no valid marker is hashed (`verifying`);
//      a wrong file is deleted. A wrong size is deleted without hashing.
//   3. free-disk check (`Failed(LowDisk)` before any request).
//   4. fetch: QR release asset first, upstream revision second, each checked
//      against the same size and SHA-256 by pinned_asset (`downloading`).
//   5. activate: the filter is told it may load (`complete`).
//
// PROGRESS goes out on the existing `task-progress` event with kind
// `privacy_model`. The total is the pinned size, known from the start, so a
// percentage never needs the server's Content-Length.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use super::{marker, paths, pin};
use crate::pinned_asset::{
    self, Expect, FetchError, PinnedFetcher, GITHUB_RELEASE_REDIRECT_HOSTS,
    HUGGINGFACE_REDIRECT_HOSTS,
};
use crate::task_progress::{ProgressThrottle, TaskProgressPayload};

/// `TaskProgressPayload::kind` of every event this task emits.
pub const PROGRESS_KIND: &str = "privacy_model";

/// Below this much free space after an I/O error, the error is a full disk.
const DISK_FULL_THRESHOLD: u64 = 16 * 1024 * 1024;

pub type ProgressFn = Arc<dyn Fn(&TaskProgressPayload) + Send + Sync>;
/// Tells the filter it may (re)load; true when it now has a loaded model.
pub type ActivateFn = Arc<dyn Fn() -> bool + Send + Sync>;
pub type DiskFn = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;
pub type HashFn = Arc<dyn Fn(&Path) -> std::io::Result<[u8; 32]> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// No source delivered the file (offline, refused, a bad status).
    DownloadFailed,
    /// Not enough free space, before or during the transfer.
    LowDisk,
    /// Bytes arrived but did not match the pinned size and hash, or the file
    /// at the final path was not the model.
    VerificationFailed,
    /// The file is in place and verified but the filter could not load it, or
    /// the files could not be read or written.
    ModelUnavailable,
}

impl FailReason {
    /// Stable code carried as the final event's `message`. Not display text.
    pub fn code(self) -> &'static str {
        match self {
            Self::DownloadFailed => "download_failed",
            Self::LowDisk => "low_disk",
            Self::VerificationFailed => "verification_failed",
            Self::ModelUnavailable => "model_unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallState {
    /// No run in this process, or the last one was cancelled.
    Idle,
    /// A run is alive and is only looking (stat, or telling the filter to
    /// load). Nothing user-visible.
    Checking,
    /// A run is hashing the file at the final path.
    Verifying,
    /// A run is fetching; `current` bytes are in the partial file.
    Downloading {
        current: u64,
    },
    /// The model is verified and the filter loaded it.
    Installed,
    Failed(FailReason),
}

impl InstallState {
    pub fn is_running(self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Verifying | Self::Downloading { .. }
        )
    }
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Installed,
    Failed(FailReason),
    Cancelled,
    /// Another run was alive; nothing was done.
    AlreadyRunning,
}

/// Everything a run touches outside itself, so tests supply their own.
pub struct Env {
    pub models_dir: PathBuf,
    /// Tried in order.
    pub sources: Vec<String>,
    pub expect: Expect,
    /// Free space required beyond the bytes still to fetch.
    pub disk_margin: u64,
    pub fetcher: PinnedFetcher,
    pub available_space: DiskFn,
    pub hash: HashFn,
    pub activate: ActivateFn,
    pub sink: Option<ProgressFn>,
}

/// The pinned size and hash as pinned_asset's expectation.
pub fn pin_expect() -> Expect {
    Expect::from_hex(pin::SIZE, pin::SHA256_HEX).expect("pin.rs holds a valid SHA-256")
}

impl Env {
    /// The production environment under `data_root`. `activate` is how the
    /// filter is told to load; `sink` carries progress to the webview.
    pub fn production(data_root: &Path, activate: ActivateFn, sink: Option<ProgressFn>) -> Self {
        // One fetcher serves both sources, so its redirect list is the union.
        // Every body is hash-pinned, so a wider list costs availability at
        // worst (see pinned_asset's header).
        let hosts: Vec<&str> = GITHUB_RELEASE_REDIRECT_HOSTS
            .iter()
            .chain(HUGGINGFACE_REDIRECT_HOSTS.iter())
            .copied()
            .collect();
        Self {
            models_dir: paths::models_dir_in(data_root),
            sources: vec![pin::release_asset_url(), pin::upstream_url()],
            expect: pin_expect(),
            disk_margin: pin::DISK_MARGIN,
            fetcher: PinnedFetcher::new(&hosts),
            available_space: Arc::new(crate::auth::key_rotation_io::available_space),
            hash: Arc::new(marker::hash_file),
            activate,
            sink,
        }
    }

    fn model_path(&self) -> PathBuf {
        self.models_dir.join(pin::FILE_NAME)
    }
}

pub struct Installer {
    env: Env,
    state: Mutex<InstallState>,
    cancel: CancellationToken,
}

impl Installer {
    pub fn new(env: Env) -> Self {
        Self {
            env,
            state: Mutex::new(InstallState::Idle),
            cancel: CancellationToken::new(),
        }
    }

    pub fn state(&self) -> InstallState {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stop a running transfer (the partial file is kept). Used when the app
    /// exits; there is no user-facing cancel because the model is required.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    fn set(&self, next: InstallState) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = next;
    }

    /// Claim the single flight: true when no run was alive and one may start.
    fn try_begin(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_running() {
            return false;
        }
        *state = InstallState::Checking;
        true
    }

    /// Start a run in the background under task supervision. False when a run
    /// is already alive (nothing started).
    pub fn start(self: &Arc<Self>) -> bool {
        if !self.try_begin() {
            return false;
        }
        let me = Arc::clone(self);
        crate::task_supervision::spawn_supervised("privacy_model_setup", false, move || {
            let me = Arc::clone(&me);
            async move {
                me.run_claimed().await;
            }
        });
        true
    }

    /// One run, awaited. Tests and `start` use it.
    pub async fn run(&self) -> Outcome {
        if !self.try_begin() {
            return Outcome::AlreadyRunning;
        }
        self.run_claimed().await
    }

    async fn run_claimed(&self) -> Outcome {
        // A panic or a dropped future must not leave the single flight held.
        let mut guard = RunGuard {
            installer: self,
            finished: false,
        };
        let mut emitter = Emitter::new(self.env.sink.clone(), uuid::Uuid::new_v4().to_string());
        let outcome = self.run_inner(&mut emitter).await;
        guard.finished = true;
        outcome
    }

    async fn run_inner(&self, emitter: &mut Emitter) -> Outcome {
        let model = self.env.model_path();
        let marker_path = paths::marker_path_for(&model);
        let expect = self.env.expect.clone();

        // 1-2: the cheap check, then hash only when the marker cannot vouch.
        match marker::check(&model, &marker_path, &expect) {
            marker::Check::Valid => return self.activate(emitter).await,
            marker::Check::Missing => {}
            marker::Check::WrongSize | marker::Check::Stale => {
                self.set(InstallState::Verifying);
                emitter.emit("verifying", None, None, None);
                let (m, mk, ex, hash) = (
                    model.clone(),
                    marker_path.clone(),
                    expect.clone(),
                    Arc::clone(&self.env.hash),
                );
                let verified = tokio::task::spawn_blocking(move || {
                    marker::verify_existing(&m, &mk, &ex, &*hash)
                })
                .await;
                match verified {
                    Ok(Ok(marker::Verified::Verified)) => return self.activate(emitter).await,
                    Ok(Ok(marker::Verified::Missing | marker::Verified::Rejected)) => {}
                    Ok(Err(e)) => {
                        log::warn!("privacy_model: could not verify the model file: {e}");
                        return self.fail(emitter, FailReason::ModelUnavailable);
                    }
                    Err(e) => {
                        log::warn!("privacy_model: the verify task failed: {e}");
                        return self.fail(emitter, FailReason::ModelUnavailable);
                    }
                }
            }
        }

        // 3: free space, before any request.
        if let Err(e) = tokio::fs::create_dir_all(&self.env.models_dir).await {
            log::warn!("privacy_model: could not create the models directory: {e}");
            return self.fail(emitter, FailReason::DownloadFailed);
        }
        let part = pinned_asset::part_path(&model);
        let have = match tokio::fs::metadata(&part).await {
            Ok(m) if m.len() <= expect.size => m.len(),
            _ => 0,
        };
        let needed = (expect.size - have).saturating_add(self.env.disk_margin);
        match (self.env.available_space)(&self.env.models_dir) {
            Some(free) if free < needed => {
                log::warn!("privacy_model: {free} bytes free, {needed} needed");
                return self.fail(emitter, FailReason::LowDisk);
            }
            Some(_) => {}
            None => log::info!("privacy_model: free space unknown, proceeding"),
        }

        // 4: fetch.
        self.set(InstallState::Downloading { current: have });
        emitter.emit("downloading", Some(have), Some(expect.size), None);
        let result = {
            let state = &self.state;
            let mut progress = |current: u64, total: u64| {
                *state.lock().unwrap_or_else(|e| e.into_inner()) =
                    InstallState::Downloading { current };
                emitter.emit_throttled("downloading", current, total);
            };
            let sources: Vec<&str> = self.env.sources.iter().map(String::as_str).collect();
            self.env
                .fetcher
                .fetch_to_file(&sources, &expect, &model, &self.cancel, &mut progress)
                .await
        };
        match result {
            Ok(()) => {}
            Err(FetchError::Cancelled) => {
                self.set(InstallState::Idle);
                emitter.finish("cancelled", None);
                return Outcome::Cancelled;
            }
            Err(e) => {
                log::warn!("privacy_model: no source delivered the model: {e}");
                let free = (self.env.available_space)(&self.env.models_dir);
                return self.fail(emitter, classify_fetch_error(&e, free));
            }
        }

        // pinned_asset hashed the bytes as they arrived, so the marker is
        // recorded from the file's metadata without hashing again.
        if let Err(e) = marker::record(&model, &marker_path, &expect) {
            log::warn!("privacy_model: could not record the verified marker: {e}");
            return self.fail(emitter, FailReason::ModelUnavailable);
        }
        self.activate(emitter).await
    }

    async fn activate(&self, emitter: &mut Emitter) -> Outcome {
        let activate = Arc::clone(&self.env.activate);
        let loaded = tokio::task::spawn_blocking(move || activate())
            .await
            .unwrap_or(false);
        if loaded {
            self.set(InstallState::Installed);
            emitter.finish("complete", None);
            Outcome::Installed
        } else {
            self.fail(emitter, FailReason::ModelUnavailable)
        }
    }

    fn fail(&self, emitter: &mut Emitter, reason: FailReason) -> Outcome {
        self.set(InstallState::Failed(reason));
        emitter.finish("failed", Some(reason.code()));
        Outcome::Failed(reason)
    }
}

/// Maps the error pinned_asset returns when every source failed. `free` is the
/// free space measured after the failure: an I/O error with almost nothing
/// free is a full disk (pinned_asset reports I/O errors as text, so the kind is
/// not available here).
pub(crate) fn classify_fetch_error(e: &FetchError, free: Option<u64>) -> FailReason {
    match e {
        FetchError::HashMismatch | FetchError::SizeMismatch { .. } => {
            FailReason::VerificationFailed
        }
        FetchError::Io(_) if free.is_some_and(|f| f < DISK_FULL_THRESHOLD) => FailReason::LowDisk,
        _ => FailReason::DownloadFailed,
    }
}

struct RunGuard<'a> {
    installer: &'a Installer,
    finished: bool,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            log::error!("privacy_model: the setup run ended without a result");
            let mut state = self
                .installer
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if state.is_running() {
                *state = InstallState::Failed(FailReason::DownloadFailed);
            }
        }
    }
}

struct Emitter {
    sink: Option<ProgressFn>,
    task_id: String,
    throttle: ProgressThrottle,
}

impl Emitter {
    fn new(sink: Option<ProgressFn>, task_id: String) -> Self {
        Self {
            sink,
            task_id,
            throttle: ProgressThrottle::new(),
        }
    }

    fn emit(&self, phase: &str, current: Option<u64>, total: Option<u64>, message: Option<&str>) {
        if let Some(sink) = &self.sink {
            sink(&TaskProgressPayload {
                task_id: self.task_id.clone(),
                kind: PROGRESS_KIND.to_owned(),
                phase: phase.to_owned(),
                current,
                total,
                message: message.map(str::to_owned),
            });
        }
    }

    fn emit_throttled(&mut self, phase: &str, current: u64, total: u64) {
        if self.throttle.should_emit(&self.task_id, current, total) {
            self.emit(phase, Some(current), Some(total), None);
        }
    }

    /// The last event of a run: authoritative, never throttled.
    fn finish(&mut self, phase: &str, message: Option<&str>) {
        self.throttle.clear(&self.task_id);
        self.emit(phase, None, None, message);
    }
}
