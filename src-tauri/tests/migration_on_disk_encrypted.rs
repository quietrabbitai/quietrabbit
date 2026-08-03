// src-tauri/tests/migration_on_disk_encrypted.rs
//
// items.id=95: proves the actual migration pipeline (migrations.rs's
// run_migrations/run_pending logic) applies correctly against a real
// on-disk SQLCipher-encrypted file -- not ':memory:'. Companion to
// sqlcipher_linkage.rs, which proves on-disk SQLCipher correctness with a
// single ad-hoc CREATE TABLE but does not exercise the migration runner.
//
// Uses the "shared" prefix against a NamedTempFile under a real key,
// mirroring the connection pattern established in sqlcipher_linkage.rs.
// Assertions are framework-level (schema_version tracking, idempotent
// rerun) rather than schema-content-level, so this stays stable as the
// "shared" prefix gains future migration versions.

use quietrabbit_lib::persistence::migrations::run_migrations;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use tempfile::NamedTempFile;

const KEY_HEX: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

#[tokio::test]
async fn test_migration_pipeline_applies_on_real_encrypted_file() {
    let temp_file = NamedTempFile::new().unwrap();
    let (file_handle, temp_path) = temp_file.into_parts();
    drop(file_handle);

    // First pass: run the real migration pipeline against a real on-disk
    // encrypted file (not :memory:).
    let first_pass_applied;
    {
        let mut conn = SqliteConnectOptions::new()
            .filename(&temp_path)
            .create_if_missing(true)
            .connect()
            .await
            .expect("failed to open on-disk connection for first migration pass");

        first_pass_applied = run_migrations(&mut conn, "shared", Some(KEY_HEX))
            .await
            .expect("migration pipeline must apply cleanly on a real encrypted file");
        assert!(
            first_pass_applied > 0,
            "expected at least one shared schema version to apply on a fresh file"
        );
    }

    // Second pass: reopen the same file with the correct key. Confirms the
    // migration's schema_version bookkeeping landed durably on disk, not
    // just in the transient connection above, and that re-running the
    // pipeline against already-migrated on-disk state is idempotent --
    // arguably the most important property of a migration runner.
    {
        let mut conn = SqliteConnectOptions::new()
            .filename(&temp_path)
            .pragma("key", format!("\"x'{KEY_HEX}'\""))
            .connect()
            .await
            .expect("failed reopening encrypted database after migration");

        let version: (i64,) = sqlx::query_as("SELECT MAX(version) FROM schema_version")
            .fetch_one(&mut conn)
            .await
            .expect("schema_version must be readable after reopening on disk");
        assert_eq!(
            version.0 as u32, first_pass_applied,
            "persisted schema_version must match what the first pass applied"
        );

        let second_pass_applied = run_migrations(&mut conn, "shared", Some(KEY_HEX))
            .await
            .expect("re-running migrations against an already-migrated file must succeed");
        assert_eq!(
            second_pass_applied, 0,
            "re-running migrations against already-migrated on-disk state must be a no-op"
        );
    }
}

#[tokio::test]
async fn test_migration_pipeline_wrong_key_cannot_read_migrated_file() {
    let temp_file = NamedTempFile::new().unwrap();
    let (file_handle, temp_path) = temp_file.into_parts();
    drop(file_handle);

    {
        let mut conn = SqliteConnectOptions::new()
            .filename(&temp_path)
            .create_if_missing(true)
            .connect()
            .await
            .expect("failed to open on-disk connection for migration");
        run_migrations(&mut conn, "shared", Some(KEY_HEX))
            .await
            .expect("migration pipeline must apply cleanly");
    }

    // Wrong key must not be able to read the migrated schema -- proves the
    // migration output is actually encrypted on disk, not left in plaintext
    // by some step of the pipeline. SQLCipher may reject at connect time or
    // defer rejection to first query, depending on build -- both are valid
    // (same tolerance as sqlcipher_linkage.rs).
    const WRONG_KEY_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    let wrong_key_result = SqliteConnectOptions::new()
        .filename(&temp_path)
        .pragma("key", format!("\"x'{WRONG_KEY_HEX}'\""))
        .connect()
        .await;

    let read_failed = match wrong_key_result {
        Err(_) => true,
        Ok(mut conn) => {
            let result: Result<(i64,), _> =
                sqlx::query_as("SELECT MAX(version) FROM schema_version")
                    .fetch_one(&mut conn)
                    .await;
            result.is_err()
        }
    };
    assert!(
        read_failed,
        "wrong key must not be able to connect to or read the migrated encrypted file"
    );
}
