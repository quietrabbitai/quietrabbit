// src-tauri/src/test_harness.rs
//
// Process-level hooks for the quietrabbit_lib unit-test binary only
// (items.id=846; lib.rs gates this module on cfg(all(test, target_os =
// "linux"))). Nothing here compiles into the app or into any other binary.
//
// Two hooks, both installed by one .init_array entry that runs before main()
// and so before libtest starts any test thread:
//
// A. FIRST-INIT RACE (items.id=805, decisions.id=914). sqlite3_initialize()
//    sets `isInit = 1` and releases its init mutex before it runs SQLCipher's
//    SQLITE_EXTRA_INIT hook. A second thread that opens a connection in that
//    window takes the already-initialized fast path and its `PRAGMA key` fails
//    with "PRAGMA key requires a key of one or more characters" (or "sqlcipher
//    not initialized 1") on a valid key. The integration test binaries close
//    the window with a Once in tests/common/mod.rs; this binary has no single
//    entry point, so the initialization runs here, single-threaded, before any
//    test exists.
//
// B. EXIT DRAIN (items.id=801, decisions.id=911). SQLCipher registers
//    sqlcipher_atexit during its first init; it frees the crypto provider, the
//    private heap and the mutexes without waiting for open connections. libtest
//    calls exit() after the last test, and a sqlx-sqlite worker thread that is
//    still inside sqlite3_close (a connection that was dropped, not closed:
//    about 346 sites in this binary) then runs against freed state and the
//    process dies with SIGSEGV or SIGABRT after "test result: ok". atexit
//    handlers run in reverse registration order, so a handler registered
//    AFTER hook A's initialization runs BEFORE sqlcipher_atexit. It waits,
//    bounded, until no sqlx-sqlite worker thread has been seen for DRAIN_QUIET. sqlx exposes no join
//    handle, so the wait scans /proc/self/task.
//
// The drain is a bounded wait, not a guarantee: a worker whose connection is
// still held by something that outlives main (a leaked or static pool) never
// finishes, and the wait gives up silently after DRAIN_CAP. It never fails or
// delays a run beyond that cap and it does not retry anything.

use std::time::{Duration, Instant};

/// The kernel truncates a thread name to 15 bytes in /proc/<pid>/task/<tid>/comm,
/// so sqlx-sqlite's "sqlx-sqlite-worker-{id}" (sqlx-sqlite 0.8 options/mod.rs)
/// reads as exactly this prefix.
const WORKER_COMM_PREFIX: &str = "sqlx-sqlite-wor";

/// Hard cap on the drain. A healthy exit finishes in milliseconds.
const DRAIN_CAP: Duration = Duration::from_secs(3);

const DRAIN_POLL: Duration = Duration::from_millis(2);

/// No worker may be seen for this long before the drain returns. exit() runs
/// while finished tests' threads are still unwinding (a thread named after its
/// test is still listed in about a tenth of exits), and a single empty scan
/// left residual crashes at about 1 in 6000 exits (items.id=846 measurement).
const DRAIN_QUIET: Duration = Duration::from_millis(20);

#[used]
#[link_section = ".init_array"]
static HARNESS_INIT: extern "C" fn() = harness_init;

extern "C" fn harness_init() {
    // SAFETY: sqlite3_initialize takes no arguments and may be called at any
    // time. This runs from .init_array, single-threaded and before main(), so
    // it cannot race another initializer. A non-OK result is left for the
    // first test that opens a database to report.
    unsafe {
        libsqlite3_sys::sqlite3_initialize();
        // Registered after sqlite3_initialize() returned, i.e. after
        // sqlcipher_extra_init registered sqlcipher_atexit, so it runs first.
        libc::atexit(drain_sqlx_workers);
    }
}

extern "C" fn drain_sqlx_workers() {
    let deadline = Instant::now() + DRAIN_CAP;
    let mut quiet_since = Instant::now();
    while Instant::now() < deadline {
        if sqlx_worker_threads_alive() {
            quiet_since = Instant::now();
        } else if quiet_since.elapsed() >= DRAIN_QUIET {
            return;
        }
        std::thread::sleep(DRAIN_POLL);
    }
}

fn sqlx_worker_threads_alive() -> bool {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return false;
    };
    tasks.flatten().any(|task| {
        // A thread that exits between the listing and the read is simply gone.
        std::fs::read_to_string(task.path().join("comm"))
            .is_ok_and(|comm| comm.starts_with(WORKER_COMM_PREFIX))
    })
}
