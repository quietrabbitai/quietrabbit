//! Where CEF keeps its profile (items.id=622).
//!
//! CEF's Chrome runtime (measured on 151; the bump to 156, items.id=872, kept the
//! same layout in the scratch runs, see that item) keeps a full browser profile under
//! `root_cache_path` (cookies, localStorage, IndexedDB, cache, service workers)
//! whatever `cache_path` says, and with a persistent directory that profile
//! survived sign-out and app restart and handed one user's provider sessions to
//! the next (measured in the items.id=622 step 0 run). So the profile now lives
//! in a directory of its own for each run, on a tmpfs, and is deleted at exit:
//!
//! - `$XDG_RUNTIME_DIR/ai.quietrabbit.app/cef-<pid>-<random>`, else
//!   `/dev/shm/ai.quietrabbit.app-<uid>/cef-<pid>-<random>`, else **no CEF**.
//!   There is no disk fallback: when neither is a writable tmpfs with room,
//!   [`init_run_dir`] fails and Cloud Chat is unavailable for the run (the
//!   items.id=646 unavailable path). Nothing the browser stores reaches a real
//!   disk, and a crash or reboot clears what an exit did not.
//! - The directory is created 0700, its parent is checked to be ours and 0700
//!   (`/dev/shm` is world-writable), and its filesystem must report the tmpfs
//!   magic, not merely live under a path with the right name.
//! - A tmpfs is RAM: the browser's disk and media caches are capped in
//!   `bootstrap.rs`, and a floor of free space is required here. A full tmpfs at
//!   run time is not fatal (measured: panes keep rendering, cache and cookie
//!   file writes fail and are logged); QR's own databases are elsewhere.
//!
//! Also here, the cleanup of what the persistent profile left behind:
//! [`sweep_legacy_profiles`] removes exactly `cloud_chat_gpu_pane_cache` and
//! `tier3_pane_cache.bak-*` under the app data directory, never through a
//! symlink and never while a live process holds the profile's `SingletonLock`;
//! [`report_foreign_cef_dirs`] only logs the default CEF locations
//! (`~/.config/cef_user_data`, `~/.cache/cef_user_data`), which another CEF
//! application may own.

use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Directory name under `$XDG_RUNTIME_DIR` (and the stem of the `/dev/shm`
/// one). The Tauri identifier, as for the app data directory.
const APP_DIR_NAME: &str = "ai.quietrabbit.app";

/// Prefix of a per-run profile directory: `cef-<pid>-<random hex>`.
const RUN_DIR_PREFIX: &str = "cef-";

/// Free space the tmpfs must have before CEF starts. The measured footprint of
/// four providers is about 50 MB uncapped; with the caps in `bootstrap.rs`
/// (64 MiB disk cache, 16 MiB media cache) the ceiling is near 120 MB.
pub const MIN_FREE_BYTES: u64 = 192 * 1024 * 1024;

/// `statfs.f_type` of tmpfs (linux/magic.h TMPFS_MAGIC). ramfs and hugetlbfs
/// are not accepted: ramfs has no size limit.
#[cfg(target_os = "linux")]
const TMPFS_MAGIC: i64 = 0x0102_1994;

/// The legacy profile directory under the app data directory.
const LEGACY_PROFILE_DIR: &str = "cloud_chat_gpu_pane_cache";

/// Name prefix of the pre-items.id=528 backup of the profile.
const LEGACY_BACKUP_PREFIX: &str = "tier3_pane_cache.bak-";

/// Entries a size walk visits before it stops counting (the walk only feeds a
/// log line).
const SIZE_WALK_LIMIT: usize = 200_000;

/// This run's profile directory, set once by [`init_run_dir`].
static RUN_DIR: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
pub enum ProfileDirError {
    /// No candidate location was usable. Carries one reason per candidate.
    #[error("no usable tmpfs for the CEF profile ({})", .0.join("; "))]
    NoTmpfs(Vec<String>),
    #[error("could not create the CEF profile directory: {0}")]
    Create(#[from] io::Error),
    #[error("the CEF profile directory was already set for this run")]
    AlreadySet,
}

/// One place a per-run directory may be made: `parent` is the existing tmpfs
/// directory, `base` the application directory made inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub parent: PathBuf,
    pub base: PathBuf,
}

/// The candidate locations in order of preference. `xdg_runtime_dir` is used
/// only when it is an absolute path.
pub fn candidates(xdg_runtime_dir: Option<&OsStr>, uid: u32) -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(dir) = xdg_runtime_dir {
        let dir = Path::new(dir);
        if dir.is_absolute() {
            out.push(Candidate {
                parent: dir.to_path_buf(),
                base: dir.join(APP_DIR_NAME),
            });
        }
    }
    let shm = Path::new("/dev/shm");
    out.push(Candidate {
        parent: shm.to_path_buf(),
        base: shm.join(format!("{APP_DIR_NAME}-{uid}")),
    });
    out
}

/// `cef-<pid>-<random hex>`.
fn run_dir_name(pid: u32, random: u64) -> String {
    format!("{RUN_DIR_PREFIX}{pid}-{random:016x}")
}

/// The pid in a `cef-<pid>-<random>` name, `None` for any other name.
fn run_dir_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(RUN_DIR_PREFIX)?;
    let (pid, random) = rest.split_once('-')?;
    if random.is_empty() || !random.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    pid.parse().ok()
}

/// Makes this run's directory in the first candidate that passes `check`
/// (called with the candidate's `parent`) and whose base directory is ours and
/// private. Fails closed: when none works the error lists why for each.
pub fn create_run_dir(
    candidates: &[Candidate],
    check: impl Fn(&Path) -> Result<(), String>,
    uid: u32,
    pid: u32,
    random: u64,
) -> Result<PathBuf, ProfileDirError> {
    let mut reasons = Vec::new();
    for candidate in candidates {
        match try_candidate(candidate, &check, uid, pid, random) {
            Ok(dir) => {
                if !reasons.is_empty() {
                    log::warn!(
                        "cloud_chat_gpu_pane: earlier profile locations were not usable ({}); using {dir:?}",
                        reasons.join("; ")
                    );
                }
                return Ok(dir);
            }
            Err(reason) => reasons.push(format!("{}: {reason}", candidate.parent.display())),
        }
    }
    Err(ProfileDirError::NoTmpfs(reasons))
}

fn try_candidate(
    candidate: &Candidate,
    check: &impl Fn(&Path) -> Result<(), String>,
    uid: u32,
    pid: u32,
    random: u64,
) -> Result<PathBuf, String> {
    check(&candidate.parent)?;

    // The base is made 0700 and then verified rather than trusted: /dev/shm is
    // world-writable, so another user could have made the name first, as a
    // directory they own or as a symlink.
    match std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&candidate.base)
    {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("cannot create {}: {e}", candidate.base.display())),
    }
    let meta = std::fs::symlink_metadata(&candidate.base)
        .map_err(|e| format!("cannot stat {}: {e}", candidate.base.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", candidate.base.display()));
    }
    if meta.uid() != uid {
        return Err(format!(
            "{} is owned by another user",
            candidate.base.display()
        ));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{} is accessible to others",
            candidate.base.display()
        ));
    }

    let dir = candidate.base.join(run_dir_name(pid, random));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// The real check for a candidate's parent: an existing directory on a tmpfs
/// with at least [`MIN_FREE_BYTES`] free.
#[cfg(target_os = "linux")]
pub fn check_tmpfs_parent(parent: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(parent).map_err(|e| format!("not usable: {e}"))?;
    if !meta.is_dir() {
        return Err("not a directory".into());
    }
    let (fs_type, free) = statfs(parent).map_err(|e| format!("statfs failed: {e}"))?;
    if fs_type != TMPFS_MAGIC {
        return Err(format!("not a tmpfs (filesystem type {fs_type:#x})"));
    }
    if free < MIN_FREE_BYTES {
        return Err(format!(
            "only {} MiB free, need {} MiB",
            free / (1024 * 1024),
            MIN_FREE_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn check_tmpfs_parent(_parent: &Path) -> Result<(), String> {
    Err("per-run tmpfs profile directories are only implemented on Linux".into())
}

/// `(f_type, bytes available to an unprivileged user)` of the filesystem
/// holding `path`.
#[cfg(target_os = "linux")]
fn statfs(path: &Path) -> io::Result<(i64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call,
    // and `st` is a properly sized, writable `statfs` that the call fills in.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)]
    let free = (st.f_bavail as u64).saturating_mul(st.f_bsize as u64);
    #[allow(clippy::unnecessary_cast)]
    Ok((st.f_type as i64, free))
}

/// Makes this run's profile directory from the environment, records it for
/// [`remove_run_dir`], and returns it. Call once, before `initialize_cef`.
pub fn init_run_dir() -> Result<PathBuf, ProfileDirError> {
    // SAFETY: getuid never fails and has no preconditions.
    let uid = unsafe { libc::getuid() };
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|e| io::Error::other(e.to_string()))?;
    let dir = create_run_dir(
        &candidates(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), uid),
        check_tmpfs_parent,
        uid,
        std::process::id(),
        u64::from_le_bytes(random),
    )?;
    if RUN_DIR.set(dir.clone()).is_err() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(ProfileDirError::AlreadySet);
    }
    Ok(dir)
}

/// Deletes this run's profile directory. Idempotent, best effort, and safe to
/// call when none was made. Call after `cef::shutdown()`, and when
/// `initialize_cef` failed.
pub fn remove_run_dir() {
    if let Some(dir) = RUN_DIR.get() {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => log::info!("cloud_chat_gpu_pane: removed the CEF profile {dir:?}"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                log::warn!("cloud_chat_gpu_pane: could not remove the CEF profile {dir:?}: {e}")
            }
        }
    }
}

/// What a profile directory's `SingletonLock` says about its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    /// No lock: nothing holds the directory.
    Absent,
    /// A lock naming a live process on this host (or one on another host,
    /// which cannot be checked and is treated as live).
    InUse,
    /// A lock naming a process on this host that no longer exists.
    Stale,
}

/// Reads `dir/SingletonLock` without following it: Chromium makes it a
/// symlink whose target is `<hostname>-<pid>`.
pub fn lock_state(dir: &Path, our_host: &str) -> LockState {
    let Ok(target) = std::fs::read_link(dir.join("SingletonLock")) else {
        // Absent, or not a symlink (then it is not Chromium's lock either).
        return match std::fs::symlink_metadata(dir.join("SingletonLock")) {
            Ok(_) => LockState::InUse,
            Err(_) => LockState::Absent,
        };
    };
    lock_state_from_target(&target.to_string_lossy(), our_host, pid_alive)
}

fn lock_state_from_target(target: &str, our_host: &str, alive: impl Fn(u32) -> bool) -> LockState {
    let Some((host, pid)) = target.rsplit_once('-') else {
        return LockState::InUse;
    };
    let Ok(pid) = pid.parse::<u32>() else {
        return LockState::InUse;
    };
    if host != our_host {
        return LockState::InUse;
    }
    if alive(pid) {
        LockState::InUse
    } else {
        LockState::Stale
    }
}

/// Whether `pid` names a process that exists (one we may not signal still
/// exists).
fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists and may be signalled.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn our_host() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// What happened to one directory a sweep looked at.
#[derive(Debug, PartialEq, Eq)]
pub enum SweepAction {
    Removed { bytes: u64 },
    SkippedSymlink,
    SkippedNotDirectory,
    SkippedInUse,
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
pub struct SweepOutcome {
    pub path: PathBuf,
    pub action: SweepAction,
}

/// Removes one directory if it is a real directory (not a symlink, which is
/// never followed or removed) with no live `SingletonLock` owner.
fn sweep_dir(path: &Path, host: &str) -> SweepOutcome {
    let action = match std::fs::symlink_metadata(path) {
        Err(e) => SweepAction::Failed(format!("cannot stat: {e}")),
        Ok(meta) if meta.file_type().is_symlink() => SweepAction::SkippedSymlink,
        Ok(meta) if !meta.is_dir() => SweepAction::SkippedNotDirectory,
        Ok(_) if lock_state(path, host) == LockState::InUse => SweepAction::SkippedInUse,
        Ok(_) => {
            let (bytes, _) = dir_size_bounded(path);
            match std::fs::remove_dir_all(path) {
                Ok(()) => SweepAction::Removed { bytes },
                Err(e) => SweepAction::Failed(e.to_string()),
            }
        }
    };
    SweepOutcome {
        path: path.to_path_buf(),
        action,
    }
}

/// Removes the profile directories the persistent-profile design left under
/// `app_data_dir`: exactly `cloud_chat_gpu_pane_cache` and
/// `tier3_pane_cache.bak-*`, nothing else. Skips a symlink, a non-directory and
/// a profile whose `SingletonLock` names a live process (an older QR build still
/// running on it). Every outcome is logged.
pub fn sweep_legacy_profiles(app_data_dir: &Path) -> Vec<SweepOutcome> {
    sweep_legacy_profiles_for_host(app_data_dir, &our_host())
}

fn sweep_legacy_profiles_for_host(app_data_dir: &Path, host: &str) -> Vec<SweepOutcome> {
    let mut targets = vec![app_data_dir.join(LEGACY_PROFILE_DIR)];
    if let Ok(entries) = std::fs::read_dir(app_data_dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(LEGACY_BACKUP_PREFIX)
            {
                targets.push(entry.path());
            }
        }
    }
    targets.sort();

    let mut outcomes = Vec::new();
    for path in targets {
        if std::fs::symlink_metadata(&path).is_err() {
            continue; // not there: nothing to report
        }
        let outcome = sweep_dir(&path, host);
        match &outcome.action {
            SweepAction::Removed { bytes } => log::info!(
                "cloud_chat_gpu_pane: removed the old CEF profile {path:?} ({} MiB)",
                bytes / (1024 * 1024)
            ),
            SweepAction::Failed(e) => {
                log::warn!(
                    "cloud_chat_gpu_pane: could not remove the old CEF profile {path:?}: {e}"
                )
            }
            other => log::warn!(
                "cloud_chat_gpu_pane: left the old CEF profile {path:?} in place ({other:?})"
            ),
        }
        outcomes.push(outcome);
    }
    outcomes
}

/// Removes per-run directories in `base` whose process is gone (a kill -9 or a
/// crash leaves one; a reboot clears the tmpfs anyway). Our own is never touched.
pub fn sweep_stale_run_dirs(base: &Path, our_pid: u32) -> Vec<SweepOutcome> {
    sweep_stale_run_dirs_with(base, our_pid, pid_alive, &our_host())
}

fn sweep_stale_run_dirs_with(
    base: &Path,
    our_pid: u32,
    alive: impl Fn(u32) -> bool,
    host: &str,
) -> Vec<SweepOutcome> {
    let mut outcomes = Vec::new();
    let Ok(entries) = std::fs::read_dir(base) else {
        return outcomes;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = run_dir_pid(&name) else {
            continue;
        };
        if pid == our_pid || alive(pid) {
            continue;
        }
        let outcome = sweep_dir(&entry.path(), host);
        if let SweepAction::Removed { .. } = outcome.action {
            log::info!(
                "cloud_chat_gpu_pane: removed a stale CEF profile {:?}",
                outcome.path
            );
        }
        outcomes.push(outcome);
    }
    outcomes
}

/// Sweeps stale per-run directories next to this run's own.
pub fn sweep_stale_siblings() {
    if let Some(base) = RUN_DIR.get().and_then(|dir| dir.parent()) {
        sweep_stale_run_dirs(base, std::process::id());
    }
}

/// The default CEF user-data directories that exist, with a bounded size. They
/// are only reported: another CEF application may own them.
pub fn report_foreign_cef_dirs(
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
    xdg_cache_home: Option<&Path>,
) -> Vec<(PathBuf, u64)> {
    let config = xdg_config_home
        .map(Path::to_path_buf)
        .or_else(|| home.map(|h| h.join(".config")));
    let cache = xdg_cache_home
        .map(Path::to_path_buf)
        .or_else(|| home.map(|h| h.join(".cache")));
    let mut found = Vec::new();
    for dir in [config, cache].into_iter().flatten() {
        let path = dir.join("cef_user_data");
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            let (bytes, _) = dir_size_bounded(&path);
            log::warn!(
                "cloud_chat_gpu_pane: found {path:?} ({} MiB), the default CEF profile location. \
                 Quiet Rabbit no longer uses it and leaves it alone; delete it yourself if no other \
                 application needs it",
                bytes / (1024 * 1024)
            );
            found.push((path, bytes));
        }
    }
    found
}

/// Logs the default CEF user-data directories from the process environment.
pub fn report_foreign_cef_dirs_from_env() {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let cache = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from);
    report_foreign_cef_dirs(home.as_deref(), config.as_deref(), cache.as_deref());
}

/// Total size of the files under `path`, never following a symlink, and
/// whether the walk stopped early at [`SIZE_WALK_LIMIT`] entries.
fn dir_size_bounded(path: &Path) -> (u64, bool) {
    let mut total = 0u64;
    let mut seen = 0usize;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > SIZE_WALK_LIMIT {
                return (total, true);
            }
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    (total, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn me() -> u32 {
        // SAFETY: getuid never fails.
        unsafe { libc::getuid() }
    }

    fn ok(_: &Path) -> Result<(), String> {
        Ok(())
    }

    fn candidate_in(dir: &Path) -> Candidate {
        Candidate {
            parent: dir.to_path_buf(),
            base: dir.join("app"),
        }
    }

    #[test]
    fn candidates_prefer_the_runtime_dir_then_dev_shm() {
        let c = candidates(Some(OsStr::new("/run/user/1000")), 1000);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].parent, PathBuf::from("/run/user/1000"));
        assert_eq!(
            c[0].base,
            PathBuf::from("/run/user/1000/ai.quietrabbit.app")
        );
        assert_eq!(c[1].parent, PathBuf::from("/dev/shm"));
        assert_eq!(c[1].base, PathBuf::from("/dev/shm/ai.quietrabbit.app-1000"));
    }

    #[test]
    fn candidates_ignore_an_unset_or_relative_runtime_dir() {
        for value in [None, Some(OsStr::new("relative/run")), Some(OsStr::new(""))] {
            let c = candidates(value, 7);
            assert_eq!(c.len(), 1, "{value:?}");
            assert_eq!(c[0].parent, PathBuf::from("/dev/shm"));
        }
    }

    #[test]
    fn run_dir_names_round_trip_and_reject_other_names() {
        let name = run_dir_name(4242, 0xdead_beef);
        assert_eq!(name, "cef-4242-00000000deadbeef");
        assert_eq!(run_dir_pid(&name), Some(4242));
        for bad in [
            "cef-",
            "cef-12",
            "cef-12-",
            "cef-x-ab",
            "cef-12-zz",
            "tmp-12-ab",
            "cef--ab",
            "Default",
        ] {
            assert_eq!(run_dir_pid(bad), None, "{bad}");
        }
    }

    #[test]
    fn create_run_dir_makes_a_private_directory() {
        let parent = tempfile::tempdir().unwrap();
        let dir = create_run_dir(&[candidate_in(parent.path())], ok, me(), 11, 22).unwrap();
        assert_eq!(
            dir,
            parent.path().join("app").join("cef-11-0000000000000016")
        );
        for p in [dir.as_path(), dir.parent().unwrap()] {
            let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{p:?}");
        }
    }

    #[test]
    fn create_run_dir_fails_closed_and_lists_every_reason() {
        let parent = tempfile::tempdir().unwrap();
        let a = candidate_in(&parent.path().join("a"));
        let b = candidate_in(&parent.path().join("b"));
        let err = create_run_dir(&[a, b], |_| Err("not a tmpfs".into()), me(), 1, 1).unwrap_err();
        let ProfileDirError::NoTmpfs(reasons) = &err else {
            panic!("expected NoTmpfs, got {err:?}");
        };
        assert_eq!(reasons.len(), 2);
        assert!(reasons.iter().all(|r| r.contains("not a tmpfs")));
        assert!(
            !parent.path().join("a").exists(),
            "nothing may be created on a failed check"
        );
        // no candidates at all is also a refusal
        assert!(create_run_dir(&[], ok, me(), 1, 1).is_err());
    }

    #[test]
    fn create_run_dir_falls_through_to_the_next_candidate() {
        let parent = tempfile::tempdir().unwrap();
        let bad = candidate_in(&parent.path().join("missing-parent-check"));
        let good = candidate_in(parent.path());
        let check = |p: &Path| {
            if p == parent.path() {
                Ok(())
            } else {
                Err("no".into())
            }
        };
        let dir = create_run_dir(&[bad, good], check, me(), 5, 6).unwrap();
        assert!(dir.starts_with(parent.path().join("app")));
    }

    #[test]
    fn create_run_dir_refuses_a_base_that_is_not_ours_and_private() {
        let parent = tempfile::tempdir().unwrap();
        let base = parent.path().join("app");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(create_run_dir(&[candidate_in(parent.path())], ok, me(), 1, 1).is_err());

        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            create_run_dir(&[candidate_in(parent.path())], ok, me() + 1, 1, 1).is_err(),
            "a base owned by someone else is refused"
        );
    }

    #[test]
    fn create_run_dir_refuses_a_symlinked_base() {
        let parent = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        symlink(elsewhere.path(), parent.path().join("app")).unwrap();
        assert!(create_run_dir(&[candidate_in(parent.path())], ok, me(), 1, 1).is_err());
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_real_check_rejects_a_directory_that_is_not_a_tmpfs() {
        // The repository checkout is on a disk filesystem on every machine this
        // runs on; a tmpfs checkout would make the assertion vacuous, not wrong.
        let on_disk = Path::new(env!("CARGO_MANIFEST_DIR"));
        let (fs_type, _) = statfs(on_disk).unwrap();
        if fs_type != TMPFS_MAGIC {
            assert!(check_tmpfs_parent(on_disk)
                .unwrap_err()
                .contains("not a tmpfs"));
        }
        assert!(check_tmpfs_parent(Path::new("/definitely/not/here")).is_err());
    }

    #[test]
    fn lock_state_reads_the_symlink_target() {
        let alive = |pid: u32| pid == 100;
        assert_eq!(
            lock_state_from_target("host-100", "host", alive),
            LockState::InUse
        );
        assert_eq!(
            lock_state_from_target("host-200", "host", alive),
            LockState::Stale
        );
        assert_eq!(
            lock_state_from_target("my-host-name-200", "my-host-name", alive),
            LockState::Stale
        );
        // another host, or an unreadable target, cannot be proven stale
        assert_eq!(
            lock_state_from_target("other-200", "host", alive),
            LockState::InUse
        );
        assert_eq!(
            lock_state_from_target("nonsense", "host", alive),
            LockState::InUse
        );
        assert_eq!(
            lock_state_from_target("host-abc", "host", alive),
            LockState::InUse
        );
    }

    #[test]
    fn lock_state_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(lock_state(dir.path(), "h"), LockState::Absent);
        symlink("h-1", dir.path().join("SingletonLock")).unwrap();
        // pid 1 is alive on this host
        assert_eq!(lock_state(dir.path(), "h"), LockState::InUse);
        std::fs::remove_file(dir.path().join("SingletonLock")).unwrap();
        // a regular file named SingletonLock is not Chromium's lock: leave the dir
        std::fs::write(dir.path().join("SingletonLock"), b"x").unwrap();
        assert_eq!(lock_state(dir.path(), "h"), LockState::InUse);
    }

    #[test]
    fn legacy_sweep_removes_exactly_the_two_profile_kinds() {
        let app = tempfile::tempdir().unwrap();
        let p = app.path();
        std::fs::create_dir_all(p.join("cloud_chat_gpu_pane_cache/Default")).unwrap();
        std::fs::write(
            p.join("cloud_chat_gpu_pane_cache/Default/Cookies"),
            vec![0u8; 4096],
        )
        .unwrap();
        std::fs::create_dir_all(p.join("tier3_pane_cache.bak-1786124713")).unwrap();
        std::fs::create_dir_all(p.join("tier3_pane_cache.bak-2")).unwrap();
        // not ours to touch
        for keep in [
            "tier3_pane_cache",
            "cloud_chat_gpu_pane_cache_other",
            "users",
            "models",
            "tier3_pane_cache.bak",
        ] {
            std::fs::create_dir_all(p.join(keep)).unwrap();
            std::fs::write(p.join(keep).join("f"), b"keep").unwrap();
        }
        std::fs::write(p.join("cloud_chat_gpu_pane_cache.txt"), b"keep").unwrap();

        let out = sweep_legacy_profiles_for_host(p, "h");
        assert_eq!(out.len(), 3);
        assert!(
            out.iter()
                .all(|o| matches!(o.action, SweepAction::Removed { .. })),
            "{out:?}"
        );
        assert!(!p.join("cloud_chat_gpu_pane_cache").exists());
        assert!(!p.join("tier3_pane_cache.bak-1786124713").exists());
        assert!(!p.join("tier3_pane_cache.bak-2").exists());
        for keep in [
            "tier3_pane_cache",
            "cloud_chat_gpu_pane_cache_other",
            "users",
            "models",
            "tier3_pane_cache.bak",
        ] {
            assert!(p.join(keep).join("f").exists(), "{keep} must survive");
        }
        assert!(p.join("cloud_chat_gpu_pane_cache.txt").exists());
        let Some(SweepAction::Removed { bytes }) = out
            .iter()
            .find(|o| o.path.ends_with("cloud_chat_gpu_pane_cache"))
            .map(|o| &o.action)
        else {
            panic!("the main profile must be reported as removed");
        };
        assert_eq!(*bytes, 4096);
    }

    #[test]
    fn legacy_sweep_never_follows_or_removes_a_symlink() {
        let app = tempfile::tempdir().unwrap();
        let precious = tempfile::tempdir().unwrap();
        std::fs::write(precious.path().join("data"), b"precious").unwrap();
        symlink(
            precious.path(),
            app.path().join("cloud_chat_gpu_pane_cache"),
        )
        .unwrap();
        symlink(precious.path(), app.path().join("tier3_pane_cache.bak-1")).unwrap();

        let out = sweep_legacy_profiles_for_host(app.path(), "h");
        assert_eq!(out.len(), 2);
        assert!(
            out.iter().all(|o| o.action == SweepAction::SkippedSymlink),
            "{out:?}"
        );
        assert!(
            precious.path().join("data").exists(),
            "the link's target must be untouched"
        );
        assert!(
            std::fs::symlink_metadata(app.path().join("cloud_chat_gpu_pane_cache")).is_ok(),
            "the symlink itself is left in place"
        );
    }

    #[test]
    fn legacy_sweep_skips_a_profile_a_live_process_holds() {
        let app = tempfile::tempdir().unwrap();
        let profile = app.path().join("cloud_chat_gpu_pane_cache");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("Cookies"), b"x").unwrap();
        symlink(
            format!("h-{}", std::process::id()),
            profile.join("SingletonLock"),
        )
        .unwrap();

        let out = sweep_legacy_profiles_for_host(app.path(), "h");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].action, SweepAction::SkippedInUse);
        assert!(profile.join("Cookies").exists());

        // the same lock on a different host cannot be proven stale either
        let out = sweep_legacy_profiles_for_host(app.path(), "another-host");
        assert_eq!(out[0].action, SweepAction::SkippedInUse);
        assert!(profile.join("Cookies").exists());
    }

    #[test]
    fn legacy_sweep_removes_a_profile_with_a_stale_lock() {
        let app = tempfile::tempdir().unwrap();
        let profile = app.path().join("cloud_chat_gpu_pane_cache");
        std::fs::create_dir_all(&profile).unwrap();
        // pid_max is at most 2^22 on Linux, so this pid cannot exist
        symlink("h-4294967294", profile.join("SingletonLock")).unwrap();
        let out = sweep_legacy_profiles_for_host(app.path(), "h");
        assert!(
            matches!(out[0].action, SweepAction::Removed { .. }),
            "{out:?}"
        );
        assert!(!profile.exists());
    }

    #[test]
    fn legacy_sweep_with_nothing_to_do_reports_nothing() {
        let app = tempfile::tempdir().unwrap();
        assert!(sweep_legacy_profiles_for_host(app.path(), "h").is_empty());
        assert!(sweep_legacy_profiles_for_host(&app.path().join("missing"), "h").is_empty());
    }

    #[test]
    fn stale_run_dirs_are_removed_and_live_or_foreign_ones_kept() {
        let base = tempfile::tempdir().unwrap();
        let b = base.path();
        for name in ["cef-10-aa", "cef-20-bb", "cef-30-cc", "notes", "cef-bad"] {
            std::fs::create_dir_all(b.join(name)).unwrap();
            std::fs::write(b.join(name).join("f"), b"x").unwrap();
        }
        // pid 20 is alive, pid 30 is us
        let out = sweep_stale_run_dirs_with(b, 30, |pid| pid == 20, "h");
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(!b.join("cef-10-aa").exists());
        for keep in ["cef-20-bb", "cef-30-cc", "notes", "cef-bad"] {
            assert!(b.join(keep).join("f").exists(), "{keep}");
        }
        assert!(sweep_stale_run_dirs_with(&b.join("missing"), 1, |_| false, "h").is_empty());
    }

    #[test]
    fn foreign_cef_dirs_are_reported_but_never_removed() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join(".config/cef_user_data/Default")).unwrap();
        std::fs::write(h.join(".config/cef_user_data/Default/f"), vec![0u8; 100]).unwrap();
        std::fs::create_dir_all(h.join(".cache/cef_user_data")).unwrap();
        // ~/.local/share/cef is deliberately not looked at
        std::fs::create_dir_all(h.join(".local/share/cef")).unwrap();

        let found = report_foreign_cef_dirs(Some(h), None, None);
        assert_eq!(found.len(), 2);
        assert!(found
            .iter()
            .any(|(p, n)| p.ends_with(".config/cef_user_data") && *n == 100));
        assert!(
            found.iter().all(|(p, _)| p.exists()),
            "report-only: nothing is deleted"
        );
        assert!(h.join(".local/share/cef").exists());

        // XDG overrides win over HOME
        let other = tempfile::tempdir().unwrap();
        assert!(
            report_foreign_cef_dirs(Some(h), Some(other.path()), Some(other.path())).is_empty()
        );
        assert!(report_foreign_cef_dirs(None, None, None).is_empty());
    }

    #[test]
    fn size_walk_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let big = tempfile::tempdir().unwrap();
        std::fs::write(big.path().join("big"), vec![0u8; 10_000]).unwrap();
        std::fs::write(dir.path().join("small"), vec![0u8; 10]).unwrap();
        symlink(big.path(), dir.path().join("link")).unwrap();
        assert_eq!(dir_size_bounded(dir.path()), (10, false));
    }
}
