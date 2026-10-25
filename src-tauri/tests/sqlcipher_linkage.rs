//! SQLCipher linkage verification.
//!
//! Proves that the build is linked against SQLCipher and not vanilla SQLite
//! by validating behavioral encryption constraints on disk. Plain SQLite
//! silently ignores PRAGMA key — these tests would pass the connect step
//! but allow unencrypted reads, which would fail the wrong-key/no-key assertions.

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use tempfile::NamedTempFile;

// items.id=566: every connection in this file is closed with `close().await`,
// never just dropped. A dropped sqlx-sqlite connection runs sqlite3_close
// later, on its worker thread, so once the last test returns, exit() can run
// SQLCipher's atexit shutdown (frees the crypto provider and private heap)
// while a worker is still inside sqlite3_close, and the process dies with
// SIGSEGV/SIGABRT after "test result: ok". close().await returns only once
// the worker has finished sqlite3_close.

const KEY_HEX: &str = "x'deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef'";
const WRONG_KEY_HEX: &str = "x'0000000000000000000000000000000000000000000000000000000000000000'";

/// Open an encrypted connection using the options builder (avoids URI
/// character-escaping pitfalls). PRAGMA key fires before journal_mode in
/// the pragma batch that executes after sqlite3_open_v2.
///
/// `wal = false` sends no journal_mode pragma (sqlx sends none by default).
/// The wrong-key and no-key opens use it: `PRAGMA journal_mode = WAL` reads
/// page 1, so on a bad key sqlx's connect() fails *after* it has built the
/// connection and drops it without waiting for its worker to run
/// sqlite3_close, and the test can neither close nor await it. Without the
/// pragma connect() succeeds and the rejection shows at the first query, on a
/// connection the test can close (items.id=566).
async fn open_connection(
    path: &std::path::Path,
    key: Option<&str>,
    wal: bool,
) -> Result<sqlx::SqliteConnection, sqlx::Error> {
    let mut opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);

    if let Some(k) = key {
        opts = opts.pragma("key", format!("\"{k}\""));
    }

    if wal {
        opts = opts.pragma("journal_mode", "WAL");
    }

    opts.connect().await
}

/// Assert that a DatabaseError is an authentic encryption rejection.
/// Prefers structured error code (more stable) over message text (fallback).
/// SQLITE_NOTADB = 26: "file is not a database" — SQLCipher's wrong/missing
/// key error, confirmed on SQLCipher 3.51.3.
fn assert_encryption_db_error(db_err: &dyn sqlx::error::DatabaseError) {
    if let Some(code) = db_err.code() {
        if code.as_ref() == "26" {
            return; // SQLITE_NOTADB — confirmed encryption rejection
        }
    }
    // Fallback for other SQLCipher versions that may use different codes.
    let msg = db_err.message().to_lowercase();
    assert!(
        msg.contains("not a database") || msg.contains("malformed") || msg.contains("encrypted"),
        "Expected encryption-related error, got code={:?} message='{}'",
        db_err.code(),
        db_err.message()
    );
}

/// Assert that a Result is an encryption-related failure, tolerating both
/// connect-time rejection (SQLCipher 3.51.3) and deferred query-time
/// rejection (other SQLCipher builds).
fn assert_encryption_error(err: sqlx::Error) {
    match err {
        sqlx::Error::Database(db_err) => assert_encryption_db_error(db_err.as_ref()),
        e => panic!(
            "Expected a Database error for encryption rejection, got: {:?}",
            e
        ),
    }
}

#[tokio::test]
async fn test_sqlcipher_write_and_read() {
    let temp_file = NamedTempFile::new().unwrap();
    let (file_handle, temp_path) = temp_file.into_parts();
    drop(file_handle);

    // Write encrypted data.
    {
        let mut conn = open_connection(&temp_path, Some(KEY_HEX), true)
            .await
            .expect("SQLCipher connection (write) failed");
        sqlx::query("CREATE TABLE t (v TEXT)")
            .execute(&mut conn)
            .await
            .expect("CREATE TABLE failed");
        sqlx::query("INSERT INTO t VALUES ('sentinel')")
            .execute(&mut conn)
            .await
            .expect("INSERT failed");
        conn.close().await.expect("write connection must close");
    }

    // Reopen with correct key — data must be readable.
    {
        let mut conn = open_connection(&temp_path, Some(KEY_HEX), true)
            .await
            .expect("SQLCipher connection (correct key) failed");
        let row: (String,) = sqlx::query_as("SELECT v FROM t")
            .fetch_one(&mut conn)
            .await
            .expect("SELECT failed with correct key");
        conn.close().await.expect("read connection must close");
        assert_eq!(row.0, "sentinel");
    }
}

#[tokio::test]
async fn test_sqlcipher_wrong_key_cannot_read() {
    let temp_file = NamedTempFile::new().unwrap();
    let (file_handle, temp_path) = temp_file.into_parts();
    drop(file_handle);

    {
        let mut conn = open_connection(&temp_path, Some(KEY_HEX), true)
            .await
            .expect("SQLCipher connection (write) failed");
        sqlx::query("CREATE TABLE t (v TEXT)")
            .execute(&mut conn)
            .await
            .expect("CREATE TABLE failed");
        sqlx::query("INSERT INTO t VALUES ('sentinel')")
            .execute(&mut conn)
            .await
            .expect("INSERT failed");
        conn.close().await.expect("write connection must close");
    }

    // Opened without the WAL pragma (see open_connection), so the bad key is
    // rejected at the first query. A connect-time rejection would also be a
    // valid encryption proof, so that arm is kept, but it leaves a dropped
    // connection the test cannot close.
    match open_connection(&temp_path, Some(WRONG_KEY_HEX), false).await {
        Err(sqlx::Error::Database(db_err)) => {
            assert_encryption_db_error(db_err.as_ref());
        }
        Ok(mut conn) => {
            let result: Result<(String,), _> =
                sqlx::query_as("SELECT v FROM t").fetch_one(&mut conn).await;
            conn.close().await.expect("wrong-key connection must close");
            assert!(
                result.is_err(),
                "Wrong key must not be able to read encrypted data"
            );
            assert_encryption_error(result.unwrap_err());
        }
        Err(e) => panic!("Unexpected non-database connection error: {:?}", e),
    }
}

#[tokio::test]
async fn test_sqlcipher_no_key_cannot_read() {
    let temp_file = NamedTempFile::new().unwrap();
    let (file_handle, temp_path) = temp_file.into_parts();
    drop(file_handle);

    {
        let mut conn = open_connection(&temp_path, Some(KEY_HEX), true)
            .await
            .expect("SQLCipher connection (write) failed");
        sqlx::query("CREATE TABLE t (v TEXT)")
            .execute(&mut conn)
            .await
            .expect("CREATE TABLE failed");
        sqlx::query("INSERT INTO t VALUES ('sentinel')")
            .execute(&mut conn)
            .await
            .expect("INSERT failed");
        conn.close().await.expect("write connection must close");
    }

    // No key supplied. Plain SQLite would read this without error —
    // SQLCipher must reject it, proving encryption is active.
    match open_connection(&temp_path, None, false).await {
        Err(sqlx::Error::Database(db_err)) => {
            assert_encryption_db_error(db_err.as_ref());
        }
        Ok(mut conn) => {
            let result: Result<(String,), _> =
                sqlx::query_as("SELECT v FROM t").fetch_one(&mut conn).await;
            conn.close().await.expect("unkeyed connection must close");
            assert!(
                result.is_err(),
                "Unkeyed access must not read encrypted data"
            );
            assert_encryption_error(result.unwrap_err());
        }
        Err(e) => panic!("Unexpected non-database connection error: {:?}", e),
    }
}

#[tokio::test]
async fn test_sqlcipher_wal_mode_active() {
    let temp_file = NamedTempFile::new().unwrap();
    let (file_handle, temp_path) = temp_file.into_parts();
    drop(file_handle);

    let mut conn = open_connection(&temp_path, Some(KEY_HEX), true)
        .await
        .expect("SQLCipher connection failed");

    // Create a table first — some SQLite behaviors are deferred until
    // the database is initialized.
    sqlx::query("CREATE TABLE init_check (id INTEGER PRIMARY KEY)")
        .execute(&mut conn)
        .await
        .expect("Database initialization failed");

    // Verifies the connection is operating in WAL mode.
    let row: (String,) = sqlx::query_as("PRAGMA journal_mode")
        .fetch_one(&mut conn)
        .await
        .expect("PRAGMA journal_mode query failed");
    conn.close().await.expect("connection must close");
    assert_eq!(row.0, "wal");
}
