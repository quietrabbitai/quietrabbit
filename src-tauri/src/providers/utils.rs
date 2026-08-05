// src-tauri/src/providers/utils.rs
//
// Shared database utilities — imported by all persistence stores.
// Full port of providers/utils.py.
//
// DATABASE ACCESS CONTRACTS (matches Python oracle exactly):
//
//   connect_options_unencrypted() — for shared.db only (instance-level, no key)
//     Sets journal_mode. No PRAGMA key.
//
//   connect_options_encrypted()   — for personal.db, outputs.db, integration_keys.db
//     PRAGMA key MUST be first (SQLCipher requirement).
//     journal_mode is set AFTER key via .pragma() call order.
//     Never use connect_options_unencrypted() for encrypted databases.
//
// PRAGMA format (corrected items.id=206, 2026-08-02 -- the prior version of
// this comment asserted the wrong form was correct; see the function doc
// comment below for the full explanation and the proof source):
//   .pragma("key", format!("\"x'{key_hex}'\""))  — outer double quotes required
//   .pragma("journal_mode", "WAL"|"DELETE")       — string value, not SqliteJournalMode enum
//
// Path construction: /users/{user_id}/personas/{persona_id}/
// (D6-224/D6-225: path_id -> focus_id; D6-298: lives -> personas)
//
// Auto-migration: not reproduced here. sqlx migrator handles schema init
// (commit 7b7ad89). Stores call sqlx::migrate!() directly on pool open.

use std::path::{Path, PathBuf};

use sqlx::sqlite::SqliteConnectOptions;

// ---------------------------------------------------------------------------
// Timestamp
// ---------------------------------------------------------------------------

/// Return the current UTC time as an RFC3339 string.
/// Python oracle: datetime.now(timezone.utc).isoformat()
/// RFC3339-compatible; precision intentionally differs (Rust nanosecond
/// vs Python microsecond). Difference is inconsequential for TEXT storage.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// Data root
// ---------------------------------------------------------------------------

/// Returns QR_DATA_ROOT as a PathBuf.
/// Falls back to ./quietrabbit-data for Topology A (zero-config).
/// Python oracle: get_data_root()
pub fn get_data_root() -> PathBuf {
    let root = std::env::var("QR_DATA_ROOT").unwrap_or_else(|_| "./quietrabbit-data".to_owned());
    PathBuf::from(root)
}

// ---------------------------------------------------------------------------
// Journal mode
// ---------------------------------------------------------------------------

/// Returns the journal mode pragma value for this environment.
/// QR_NETWORK_STORAGE=true -> "DELETE" (rollback journal, NAS-safe)
/// Otherwise -> "WAL" (default, local storage)
/// Only "true" (case-insensitive) is accepted — matches Python oracle:
///   os.environ.get("QR_NETWORK_STORAGE", "false").lower() == "true"
/// Python oracle: _apply_journal_mode()
pub fn journal_mode_value() -> &'static str {
    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    if network_storage {
        "DELETE"
    } else {
        "WAL"
    }
}

// ---------------------------------------------------------------------------
// Path construction
// ---------------------------------------------------------------------------

/// Path to instance/shared.db — unencrypted, readable before any user login.
/// Python oracle: get_data_root() / "instance" / "shared.db"
pub fn db_path_shared() -> PathBuf {
    get_data_root().join("instance").join("shared.db")
}

/// Path to a user's personal.db (encrypted).
/// Python oracle: get_data_root() / "users" / user_id / "personas" / persona_id / "personal.db"
pub fn db_path_personal(user_id: &str, persona_id: &str) -> PathBuf {
    get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("personal.db")
}

/// Path to a user's outputs.db (encrypted).
/// Python oracle: get_data_root() / "users" / user_id / "personas" / persona_id / "outputs.db"
pub fn db_path_outputs(user_id: &str, persona_id: &str) -> PathBuf {
    get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("outputs.db")
}

/// Path to a user's integration_keys.db (encrypted).
/// Python oracle: get_data_root() / "users" / user_id / "integration_keys.db"
pub fn db_path_integration_keys(user_id: &str) -> PathBuf {
    get_data_root()
        .join("users")
        .join(user_id)
        .join("integration_keys.db")
}

/// Path to a user's tier3_cookies.db (encrypted). Per-user, not per-persona
/// -- mirrors db_path_integration_keys's shape exactly (items.id=224
/// resolution, decisions.id=711; see tier3_cookies_001.sql's own header).
pub fn db_path_tier3_cookies(user_id: &str) -> PathBuf {
    get_data_root()
        .join("users")
        .join(user_id)
        .join("tier3_cookies.db")
}

// ---------------------------------------------------------------------------
// SqliteConnectOptions builders
// ---------------------------------------------------------------------------

/// SqliteConnectOptions for UNENCRYPTED databases (shared.db only).
/// No PRAGMA key. Journal mode applied via .pragma().
/// Python oracle: open_db() / open_instance_db()
pub fn connect_options_unencrypted(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .pragma("journal_mode", journal_mode_value())
}

/// SqliteConnectOptions for ENCRYPTED databases (personal.db, outputs.db,
/// integration_keys.db).
///
/// SQLCipher contract: PRAGMA key MUST be the first operation after connection
/// open. .pragma() calls are applied in order — key is set before journal_mode.
///
/// PRAGMA format: "x'{key_hex}'" — the x'...' blob-literal form itself
/// wrapped in an outer pair of double quotes. SQLCipher's PRAGMA key
/// blob-literal syntax requires the outer quotes; omitting them produces
/// "syntax error near x'...'" on every SQLite/SQLCipher build tested
/// (items.id=206, 2026-08-02). This doc comment previously asserted the
/// opposite (no outer quotes) -- that was the bug. Proven correct against
/// commands/auth.rs::verify_integration_keys_db, which isolated the exact
/// failure via a standalone diagnostic test.
///
/// key_hex: hex-encoded key string from InMemoryKeyRegistry.
/// Python oracle: open_personal_db() / open_outputs_db() / open_integration_keys_db()
pub fn connect_options_encrypted(path: &Path, key_hex: &str) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .pragma("key", format!("\"x'{key_hex}'\"")) // FIRST — SQLCipher requirement
        .pragma("journal_mode", journal_mode_value()) // AFTER key
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Serialize all env-mutating tests — Rust test runner is multi-threaded.
    // ENV_MUTEX must be acquired before any set_var/remove_var call.
    // Shared across every test module that mutates QR_DATA_ROOT (items.id=
    // 205, 2026-08-01) -- see test_support.rs for why this must be one
    // true mutex, not a per-module copy.
    use crate::test_support::ENV_MUTEX;

    #[test]
    fn now_is_nonempty_rfc3339() {
        let ts = now();
        assert!(!ts.is_empty());
        assert!(ts.contains('T'));
    }

    #[test]
    fn get_data_root_default() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let saved = std::env::var("QR_DATA_ROOT").ok();
        std::env::remove_var("QR_DATA_ROOT");

        let root = get_data_root();
        assert_eq!(root, PathBuf::from("./quietrabbit-data"));

        if let Some(v) = saved {
            std::env::set_var("QR_DATA_ROOT", v);
        }
    }

    #[test]
    fn get_data_root_env_override() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let saved = std::env::var("QR_DATA_ROOT").ok();

        std::env::set_var("QR_DATA_ROOT", "/tmp/qr-test-root");
        let root = get_data_root();
        assert_eq!(root, PathBuf::from("/tmp/qr-test-root"));

        std::env::remove_var("QR_DATA_ROOT");
        if let Some(v) = saved {
            std::env::set_var("QR_DATA_ROOT", v);
        }
    }

    #[test]
    fn db_path_shared_contains_instance() {
        let path = db_path_shared();
        let s = path.to_string_lossy();
        assert!(s.contains("instance"));
        assert!(s.ends_with("shared.db"));
    }

    #[test]
    fn db_path_personal_correct_structure() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let saved = std::env::var("QR_DATA_ROOT").ok();

        std::env::set_var("QR_DATA_ROOT", "/data");
        let path = db_path_personal("user-1", "persona-abc");
        assert_eq!(
            path,
            PathBuf::from("/data/users/user-1/personas/persona-abc/personal.db")
        );

        std::env::remove_var("QR_DATA_ROOT");
        if let Some(v) = saved {
            std::env::set_var("QR_DATA_ROOT", v);
        }
    }

    #[test]
    fn db_path_outputs_correct_structure() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let saved = std::env::var("QR_DATA_ROOT").ok();

        std::env::set_var("QR_DATA_ROOT", "/data");
        let path = db_path_outputs("user-1", "persona-abc");
        assert_eq!(
            path,
            PathBuf::from("/data/users/user-1/personas/persona-abc/outputs.db")
        );

        std::env::remove_var("QR_DATA_ROOT");
        if let Some(v) = saved {
            std::env::set_var("QR_DATA_ROOT", v);
        }
    }

    #[test]
    fn db_path_integration_keys_correct_structure() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let saved = std::env::var("QR_DATA_ROOT").ok();

        std::env::set_var("QR_DATA_ROOT", "/data");
        let path = db_path_integration_keys("user-1");
        assert_eq!(
            path,
            PathBuf::from("/data/users/user-1/integration_keys.db")
        );

        std::env::remove_var("QR_DATA_ROOT");
        if let Some(v) = saved {
            std::env::set_var("QR_DATA_ROOT", v);
        }
    }

    #[test]
    fn connect_options_encrypted_constructs_without_panic() {
        let path = Path::new("/tmp/test.db");
        let _opts = connect_options_encrypted(path, "deadbeef1234");
    }

    /// Regression test for items.id=206: connect_options_encrypted's
    /// PRAGMA key blob-literal form was missing the outer double quotes
    /// SQLCipher requires. connect_options_encrypted_constructs_without_
    /// panic above never calls .connect(), so it could not have caught
    /// this -- it only proves the SqliteConnectOptions builder doesn't
    /// panic, not that the resulting PRAGMA is valid SQL. This test
    /// actually connects: it creates a real encrypted file via a
    /// bootstrap connect using the same corrected PRAGMA form, then
    /// reopens it through connect_options_encrypted itself, proving the
    /// fix round-trips against a real file. This also covers
    /// output_store.rs::open_outputs_db, topic_store.rs::open_outputs_db,
    /// and conductor/lifecycle.rs, all of which delegate to
    /// connect_options_encrypted rather than building their own
    /// SqliteConnectOptions.
    #[tokio::test]
    async fn connect_options_encrypted_opens_a_real_encrypted_file() {
        use sqlx::ConnectOptions;

        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let db_path = tempdir.path().join("test.db");
        let key_hex = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";

        // Bootstrap: create the file under the key via
        // connect_options_encrypted itself (create_if_missing(true)),
        // establishing the key on first write.
        {
            let mut conn = connect_options_encrypted(&db_path, key_hex)
                .connect()
                .await
                .expect("bootstrap connect with corrected PRAGMA form must succeed");
            sqlx::query("CREATE TABLE t (id INTEGER)")
                .execute(&mut conn)
                .await
                .expect("must be able to write to a freshly-keyed encrypted db");
        }

        // Verification: connect_options_encrypted must be able to reopen
        // the same file under the same key.
        let reopened = connect_options_encrypted(&db_path, key_hex)
            .create_if_missing(false)
            .connect()
            .await;
        assert!(
            reopened.is_ok(),
            "connect_options_encrypted must reopen a real encrypted file with the corrected PRAGMA form: {reopened:?}"
        );
    }

    #[test]
    fn connect_options_unencrypted_constructs_without_panic() {
        let path = Path::new("/tmp/shared.db");
        let _opts = connect_options_unencrypted(path);
    }

    #[test]
    fn journal_mode_value_default_is_wal() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let saved = std::env::var("QR_NETWORK_STORAGE").ok();
        std::env::remove_var("QR_NETWORK_STORAGE");

        assert_eq!(journal_mode_value(), "WAL");

        if let Some(v) = saved {
            std::env::set_var("QR_NETWORK_STORAGE", v);
        }
    }

    #[test]
    fn journal_mode_value_network_storage_true_is_delete() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let saved = std::env::var("QR_NETWORK_STORAGE").ok();

        std::env::set_var("QR_NETWORK_STORAGE", "true");
        assert_eq!(journal_mode_value(), "DELETE");

        std::env::remove_var("QR_NETWORK_STORAGE");
        if let Some(v) = saved {
            std::env::set_var("QR_NETWORK_STORAGE", v);
        }
    }
}
