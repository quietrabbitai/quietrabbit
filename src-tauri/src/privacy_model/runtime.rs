// src-tauri/src/privacy_model/runtime.rs
//
// The process-wide Privacy Filter model installer and the three ways it is
// reached (items.id=678): `init` at startup, `ensure_started` at every login,
// `retry` from the IPC command. The installer itself (install.rs) is a plain
// value that tests build with their own environment; this file holds the one
// production instance and the rules for when it may start.
//
// WHEN IT STARTS. Only in a build that has the Privacy Filter library linked
// (nothing is fetched for a build that cannot use it), and only for the
// installed location under the data root. A developer's override or legacy file
// (debug builds) is already where its owner wants it; fetching 1.6 GB next to
// it would be a surprise.

use std::sync::{Arc, OnceLock};

use super::install::{ActivateFn, Env, InstallState, Installer, ProgressFn};
use super::paths::{self, LocationKind};

static INSTALLER: OnceLock<Arc<Installer>> = OnceLock::new();

/// A progress sink that emits on the `task-progress` event.
pub fn tauri_sink(handle: tauri::AppHandle) -> ProgressFn {
    Arc::new(move |payload| crate::task_progress::emit_progress(&handle, payload))
}

/// Build the production installer. Called once from main.rs's setup, after the
/// data root is recorded. False if one already exists (the first stays).
pub fn init(sink: Option<ProgressFn>) -> bool {
    let root = crate::providers::utils::get_data_root();
    let activate: ActivateFn = Arc::new(crate::conductor::privacy::privacy_filter::reinitialize);
    let installer = Arc::new(Installer::new(Env::production(&root, activate, sink)));
    INSTALLER.set(installer).is_ok()
}

/// What the setup task is doing; `Idle` before `init`.
pub fn current_state() -> InstallState {
    INSTALLER.get().map_or(InstallState::Idle, |i| i.state())
}

/// Stop a running transfer (the partial file is kept). Called when the app
/// exits.
pub fn cancel() {
    if let Some(installer) = INSTALLER.get() {
        installer.cancel();
    }
}

/// Start the setup task unless one is running or the build/location does not
/// call for it. Called at every login, so a failed attempt is retried. Never
/// waits. True when a run was started.
pub fn ensure_started() -> bool {
    match INSTALLER.get() {
        Some(installer) => start_if_applicable(
            installer,
            cfg!(privacy_filter_available),
            paths::current_location().map(|l| l.kind),
        ),
        None => false,
    }
}

pub(crate) fn start_if_applicable(
    installer: &Arc<Installer>,
    compiled_in: bool,
    kind: Option<LocationKind>,
) -> bool {
    compiled_in && kind == Some(LocationKind::Installed) && installer.start()
}

/// Why a retry request did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryRefusal {
    NotCompiledIn,
    DeveloperModel,
    NotInitialised,
}

impl std::fmt::Display for RetryRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotCompiledIn => "the privacy filter is not part of this build",
            Self::DeveloperModel => "a developer model file is in use",
            Self::NotInitialised => "the privacy model setup is not ready",
        })
    }
}

/// The retry command's action: start a run if none is alive. `Ok` also when a
/// run is already alive (the request is idempotent).
pub fn retry() -> Result<(), RetryRefusal> {
    retry_with(
        INSTALLER.get(),
        cfg!(privacy_filter_available),
        paths::current_location().map(|l| l.kind),
    )
}

pub(crate) fn retry_with(
    installer: Option<&Arc<Installer>>,
    compiled_in: bool,
    kind: Option<LocationKind>,
) -> Result<(), RetryRefusal> {
    if !compiled_in {
        return Err(RetryRefusal::NotCompiledIn);
    }
    let Some(installer) = installer else {
        return Err(RetryRefusal::NotInitialised);
    };
    match kind {
        Some(LocationKind::Installed) => {
            installer.start();
            Ok(())
        }
        Some(LocationKind::DevOverride | LocationKind::DevLegacy) => {
            Err(RetryRefusal::DeveloperModel)
        }
        None => Err(RetryRefusal::NotInitialised),
    }
}
