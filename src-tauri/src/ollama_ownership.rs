//! Ownership proof and orphan reclaim for QR's own Ollama sidecar and its
//! runner subprocesses (items.id=586, items.id=668, decisions.id=863).
//!
//! # What QR will signal
//! QR signals a process only if it is a live member of a process group QR
//! itself recorded when it spawned a sidecar, *and* the member's executable
//! file name says it is the sidecar or one of its runners ([`member_is_provable`]).
//! Nothing else is ever signalled: not a process that merely has an argument
//! or an environment variable pointing at QR's `ollama_models` directory
//! (that match was the old proof and killed a user's `du` or editor), not a
//! user's own `ollama`, and not the holder of a port.
//!
//! # The record
//! [`record_spawn`] writes `{pid, start_time, pgid, owner_pid,
//! owner_start_time}` into `ollama_sidecar.pid` (a JSON array, so a second
//! QR instance does not overwrite the first's record) the moment a sidecar is
//! spawned, before it is ready, so a `serve` that dies during the readiness
//! window and leaves a runner behind is still tracked. The owner is the QR
//! process that spawned it.
//!
//! # Reclaim ([`reclaim_orphans`])
//! Only recorded groups are considered, each classified by [`classify`]:
//!
//! - owner still alive (pid exists and `start_time` matches): a live sibling
//!   QR's sidecar. Not touched, the record is kept.
//! - leader (`pid`) alive with a different `start_time`: the pid was reused,
//!   which cannot happen while the original group still has a member (the
//!   kernel does not hand out a pid that is still a process group id), so the
//!   original group is gone. Stale record, dropped, nothing signalled.
//! - leader alive with the recorded `start_time`: it must itself be provable
//!   (exe name, `OLLAMA_MODELS` equal to `models_dir` in its environment) and
//!   orphaned ([`is_orphaned`]); otherwise not touched.
//! - leader gone: the group is headless. This is the shape an abrupt stop
//!   leaves, because `PR_SET_PDEATHSIG` is armed on `serve` only, so its
//!   runner outlives it. Whatever members the group still has are handed to
//!   [`kill_process_group`], which signals only the provable ones.
//!
//! # Residual risk (documented, accepted)
//! If the leader is gone and the whole original group has also died, the
//! pid may later be recycled by an unrelated process that becomes a group
//! leader and then exits while leaving members behind; those members would
//! then sit in a group with the recorded number. They are still signalled
//! only if their executable is named `qr-ollama` or `llama-server` (or
//! `ollama` in a debug build, which is the PATH fallback). Reaching that
//! needs the group to die, the pid counter to wrap, the pid to be reused as
//! a group leader and exit, all between two QR starts. There is no signalling
//! race-free primitive on every platform (a pid can also be recycled between
//! the membership check and the `kill`), so this is a bound, not a proof.
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

/// Executable file names QR's own sidecar and runners may have. The bundled
/// binary is `qr-ollama` (decisions.id=863); its runner is `llama-server` in
/// the layouts observed so far, or `qr-ollama runner`, which already matches.
/// items.id=583 owns the packaged layout and must revisit this list if the
/// runner is named differently there.
const MEMBER_EXE_NAMES: &[&str] = &["qr-ollama", "llama-server"];

/// Additionally accepted in debug builds, where the debug-only PATH fallback
/// launches the system `ollama` as QR's sidecar (items.id=583 retires it).
/// Never accepted in a release build.
const DEV_MEMBER_EXE_NAMES: &[&str] = &["ollama"];

/// True iff a process whose executable is `exe` may be signalled as a member
/// of a recorded sidecar group. `None` (the executable could not be read:
/// permission, timing, a vanished process) is **not provable and never
/// signalled**.
pub fn member_is_provable(exe: Option<&Path>, dev_names: bool) -> bool {
    let Some(name) = exe.and_then(Path::file_name).and_then(|n| n.to_str()) else {
        return false;
    };
    let name = if cfg!(windows) {
        name.strip_suffix(".exe").unwrap_or(name)
    } else {
        name
    };
    MEMBER_EXE_NAMES.contains(&name) || (dev_names && DEV_MEMBER_EXE_NAMES.contains(&name))
}

fn member_exe_ok(pid: Pid, sys: &System) -> bool {
    member_is_provable(
        sys.process(pid).and_then(|p| p.exe()),
        cfg!(debug_assertions),
    )
}

/// True iff `pid`'s environment has `OLLAMA_MODELS` equal to `models_dir`
/// (how QR spawns `serve` -- see `ollama_sidecar::try_start`). Used only as an
/// additional condition on a recorded, live leader, never as proof by itself:
/// a runner does not reliably inherit it, and any process can carry it.
pub fn leader_environ_matches(pid: Pid, models_dir: &Path, sys: &System) -> bool {
    let Some(proc_) = sys.process(pid) else {
        return false;
    };
    proc_.environ().iter().any(|e| {
        e.to_str()
            .and_then(|s| s.strip_prefix("OLLAMA_MODELS="))
            .map(|v| Path::new(v) == models_dir)
            .unwrap_or(false)
    })
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
///
/// Applied to a recorded group's *leader* only. A runner's parent is `serve`,
/// not QR, so it would read as orphaned inside a perfectly live group.
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

fn snapshot() -> System {
    let mut sys = System::new();
    refresh(&mut sys);
    sys
}

fn refresh(sys: &mut System) {
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
}

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

/// Every live, non-zombie pid currently in process group `pgid`, per a fresh
/// query of each process in `sys` (not cached group membership -- group
/// membership can change at any time via `setpgid`, though nothing in this
/// codebase does that for a sidecar/runner after spawn). A zombie is
/// excluded: it is already dead and cannot be meaningfully signalled.
fn group_members(pgid: libc::pid_t, sys: &System) -> Vec<Pid> {
    sys.processes()
        .iter()
        .filter(|(_, p)| p.status() != ProcessStatus::Zombie)
        .map(|(pid, _)| *pid)
        .filter(|&pid| current_pgid(pid) == Some(pgid))
        .collect()
}

/// What [`kill_process_group`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupKill {
    /// Provable members that were sent SIGTERM.
    pub signalled: usize,
    /// Provable members still alive after SIGKILL. Non-zero means the record
    /// must be kept so the next start can try again.
    pub survivors: usize,
}

/// `SIGTERM` then `SIGKILL` to the provable members of process group `pgid`
/// and to nothing else.
///
/// `pgid` must be a group QR recorded or spawned itself; this function does
/// not know that, so every caller is one of: the reclaim of a recorded group,
/// `stop()`/`liveness_tick` of the child QR holds, or the cleanup of a start
/// attempt that failed.
///
/// Refuses to signal process group 1 (init) or this process's own group
/// outright. Beyond that: enumerates every live member of `pgid` and
/// checks each against [`member_is_provable`] before touching anything. Only
/// when *every* member passes does this signal the whole group by pgid, the
/// cheap/simple path that also catches a runner forked in the gap. A group
/// containing even one member that isn't provable (a shell that launched a
/// pre-fix orphan, a process whose `exe()` could not be read) is never
/// groupkilled; only the individually-proven members are signalled, one pid
/// at a time, and everything else in that group is left alone.
pub async fn kill_process_group(pgid: libc::pid_t) -> GroupKill {
    let own_pgid = unsafe { libc::getpgid(0) };
    if pgid <= 1 || pgid == own_pgid {
        log::warn!(
            "ollama_ownership: refusing to signal process group {pgid} (own group {own_pgid})"
        );
        return GroupKill::default();
    }

    let mut sys = snapshot();
    let members = group_members(pgid, &sys);
    if members.is_empty() {
        return GroupKill::default();
    }
    let proven: Vec<Pid> = members
        .iter()
        .copied()
        .filter(|&pid| member_exe_ok(pid, &sys))
        .collect();
    if proven.is_empty() {
        log::warn!(
            "ollama_ownership: process group {pgid} has {} live member(s), none provably \
             QR's own -- nothing signalled",
            members.len()
        );
        return GroupKill::default();
    }

    if proven.len() == members.len() {
        log::info!("ollama_ownership: SIGTERM to process group {pgid}");
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
    } else {
        log::warn!(
            "ollama_ownership: process group {pgid} has {} member(s) not provably QR's own -- \
             signaling only the {} proven member(s) individually, never the group",
            members.len() - proven.len(),
            proven.len()
        );
        for &pid in &proven {
            unsafe {
                libc::kill(pid.as_u32() as libc::pid_t, libc::SIGTERM);
            }
        }
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Still in the group, still alive, still provable: a pid recycled in the
    // meantime fails one of the three.
    let still_there = |sys: &System| -> Vec<Pid> {
        proven
            .iter()
            .copied()
            .filter(|&pid| {
                sys.process(pid)
                    .is_some_and(|p| p.status() != ProcessStatus::Zombie)
                    && current_pgid(pid) == Some(pgid)
                    && member_exe_ok(pid, sys)
            })
            .collect()
    };
    refresh(&mut sys);
    let survivors = still_there(&sys);
    if survivors.is_empty() {
        return GroupKill {
            signalled: proven.len(),
            survivors: 0,
        };
    }
    log::warn!(
        "ollama_ownership: {} proven process(es) survived SIGTERM, sending SIGKILL",
        survivors.len()
    );
    for &pid in &survivors {
        unsafe {
            libc::kill(pid.as_u32() as libc::pid_t, libc::SIGKILL);
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    refresh(&mut sys);
    GroupKill {
        signalled: proven.len(),
        survivors: still_there(&sys).len(),
    }
}

// ---------------------------------------------------------------------------
// Pidfile
// ---------------------------------------------------------------------------

/// One recorded sidecar group. Written at spawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PidfileRecord {
    /// The group leader: the `serve` process (`pid == pgid` by construction,
    /// `process_group(0)`).
    pid: u32,
    /// `sysinfo::Process::start_time()` of `pid` at spawn -- guards against a
    /// PID being reused by an unrelated process before this record is next
    /// read.
    start_time: u64,
    pgid: libc::pid_t,
    /// The QR process that spawned it, and its start time: while it is alive
    /// this sidecar belongs to a live QR and is never reclaimed.
    owner_pid: u32,
    owner_start_time: u64,
}

/// Test-only: `(leader pid, pgid)` of every recorded group.
#[cfg(test)]
pub fn recorded_groups_for_test() -> Vec<(u32, libc::pid_t)> {
    read_records().iter().map(|r| (r.pid, r.pgid)).collect()
}

pub fn pidfile_path() -> PathBuf {
    crate::providers::utils::get_data_root().join("ollama_sidecar.pid")
}

fn start_time_of(pid: u32) -> Option<u64> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::everything(),
    );
    sys.process(Pid::from_u32(pid)).map(|p| p.start_time())
}

/// An absent file, a corrupt one and the pre-items.id=668 single-object format
/// (no group was recorded in it, so nothing in it can be proven) all read as
/// "no records"; the next write replaces the file.
fn read_records() -> Vec<PidfileRecord> {
    let Ok(bytes) = std::fs::read(pidfile_path()) else {
        return Vec::new();
    };
    match serde_json::from_slice(&bytes) {
        Ok(records) => records,
        Err(e) => {
            log::warn!(
                "ollama_ownership: pidfile is unreadable or in the old single-record format \
                 ({e}) -- ignoring it; nothing in it can be proven"
            );
            Vec::new()
        }
    }
}

/// Atomic (temp file, then rename). An empty list removes the file. Not
/// locked across two QR instances: two instances starting in the same instant
/// can lose a record to this read-modify-write; items.id=667 (single
/// instance) removes that case.
fn write_records(records: &[PidfileRecord]) {
    let path = pidfile_path();
    if records.is_empty() {
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != io::ErrorKind::NotFound {
                log::debug!("ollama_ownership: could not remove pidfile {path:?}: {e}");
            }
        }
        return;
    }
    let tmp = path.with_extension("pid.tmp");
    let result: io::Result<()> = serde_json::to_vec(records)
        .map_err(io::Error::other)
        .and_then(|bytes| std::fs::write(&tmp, bytes))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = result {
        log::warn!("ollama_ownership: failed to write pidfile {path:?}: {e}");
    }
}

/// Records the sidecar group just spawned. Called right after the spawn and
/// before readiness. Best-effort: a failure only means this group cannot be
/// reclaimed by the next start, not a reason to fail the startup.
pub fn record_spawn(pid: u32, pgid: libc::pid_t) {
    let owner_pid = std::process::id();
    let (Some(start_time), Some(owner_start_time)) = (start_time_of(pid), start_time_of(owner_pid))
    else {
        log::debug!(
            "ollama_ownership: not recording pid {pid} -- it (or this process) could not be read"
        );
        return;
    };
    let mut records = read_records();
    records.retain(|r| !(r.pid == pid && r.start_time == start_time));
    records.push(PidfileRecord {
        pid,
        start_time,
        pgid,
        owner_pid,
        owner_start_time,
    });
    write_records(&records);
}

/// Drops the record of the group led by `pid`, if any.
pub fn remove_record(pid: u32) {
    let mut records = read_records();
    let before = records.len();
    records.retain(|r| r.pid != pid);
    if records.len() != before {
        write_records(&records);
    }
}

// ---------------------------------------------------------------------------
// Reclaim
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Its owner QR is alive. Leave it and keep the record.
    LiveSibling,
    /// The group is gone or the pid was reused. Drop the record.
    Stale,
    /// Something here cannot be proven. Leave it and keep the record.
    Unprovable,
    /// Hand the group to [`kill_process_group`].
    Reclaim,
}

fn classify(
    record: &PidfileRecord,
    models_dir: &Path,
    qr_exe_path: &Path,
    sys: &System,
) -> Verdict {
    let owner_alive = sys
        .process(Pid::from_u32(record.owner_pid))
        .is_some_and(|p| {
            p.start_time() == record.owner_start_time && p.status() != ProcessStatus::Zombie
        });
    if owner_alive {
        return Verdict::LiveSibling;
    }
    if record.pgid <= 1 {
        return Verdict::Stale;
    }

    let leader = Pid::from_u32(record.pid);
    match sys
        .process(leader)
        .filter(|p| p.status() != ProcessStatus::Zombie)
    {
        Some(p) => {
            if p.start_time() != record.start_time {
                return Verdict::Stale;
            }
            if current_pgid(leader) != Some(record.pgid)
                || !member_exe_ok(leader, sys)
                || !leader_environ_matches(leader, models_dir, sys)
                || !is_orphaned(leader, qr_exe_path, sys)
            {
                return Verdict::Unprovable;
            }
            Verdict::Reclaim
        }
        None => {
            if group_members(record.pgid, sys).is_empty() {
                Verdict::Stale
            } else {
                Verdict::Reclaim
            }
        }
    }
}

/// Reclaims the recorded sidecar groups whose owner QR is gone (see the
/// module doc comment for the rules). Returns `true` iff something was
/// actually signalled -- callers use that to wait for the preferred port to
/// free up before starting fresh.
///
/// Rewrites the pidfile afterward: records of a live sibling or of anything
/// unprovable are kept, a group that still has a provable survivor is kept,
/// everything else (reclaimed, stale) is dropped. A fresh sidecar is about to
/// start and will record itself.
pub async fn reclaim_orphans(models_dir: &Path, qr_exe_path: &Path) -> bool {
    let records = read_records();
    if records.is_empty() {
        write_records(&[]);
        return false;
    }
    let sys = snapshot();
    let mut keep = Vec::new();
    let mut signalled_any = false;
    for record in records {
        match classify(&record, models_dir, qr_exe_path, &sys) {
            Verdict::LiveSibling | Verdict::Unprovable => keep.push(record),
            Verdict::Stale => {}
            Verdict::Reclaim => {
                log::info!(
                    "ollama_ownership: reclaiming recorded sidecar group {} (leader pid {})",
                    record.pgid,
                    record.pid
                );
                let outcome = kill_process_group(record.pgid).await;
                signalled_any |= outcome.signalled > 0;
                if outcome.survivors > 0 {
                    keep.push(record);
                }
            }
        }
    }
    write_records(&keep);
    signalled_any
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::procs::{
        dead_pid, is_effectively_dead, named_copy, spawn_retry, spawn_sleep_stub,
        start_time_of as start_time, ChildGuard,
    };
    use crate::test_support::DataRoot;
    use std::process::Command;

    fn fresh_sys() -> System {
        snapshot()
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
    ///
    /// `program` is `sh` or a copy of it under another file name.
    fn spawn_sleep_as(
        program: &Path,
        extra_env: &[(&str, &str)],
        extra_args: &[&str],
    ) -> ChildGuard {
        let mut cmd = Command::new(program);
        cmd.arg("-c").arg("while true; do sleep 1; done").arg("sh");
        for arg in extra_args {
            cmd.arg(arg);
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        ChildGuard::new(spawn_retry(&mut cmd))
    }

    fn spawn_sleep(extra_env: &[(&str, &str)], extra_args: &[&str]) -> ChildGuard {
        spawn_sleep_as(Path::new("sh"), extra_env, extra_args)
    }

    const NOT_QR_EXE: &str = "/not/actually/quietrabbit";

    // -- member_is_provable --------------------------------------------------

    #[test]
    fn member_names_are_the_sidecar_and_its_runner() {
        for yes in ["/opt/qr/qr-ollama", "/opt/qr/llama-server", "qr-ollama"] {
            assert!(member_is_provable(Some(Path::new(yes)), false), "{yes}");
        }
        for no in [
            "/usr/bin/ollama",
            "/usr/bin/sleep",
            "/bin/sh",
            "/usr/bin/du",
            "/opt/qr/qr-ollama.bak",
            "/opt/qr/not-qr-ollama",
            "/opt/qr/llama-server-extra",
            "/",
        ] {
            assert!(!member_is_provable(Some(Path::new(no)), false), "{no}");
        }
    }

    #[test]
    fn plain_ollama_is_provable_only_in_a_debug_build() {
        let ollama = Some(Path::new("/usr/bin/ollama"));
        assert!(!member_is_provable(ollama, false));
        assert!(member_is_provable(ollama, true));
    }

    #[test]
    fn unreadable_exe_is_never_provable() {
        assert!(!member_is_provable(None, false));
        assert!(!member_is_provable(None, true));
    }

    // -- leader_environ_matches ----------------------------------------------

    #[test]
    fn leader_environ_matches_true_via_environ() {
        let models_dir = Path::new("/tmp/quietrabbit-ownership-test-models");
        let child = spawn_sleep(&[("OLLAMA_MODELS", models_dir.to_str().unwrap())], &[]);
        let sys = fresh_sys();
        assert!(leader_environ_matches(
            Pid::from_u32(child.id()),
            models_dir,
            &sys
        ));
    }

    /// The old proof matched any command-line argument under `models_dir`.
    /// That is not proof of anything: a user's `du` or editor has one too.
    #[test]
    fn an_argument_under_models_dir_is_not_proof() {
        let models_dir = Path::new("/tmp/quietrabbit-ownership-test-models");
        let model_path = models_dir.join("blobs/sha256-fake");
        let child = spawn_sleep(&[], &[model_path.to_str().unwrap()]);
        let sys = fresh_sys();
        assert!(!leader_environ_matches(
            Pid::from_u32(child.id()),
            models_dir,
            &sys
        ));
    }

    #[test]
    fn leader_environ_matches_false_when_neither_matches() {
        let models_dir = Path::new("/tmp/quietrabbit-ownership-test-models");
        let child = spawn_sleep(&[], &[]);
        let sys = fresh_sys();
        assert!(!leader_environ_matches(
            Pid::from_u32(child.id()),
            models_dir,
            &sys
        ));
    }

    // -- is_orphaned ------------------------------------------------------

    #[test]
    fn is_orphaned_false_when_parent_is_live_qr_exe() {
        // This test process stands in for "QR": the spawned child's real
        // parent is this very process, so passing this process's own exe
        // path as `qr_exe_path` reproduces Jason's "live sibling instance"
        // safety case without needing a separate fake wrapper binary.
        let child = spawn_sleep(&[], &[]);
        let sys = fresh_sys();
        let qr_exe = std::env::current_exe().unwrap();
        assert!(!is_orphaned(Pid::from_u32(child.id()), &qr_exe, &sys));
    }

    #[test]
    fn is_orphaned_true_when_parent_alive_but_not_qr_exe() {
        let child = spawn_sleep(&[], &[]);
        let sys = fresh_sys();
        assert!(is_orphaned(
            Pid::from_u32(child.id()),
            Path::new("/not/actually/quietrabbit"),
            &sys
        ));
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
        // A leader in its own dedicated group plus a runner that joined it:
        // the same shape `qr-ollama serve` + `llama-server` have in a real
        // sidecar group. Both are provable, so the group-kill path (not the
        // mixed-group fallback) is what's under test here.
        let dir = tempfile::tempdir().unwrap();
        let serve = named_copy(dir.path(), "/bin/sleep", "qr-ollama");
        let runner = named_copy(dir.path(), "/bin/sleep", "llama-server");
        let mut leader = spawn_sleep_stub(&serve, 0, &[]);
        let pgid = leader.id() as libc::pid_t;
        let member = spawn_sleep_stub(&runner, pgid, &[]);

        let sys = fresh_sys();
        let members_before = group_members(pgid, &sys);
        assert_eq!(
            members_before.len(),
            2,
            "expected leader + runner in group {pgid}, found {members_before:?}"
        );

        // A plain kill of only the leader leaves the runner alive.
        leader.kill_and_wait();
        let sys = fresh_sys();
        assert_eq!(
            group_members(pgid, &sys),
            vec![Pid::from_u32(member.id())],
            "a plain kill of the leader should not have taken down the runner"
        );

        // The group kill takes down whatever the plain kill left behind.
        let outcome = kill_process_group(pgid).await;
        assert_eq!(
            outcome,
            GroupKill {
                signalled: 1,
                survivors: 0
            }
        );
        assert!(is_effectively_dead(member.id()));
    }

    #[tokio::test]
    async fn kill_process_group_refuses_to_signal_its_own_group() {
        let own_pgid = unsafe { libc::getpgid(0) };
        // Should log and return without touching anything -- this test's
        // own process surviving to assert afterward IS the assertion.
        assert_eq!(kill_process_group(own_pgid).await, GroupKill::default());
        assert_eq!(kill_process_group(1).await, GroupKill::default());
        assert_eq!(kill_process_group(0).await, GroupKill::default());
    }

    /// items.id=586 fix-up (Jason): a pre-fix orphan can share its process
    /// group with an unrelated live process -- today's real shape, `ollama
    /// serve` PID 204157 sharing pgid 203739 with the whole shell/cargo-
    /// tauri tree that launched it. `kill_process_group` must never signal
    /// that whole group; only the individually-proven member(s).
    #[tokio::test]
    async fn group_kill_spares_an_unrelated_process_sharing_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let unrelated = named_copy(dir.path(), "/bin/sleep", "some-shell");
        let runner = named_copy(dir.path(), "/bin/sleep", "llama-server");

        // The group LEADER is the "shell" -- unrelated -- matching the real
        // pre-fix shape where the launching shell/cargo-tauri was the leader
        // and the runner just a member.
        let leader = spawn_sleep_stub(&unrelated, 0, &[]);
        let pgid = leader.id() as libc::pid_t;
        let orphan = spawn_sleep_stub(&runner, pgid, &[]);

        let outcome = kill_process_group(pgid).await;

        assert_eq!(outcome.survivors, 0);
        assert_eq!(outcome.signalled, 1);
        assert!(
            is_effectively_dead(orphan.id()),
            "the provable runner should have been killed"
        );
        assert!(
            !is_effectively_dead(leader.id()),
            "the unrelated group leader must survive -- the group itself was never signaled"
        );
    }

    #[tokio::test]
    async fn group_with_no_provable_member_is_left_entirely_alone() {
        let dir = tempfile::tempdir().unwrap();
        let a = named_copy(dir.path(), "/bin/sleep", "some-shell");
        let b = named_copy(dir.path(), "/bin/sleep", "an-editor");
        let leader = spawn_sleep_stub(&a, 0, &[]);
        let pgid = leader.id() as libc::pid_t;
        let member = spawn_sleep_stub(&b, pgid, &[]);

        assert_eq!(kill_process_group(pgid).await, GroupKill::default());
        assert!(!is_effectively_dead(leader.id()));
        assert!(!is_effectively_dead(member.id()));
    }

    // -- pidfile ------------------------------------------------------

    #[tokio::test]
    async fn record_spawn_roundtrips_and_remove_record_drops_it() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _root = DataRoot::isolated();

        let child = spawn_sleep(&[], &[]);
        let pgid = unsafe { libc::getpgid(child.id() as libc::pid_t) };
        record_spawn(child.id(), pgid);
        let records = read_records();
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(r.pid, child.id());
        assert_eq!(r.pgid, pgid);
        assert_eq!(r.start_time, start_time(child.id()));
        assert_eq!(r.owner_pid, std::process::id());
        assert_eq!(r.owner_start_time, start_time(std::process::id()));

        // A second sidecar's record does not replace the first's.
        let other = spawn_sleep(&[], &[]);
        record_spawn(other.id(), pgid);
        assert_eq!(read_records().len(), 2);

        remove_record(child.id());
        let records = read_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].pid, other.id());
        remove_record(other.id());
        assert!(read_records().is_empty());
        assert!(!pidfile_path().exists());
    }

    #[tokio::test]
    async fn legacy_single_object_pidfile_is_ignored_and_replaced() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let root = DataRoot::isolated();
        let path = root.path().join("ollama_sidecar.pid");
        std::fs::write(&path, br#"{"pid":1,"start_time":2}"#).unwrap();

        assert!(read_records().is_empty());
        assert!(!reclaim_orphans(Path::new("/irrelevant"), Path::new(NOT_QR_EXE)).await);
        assert!(!path.exists(), "the unusable file should be replaced");
    }

    // -- classify / reclaim -------------------------------------------------

    /// A recorded group whose leader has already exited and been reaped, with
    /// one provable runner left in it: the shape an abrupt stop leaves behind
    /// (`serve` dies to PR_SET_PDEATHSIG, its runner does not).
    struct HeadlessGroup {
        runner: ChildGuard,
        record: PidfileRecord,
    }

    fn headless_group(dir: &Path, runner_name: &str) -> HeadlessGroup {
        // Separate directories: the runner may share the leader's file name,
        // and a running executable cannot be overwritten.
        let (serve_dir, runner_dir) = (dir.join("serve"), dir.join("runner"));
        std::fs::create_dir_all(&serve_dir).unwrap();
        std::fs::create_dir_all(&runner_dir).unwrap();
        let serve = named_copy(&serve_dir, "/bin/sleep", "qr-ollama");
        let runner = named_copy(&runner_dir, "/bin/sleep", runner_name);
        let mut leader = spawn_sleep_stub(&serve, 0, &[]);
        let pgid = leader.id() as libc::pid_t;
        let runner = spawn_sleep_stub(&runner, pgid, &[]);
        let record = PidfileRecord {
            pid: leader.id(),
            start_time: start_time(leader.id()),
            pgid,
            owner_pid: dead_pid(),
            owner_start_time: 1,
        };
        leader.kill_and_wait();
        HeadlessGroup { runner, record }
    }

    #[tokio::test]
    async fn reclaim_kills_an_orphaned_runner_in_a_headless_recorded_group() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _root = DataRoot::isolated();
        let dir = tempfile::tempdir().unwrap();
        let g = headless_group(dir.path(), "llama-server");
        write_records(std::slice::from_ref(&g.record));

        let signalled =
            reclaim_orphans(Path::new("/irrelevant-models"), Path::new(NOT_QR_EXE)).await;

        assert!(signalled);
        assert!(is_effectively_dead(g.runner.id()));
        assert!(read_records().is_empty(), "a reclaimed group's record goes");
    }

    /// The headline test of items.id=668: a process named `ollama` with
    /// `OLLAMA_MODELS` set and an argument under `models_dir`, and a `sleep`
    /// shell with an argument under `models_dir`, are untouched while a real
    /// orphan in the recorded group is reclaimed.
    #[tokio::test]
    async fn reclaim_never_touches_lookalikes_outside_the_recorded_group() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _root = DataRoot::isolated();
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("ollama_models");
        let models = models_dir.to_str().unwrap();
        let blob = models_dir.join("blobs/sha256-fake");

        let sh_as_ollama = named_copy(dir.path(), "/bin/sh", "ollama");
        let user_ollama = spawn_sleep_as(
            &sh_as_ollama,
            &[("OLLAMA_MODELS", models)],
            &[blob.to_str().unwrap()],
        );
        let user_tool = spawn_sleep(&[], &[blob.to_str().unwrap()]);
        let g = headless_group(dir.path(), "qr-ollama");
        write_records(std::slice::from_ref(&g.record));

        assert!(reclaim_orphans(&models_dir, Path::new(NOT_QR_EXE)).await);

        assert!(
            is_effectively_dead(g.runner.id()),
            "the orphan is reclaimed"
        );
        assert!(
            !is_effectively_dead(user_ollama.id()),
            "a process named ollama with OLLAMA_MODELS and an arg under models_dir"
        );
        assert!(
            !is_effectively_dead(user_tool.id()),
            "a shell with an arg under models_dir"
        );
    }

    /// No record, no signal: the sweep over every process on the machine is
    /// gone, so even a lookalike that would have passed the old proof is
    /// safe when nothing is recorded.
    #[tokio::test]
    async fn reclaim_with_no_record_signals_nothing() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _root = DataRoot::isolated();
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("ollama_models");
        let orphan_like = named_copy(dir.path(), "/bin/sh", "qr-ollama");
        let lookalike = spawn_sleep_as(
            &orphan_like,
            &[("OLLAMA_MODELS", models_dir.to_str().unwrap())],
            &[models_dir.join("blobs/x").to_str().unwrap()],
        );

        assert!(!reclaim_orphans(&models_dir, Path::new(NOT_QR_EXE)).await);
        assert!(!is_effectively_dead(lookalike.id()));
    }

    #[tokio::test]
    async fn reclaim_in_a_mixed_headless_group_spares_the_unrelated_member() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _root = DataRoot::isolated();
        let dir = tempfile::tempdir().unwrap();
        let g = headless_group(dir.path(), "llama-server");
        let stranger = named_copy(dir.path(), "/bin/sleep", "some-shell");
        let stranger = spawn_sleep_stub(&stranger, g.record.pgid, &[]);
        write_records(std::slice::from_ref(&g.record));

        assert!(reclaim_orphans(Path::new("/irrelevant"), Path::new(NOT_QR_EXE)).await);

        assert!(is_effectively_dead(g.runner.id()));
        assert!(!is_effectively_dead(stranger.id()));
    }

    #[tokio::test]
    async fn recorded_pid_with_a_different_start_time_is_untouched() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _root = DataRoot::isolated();
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("ollama_models");
        let serve = named_copy(dir.path(), "/bin/sleep", "qr-ollama");
        // A live, otherwise provable leader (right exe, right OLLAMA_MODELS,
        // orphaned from the "QR" below) -- only its start_time is wrong, the
        // "pid reused since the record was written" case.
        let leader = spawn_sleep_stub(
            &serve,
            0,
            &[("OLLAMA_MODELS", models_dir.to_str().unwrap())],
        );
        let real = start_time(leader.id());
        let mut record = PidfileRecord {
            pid: leader.id(),
            start_time: real.wrapping_add(999_999),
            pgid: leader.id() as libc::pid_t,
            owner_pid: dead_pid(),
            owner_start_time: 1,
        };

        let sys = fresh_sys();
        assert_eq!(
            classify(&record, &models_dir, Path::new(NOT_QR_EXE), &sys),
            Verdict::Stale
        );
        // Same record with the right start_time is reclaimable, so the
        // mismatch is what made the difference.
        record.start_time = real;
        assert_eq!(
            classify(&record, &models_dir, Path::new(NOT_QR_EXE), &sys),
            Verdict::Reclaim
        );

        record.start_time = real.wrapping_add(999_999);
        write_records(&[record]);
        assert!(!reclaim_orphans(&models_dir, Path::new(NOT_QR_EXE)).await);
        assert!(!is_effectively_dead(leader.id()));
        assert!(read_records().is_empty(), "a stale record is dropped");
    }

    #[tokio::test]
    async fn live_leader_is_reclaimed_with_its_runner_when_all_conditions_hold() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _root = DataRoot::isolated();
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("ollama_models");
        let serve = named_copy(dir.path(), "/bin/sleep", "qr-ollama");
        let runner = named_copy(dir.path(), "/bin/sleep", "llama-server");
        let leader = spawn_sleep_stub(
            &serve,
            0,
            &[("OLLAMA_MODELS", models_dir.to_str().unwrap())],
        );
        let pgid = leader.id() as libc::pid_t;
        let member = spawn_sleep_stub(&runner, pgid, &[]);
        write_records(&[PidfileRecord {
            pid: leader.id(),
            start_time: start_time(leader.id()),
            pgid,
            owner_pid: dead_pid(),
            owner_start_time: 1,
        }]);

        assert!(reclaim_orphans(&models_dir, Path::new(NOT_QR_EXE)).await);

        assert!(is_effectively_dead(leader.id()));
        assert!(is_effectively_dead(member.id()));
        assert!(read_records().is_empty());
    }

    #[tokio::test]
    async fn live_leader_without_models_dir_in_its_environment_is_untouched() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _root = DataRoot::isolated();
        let dir = tempfile::tempdir().unwrap();
        let serve = named_copy(dir.path(), "/bin/sleep", "qr-ollama");
        let leader = spawn_sleep_stub(&serve, 0, &[]);
        let record = PidfileRecord {
            pid: leader.id(),
            start_time: start_time(leader.id()),
            pgid: leader.id() as libc::pid_t,
            owner_pid: dead_pid(),
            owner_start_time: 1,
        };
        write_records(std::slice::from_ref(&record));

        assert!(!reclaim_orphans(&dir.path().join("ollama_models"), Path::new(NOT_QR_EXE)).await);

        assert!(!is_effectively_dead(leader.id()));
        assert_eq!(read_records(), vec![record], "an unprovable record is kept");
    }

    #[tokio::test]
    async fn live_leader_whose_parent_is_a_live_qr_exe_is_untouched() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _root = DataRoot::isolated();
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("ollama_models");
        let serve = named_copy(dir.path(), "/bin/sleep", "qr-ollama");
        let leader = spawn_sleep_stub(
            &serve,
            0,
            &[("OLLAMA_MODELS", models_dir.to_str().unwrap())],
        );
        write_records(&[PidfileRecord {
            pid: leader.id(),
            start_time: start_time(leader.id()),
            pgid: leader.id() as libc::pid_t,
            owner_pid: dead_pid(),
            owner_start_time: 1,
        }]);

        // This test process stands in for "QR": it is the leader's parent.
        let qr_exe = std::env::current_exe().unwrap();
        assert!(!reclaim_orphans(&models_dir, &qr_exe).await);
        assert!(!is_effectively_dead(leader.id()));
    }

    /// A sibling QR (items.id=667 is not built, so two can run on one data
    /// dir) owns a live sidecar group: never touched, record kept.
    #[tokio::test]
    async fn a_live_owner_means_a_live_sibling_and_nothing_is_signalled() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _root = DataRoot::isolated();
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("ollama_models");
        let serve = named_copy(dir.path(), "/bin/sleep", "qr-ollama");
        let leader = spawn_sleep_stub(
            &serve,
            0,
            &[("OLLAMA_MODELS", models_dir.to_str().unwrap())],
        );
        // Every other condition says "reclaimable"; only the owner differs.
        let record = PidfileRecord {
            pid: leader.id(),
            start_time: start_time(leader.id()),
            pgid: leader.id() as libc::pid_t,
            owner_pid: std::process::id(),
            owner_start_time: start_time(std::process::id()),
        };
        write_records(std::slice::from_ref(&record));

        assert!(!reclaim_orphans(&models_dir, Path::new(NOT_QR_EXE)).await);

        assert!(!is_effectively_dead(leader.id()));
        assert_eq!(read_records(), vec![record], "the sibling's record is kept");
    }

    #[tokio::test]
    async fn an_owner_with_a_reused_pid_does_not_count_as_alive() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _root = DataRoot::isolated();
        let dir = tempfile::tempdir().unwrap();
        let g = headless_group(dir.path(), "llama-server");
        // Owner pid is a live process (this one), but not the one that
        // spawned the sidecar: its start_time differs.
        let mut record = g.record.clone();
        record.owner_pid = std::process::id();
        record.owner_start_time = start_time(std::process::id()).wrapping_add(999_999);
        write_records(&[record]);

        assert!(reclaim_orphans(Path::new("/irrelevant"), Path::new(NOT_QR_EXE)).await);
        assert!(is_effectively_dead(g.runner.id()));
    }
}
