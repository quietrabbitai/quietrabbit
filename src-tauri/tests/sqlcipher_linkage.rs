//! SQLCipher linkage verification.
//!
//! Proves that the build is linked against SQLCipher and not vanilla SQLite
//! by validating behavioral encryption constraints on disk. Plain SQLite
//! silently ignores PRAGMA key — these tests would pass the connect step
//! but allow unencrypted reads, which would fail the wrong-key/no-key assertions.

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use tempfile::NamedTempFile;

const KEY_HEX: &str =
    "x'deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef'";
const WRONG_KEY_HEX: &str =
    "x'0000000000000000000000000000000000000000000000000000000000000000'";

/// Open an encrypted connection using the options builder (avoids URI
/// character-escaping pitfalls). PRAGMA key fires before journal_mode in
/// the pragma batch that executes after sqlite3_open_v2.
async fn open_connection(
    path: &std::path::Path,
    key: Option<&str>,
) -> Result<sqlx::SqliteConnection, sqlx::Error> {
    let mut opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);

    if let Some(k) = key {
        opts = opts.pragma("key", format!("\"{k}\""));
    }

    opts.pragma("journal_mode", "WAL").connect().await
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
        msg.contains("not a database")
            || msg.contains("malformed")
            || msg.contains("encrypted"),
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
        e => panic!("Expected a Database error for encryption rejection, got: {:?}", e),
    }
}

#[tokio::test]
async fn test_sqlcipher_write_and_read() {
    let temp_file = NamedTempFile::new().unwrap();
    let (file_handle, temp_path) = temp_file.into_parts();
    drop(file_handle);

    // Write encrypted data.
    {
        let mut conn = open_connection(&temp_path, Some(KEY_HEX))
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
    }

    // Reopen with correct key — data must be readable.
    {
        let mut conn = open_connection(&temp_path, Some(KEY_HEX))
            .await
            .expect("SQLCipher connection (correct key) failed");
        let row: (String,) = sqlx::query_as("SELECT v FROM t")
            .fetch_one(&mut conn)
            .await
            .expect("SELECT failed with correct key");
        assert_eq!(row.0, "sentinel");
    }
}

#[tokio::test]
async fn test_sqlcipher_wrong_key_cannot_read() {
    let temp_file = NamedTempFile::new().unwrap();
    let (file_handle, temp_path) = temp_file.into_parts();
    drop(file_handle);

    {
        let mut conn = open_connection(&temp_path, Some(KEY_HEX))
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
    }

    // SQLCipher 3.51.3 rejects invalid keys at connect time (WAL init triggers
    // an immediate page read). Some builds defer to query time — both are valid.
    match open_connection(&temp_path, Some(WRONG_KEY_HEX)).await {
        Err(sqlx::Error::Database(db_err)) => {
            assert_encryption_db_error(db_err.as_ref());
        }
        Ok(mut conn) => {
            let result: Result<(String,), _> =
                sqlx::query_as("SELECT v FROM t").fetch_one(&mut conn).await;
            assert!(result.is_err(), "Wrong key must not be able to read encrypted data");
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
        let mut conn = open_connection(&temp_path, Some(KEY_HEX))
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
    }

    // No key supplied. Plain SQLite would read this without error —
    // SQLCipher must reject it, proving encryption is active.
    match open_connection(&temp_path, None).await {
        Err(sqlx::Error::Database(db_err)) => {
            assert_encryption_db_error(db_err.as_ref());
        }
        Ok(mut conn) => {
            let result: Result<(String,), _> =
                sqlx::query_as("SELECT v FROM t").fetch_one(&mut conn).await;
            assert!(result.is_err(), "Unkeyed access must not read encrypted data");
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

    let mut conn = open_connection(&temp_path, Some(KEY_HEX))
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
    assert_eq!(row.0, "wal");
}
