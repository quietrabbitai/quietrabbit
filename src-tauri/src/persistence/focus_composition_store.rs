// src-tauri/src/persistence/focus_composition_store.rs
//
// items.id=496: read-only queries backing conductor::lifecycle::
// load_focus_definition_from_db() -- the DB-composition path for a Focus
// authored as focus_block_compositions/focus_block_customization rows
// (shared_021.sql) rather than a .focus YAML file. These are Focus-TYPE
// tables (one row set per Focus, not per run/instance), which is why they
// live in shared.db alongside focus_settings -- same D6-299 reasoning:
// read at Phase 1 LOAD, before any encrypted per-persona outputs.db opens.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros (many-
// small-encrypted-DB topology; shared.db itself is unencrypted but this
// crate keeps the rule uniform).
//
// CONNECTION MODEL: pooled (items.id=483) -- callers pass a &sqlx::SqlitePool.

use sqlx::Row;

#[derive(Debug, Clone)]
pub struct FocusRow {
    pub focus_id: String,
    pub display_name: String,
    pub description: String,
    pub version: String,
    pub max_routing_tier: String,
    pub output_type: String,
    pub suggest_in_focuses: Vec<String>,
    pub multi_source_validation: bool,
    pub generic_title_template: String,
    pub high_priority_trigger_anchor_field: Option<String>,
    pub high_priority_trigger_offset: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompositionRow {
    pub step_id: String,
    pub block_stable_id: Option<String>,
    pub sequence_index: i64,
    pub schedule_trigger_anchor_field: Option<String>,
    pub schedule_trigger_offset: Option<String>,
    /// The Focus-type-default (persona_id IS NULL) customization row's JSON,
    /// or `"{}"` when no such row exists yet (LEFT JOIN — see
    /// list_compositions()'s own query). No Persona-scoped override row is
    /// ever read here (items.id=496: representable, not yet real).
    pub customization: String,
}

/// `None` when no `focuses` row exists for `focus_id` — the caller's
/// signal to fall back to the .focus YAML path (conductor::lifecycle::
/// load_focus_definition()'s dual-path).
pub async fn get_focus(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
) -> Result<Option<FocusRow>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let row = sqlx::query(
        "SELECT focus_id, display_name, description, version, max_routing_tier,
                output_type, suggest_in_focuses, multi_source_validation,
                generic_title_template, high_priority_trigger_anchor_field,
                high_priority_trigger_offset
         FROM focuses WHERE focus_id = ?",
    )
    .bind(focus_id)
    .fetch_optional(&mut *conn)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let suggest_raw: String = row.try_get("suggest_in_focuses")?;
    let suggest_in_focuses: Vec<String> = serde_json::from_str(&suggest_raw).unwrap_or_default();

    Ok(Some(FocusRow {
        focus_id: row.try_get("focus_id")?,
        display_name: row.try_get("display_name")?,
        description: row.try_get("description")?,
        version: row.try_get("version")?,
        max_routing_tier: row.try_get("max_routing_tier")?,
        output_type: row.try_get("output_type")?,
        suggest_in_focuses,
        multi_source_validation: row.try_get::<i64, _>("multi_source_validation")? != 0,
        generic_title_template: row.try_get("generic_title_template")?,
        high_priority_trigger_anchor_field: row.try_get("high_priority_trigger_anchor_field")?,
        high_priority_trigger_offset: row.try_get("high_priority_trigger_offset")?,
    }))
}

/// Ordered composition rows for a Focus (sequence_index ASC — the same
/// order Vec<StepDefinition> needs). Joined against each row's Focus-type-
/// default customization row via LEFT JOIN, not INNER — a composition row
/// authored without a customization row yet is valid (defaults to `{}`),
/// same "missing = default, not an error" shape focus_settings.
/// voice_override already uses.
pub async fn list_compositions(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
) -> Result<Vec<CompositionRow>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let rows = sqlx::query(
        "SELECT c.step_id, c.block_stable_id, c.sequence_index,
                c.schedule_trigger_anchor_field, c.schedule_trigger_offset,
                COALESCE(cu.customization, '{}') AS customization
         FROM focus_block_compositions c
         LEFT JOIN focus_block_customization cu
           ON cu.composition_id = c.id AND cu.persona_id IS NULL
         WHERE c.focus_id = ?
         ORDER BY c.sequence_index ASC",
    )
    .bind(focus_id)
    .fetch_all(&mut *conn)
    .await?;

    rows.iter()
        .map(|row| {
            Ok(CompositionRow {
                step_id: row.try_get("step_id")?,
                block_stable_id: row.try_get("block_stable_id")?,
                sequence_index: row.try_get("sequence_index")?,
                schedule_trigger_anchor_field: row.try_get("schedule_trigger_anchor_field")?,
                schedule_trigger_offset: row.try_get("schedule_trigger_offset")?,
                customization: row.try_get("customization")?,
            })
        })
        .collect()
}

/// LOAD-time validation gate for a composition row's block_stable_id
/// (items.id=496 judgment call 7): this table is a validation-only mirror
/// of qr_docs.db's own catalog, never the dispatch source (Rust's match
/// statement in conductor::lifecycle::execute_step() dispatches) -- this
/// just lets an unknown or non-inline_composable stable_id fail at LOAD,
/// the same failure class validate_step() already produces for a malformed
/// .focus YAML file, rather than surfacing mid-run.
pub async fn is_inline_composable_block(
    pool: &sqlx::SqlitePool,
    stable_id: &str,
) -> Result<bool, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let row = sqlx::query(
        "SELECT 1 FROM building_blocks WHERE stable_id = ? AND invocation_mode = 'inline_composable'",
    )
    .bind(stable_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_MUTEX;

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
        pool: sqlx::SqlitePool,
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
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration must succeed in test setup");

        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool must connect");

        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
            pool,
        }
    }

    #[tokio::test]
    async fn get_focus_returns_none_when_absent() {
        let env = setup().await;
        assert!(get_focus(&env.pool, "no-such-focus")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn get_focus_and_list_compositions_round_trip() {
        let env = setup().await;
        let pool = &env.pool;
        let mut conn = pool.acquire().await.unwrap();

        sqlx::query(
            "INSERT INTO focuses
                (focus_id, display_name, description, version, max_routing_tier,
                 output_type, suggest_in_focuses, multi_source_validation,
                 generic_title_template, status, created_at, updated_at)
             VALUES ('test-focus', 'Test Focus', 'desc', '1.0', 'local_only',
                     'general', '[]', 0, 'Hidden item', 'shipped',
                     datetime('now'), datetime('now'))",
        )
        .execute(&mut *conn)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO focus_block_compositions
                (id, focus_id, step_id, block_stable_id, sequence_index,
                 created_at, updated_at)
             VALUES ('comp-1', 'test-focus', 'step_a', NULL, 0,
                     datetime('now'), datetime('now'))",
        )
        .execute(&mut *conn)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO focus_block_customization
                (id, composition_id, persona_id, customization, created_at, updated_at)
             VALUES ('cust-1', 'comp-1', NULL, '{\"prompt_template\":\"Hi {user_input}\"}',
                     datetime('now'), datetime('now'))",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        drop(conn);

        let focus = get_focus(pool, "test-focus").await.unwrap().unwrap();
        assert_eq!(focus.display_name, "Test Focus");

        let compositions = list_compositions(pool, "test-focus").await.unwrap();
        assert_eq!(compositions.len(), 1);
        assert_eq!(compositions[0].step_id, "step_a");
        assert_eq!(compositions[0].block_stable_id, None);
        assert!(compositions[0].customization.contains("Hi {user_input}"));
    }

    #[tokio::test]
    async fn list_compositions_defaults_customization_when_row_missing() {
        let env = setup().await;
        let pool = &env.pool;
        let mut conn = pool.acquire().await.unwrap();
        sqlx::query(
            "INSERT INTO focuses
                (focus_id, display_name, description, version, max_routing_tier,
                 output_type, suggest_in_focuses, multi_source_validation,
                 generic_title_template, status, created_at, updated_at)
             VALUES ('test-focus-2', 'Test Focus 2', '', '1.0', 'local_only',
                     'general', '[]', 0, 'Hidden item', 'designed',
                     datetime('now'), datetime('now'))",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO focus_block_compositions
                (id, focus_id, step_id, block_stable_id, sequence_index,
                 created_at, updated_at)
             VALUES ('comp-2', 'test-focus-2', 'step_a', NULL, 0,
                     datetime('now'), datetime('now'))",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        drop(conn);

        let compositions = list_compositions(pool, "test-focus-2").await.unwrap();
        assert_eq!(compositions[0].customization, "{}");
    }

    #[tokio::test]
    async fn is_inline_composable_block_seed_rows() {
        let env = setup().await;
        let pool = &env.pool;
        assert!(is_inline_composable_block(pool, "cb-01").await.unwrap());
        assert!(!is_inline_composable_block(pool, "cb-04").await.unwrap()); // standing_gateway, never seeded as composable
        assert!(!is_inline_composable_block(pool, "pc-01").await.unwrap()); // promotion_candidate, deliberately not seeded
        assert!(!is_inline_composable_block(pool, "not-a-block")
            .await
            .unwrap());
    }
}
