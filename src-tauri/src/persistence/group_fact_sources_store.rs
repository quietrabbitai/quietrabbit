// src-tauri/src/persistence/group_fact_sources_store.rs
//
// items.id=296: group_fact_sources CRUD (personal.db, schema/
// personal_006.sql) -- the persona-level, one-time opt-in recording which
// group_ids a persona's context assembly checks for group facts
// (GROUP_DB_DESIGN_20260802.md Section 3.2/3.3).
//
// Reuses personal_store::open_personal_db and PersonalStoreError directly
// rather than duplicating the opener or wrapping it in a new error enum --
// same convention as group_key_store.rs/dedup_store.rs/entity_store.rs/
// document_fork_store.rs: a second opener would be a P4 (One Home)
// violation on the same physical database file.
//
// UPSERT, not plain INSERT: group_id is the table's PRIMARY KEY, and
// opting a persona back into a group it was previously opted out of (or
// re-recording the same opt-in) legitimately retargets the same row.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros (many-
// small-encrypted-DB topology, no static DATABASE_URL).

use sqlx::Row;

use crate::persistence::personal_store::{open_personal_db, PersonalStoreError};

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Opt `persona_id` into checking `group_id`'s facts during context
/// assembly. Upsert -- see this module's own header for why.
pub async fn opt_in_to_group_facts(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    group_id: &str,
) -> Result<(), PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    let now = crate::providers::utils::now();

    sqlx::query(
        "INSERT INTO group_fact_sources (group_id, opted_in_at)
         VALUES (?, ?)
         ON CONFLICT(group_id) DO UPDATE SET opted_in_at = excluded.opted_in_at",
    )
    .bind(group_id)
    .bind(&now)
    .execute(&mut conn)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// Opt `persona_id` back out of checking `group_id`'s facts. A no-op (not
/// an error) if the group was never opted into -- matches group_key_store
/// ::delete_group_key's own no-op-on-missing-row semantics.
pub async fn opt_out_of_group_facts(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    group_id: &str,
) -> Result<(), PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    sqlx::query("DELETE FROM group_fact_sources WHERE group_id = ?")
        .bind(group_id)
        .execute(&mut conn)
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// Every group_id `persona_id` is currently opted into, in no particular
/// order. Called by conductor::lifecycle::FocusRun::build_personal_track()
/// on every Focus run, and by commands::group::get_group_fact_sources for
/// the IPC surface.
pub async fn list_group_fact_sources(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<Vec<String>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    let rows = sqlx::query("SELECT group_id FROM group_fact_sources")
        .fetch_all(&mut conn)
        .await?;

    let mut group_ids = Vec::with_capacity(rows.len());
    for r in rows {
        group_ids.push(r.try_get("group_id")?);
    }
    Ok(group_ids)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const PERSONAL_KEY_HEX: &str = "aabbccddeeff00112233445566778899aabbccddeeff0011223344556677aa";

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn setup() -> TestEnv {
        let lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
        }
    }

    #[tokio::test]
    async fn list_on_fresh_persona_is_empty() {
        let _env = setup().await;
        let rows = list_group_fact_sources("gfs-user", "gfs-persona", PERSONAL_KEY_HEX)
            .await
            .expect("list_group_fact_sources must succeed on a never-written persona");
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn opt_in_then_list_round_trips() {
        let _env = setup().await;
        opt_in_to_group_facts("gfs-user", "gfs-persona", PERSONAL_KEY_HEX, "group-1")
            .await
            .expect("opt_in_to_group_facts must succeed");

        let rows = list_group_fact_sources("gfs-user", "gfs-persona", PERSONAL_KEY_HEX)
            .await
            .expect("list_group_fact_sources must succeed");
        assert_eq!(rows, vec!["group-1".to_owned()]);
    }

    #[tokio::test]
    async fn opt_in_upserts_rather_than_duplicating() {
        let _env = setup().await;
        opt_in_to_group_facts("gfs-user", "gfs-persona", PERSONAL_KEY_HEX, "group-1")
            .await
            .unwrap();
        opt_in_to_group_facts("gfs-user", "gfs-persona", PERSONAL_KEY_HEX, "group-1")
            .await
            .unwrap();

        let rows = list_group_fact_sources("gfs-user", "gfs-persona", PERSONAL_KEY_HEX)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "same group_id must overwrite, not duplicate");
    }

    #[tokio::test]
    async fn opt_in_keeps_different_groups_independent() {
        let _env = setup().await;
        opt_in_to_group_facts("gfs-user", "gfs-persona", PERSONAL_KEY_HEX, "group-1")
            .await
            .unwrap();
        opt_in_to_group_facts("gfs-user", "gfs-persona", PERSONAL_KEY_HEX, "group-2")
            .await
            .unwrap();

        let mut rows = list_group_fact_sources("gfs-user", "gfs-persona", PERSONAL_KEY_HEX)
            .await
            .unwrap();
        rows.sort();
        assert_eq!(rows, vec!["group-1".to_owned(), "group-2".to_owned()]);
    }

    #[tokio::test]
    async fn opt_out_removes_only_the_targeted_group() {
        let _env = setup().await;
        opt_in_to_group_facts("gfs-user", "gfs-persona", PERSONAL_KEY_HEX, "group-1")
            .await
            .unwrap();
        opt_in_to_group_facts("gfs-user", "gfs-persona", PERSONAL_KEY_HEX, "group-2")
            .await
            .unwrap();

        opt_out_of_group_facts("gfs-user", "gfs-persona", PERSONAL_KEY_HEX, "group-1")
            .await
            .expect("opt_out_of_group_facts must succeed");

        let rows = list_group_fact_sources("gfs-user", "gfs-persona", PERSONAL_KEY_HEX)
            .await
            .unwrap();
        assert_eq!(rows, vec!["group-2".to_owned()]);
    }

    #[tokio::test]
    async fn opt_out_on_missing_row_is_a_noop() {
        let _env = setup().await;
        opt_out_of_group_facts(
            "gfs-user",
            "gfs-persona",
            PERSONAL_KEY_HEX,
            "never-opted-in",
        )
        .await
        .expect("opt_out_of_group_facts must not error when the row doesn't exist");
    }
}
