//! Ownership proof and orphan reclaim for QR's own Ollama sidecar and its
//! runner subprocesses (items.id=586).
//!
//! Two independent proofs, combined by every caller that actually touches a
//! process -- neither is sufficient alone:
//!
//! - [`is_qr_owned`]: is this process provably QR's own (its `OLLAMA_MODELS`
//!   environment variable, or a command-line argument, points at QR's own
//!   `ollama_models` directory)? Verified empirically this session: the
//!   `serve` process carries `OLLAMA_MODELS` in its environment; a runner
//!   (`llama-server`) does not reliably inherit that but always carries
//!   `--model <path-under-ollama_models/blobs/>` directly on its command
//!   line, so checking both covers either kind of process with one
//!   function.
//! - [`is_orphaned`]: is this process's parent gone, a zombie, or simply not
//!   QR's own running executable? There is no single-instance guard on QR
//!   itself, so two live QR instances can legitimately share one data dir,
//!   each with its own live sidecar -- `is_qr_owned` alone cannot tell that
//!   case apart from a genuine leftover orphan. Only a process that is
//!   *both* QR-owned *and* orphaned is ever reclaimed; a provably-QR
//!   process whose parent is a live QR executable is left alone.
//!
//! Process-group identification deliberately does NOT use `sysinfo::
//! Process::group_id()` -- despite its doc comment ("the process group ID
//! of the process"), reading this crate's own source (`unix/linux/process.
//! rs::refresh_user_group_ids`, `unix/apple/macos/process.rs`) shows it is
//! actually populated from `/proc/pid/status`'s `Gid:` line / `pbi_rgid` --
//! the Unix *user* group ID, not the process group (`pgid`) `setpgid`/`kill
//! -<pgid>` deal in. `libc::getpgid` is used directly instead.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

// ---------------------------------------------------------------------------
// Identity proof
// ---------------------------------------------------------------------------

/// True iff `pid` is alive and provably QR's own: its environment has
/// `OLLAMA_MODELS` equal to `models_dir` (how `serve` is spawned -- see
/// `ollama_sidecar::try_start`), or one of its command-line arguments is a
/// path under `models_dir` (how a runner receives its model file, observed
/// directly this session: `llama-server --model <models_dir>/blobs/...`).
pub fn is_qr_owned(pid: Pid, models_dir: &Path, sys: &System) -> bool {
    let Some(proc_) = sys.process(pid) else {
        return false;
    };

    let env_match = proc_.environ().iter().any(|e| {
        e.to_str()
            .and_then(|s| s.strip_prefix("OLLAMA_MODELS="))
            .map(|v| Path::new(v) == models_dir)
            .unwrap_or(false)
    });
    if env_match {
        return true;
    }

    proc_
        .cmd()
        .iter()
        .any(|arg| Path::new(arg).starts_with(models_dir))
}

/// True iff `pid` should be treated as an orphan safe to reclaim: its
/// parent no longer exists, is a zombie, or exists but is not `qr_exe_path`
/// (i.e. it has been reparented away from a live QR process to `init`/the
/// subreaper/a shell -- the real QR parent is gone). False -- *not*
/// orphaned, never reclaim -- whenever the parent is alive and matches
/// `qr_exe_path`, since that's a live sibling QR instance's own sidecar,
/// and also, deliberately conservative, whenever the parent is alive but
/// its own executable path couldn't be determined (permission or timing
/// issue): "uncertain" must never resolve to "safe to kill" here.
pub fn is_orphaned(pid: Pid, qr_exe_path: &Path, sys: &System) -> bool {
    let Some(proc_) = sys.process(pid) else {
        return true;
    };
    let Some(parent_pid) = proc_.parent() else {
        return true;
    };
    let Some(parent) = sys.process(parent_pid) else {
        return true;
    };
    if parent.status() == ProcessStatus::Zombie {
        return true;
    }
    match parent.exe() {
        Some(exe) => exe != qr_exe_path,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Process group
// ---------------------------------------------------------------------------

/// The real process group ID of `pid`, via `getpgid` -- not `sysinfo`'s
/// `group_id()` (see module doc comment for why). `None` if the process is
/// already gone or `getpgid` otherwise fails.
fn current_pgid(pid: Pid) -> Option<libc::pid_t> {
    let raw = unsafe { libc::getpgid(pid.as_u32() as libc::pid_t) };
    if raw < 0 {
        None
    } else {
        Some(raw)
    }
}

/// Every live pid currently in process group `pgid`, per a fresh query of
/// each process in `sys` (not cached group membership -- group membership
/// can change at any time via `setpgid`, though nothing in this codebase
/// does that for a sidecar/runner after spawn).
fn group_members(pgid: libc::pid_t, sys: &System) -> Vec<Pid> {
    sys.processes()
        .keys()
        .copied()
        .filter(|&pid| current_pgid(pid) == Some(pgid))
        .collect()
}

/// `SIGTERM` then `SIGKILL`, but never wider than provably QR's own.
///
/// Refuses to signal process group 1 (init) or this process's own group
/// outright. Beyond that: enumerates every live member of `pgid` and
/// checks each against [`is_qr_owned`] (`models_dir`) before touching
/// anything. Only when *every* member passes does this signal the whole
/// group by pgid, the cheap/simple path. A pre-fix orphan can share its
/// group with the shell/terminal/`cargo-tauri` that launched it -- that's
/// exactly today's real shape (`ollama serve` PID 204157 sharing pgid
/// 203739 with its whole launching shell tree) -- so a group containing
/// even one process that isn't provably QR's own is never groupkilled;
/// only the individually-proven members are signaled directly, one PID at
/// a time, and everything else in that group is left alone.
pub async fn kill_process_group(pgid: libc::pid_t, models_dir: &Path) {
    let own_pgid = unsafe { libc::getpgid(0) };
    if pgid <= 1 || pgid == own_pgid {
        log::warn!(
            "ollama_ownership: refusing to signal process group {pgid} (own group {own_pgid})"
        );
        return;
    }

    let mut sys = System::new_all();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    let members = group_members(pgid, &sys);
    if members.is_empty() {
        return;
    }
    let owned: Vec<Pid> = members
        .iter()
        .copied()
        .filter(|&pid| is_qr_owned(pid, models_dir, &sys))
        .collect();

    if owned.len() == members.len() {
        log::info!("ollama_ownership: SIGTERM to process group {pgid}");
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
    } else {
        log::warn!(
            "ollama_ownership: process group {pgid} has {} member(s) not provably QR's own -- \
             signaling only the {} proven-QR member(s) individually, never the group",
            members.len() - owned.len(),
            owned.len()
        );
        for &pid in &owned {
            unsafe {
                libc::kill(pid.as_u32() as libc::pid_t, libc::SIGTERM);
            }
        }
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;

    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    let survivors: Vec<Pid> = owned
        .iter()
        .copied()
        .filter(|&pid| sys.process(pid).is_some())
        .collect();
    if !survivors.is_empty() {
        log::warn!(
            "ollama_ownership: {} proven-QR process(es) survived SIGTERM, sending SIGKILL",
            survivors.len()
        );
        for &pid in &survivors {
            unsafe {
                libc::kill(pid.as_u32() as libc::pid_t, libc::SIGKILL);
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

// ---------------------------------------------------------------------------
// Pidfile
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PidfileRecord {
    pid: u32,
    /// `sysinfo::Process::start_time()` at write time -- guards against a
    /// PID being reused by an unrelated process across a reboot before this
    /// pidfile is next read.
    start_time: u64,
}

pub fn pidfile_path() -> PathBuf {
    crate::providers::utils::get_data_root().join("ollama_sidecar.pid")
}

/// Best-effort write -- a failure here only means the next startup falls
/// back to the broad sweep instead of the pidfile-targeted pass, not a
/// reason to fail the sidecar startup that's already succeeded. Builds its
/// own `System` snapshot (called once per successful startup, not on any
/// hot path) rather than asking the caller for one.
pub fn write_pidfile(pid: u32) {
    let mut sys = System::new_all();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::everything(),
    );
    let Some(proc_) = sys.process(Pid::from_u32(pid)) else {
        log::warn!(
            "ollama_ownership: cannot write pidfile -- pid {pid} not found right after spawn"
        );
        return;
    };
    let record = PidfileRecord {
        pid,
        start_time: proc_.start_time(),
    };
    let path = pidfile_path();
    let write_result: io::Result<()> = serde_json::to_vec(&record)
        .map_err(io::Error::other)
        .and_then(|bytes| std::fs::write(&path, bytes));
    if let Err(e) = write_result {
        log::warn!("ollama_ownership: failed to write pidfile {path:?}: {e}");
    }
}

/// `None` on any error (missing file, corrupt content) -- never panics;
/// a missing/corrupt pidfile just means the pidfile-targeted reclaim pass
/// finds nothing and the broad sweep is relied on instead.
fn read_pidfile() -> Option<PidfileRecord> {
    let bytes = std::fs::read(pidfile_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn remove_pidfile() {
    let path = pidfile_path();
    if let Err(e) = std::fs::remove_file(&path) {
        if e.kind() != io::ErrorKind::NotFound {
            log::debug!("ollama_ownership: could not remove pidfile {path:?}: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Reclaim
// ---------------------------------------------------------------------------

/// Two passes, in order, both gated on `is_qr_owned(..) && is_orphaned(..)`
/// -- never ownership proof alone (see module doc comment):
///
/// 1. Pidfile-targeted: the one process this QR data dir's own last sidecar
///    recorded, if its `start_time` still matches (not a reused PID).
/// 2. Broad sweep: every currently running process, catching an orphaned
///    runner left behind with no live `serve` parent at all -- the
///    pidfile alone only ever tracked `serve`, not its runners.
///
/// Always removes the pidfile afterward -- a fresh sidecar is about to
/// start and will write its own once ready, regardless of whether this
/// call found anything to reclaim.
/// The pidfile pass's reclaim condition, extracted so it's testable on its
/// own -- in particular the stale-`start_time` (PID reused by an unrelated
/// process since the file was written) case, which the broad sweep that
/// always runs afterward would otherwise independently also catch (it does
/// not key off the pidfile at all), making that case unobservable through
/// `reclaim_orphans` alone.
fn pidfile_target_is_reclaimable(
    record: &PidfileRecord,
    models_dir: &Path,
    qr_exe_path: &Path,
    sys: &System,
) -> bool {
    let pid = Pid::from_u32(record.pid);
    sys.process(pid)
        .map(|p| p.start_time() == record.start_time)
        .unwrap_or(false)
        && is_qr_owned(pid, models_dir, sys)
        && is_orphaned(pid, qr_exe_path, sys)
}

/// Returns `true` iff this call actually reclaimed something -- callers
/// use that to decide whether to wait for the port to free up and retry
/// the fresh start (items.id=586 fix-up: a group-kill that spared an
/// unrelated process, or any kill at all, can leave the OS a brief moment
/// before `QR_OLLAMA_PORT` is actually rebindable; confirmed empirically
/// this session, "listen tcp 127.0.0.1:21434: bind: address already in
/// use" on the very next start attempt).
pub async fn reclaim_orphans(models_dir: &Path, qr_exe_path: &Path) -> bool {
    let mut killed_anything = false;
    let mut sys = System::new_all();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );

    if let Some(record) = read_pidfile() {
        let pid = Pid::from_u32(record.pid);
        if pidfile_target_is_reclaimable(&record, models_dir, qr_exe_path, &sys) {
            if let Some(pgid) = current_pgid(pid) {
                log::info!(
                    "ollama_ownership: reclaiming pidfile-tracked orphan pid {} (pgid {pgid})",
                    record.pid
                );
                kill_process_group(pgid, models_dir).await;
                killed_anything = true;
            }
        }
    }
    remove_pidfile();

    // Re-snapshot: the pidfile pass above may have just killed things, and
    // this is the broad sweep's own independent pass regardless.
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    let targets: Vec<Pid> = sys
        .processes()
        .keys()
        .copied()
        .filter(|&pid| is_qr_owned(pid, models_dir, &sys) && is_orphaned(pid, qr_exe_path, &sys))
        .collect();
    for pid in targets {
        if let Some(pgid) = current_pgid(pid) {
            log::info!("ollama_ownership: broad sweep reclaiming orphan pid {pid} (pgid {pgid})");
            kill_process_group(pgid, models_dir).await;
            killed_anything = true;
        }
    }

    killed_anything
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};

    fn fresh_sys() -> System {
        let mut sys = System::new_all();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::everything(),
        );
        sys
    }

    /// `extra_args` land as trailing positional args on the long-lived `sh`
    /// process itself (visible via `Process::cmd()`) without affecting what
    /// the script does. Deliberately a loop, not a single `sleep 5`: a
    /// shell tail-exec-optimizes a lone simple external command (replacing
    /// its own image via `execve`, discarding the positional args that
    /// were only ever passed to the original `sh` invocation), but a `while`
    /// loop is a compound command, so `sh` keeps running as the script
    /// interpreter -- forking a short-lived `sleep 1` repeatedly -- with its
    /// own original argv, extra args included, intact for the test to
    /// observe. Plain `sleep 5 <extra_arg>` doesn't work for this either
    /// way: `sleep` parses every argument as another time interval.
    fn spawn_sleep(extra_env: &[(&str, &str)], extra_args: &[&str]) -> Child {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("while true; do sleep 1; done").arg("sh");
        for arg in extra_args {
            cmd.arg(arg);
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd.spawn().expect("failed to spawn test `sh` process")
    }

    /// `kill_process_group`'s job ends at signaling -- reaping is the
    /// actual parent's responsibility, which in production is never this
    /// process for a genuine orphan (whatever adopted it, e.g. systemd,
    /// reaps it). A handful of tests below spawn the "orphan" directly
    /// from the test itself for convenience, which makes the test process
    /// its real parent, so a successfully-killed process shows up as a
    /// zombie here rather than vanishing outright until something calls
    /// `wait()` on it (confirmed empirically -- `sys.process()` returned
    /// `Some(Zombie)`, not `None`, right after a kill that did work).
    /// Checking for *either* is therefore the correct assertion, not a
    /// loosened one.
    fn is_effectively_dead(sys: &System, pid: Pid) -> bool {
        sys.process(pid)
            .map(|p| p.status() == ProcessStatus::Zombie)
            .unwrap_or(true)
    }

    // -- is_qr_owned ----------------------------------------------------

    #[test]
    fn is_qr_owned_true_via_environ() {
        let models_dir = Path::new("/tmp/quietrabbit-ownership-test-models");
        let mut child = spawn_sleep(&[("OLLAMA_MODELS", models_dir.to_str().unwrap())], &[]);
        let pid = Pid::from_u32(child.id());
        let sys = fresh_sys();
        assert!(is_qr_owned(pid, models_dir, &sys));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn is_qr_owned_true_via_cmdline_path() {
        let models_dir = Path::new("/tmp/quietrabbit-ownership-test-models");
        let model_path = models_dir.join("blobs/sha256-fake");
        let mut child = spawn_sleep(&[], &[model_path.to_str().unwrap()]);
        let pid = Pid::from_u32(child.id());
        let sys = fresh_sys();
        assert!(is_qr_owned(pid, models_dir, &sys));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn is_qr_owned_false_when_neither_matches() {
        let models_dir = Path::new("/tmp/quietrabbit-ownership-test-models");
        let mut child = spawn_sleep(&[], &[]);
        let pid = Pid::from_u32(child.id());
        let sys = fresh_sys();
        assert!(!is_qr_owned(pid, models_dir, &sys));
        let _ = child.kill();
        let _ = child.wait();
    }

    // -- is_orphaned ------------------------------------------------------

    #[test]
    fn is_orphaned_false_when_parent_is_live_qr_exe() {
        // This test process stands in for "QR": the spawned child's real
        // parent is this very process, so passing this process's own exe
        // path as `qr_exe_path` reproduces Jason's "live sibling instance"
        // safety case without needing a separate fake wrapper binary.
        let mut child = spawn_sleep(&[], &[]);
        let pid = Pid::from_u32(child.id());
        let sys = fresh_sys();
        let qr_exe = std::env::current_exe().unwrap();
        assert!(!is_orphaned(pid, &qr_exe, &sys));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn is_orphaned_true_when_parent_alive_but_not_qr_exe() {
        let mut child = spawn_sleep(&[], &[]);
        let pid = Pid::from_u32(child.id());
        let sys = fresh_sys();
        let not_qr_exe = Path::new("/not/actually/quietrabbit");
        assert!(is_orphaned(pid, not_qr_exe, &sys));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn is_orphaned_true_when_pid_does_not_exist() {
        let sys = fresh_sys();
        let bogus_pid = Pid::from_u32(u32::MAX - 1);
        assert!(is_orphaned(bogus_pid, Path::new("/anything"), &sys));
    }

    // -- process-group kill -------------------------------------------------

    #[tokio::test]
    async fn group_kill_takes_down_a_member_a_plain_kill_would_miss() {
        use std::os::unix::process::CommandExt;

        // A parent in its own dedicated group, which spawns a child that
        // inherits that group -- the same shape `ollama serve` + a runner
        // will have once `ollama_sidecar.rs` spawns `serve` with
        // `process_group(0)`. OLLAMA_MODELS inherits to the child too, so
        // both members pass is_qr_owned and the group-kill path (not the
        // mixed-group fallback) is what's under test here.
        let models_dir = Path::new("/tmp/quietrabbit-ownership-test-group-kill");
        let mut parent = Command::new("sh");
        parent.arg("-c").arg("sleep 20 & wait");
        parent.env("OLLAMA_MODELS", models_dir);
        parent.process_group(0);
        let mut parent = parent.spawn().expect("failed to spawn test parent");
        let parent_pid = parent.id();
        let pgid = current_pgid(Pid::from_u32(parent_pid)).expect("parent must have a pgid");

        // Give the shell a moment to fork its own `sleep` child.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let sys = fresh_sys();
        let members_before = group_members(pgid, &sys);
        assert!(
            members_before.len() >= 2,
            "expected parent + child in group {pgid}, found {members_before:?}"
        );

        // A plain kill of only the parent pid leaves the child alive.
        unsafe {
            libc::kill(parent_pid as libc::pid_t, libc::SIGKILL);
        }
        let _ = parent.wait();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let sys = fresh_sys();
        let survivors_after_plain_kill = group_members(pgid, &sys);
        assert!(
            !survivors_after_plain_kill.is_empty(),
            "a plain kill(parent_pid) should not have taken down the whole group"
        );

        // The group kill takes down whatever plain kill left behind.
        kill_process_group(pgid, models_dir).await;
        let sys = fresh_sys();
        assert!(
            group_members(pgid, &sys).is_empty(),
            "group {pgid} should be fully gone after kill_process_group"
        );
    }

    #[tokio::test]
    async fn kill_process_group_refuses_to_signal_its_own_group() {
        let own_pgid = unsafe { libc::getpgid(0) };
        // Should log and return without touching anything -- this test's
        // own process surviving to assert afterward IS the assertion. The
        // own-group/pgid<=1 refusal happens before any ownership check, so
        // the models_dir value here is irrelevant.
        kill_process_group(own_pgid, Path::new("/irrelevant")).await;
    }

    /// items.id=586 fix-up (Jason): a pre-fix orphan can share its process
    /// group with an unrelated live process -- today's real shape, `ollama
    /// serve` PID 204157 sharing pgid 203739 with the whole shell/cargo-
    /// tauri tree that launched it. `kill_process_group` must never signal
    /// that whole group; only the individually-proven-QR member(s).
    #[tokio::test]
    async fn group_kill_spares_an_unrelated_process_sharing_the_group() {
        use std::os::unix::process::CommandExt;

        let models_dir = Path::new("/tmp/quietrabbit-ownership-test-mixed-group");

        // The group LEADER is the "shell" -- unrelated, no OLLAMA_MODELS --
        // matching the real pre-fix shape where the launching shell/
        // cargo-tauri was the leader and `ollama serve` just a member.
        let mut leader = Command::new("sh");
        leader.arg("-c").arg("sleep 20");
        leader.process_group(0);
        let mut leader = leader.spawn().expect("failed to spawn test leader");
        let leader_pid = leader.id();
        let pgid = current_pgid(Pid::from_u32(leader_pid)).expect("leader must have a pgid");

        // The "orphan" -- provably QR's own -- joins that same group
        // directly (`process_group(pgid)` with a positive value means
        // "join this existing group", not "start a new one").
        let mut orphan = Command::new("sh");
        orphan.arg("-c").arg("sleep 20");
        orphan.env("OLLAMA_MODELS", models_dir);
        orphan.process_group(pgid);
        let mut orphan = orphan.spawn().expect("failed to spawn test orphan");
        let orphan_pid = orphan.id();

        tokio::time::sleep(Duration::from_millis(200)).await;
        let sys = fresh_sys();
        assert_eq!(
            group_members(pgid, &sys).len(),
            2,
            "expected exactly leader + orphan in group {pgid}"
        );

        kill_process_group(pgid, models_dir).await;

        let sys = fresh_sys();
        assert!(
            is_effectively_dead(&sys, Pid::from_u32(orphan_pid)),
            "the provably-QR orphan should have been killed"
        );
        assert!(
            !is_effectively_dead(&sys, Pid::from_u32(leader_pid)),
            "the unrelated group leader must survive -- the group itself was never signaled"
        );

        let _ = orphan.wait();
        let _ = leader.kill();
        let _ = leader.wait();
    }

    // -- pidfile ------------------------------------------------------

    #[tokio::test]
    async fn write_read_pidfile_roundtrip() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let saved = std::env::var("QR_DATA_ROOT").ok();
        std::env::set_var("QR_DATA_ROOT", tmp.path());

        let mut child = spawn_sleep(&[], &[]);
        write_pidfile(child.id());
        let record = read_pidfile().expect("pidfile should be readable right after write");
        assert_eq!(record.pid, child.id());

        remove_pidfile();
        assert!(read_pidfile().is_none());

        let _ = child.kill();
        let _ = child.wait();
        match saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    #[test]
    fn pidfile_target_declines_on_mismatched_start_time() {
        // A stand-in for "the pid was reused by an unrelated process since
        // the pidfile was written" -- same pid, same ownership/orphan
        // proof, but a start_time that no longer matches. Tested directly
        // against the extracted condition rather than through
        // `reclaim_orphans`, because the broad sweep that always runs
        // after the pidfile pass would independently catch this same
        // process anyway (it doesn't key off the pidfile), making the
        // pidfile pass's own decision unobservable any other way.
        let models_dir = Path::new("/tmp/quietrabbit-ownership-test-pidfile-stale");
        let mut child = spawn_sleep(&[("OLLAMA_MODELS", models_dir.to_str().unwrap())], &[]);
        let pid = child.id();
        let sys = fresh_sys();
        let real_start_time = sys.process(Pid::from_u32(pid)).unwrap().start_time();

        let stale_record = PidfileRecord {
            pid,
            start_time: real_start_time.wrapping_add(999_999),
        };
        assert!(!pidfile_target_is_reclaimable(
            &stale_record,
            models_dir,
            Path::new("/not/actually/quietrabbit"),
            &sys,
        ));

        let matching_record = PidfileRecord {
            pid,
            start_time: real_start_time,
        };
        assert!(pidfile_target_is_reclaimable(
            &matching_record,
            models_dir,
            Path::new("/not/actually/quietrabbit"),
            &sys,
        ));

        let _ = child.kill();
        let _ = child.wait();
    }

    // -- reclaim_orphans ------------------------------------------------

    #[tokio::test]
    async fn reclaim_leaves_a_live_qr_sibling_untouched() {
        let models_dir = Path::new("/tmp/quietrabbit-ownership-test-reclaim-live");
        let mut child = spawn_sleep(&[("OLLAMA_MODELS", models_dir.to_str().unwrap())], &[]);
        let pid = child.id();
        let qr_exe = std::env::current_exe().unwrap();

        reclaim_orphans(models_dir, &qr_exe).await;

        let sys = fresh_sys();
        assert!(
            sys.process(Pid::from_u32(pid)).is_some(),
            "a sidecar whose parent is this live QR-shaped test process must not be reclaimed"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[tokio::test]
    async fn reclaim_kills_a_genuine_orphan() {
        let models_dir = Path::new("/tmp/quietrabbit-ownership-test-reclaim-orphan");
        // Backgrounds a grandchild and prints its pid, then the wrapper
        // shell exits immediately -- the grandchild is reparented away
        // from it before this test ever looks, a real (if deliberately
        // induced) orphan rather than a simulated one.
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "OLLAMA_MODELS={} sleep 5 & echo $!",
                models_dir.display()
            ))
            .output()
            .expect("failed to spawn orphan-producing wrapper");
        let orphan_pid: u32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("wrapper should have printed the backgrounded pid");

        // Not QR's own exe -- this orphan's real parent is already gone by
        // construction, so is_orphaned is true via that branch regardless.
        reclaim_orphans(models_dir, Path::new("/not/actually/quietrabbit")).await;

        let sys = fresh_sys();
        assert!(
            sys.process(Pid::from_u32(orphan_pid)).is_none(),
            "a genuine orphan under QR's own models dir should have been reclaimed"
        );
    }

    /// items.id=586 fix-up (Jason), through the full `reclaim_orphans` path
    /// rather than calling `kill_process_group` directly -- proves the
    /// mixed-group safety rule is actually wired into both the broad sweep
    /// and `kill_process_group` together, not just correct in isolation.
    /// Mirrors today's real pre-fix shape: the group leader is an
    /// unrelated process (standing in for the launching shell/`cargo-
    /// tauri`), and a provably-QR, orphaned member shares that same group.
    #[tokio::test]
    async fn reclaim_through_mixed_group_spares_the_unrelated_leader() {
        use std::os::unix::process::CommandExt;

        let models_dir = Path::new("/tmp/quietrabbit-ownership-test-reclaim-mixed-group");

        let mut leader = Command::new("sh");
        leader.arg("-c").arg("sleep 20");
        leader.process_group(0);
        let mut leader = leader.spawn().expect("failed to spawn test leader");
        let leader_pid = leader.id();
        let pgid = current_pgid(Pid::from_u32(leader_pid)).expect("leader must have a pgid");

        let mut orphan = Command::new("sh");
        orphan.arg("-c").arg("sleep 20");
        orphan.env("OLLAMA_MODELS", models_dir);
        orphan.process_group(pgid);
        let mut orphan = orphan.spawn().expect("failed to spawn test orphan");
        let orphan_pid = orphan.id();

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Not QR's own exe -- this test process is the orphan's real
        // parent, so is_orphaned is true via the "not qr_exe_path" branch;
        // the leader is never a reclaim target at all (fails is_qr_owned),
        // so the broad sweep only ever names the orphan's pgid.
        reclaim_orphans(models_dir, Path::new("/not/actually/quietrabbit")).await;

        let sys = fresh_sys();
        assert!(
            is_effectively_dead(&sys, Pid::from_u32(orphan_pid)),
            "the provably-QR orphan should have been reclaimed"
        );
        assert!(
            !is_effectively_dead(&sys, Pid::from_u32(leader_pid)),
            "the unrelated group leader must survive reclaim -- only the member was signaled"
        );

        let _ = orphan.wait();
        let _ = leader.kill();
        let _ = leader.wait();
    }
}
