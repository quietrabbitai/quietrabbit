// src-tauri/src/persistence/group_fact_store.rs
//
// items.id=296: group_facts read side (group.db, schema/group_002.sql).
// GROUP_DB_DESIGN_20260802.md Section 3.3 -- the resolved mechanism for
// group facts is "mirror build_personal_track()'s existing entity_facts
// load, not visibility.rs" (visibility.rs has zero live callers anywhere
// in this codebase and no redact/hide concept is defined for group facts).
//
// READ SIDE ONLY: no create/update functions here. Owner-write CRUD
// (manual entry, promotion from a personal fact) is explicitly out of
// scope for this item -- see schema/group_002.sql's own header for why
// `owner_persona_id` exists in the schema unenforced, same shape
// group_001.sql's own documents.owner_persona_id shipped ahead of its own
// enforcement item.
//
// ONE OPENER, reused: this module does NOT define its own open_group_db --
// group_facts lives in the exact same group.db file documents/
// document_permissions already do, so it reuses group_store::open_group_db
// and GroupStoreError directly. A second opener for the same physical file
// would be the identical P4 (One Home) violation group_key_store.rs's own
// header already calls out for personal.db.
//
// NO PER-ROW READ-ACCESS CHECK: unlike group_store.rs's
// require_read_access_conn (documents require an explicit owner-or-grant
// row), load_group_facts_for_context applies no app-layer filter at all.
// Section 3.3 point 4 is explicit: "the resident-key check... combined
// with the opt-in, IS the complete gate. No second stacked check." Holding
// the group key (the caller's own precondition for calling this at all) is
// sufficient to read every group_facts row.
//
// group_id IS NOT a column: group_facts, like every table in group.db, is
// implicitly scoped to the single group its file belongs to (schema/
// group_002.sql, matching group_001.sql's own convention) -- group_id is
// stamped onto each returned GroupFact from the caller-supplied parameter,
// not read back from a row.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros (many-
// small-encrypted-DB topology, no static DATABASE_URL).

use sqlx::Row;

use crate::conductor::types::GroupFact;
use crate::persistence::group_store::{open_group_db, GroupStoreError};

/// Every group_facts row currently in `group_id`'s group.db, in no
/// particular order beyond the query's own field_name ordering. Called by
/// conductor::lifecycle::FocusRun::build_personal_track(), once per
/// (opted-in AND currently key-resident) group -- see that call site's own
/// doc comment for the full gate.
pub async fn load_group_facts_for_context(
    persona_id: &str,
    group_id: &str,
    group_key_hex: &str,
) -> Result<Vec<GroupFact>, GroupStoreError> {
    let mut conn = open_group_db(persona_id, group_id, group_key_hex).await?;

    let rows = sqlx::query(
        "SELECT id, field_name, field_value, sensitivity, sensitivity_severity
         FROM group_facts
         ORDER BY field_name",
    )
    .fetch_all(&mut conn)
    .await?;

    let mut facts = Vec::with_capacity(rows.len());
    for row in rows {
        facts.push(GroupFact {
            id: row.try_get("id")?,
            group_id: group_id.to_owned(),
            field_name: row.try_get("field_name")?,
            field_value: row.try_get("field_value")?,
            sensitivity: row.try_get("sensitivity")?,
            sensitivity_severity: row.try_get::<i64, _>("sensitivity_severity")? as i32,
        });
    }

    Ok(facts)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

    const TEST_KEY_HEX: &str = "aabbccddeeff00112233445566778899aabbccddeeff0011223344556677aa";

    async fn insert_group_fact(
        persona_id: &str,
        group_id: &str,
        id: &str,
        field_name: &str,
        field_value: &str,
        sensitivity: &str,
    ) {
        let mut conn = open_group_db(persona_id, group_id, TEST_KEY_HEX)
            .await
            .expect("open_group_db must succeed for test fixture insert");
        let now = crate::providers::utils::now();
        sqlx::query(
            "INSERT INTO group_facts
                (id, owner_persona_id, field_name, field_value, sensitivity, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(persona_id)
        .bind(field_name)
        .bind(field_value)
        .bind(sensitivity)
        .bind(&now)
        .bind(&now)
        .execute(&mut conn)
        .await
        .expect("fixture insert into group_facts must succeed");
    }

    #[tokio::test]
    async fn load_group_facts_for_context_returns_empty_for_a_fresh_group_db() {
        let _env = setup().await;
        let facts = load_group_facts_for_context("persona-1", "group-1", TEST_KEY_HEX)
            .await
            .expect("load_group_facts_for_context must succeed on a never-written group.db");
        assert!(facts.is_empty());
    }

    #[tokio::test]
    async fn load_group_facts_for_context_returns_inserted_rows_with_group_id_stamped() {
        let _env = setup().await;
        insert_group_fact(
            "persona-1",
            "group-1",
            "fact-1",
            "business_name",
            "Acme Household LLC",
            "general",
        )
        .await;

        let facts = load_group_facts_for_context("persona-1", "group-1", TEST_KEY_HEX)
            .await
            .expect("load_group_facts_for_context must succeed");

        assert_eq!(facts.len(), 1);
        let fact = &facts[0];
        assert_eq!(fact.id, "fact-1");
        assert_eq!(
            fact.group_id, "group-1",
            "group_id is stamped from the parameter, not a DB column"
        );
        assert_eq!(fact.field_name, "business_name");
        assert_eq!(fact.field_value, "Acme Household LLC");
        assert_eq!(fact.sensitivity, "general");
        assert_eq!(fact.sensitivity_severity, 1);
    }

    #[tokio::test]
    async fn load_group_facts_for_context_has_no_per_persona_filter() {
        // Section 3.3 point 4: the resident-key check + opt-in is the
        // complete gate -- there is deliberately no per-row read-access
        // check here the way get_document's require_read_access_conn has.
        // Any persona able to open the group.db (i.e. holding the key)
        // sees every row, regardless of who is asking.
        let _env = setup().await;
        insert_group_fact(
            "owner-persona",
            "group-1",
            "fact-1",
            "pricing_tier",
            "standard",
            "personal",
        )
        .await;

        let facts = load_group_facts_for_context("owner-persona", "group-1", TEST_KEY_HEX)
            .await
            .expect("load must succeed");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].sensitivity_severity, 2);
    }

    #[tokio::test]
    async fn load_group_facts_for_context_orders_by_field_name() {
        let _env = setup().await;
        insert_group_fact("persona-1", "group-1", "fact-b", "zebra", "v", "general").await;
        insert_group_fact("persona-1", "group-1", "fact-a", "apple", "v", "general").await;

        let facts = load_group_facts_for_context("persona-1", "group-1", TEST_KEY_HEX)
            .await
            .expect("load must succeed");
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].field_name, "apple");
        assert_eq!(facts[1].field_name, "zebra");
    }
}
