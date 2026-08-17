// src-tauri/src/persistence/group_key_store.rs
//
// items.id=290 (group.db 266g): durable group-key storage, per
// decisions.id=718. Operates on the `group_keys` table in personal.db
// (personal_005.sql) -- reuses personal_store::open_personal_db and
// PersonalStoreError directly rather than duplicating the opener or
// wrapping it in a new error enum, same convention as dedup_store.rs/
// entity_store.rs/document_fork_store.rs: a second opener would be a P4
// (One Home) violation on the same physical database file.
//
// CALL SITES (not built here -- this module is CRUD only):
//   save_group_key: auth::group_invitations::accept_invitation and
//     auth::group_membership::apply_pending_rotations, alongside their
//     existing GroupKeyRegistry::replace calls.
//   list_group_keys: commands::auth::finish_login, to rehydrate
//     GroupKeyRegistry at login (the registry itself stays deliberately
//     volatile -- see auth/registry.rs's own module header).
//   delete_group_key: auth::group_membership::remove_member, alongside its
//     existing GroupKeyRegistry::clear call.
//
// UPSERT, not plain INSERT: group_id is the table's PRIMARY KEY, and both
// write call sites can legitimately retarget the same row -- accept_
// invitation retried after a previously failed write, and apply_pending_
// rotations replacing an already-durable key with a freshly rotated one.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros (many-
// small-encrypted-DB topology, no static DATABASE_URL).

use sqlx::Row;

use crate::persistence::personal_store::{open_personal_db, PersonalStoreError};

// ---------------------------------------------------------------------------
// Data type
// ---------------------------------------------------------------------------

pub struct GroupKeyRow {
    pub group_id: String,
    pub group_key_hex: String,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Upsert one group's durable key for `persona_id`. See this module's own
/// header for why upsert rather than plain INSERT.
pub async fn save_group_key(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    group_id: &str,
    group_key_hex: &str,
    created_at: &str,
) -> Result<(), PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    sqlx::query(
        "INSERT INTO group_keys (group_id, group_key_hex, created_at)
         VALUES (?, ?, ?)
         ON CONFLICT(group_id) DO UPDATE SET
             group_key_hex = excluded.group_key_hex,
             created_at = excluded.created_at",
    )
    .bind(group_id)
    .bind(group_key_hex)
    .bind(created_at)
    .execute(&mut conn)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// Remove `persona_id`'s durable key for one group, if present. A no-op
/// (not an error) if no row exists -- matches GroupKeyRegistry::clear's own
/// no-op-on-missing-entry semantics, which this call is meant to mirror.
pub async fn delete_group_key(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    group_id: &str,
) -> Result<(), PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    sqlx::query("DELETE FROM group_keys WHERE group_id = ?")
        .bind(group_id)
        .execute(&mut conn)
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// Every durable group key recorded for `persona_id`, in no particular
/// order. The only consumer (commands::auth::finish_login) is responsible
/// for validating each row's group_key_hex before use -- this function
/// returns raw stored TEXT values as-is, no decode/length validation here.
pub async fn list_group_keys(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<Vec<GroupKeyRow>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;

    let rows = sqlx::query("SELECT group_id, group_key_hex, created_at FROM group_keys")
        .fetch_all(&mut conn)
        .await?;

    let mut result = Vec::with_capacity(rows.len());
    for r in rows {
        result.push(GroupKeyRow {
            group_id: r.try_get("group_id")?,
            group_key_hex: r.try_get("group_key_hex")?,
            created_at: r.try_get("created_at")?,
        });
    }
    Ok(result)
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
        _lock: std::sync::MutexGuard<'static, ()>,
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

    fn setup() -> TestEnv {
        let lock = crate::test_support::ENV_MUTEX.lock().unwrap();
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
    async fn save_then_list_round_trips() {
        let _env = setup();
        let user_id = "gk-user";
        let persona_id = "gk-persona";

        save_group_key(
            user_id,
            persona_id,
            PERSONAL_KEY_HEX,
            "group-1",
            "aa11",
            "2026-01-01T00:00:00Z",
        )
        .await
        .expect("save_group_key must succeed");

        let rows = list_group_keys(user_id, persona_id, PERSONAL_KEY_HEX)
            .await
            .expect("list_group_keys must succeed");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group_id, "group-1");
        assert_eq!(rows[0].group_key_hex, "aa11");
        assert_eq!(rows[0].created_at, "2026-01-01T00:00:00Z");
    }

    #[tokio::test]
    async fn save_upserts_rather_than_duplicating() {
        let _env = setup();
        let user_id = "gk-user";
        let persona_id = "gk-persona";

        save_group_key(
            user_id,
            persona_id,
            PERSONAL_KEY_HEX,
            "group-1",
            "aa11",
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        save_group_key(
            user_id,
            persona_id,
            PERSONAL_KEY_HEX,
            "group-1",
            "bb22",
            "2026-02-02T00:00:00Z",
        )
        .await
        .unwrap();

        let rows = list_group_keys(user_id, persona_id, PERSONAL_KEY_HEX)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "same group_id must overwrite, not duplicate");
        assert_eq!(rows[0].group_key_hex, "bb22");
        assert_eq!(rows[0].created_at, "2026-02-02T00:00:00Z");
    }

    #[tokio::test]
    async fn save_keeps_different_groups_independent() {
        let _env = setup();
        let user_id = "gk-user";
        let persona_id = "gk-persona";

        save_group_key(
            user_id,
            persona_id,
            PERSONAL_KEY_HEX,
            "group-1",
            "aa11",
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        save_group_key(
            user_id,
            persona_id,
            PERSONAL_KEY_HEX,
            "group-2",
            "cc33",
            "2026-01-02T00:00:00Z",
        )
        .await
        .unwrap();

        let mut rows = list_group_keys(user_id, persona_id, PERSONAL_KEY_HEX)
            .await
            .unwrap();
        rows.sort_by(|a, b| a.group_id.cmp(&b.group_id));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].group_id, "group-1");
        assert_eq!(rows[1].group_id, "group-2");
    }

    #[tokio::test]
    async fn delete_removes_only_the_targeted_group() {
        let _env = setup();
        let user_id = "gk-user";
        let persona_id = "gk-persona";

        save_group_key(
            user_id,
            persona_id,
            PERSONAL_KEY_HEX,
            "group-1",
            "aa11",
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        save_group_key(
            user_id,
            persona_id,
            PERSONAL_KEY_HEX,
            "group-2",
            "cc33",
            "2026-01-02T00:00:00Z",
        )
        .await
        .unwrap();

        delete_group_key(user_id, persona_id, PERSONAL_KEY_HEX, "group-1")
            .await
            .expect("delete_group_key must succeed");

        let rows = list_group_keys(user_id, persona_id, PERSONAL_KEY_HEX)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group_id, "group-2");
    }

    #[tokio::test]
    async fn delete_on_missing_row_is_a_no_op() {
        let _env = setup();
        let user_id = "gk-user";
        let persona_id = "gk-persona";

        delete_group_key(user_id, persona_id, PERSONAL_KEY_HEX, "nonexistent")
            .await
            .expect("delete_group_key must not error when the row doesn't exist");
    }

    #[tokio::test]
    async fn list_on_fresh_persona_is_empty() {
        let _env = setup();
        let rows = list_group_keys("gk-user", "gk-fresh-persona", PERSONAL_KEY_HEX)
            .await
            .expect("list_group_keys must succeed on a never-written persona");
        assert!(rows.is_empty());
    }
}
