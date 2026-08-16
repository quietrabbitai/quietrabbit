// src-tauri/src/persistence/group_store.rs
//
// group.db connection layer -- items.id=283 (266a). Data-layer only: this
// file provides the self-healing opener, nothing else. Document CRUD and
// permission enforcement are items.id=285's scope; nothing in this file
// reads or writes a documents/document_permissions row.
//
// Path: {QR_DATA_ROOT}/groups/{persona_id}/{group_id}/group.db -- see
// migrations.rs::migrate_group_db's own doc comment for why this is NOT
// nested under users/{user_id}/... the way personal.db/outputs.db are.
//
// SELF-HEAL FROM DAY ONE (items.id=275/278 precedent): open_group_db below
// follows the exact check-then-migrate-then-connect pattern
// personal_store.rs::open_personal_db and output_store.rs::open_outputs_db
// already establish, rather than a bare create_if_missing(false) connect
// with no migration call anywhere in the real call path -- the failure
// mode those two items spent three dispatches fixing across nine other
// stores, not repeated here.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros (many-small-
// encrypted-DB topology, no static DATABASE_URL).

use std::path::PathBuf;

use sqlx::ConnectOptions;
use sqlx::SqliteConnection;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum GroupStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Migration error: {0}")]
    Migration(#[from] crate::persistence::migrations::MigrationError),
}

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

// items.id=283 builds the opener only; no CRUD caller exists yet in this
// item's scope (items.id=285). Exercised directly by this file's own test
// below -- not dead code, just not yet consumed outside tests.
#[allow(dead_code)]
fn get_group_db_path(persona_id: &str, group_id: &str) -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("groups")
        .join(persona_id)
        .join(group_id)
        .join("group.db")
}

// ---------------------------------------------------------------------------
// DB opener
// ---------------------------------------------------------------------------

/// Open group.db with SQLCipher key, self-healing (migrating) a never-yet-
/// created file on first access. Caller supplies bare hex; wrapped in
/// SQLCipher x'...' syntax by connect_options_encrypted.
///
/// key_hex boundary: same convention as personal_store.rs/output_store.rs --
/// this is a plain library function (not a #[tauri::command]), key_hex is
/// derived by the command-layer caller from auth::registry::GroupKeyRegistry,
/// not accepted as a bare IPC parameter from the frontend.
#[allow(dead_code)] // items.id=283: opener built ahead of its first real caller (items.id=285)
pub(crate) async fn open_group_db(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, GroupStoreError> {
    let db_path = get_group_db_path(persona_id, group_id);

    if !db_path.exists() {
        crate::persistence::migrations::migrate_group_db(persona_id, group_id, key_hex).await?;
    }

    let conn = crate::providers::utils::connect_options_encrypted(&db_path, key_hex)
        .create_if_missing(false)
        .connect()
        .await?;

    Ok(conn)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression coverage for the same bug class items.id=278 fixed for
    /// personal.db: open_group_db must self-heal a never-yet-created
    /// group.db by running migrate_group_db() itself, not hard-fail with
    /// SQLITE_CANTOPEN. Deliberately does NOT pre-create the file or call
    /// migrate_group_db() anywhere in this test -- that absence is exactly
    /// the fresh-(persona,group)-pair, first-access case under test.
    #[tokio::test]
    async fn open_group_db_self_heals_a_never_created_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let persona_id = "self-heal-persona";
        let group_id = "self-heal-group";
        let key_hex = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";

        let db_path = get_group_db_path(persona_id, group_id);
        assert!(
            !db_path.exists(),
            "test setup must start with no db file -- that's the fresh-pair \
             first-access case under test"
        );

        let result = open_group_db(persona_id, group_id, key_hex).await;

        let verify = async {
            let mut conn = result.expect(
                "open_group_db must self-heal a fresh (persona, group) pair's \
                 never-created group.db, not hard-fail",
            );
            let row = sqlx::query("SELECT COUNT(*) AS n FROM documents")
                .fetch_one(&mut conn)
                .await
                .expect("documents table must exist after self-heal migration");
            let n: i64 = sqlx::Row::try_get(&row, "n").unwrap();
            assert_eq!(n, 0);
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }
}
