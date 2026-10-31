// src-tauri/src/test_support.rs
//
// Shared test-only infrastructure (items.id=205, 2026-08-01).
//
// ROOT CAUSE THIS FIXES: providers::utils, auth::user_store, and
// commands::auth each independently declared their own `static ENV_MUTEX:
// Mutex<()>` to serialize tests that mutate the process-global QR_DATA_ROOT
// env var. These were three textually-identical but STRUCTURALLY
// INDEPENDENT statics -- same name, same type, zero shared identity. Under
// cargo test's default parallel execution, a test in one module could
// freely interleave QR_DATA_ROOT mutations with a test in another module,
// since neither module's mutex had any awareness of the other's existence.
// Confirmed this session: the full suite failed 15 tests when run in
// parallel, and passed 438/438 when forced serial via --test-threads=1 --
// that gap is the signature of exactly this race, not a logic bug in any
// of the tests or the code they exercise.
//
// FIX: one true mutex, here, imported by every module that needs to
// serialize QR_DATA_ROOT-mutating tests, rather than three independent
// copies of the same pattern.

// tokio::sync::Mutex, not std::sync::Mutex: every setup() helper that uses
// this holds the guard across .await (often for the whole test, via a
// TestEnv field) by design -- that's what serializes the race described
// above -- and a std::sync::MutexGuard held across .await is exactly what
// clippy's await_holding_lock lint (correctly) flags. Same reasoning as
// auth::registry::UnlockedKey's registry Mutex.
#[cfg(test)]
pub static ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// KeyRegistry test harness (items.id=268)
//
// SAME ROOT CAUSE AS ABOVE, caught before it spread further: qr_hosted.rs and
// system.rs had each independently written a byte-identical
// mock_app_with_registry()/populate_registry() pair for standing up a
// KeyRegistry-backed #[tauri::command] test. items.id=268 added
// key_registry: State<'_, KeyRegistry> to four more command modules
// (persona.rs, library.rs, messages.rs, consent.rs) that need the exact
// same harness -- rather than let that become a fifth/sixth/seventh
// textually-identical copy, it's promoted here once, matching the ENV_MUTEX
// precedent above.
// ---------------------------------------------------------------------------

/// `pool` (items.id=483): every command module using this harness now runs
/// against a real shared.db SqlitePool, same as the production Tauri
/// builder's own `.manage(pool)` in main.rs -- a command that touches
/// shared.db via `State<sqlx::SqlitePool>` needs one managed here too, or
/// `app.state::<sqlx::SqlitePool>()` panics. Callers whose commands never
/// touch shared.db still pass one (their own setup()'s pool, even against a
/// tempdir that never ran migrate_shared_db) -- unused by those commands,
/// harmless, and keeps this harness single-shaped rather than forking a
/// second pool-less variant.
#[cfg(test)]
pub fn mock_app_with_registry(pool: sqlx::SqlitePool) -> tauri::App<tauri::test::MockRuntime> {
    use tauri::Manager;

    let app = tauri::test::mock_app();
    app.manage(crate::auth::registry::KeyRegistry::default());
    app.manage(pool);
    app
}

/// Populates an already-managed KeyRegistry. Separate from
/// mock_app_with_registry() (which only constructs the app) because
/// populating requires an .await, and #[tokio::test] already runs in a
/// Tokio runtime: tauri::async_runtime::block_on inside that context panics
/// ("cannot start a runtime from within a runtime") -- see qr_hosted.rs's
/// original version of this comment for the full history.
#[cfg(test)]
pub async fn populate_registry(
    registry: &tauri::State<'_, crate::auth::registry::KeyRegistry>,
    user_id: &str,
    master_key: [u8; crate::auth::kdf::MASTER_KEY_LEN],
) {
    let (sharing_private_key, _) =
        crate::auth::sharing_keypair::derive_sharing_keypair(&master_key, user_id, 1);
    registry
        .replace(crate::auth::registry::UnlockedKey {
            user_id: user_id.to_owned(),
            data_key: master_key,
            key_epoch: 1,
            sharing_private_key: sharing_private_key.to_bytes(),
            unlocked_at: crate::providers::utils::now(),
        })
        .await;
}

// ---------------------------------------------------------------------------
// Child-process helpers for the Ollama sidecar tests (items.id=668)
//
// Every test that spawns a child owns it through a `ChildGuard`, so a failed
// assertion cannot leave a process behind. The guard only ever signals the
// child it wraps (and, for a group leader the test itself created, that
// group): no test signals a pid it did not spawn.
// ---------------------------------------------------------------------------

#[cfg(all(test, unix))]
pub mod procs {
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};

    /// A copy of the system executable `system_exe` under the file name
    /// `name` in `dir`. A shebang script would show its interpreter as the
    /// process's exe, so ownership tests that depend on the exe file name
    /// (`qr-ollama`, `llama-server`, ...) need a real binary under that name.
    pub fn named_copy(dir: &Path, system_exe: &str, name: &str) -> PathBuf {
        let dst = dir.join(name);
        std::fs::copy(system_exe, &dst)
            .unwrap_or_else(|e| panic!("could not copy {system_exe} to {dst:?}: {e}"));
        dst
    }

    /// Kills (and reaps) the wrapped child on drop. When the child is a
    /// process-group leader the test created itself, the group is killed
    /// first, while the leader is still unreaped (an unreaped pid cannot be
    /// recycled), so a member that outlived it cannot leak either.
    pub struct ChildGuard {
        child: Option<Child>,
        leads_own_group: bool,
    }

    impl ChildGuard {
        pub fn new(child: Child) -> Self {
            Self {
                child: Some(child),
                leads_own_group: false,
            }
        }

        /// For a child spawned with `process_group(0)` by the test.
        pub fn group_leader(child: Child) -> Self {
            Self {
                child: Some(child),
                leads_own_group: true,
            }
        }

        pub fn id(&self) -> u32 {
            self.child.as_ref().expect("child already reaped").id()
        }

        /// SIGKILL this child alone and reap it.
        pub fn kill_and_wait(&mut self) {
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(mut c) = self.child.take() {
                if self.leads_own_group {
                    let pgid = c.id() as libc::pid_t;
                    let own = unsafe { libc::getpgid(0) };
                    if pgid > 1 && pgid != own {
                        unsafe {
                            libc::kill(-pgid, libc::SIGKILL);
                        }
                    }
                }
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }

    /// `spawn()`, retried while the kernel answers ETXTBSY ("text file
    /// busy"): another test thread that forked while this test still had the
    /// freshly written copy open for writing holds that descriptor until its
    /// own exec, and executing the copy in that window fails.
    pub fn spawn_retry(cmd: &mut Command) -> Child {
        for _ in 0..50 {
            match cmd.spawn() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                other => return other.expect("failed to spawn test child"),
            }
        }
        cmd.spawn().expect("failed to spawn test child")
    }

    /// A long-lived process whose exe is the copy at `program` (a copy of
    /// `sleep`, see [`named_copy`]), in process group `group` (`0` starts a
    /// new one, a positive pgid joins it). `extra_env` is added to its
    /// environment. One process, no children of its own.
    pub fn spawn_sleep_stub(
        program: &Path,
        group: libc::pid_t,
        extra_env: &[(&str, &str)],
    ) -> ChildGuard {
        let mut cmd = Command::new(program);
        cmd.arg("3600").stdin(Stdio::null());
        cmd.process_group(group);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let child = spawn_retry(&mut cmd);
        if group == 0 {
            ChildGuard::group_leader(child)
        } else {
            ChildGuard::new(child)
        }
    }

    pub fn start_time_of(pid: u32) -> u64 {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
            true,
            ProcessRefreshKind::everything(),
        );
        sys.process(Pid::from_u32(pid))
            .unwrap_or_else(|| panic!("pid {pid} not found"))
            .start_time()
    }

    /// The pid of a process that has already exited and been reaped: a
    /// stand-in for a dead owner or leader.
    pub fn dead_pid() -> u32 {
        let mut child = Command::new("true").spawn().expect("spawn `true`");
        let pid = child.id();
        let _ = child.wait();
        pid
    }

    /// Whether `pid` is gone, or only a zombie awaiting its parent's reap.
    pub fn is_effectively_dead(pid: u32) -> bool {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::everything(),
        );
        sys.process(Pid::from_u32(pid))
            .map(|p| p.status() == ProcessStatus::Zombie)
            .unwrap_or(true)
    }
}

/// Serializes on `ENV_MUTEX` (the caller holds it) and points `QR_DATA_ROOT`
/// at a scratch directory for as long as it lives, so a test that reads or
/// writes the sidecar pidfile can never touch a real one. Restores the
/// previous value on drop, including on a failed assertion.
#[cfg(test)]
pub struct DataRoot {
    saved: Option<String>,
    tmp: tempfile::TempDir,
}

#[cfg(test)]
impl DataRoot {
    pub fn isolated() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let saved = std::env::var("QR_DATA_ROOT").ok();
        std::env::set_var("QR_DATA_ROOT", tmp.path());
        Self { saved, tmp }
    }

    pub fn path(&self) -> &std::path::Path {
        self.tmp.path()
    }
}

#[cfg(test)]
impl Drop for DataRoot {
    fn drop(&mut self) {
        match &self.saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }
}

/// SIGKILLs a process group on drop, but refuses pgid <= 1 and this process's
/// own group. For a group the test created itself through the code under
/// test (a sidecar start), where the test has no `Child` of its own to wrap.
#[cfg(all(test, unix))]
pub struct KillGroupOnDrop(pub libc::pid_t);

#[cfg(all(test, unix))]
impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        let own = unsafe { libc::getpgid(0) };
        if self.0 > 1 && self.0 != own {
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }
}
