//! Dedicated long-lived OS thread for spawning the Ollama sidecar process.
//!
//! Linux's `PR_SET_PDEATHSIG` arms against the *calling thread*, not the
//! process -- if that thread is a tokio worker that later gets recycled,
//! the kernel delivers the death signal immediately even though the app is
//! still running (items.id=586's own scoped pitfall: "the signal follows
//! the spawning thread"). This module owns one `std::thread`, created once
//! and never returning for the life of the process, that performs every
//! spawn of the sidecar.
//!
//! # Design note: plain `std::process::Child`, not `tokio::process::Child`
//!
//! items.id=586's plan (Jason's amendment 5) called for empirically proving
//! a `tokio::process::Child` spawned on the dedicated thread's own
//! single-threaded runtime could safely be `.wait()`ed from a *different*
//! runtime before committing to that design. It does not hold: `wait()`
//! hung (confirmed via a real test run, not just reasoning about it) --
//! tokio's process type registers itself against the specific runtime
//! reactor active at spawn time, and polling it from an unrelated runtime
//! later just never completes.
//!
//! The fix is simpler than the design it replaces, not a workaround: plain
//! `std::process::Command`/`Child` have no reactor registration at all.
//! `fork()`+`exec()` (via `Command::spawn()`) is the only part that must
//! happen on this dedicated thread (so `pre_exec`'s `PR_SET_PDEATHSIG` call
//! arms against a thread that outlives the child); `try_wait()` is a plain
//! non-blocking syscall callable from anywhere afterward, and a genuinely
//! blocking `.wait()` is wrapped in `tokio::task::spawn_blocking` by
//! [`SpawnedChild::wait`] so it doesn't stall an async worker thread.
//! Killing goes through `ollama_ownership::kill_process_group` (by pgid,
//! not through `Child::kill()`) uniformly for both the top-level process
//! and any runner it spawned, so `SpawnedChild` itself only needs to carry
//! `pid`/`pgid`, not reimplement signaling.
//!
//! Platform scope (items.id=586 plan, decision #6): `process_group(0)` is
//! `cfg(unix)` -- both Linux and macOS support process groups the same way.
//! `PR_SET_PDEATHSIG` itself is Linux-only; there is no macOS kernel
//! equivalent. The fork/exec TOCTOU close (`parent_still_matches`) runs on
//! every unix target regardless -- on Linux it closes the gap between
//! `fork()` and the `prctl()` call actually registering (the real parent
//! dying in that exact window would otherwise arm the signal too late to
//! ever fire); on macOS it only catches a parent already gone at the
//! fork/exec instant, not one that dies afterward -- there is no primitive
//! to catch that case there (see the plan's macOS follow-on note). Windows
//! needs an entirely different mechanism (a Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) and is out of scope here, also
//! tracked as a follow-on.
//!
//! # Scrubbed environment (items.id=616)
//! The child does not inherit QR's environment. [`do_spawn`] clears it and
//! passes through only the variables in [`INHERITED_ENV`] (plus the explicit
//! `envs` the caller supplies, which win). The sidecar is the one QR process
//! that parses model files and talks to a model registry, and decisions.id=840
//! records a memory-disclosure CVE that exposes a process's environment
//! variables, so it must not hold the launching shell's API keys or tokens.
//! Every other `OLLAMA_*` variable is deliberately dropped too: those
//! configure the user's own Ollama, which QR never uses (decisions.id=840).

use std::ffi::OsString;
use std::io;
use std::process::ExitStatus;
use std::sync::mpsc as std_mpsc;
use std::sync::OnceLock;

/// Variables the sidecar may inherit from QR's own environment. Everything
/// else is dropped (items.id=616). Sources: Ollama's docs/gpu.mdx, docs/faq.mdx
/// and envconfig/config.go for the GPU, `HOME` and proxy entries; the Vulkan
/// loader (`VK_*`, `XDG_DATA_DIRS`) and Go's TLS stack (`SSL_CERT_*`) entries
/// are not from Ollama's docs. `LD_LIBRARY_PATH` is not mentioned by Ollama's
/// docs either; it stays so a user's own CUDA/ROCm library path keeps working.
/// `HTTP_PROXY` is left out on purpose (Ollama's FAQ says to avoid it).
const INHERITED_ENV: &[&str] = &[
    // Process basics.
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "XDG_DATA_DIRS",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    // Shared-library lookup.
    "LD_LIBRARY_PATH",
    // NVIDIA.
    "CUDA_VISIBLE_DEVICES",
    // AMD.
    "ROCR_VISIBLE_DEVICES",
    "HIP_VISIBLE_DEVICES",
    "GPU_DEVICE_ORDINAL",
    "HSA_OVERRIDE_GFX_VERSION",
    // Vulkan.
    "GGML_VK_VISIBLE_DEVICES",
    "VK_ICD_FILENAMES",
    "VK_DRIVER_FILES",
    // Model pulls through a proxy or a custom CA bundle.
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Per-device AMD override: `HSA_OVERRIDE_GFX_VERSION_<N>` (docs/gpu.mdx).
const HSA_PER_DEVICE_PREFIX: &str = "HSA_OVERRIDE_GFX_VERSION_";

fn is_inherited_allowed(name: &str) -> bool {
    if INHERITED_ENV.contains(&name) {
        return true;
    }
    match name.strip_prefix(HSA_PER_DEVICE_PREFIX) {
        Some(n) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// The allowlisted subset of `vars`, in their original order.
fn scrub_env(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    vars.filter(|(k, _)| k.to_str().is_some_and(is_inherited_allowed))
        .collect()
}

struct SpawnRequest {
    program: OsString,
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
    reply: tokio::sync::oneshot::Sender<io::Result<SpawnedChild>>,
}

static SPAWN_THREAD: OnceLock<std_mpsc::Sender<SpawnRequest>> = OnceLock::new();

fn spawn_thread_sender() -> &'static std_mpsc::Sender<SpawnRequest> {
    SPAWN_THREAD.get_or_init(|| {
        let (tx, rx) = std_mpsc::channel::<SpawnRequest>();
        std::thread::Builder::new()
            .name("ollama-spawn".to_owned())
            .spawn(move || thread_main(rx))
            .expect("quietrabbit: failed to start dedicated ollama spawn thread");
        tx
    })
}

/// Runs for the lifetime of the process -- never returns in practice. Its
/// own continued existence for as long as any spawned child should keep
/// `PR_SET_PDEATHSIG` armed is the entire point of this module.
fn thread_main(rx: std_mpsc::Receiver<SpawnRequest>) {
    for req in rx {
        let result = do_spawn(req.program, req.args, req.envs);
        let _ = req.reply.send(result);
    }
}

/// A child spawned via [`spawn_with_pdeathsig`]. Carries `pid`/`pgid`
/// (equal, by construction -- see [`do_spawn`]) for callers that signal it
/// via `ollama_ownership::kill_process_group`, plus the handle needed to
/// actually reap it.
pub struct SpawnedChild {
    pub pid: u32,
    pub pgid: libc::pid_t,
    inner: std::process::Child,
}

impl SpawnedChild {
    /// Non-blocking -- a single `waitpid(WNOHANG)` syscall, safe to call
    /// directly from an async context without `spawn_blocking`.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.inner.try_wait()
    }

    /// Blocks until the child exits. Runs on tokio's blocking thread pool
    /// (`spawn_blocking`), not whatever thread spawned this child -- unlike
    /// `tokio::process::Child`, `std::process::Child::wait()` has no
    /// reactor-registration tie to a particular runtime, so any thread may
    /// call it on any `Child`; `spawn_blocking` is used only to avoid
    /// stalling an async worker thread on a blocking syscall, not to work
    /// around a correctness constraint.
    pub async fn wait(self) -> io::Result<ExitStatus> {
        let mut inner = self.inner;
        tokio::task::spawn_blocking(move || inner.wait())
            .await
            .map_err(io::Error::other)?
    }
}

#[cfg(unix)]
fn do_spawn(
    program: OsString,
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
) -> io::Result<SpawnedChild> {
    use std::os::unix::process::CommandExt;

    // Captured before fork() -- this is "the expected parent" the child
    // checks itself against immediately after exec-prep, not something
    // read fresh inside the child (by then getppid() may already have
    // changed, which is exactly the race being guarded against).
    let expected_parent = std::process::id();

    let mut cmd = std::process::Command::new(&program);
    cmd.args(&args);
    // items.id=616: nothing but the allowlist is inherited; the caller's
    // explicit `envs` are applied last and win.
    cmd.env_clear();
    for (k, v) in scrub_env(std::env::vars_os()) {
        cmd.env(k, v);
    }
    for (k, v) in &envs {
        cmd.env(k, v);
    }
    // pgid 0 -> the kernel uses the new child's own pid as its pgid, so
    // `pid == pgid` below is guaranteed, not just usually true.
    cmd.process_group(0);

    // SAFETY: the closure only calls async-signal-safe functions
    // (prctl, getppid, _exit) between fork() and exec(), as required by
    // `pre_exec`'s own safety contract.
    unsafe {
        cmd.pre_exec(move || pre_exec_hook(expected_parent));
    }

    let inner = cmd.spawn()?;
    let pid = inner.id();
    Ok(SpawnedChild {
        pid,
        pgid: pid as libc::pid_t,
        inner,
    })
}

/// Runs in the child, between `fork()` and `exec()`. Arms the Linux parent-
/// death signal where available, then re-checks the parent itself closed
/// the fork/exec TOCTOU gap described in the module doc comment.
#[cfg(unix)]
fn pre_exec_hook(expected_parent: u32) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl is async-signal-safe; PR_SET_PDEATHSIG takes the
        // signal number as its second argument.
        let rc = unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0,
                0,
                0,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    if !parent_still_matches(expected_parent) {
        // The real parent is already gone (reparented to init/the
        // subreaper) -- no signal we could have armed above would ever
        // fire for a parent that already died. Exit now rather than run
        // orphaned. _exit(), not exit(): no atexit/Drop machinery is safe
        // to run this deep into fork/exec setup.
        unsafe { libc::_exit(1) };
    }

    Ok(())
}

/// Pure comparison, extracted so it's unit-testable in isolation. The true
/// fork-timing race this guards against -- the real parent dying in the
/// exact gap between `fork()` and this check running -- can't be
/// deterministically reproduced in a test; that's documented here rather
/// than faked with a flaky timing-dependent test.
#[cfg(unix)]
fn parent_still_matches(expected: u32) -> bool {
    (unsafe { libc::getppid() }) as u32 == expected
}

/// Spawn `program` with `args`/`envs` from the dedicated long-lived thread
/// (see module doc comment for why).
pub async fn spawn_with_pdeathsig(
    program: impl Into<OsString>,
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
) -> io::Result<SpawnedChild> {
    let (reply, reply_rx) = tokio::sync::oneshot::channel();
    let req = SpawnRequest {
        program: program.into(),
        args,
        envs,
        reply,
    };
    spawn_thread_sender()
        .send(req)
        .map_err(|_| io::Error::other("ollama spawn thread is gone"))?;
    reply_rx
        .await
        .map_err(|_| io::Error::other("ollama spawn thread dropped its reply"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_still_matches_detects_mismatch() {
        let real = unsafe { libc::getppid() } as u32;
        assert!(parent_still_matches(real));
        assert!(!parent_still_matches(real.wrapping_add(1)));
    }

    /// items.id=586 plan amendment 5 (Jason): prove the handoff across the
    /// dedicated thread and the main runtime is actually safe, not just
    /// reasoned about. `pid == pgid` (by construction) and `try_wait`/
    /// `wait` both behave correctly when called from this test's own
    /// (different) runtime, and no zombie is left behind.
    #[tokio::test]
    async fn child_spawned_on_dedicated_thread_is_usable_from_main_runtime() {
        let mut short_lived =
            spawn_with_pdeathsig("/bin/sh", vec!["-c".into(), "sleep 0.2".into()], vec![])
                .await
                .expect("spawn via dedicated thread should succeed");
        assert_eq!(short_lived.pid, short_lived.pgid as u32);

        // Non-blocking try_wait, polled from this runtime, before exit.
        assert!(short_lived
            .try_wait()
            .expect("try_wait should not error")
            .is_none());

        let status = tokio::time::timeout(std::time::Duration::from_secs(5), short_lived.wait())
            .await
            .expect("wait() must not hang when called from a different runtime than spawned on")
            .expect("wait() should succeed");
        assert!(status.success());

        // kill_process_group (items.id=668) only signals a group member whose
        // executable is named like QR's own sidecar -- give this child that
        // name (a copy of `sleep`, so the process image really is called
        // `qr-ollama`) so the group-kill path actually fires here, rather
        // than falling back to "nothing provably ours in this group".
        let dir = tempfile::tempdir().unwrap();
        let stub = crate::test_support::procs::named_copy(dir.path(), "/bin/sleep", "qr-ollama");
        let long_lived = spawn_with_pdeathsig(stub.as_os_str(), vec!["30".into()], vec![])
            .await
            .expect("spawn via dedicated thread should succeed");
        let _kill = crate::test_support::KillGroupOnDrop(long_lived.pgid);
        let pid = long_lived.pid;

        crate::ollama_ownership::kill_process_group(long_lived.pgid).await;
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), long_lived.wait())
            .await
            .expect("wait() after group-kill must not hang")
            .expect("wait() after group-kill should succeed");
        assert!(!status.success());

        // No zombie left behind -- give the kernel a moment to reap.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "pid {pid} should be fully reaped after wait(), not lingering as a zombie"
        );
    }

    #[test]
    fn allowlist_admits_gpu_and_basics_and_rejects_the_rest() {
        for yes in [
            "PATH",
            "HOME",
            "LD_LIBRARY_PATH",
            "CUDA_VISIBLE_DEVICES",
            "ROCR_VISIBLE_DEVICES",
            "HIP_VISIBLE_DEVICES",
            "HSA_OVERRIDE_GFX_VERSION",
            "HSA_OVERRIDE_GFX_VERSION_0",
            "HSA_OVERRIDE_GFX_VERSION_12",
            "GGML_VK_VISIBLE_DEVICES",
            "HTTPS_PROXY",
        ] {
            assert!(is_inherited_allowed(yes), "{yes}");
        }
        for no in [
            "GROQ_API_KEY",
            "MISTRAL_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "OLLAMA_HOST",
            "OLLAMA_MODELS",
            "OLLAMA_ORIGINS",
            "OLLAMA_VULKAN",
            "OLLAMA_IGPU_ENABLE",
            "OLLAMA_LLM_LIBRARY",
            "OLLAMA_API_KEY",
            "HTTP_PROXY",
            "DISPLAY",
            "HSA_OVERRIDE_GFX_VERSION_",
            "HSA_OVERRIDE_GFX_VERSION_x",
            "HSA_OVERRIDE_GFX_VERSIONS",
            "path",
            "",
        ] {
            assert!(!is_inherited_allowed(no), "{no}");
        }
    }

    /// items.id=616: a secret in QR's own environment must not reach the
    /// sidecar, while the allowlisted variables and the caller's explicit
    /// variables do. Uses the exit status rather than captured output
    /// (`do_spawn` does not pipe stdout).
    #[cfg(unix)]
    #[tokio::test]
    async fn spawned_child_does_not_inherit_secrets_or_user_ollama_settings() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        struct Restore(Vec<(&'static str, Option<OsString>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (k, v) in &self.0 {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
        let names = [
            "GROQ_API_KEY",
            "OLLAMA_HOST",
            "OLLAMA_VULKAN",
            "HSA_OVERRIDE_GFX_VERSION",
        ];
        let _restore = Restore(names.iter().map(|n| (*n, std::env::var_os(n))).collect());
        std::env::set_var("GROQ_API_KEY", "secret-for-test");
        std::env::set_var("OLLAMA_HOST", "192.168.0.2");
        std::env::set_var("OLLAMA_VULKAN", "1");
        std::env::set_var("HSA_OVERRIDE_GFX_VERSION", "10.3.0");

        // Succeeds only if: the secret and the user's OLLAMA_* are absent,
        // QR's explicit OLLAMA_HOST wins, the allowlisted variables
        // (PATH, the GPU override) and the explicit OLLAMA_MODELS arrive.
        let script = r#"
            test -z "${GROQ_API_KEY+x}" || exit 11
            test -z "${OLLAMA_VULKAN+x}" || exit 12
            test "$OLLAMA_HOST" = "127.0.0.1:1" || exit 13
            test "$OLLAMA_MODELS" = "/tmp/qr-env-test" || exit 14
            test "$HSA_OVERRIDE_GFX_VERSION" = "10.3.0" || exit 15
            test -n "$PATH" || exit 16
        "#;
        let child = spawn_with_pdeathsig(
            "/bin/sh",
            vec!["-c".into(), script.into()],
            vec![
                ("OLLAMA_HOST".into(), "127.0.0.1:1".into()),
                ("OLLAMA_MODELS".into(), "/tmp/qr-env-test".into()),
            ],
        )
        .await
        .expect("spawn should succeed");
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .expect("wait() must not hang")
            .expect("wait() should succeed");
        assert!(
            status.success(),
            "child environment check failed with {status} (11 secret leaked, 12 user OLLAMA_* \
             leaked, 13 explicit OLLAMA_HOST lost, 14 OLLAMA_MODELS lost, 15 GPU override \
             dropped, 16 PATH dropped)"
        );
    }

    #[tokio::test]
    async fn spawn_failure_surfaces_as_io_error() {
        let result = spawn_with_pdeathsig(
            "/definitely/not/a/real/binary-quietrabbit-test",
            vec![],
            vec![],
        )
        .await;
        assert!(result.is_err());
    }
}
