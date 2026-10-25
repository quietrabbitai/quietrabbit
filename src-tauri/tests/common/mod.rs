// src-tauri/tests/common/mod.rs
//
// Helpers shared by the SQLCipher integration test binaries. Lives in a
// subdirectory so cargo does not build it as a test binary of its own.

use std::sync::Once;

/// items.id=802: finish SQLite/SQLCipher's one-time initialization before any
/// test opens a connection.
///
/// sqlite3_initialize() sets `isInit = 1` and releases its init mutex before it
/// runs SQLCipher's SQLITE_EXTRA_INIT (`sqlcipher_extra_init`), which is what
/// sets `sqlcipher_init`. A second thread that calls sqlite3_initialize() in
/// that window takes the already-initialized fast path, opens its connection,
/// and its `PRAGMA key` fails with "PRAGMA key requires a key of one or more
/// characters" even though the key is valid. libtest starts every test on its
/// own thread, so the tests' first connections can land in that window. Running
/// the initialization once, under a `Once` that makes later callers wait for
/// the first to finish, closes it. The app is not exposed: its first open is
/// migrate_shared_db() in main.rs setup(), which runs before any concurrent
/// open.
pub fn init_sqlcipher() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // SAFETY: sqlite3_initialize takes no arguments and is safe to call
        // at any time and from any thread.
        let rc = unsafe { libsqlite3_sys::sqlite3_initialize() };
        assert_eq!(rc, libsqlite3_sys::SQLITE_OK, "sqlite3_initialize failed");
    });
}
