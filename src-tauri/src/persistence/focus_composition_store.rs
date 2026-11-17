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

/// items.id=920 (shared_044.sql): whether a Focus needs the one setup consent
/// before its store writers may write. The default is PerField, NEVER "not
/// required": whether a Focus needs consent is declared by the Focus, so a
/// missing or default row cannot open the gate. FocusLevel also means the
/// generic extraction pass is skipped (wired by items.id=914, not here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CollectionPosture {
    #[default]
    PerField,
    FocusLevel,
}

impl CollectionPosture {
    pub fn as_str(self) -> &'static str {
        match self {
            CollectionPosture::PerField => "per_field",
            CollectionPosture::FocusLevel => "focus_level",
        }
    }

    pub fn parse(s: &str) -> Option<CollectionPosture> {
        match s {
            "per_field" => Some(CollectionPosture::PerField),
            "focus_level" => Some(CollectionPosture::FocusLevel),
            _ => None,
        }
    }
}

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
    /// items.id=920. The CHECK in shared_044.sql guarantees a known value.
    pub collection_posture: CollectionPosture,
}

#[derive(Debug, Clone)]
pub struct CompositionRow {
    pub action_id: String,
    pub block_stable_id: Option<String>,
    pub sequence_index: i64,
    pub schedule_trigger_anchor_field: Option<String>,
    pub schedule_trigger_offset: Option<String>,
    /// items.id=878 (shared_039.sql): the raw guard columns, all None on an
    /// unguarded row. Parsed and validated by conductor::guard at LOAD, never
    /// here, so a malformed guard reaches the loader with its own error.
    pub guard_var: Option<String>,
    pub guard_op: Option<String>,
    pub guard_value: Option<String>,
    /// items.id=889 (shared_042.sql): the raw AND-guard clause list, a JSON
    /// array, None on a row with no clause list. Every clause must hold, as
    /// well as the single guard above. Parsed and validated by
    /// conductor::guard at LOAD, never here.
    pub guard_clauses: Option<String>,
    /// items.id=887 (shared_041.sql): the raw store-writer columns, all None on
    /// a row that is not a writer. Parsed and validated by conductor::store_op
    /// at LOAD, never here.
    pub store_op: Option<String>,
    pub store_target: Option<String>,
    pub store_args: Option<String>,
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
                high_priority_trigger_offset, collection_posture
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
        collection_posture: {
            let raw: String = row.try_get("collection_posture")?;
            CollectionPosture::parse(&raw).ok_or_else(|| {
                sqlx::Error::Decode(format!("unknown collection_posture '{raw}'").into())
            })?
        },
    }))
}

/// Ordered composition rows for a Focus (sequence_index ASC — the same
/// order Vec<ActionDefinition> needs). Joined against each row's Focus-type-
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
        "SELECT c.action_id, c.block_stable_id, c.sequence_index,
                c.schedule_trigger_anchor_field, c.schedule_trigger_offset,
                c.guard_var, c.guard_op, c.guard_value, c.guard_clauses,
                c.store_op, c.store_target, c.store_args,
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
                action_id: row.try_get("action_id")?,
                block_stable_id: row.try_get("block_stable_id")?,
                sequence_index: row.try_get("sequence_index")?,
                schedule_trigger_anchor_field: row.try_get("schedule_trigger_anchor_field")?,
                schedule_trigger_offset: row.try_get("schedule_trigger_offset")?,
                guard_var: row.try_get("guard_var")?,
                guard_op: row.try_get("guard_op")?,
                guard_value: row.try_get("guard_value")?,
                guard_clauses: row.try_get("guard_clauses")?,
                store_op: row.try_get("store_op")?,
                store_target: row.try_get("store_target")?,
                store_args: row.try_get("store_args")?,
                customization: row.try_get("customization")?,
            })
        })
        .collect()
}

/// LOAD-time validation gate for a composition row's block_stable_id
/// (items.id=496 judgment call 7): this table is a validation-only mirror
/// of qr_docs.db's own catalog, never the dispatch source (the compiled-in
/// conductor::block_dispatch::block_handler() registry dispatches, and the
/// loader checks handler existence separately) -- this just lets an unknown
/// or non-inline_composable stable_id fail at LOAD, the same failure class
/// validate_step() already produces for a malformed .focus YAML file, rather
/// than surfacing mid-run.
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

// ---------------------------------------------------------------------------
// The one writer (items.id=898, decisions.id=951 ruling 6)
// ---------------------------------------------------------------------------
//
// A Focus is seeded by data-only migration files (shared_038.sql is the model),
// and by the focus_dev tool and the tests through THESE functions: one row
// shape, one set of INSERTs. The structs are plain data and always compiled (the
// focus_dev expander and SQL renderer build them). The functions that WRITE are
// compiled only in a debug build or a test build (decisions.id=951 amendment 2),
// so a release build of the app contains no writer into shared.db's Focus tables.
//
// Ids follow the convention every existing seed uses: a composition id is
// "{focus_id}:{action_id}" and its default customization id is "{composition_id}:default".
// sort_order and sequence_index are the vector position, so order is the author's.

/// The `focuses` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusSeedRow {
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
    /// designed, in_build, shipped or deferred (the table's CHECK).
    pub status: String,
    /// per_field or focus_level (the table's CHECK).
    pub collection_posture: String,
}

/// One composition row plus its default customization row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositionSeed {
    pub action_id: String,
    pub block_stable_id: Option<String>,
    pub schedule_trigger_anchor_field: Option<String>,
    pub schedule_trigger_offset: Option<String>,
    pub guard_var: Option<String>,
    pub guard_op: Option<String>,
    pub guard_value: Option<String>,
    pub guard_clauses: Option<String>,
    pub store_op: Option<String>,
    pub store_target: Option<String>,
    pub store_args: Option<String>,
    /// The default (persona_id NULL) customization JSON text.
    pub customization: String,
}

impl CompositionSeed {
    /// A row with no guard, schedule or store columns.
    pub fn plain(action_id: &str, block: Option<&str>, customization: &str) -> Self {
        Self {
            action_id: action_id.to_owned(),
            block_stable_id: block.map(str::to_owned),
            schedule_trigger_anchor_field: None,
            schedule_trigger_offset: None,
            guard_var: None,
            guard_op: None,
            guard_value: None,
            guard_clauses: None,
            store_op: None,
            store_target: None,
            store_args: None,
            customization: customization.to_owned(),
        }
    }
}

/// One option of a launch question. `envelope` is the focus_mode_profiles JSON of a
/// mode question's option (None for a choice option).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOptionSeed {
    pub option_id: String,
    pub label: String,
    pub hint: String,
    pub envelope: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchQuestionSeed {
    pub question_id: String,
    pub slot: String,
    /// mode or choice.
    pub kind: String,
    pub asked: bool,
    pub default_option: String,
    pub prompt: String,
    pub options: Vec<LaunchOptionSeed>,
}

/// Everything a Focus owns in shared.db.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusSeed {
    pub focus: FocusSeedRow,
    pub compositions: Vec<CompositionSeed>,
    pub questions: Vec<LaunchQuestionSeed>,
    /// focus_launch_display.ui_one_liner.
    pub ui_one_liner: Option<String>,
}

#[cfg(any(test, debug_assertions))]
pub async fn write_focus(
    conn: &mut sqlx::SqliteConnection,
    seed: &FocusSeed,
) -> Result<(), sqlx::Error> {
    let f = &seed.focus;
    sqlx::query(
        "INSERT INTO focuses
            (focus_id, display_name, description, version, max_routing_tier,
             output_type, suggest_in_focuses, multi_source_validation,
             generic_title_template, high_priority_trigger_anchor_field,
             high_priority_trigger_offset, status, collection_posture,
             created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))",
    )
    .bind(&f.focus_id)
    .bind(&f.display_name)
    .bind(&f.description)
    .bind(&f.version)
    .bind(&f.max_routing_tier)
    .bind(&f.output_type)
    .bind(serde_json::to_string(&f.suggest_in_focuses).unwrap_or_else(|_| "[]".to_owned()))
    .bind(i64::from(f.multi_source_validation))
    .bind(&f.generic_title_template)
    .bind(&f.high_priority_trigger_anchor_field)
    .bind(&f.high_priority_trigger_offset)
    .bind(&f.status)
    .bind(&f.collection_posture)
    .execute(&mut *conn)
    .await?;
    for (i, c) in seed.compositions.iter().enumerate() {
        let comp = format!("{}:{}", f.focus_id, c.action_id);
        sqlx::query(
            "INSERT INTO focus_block_compositions
                (id, focus_id, action_id, block_stable_id, sequence_index,
                 schedule_trigger_anchor_field, schedule_trigger_offset,
                 guard_var, guard_op, guard_value, guard_clauses,
                 store_op, store_target, store_args, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))",
        )
        .bind(&comp)
        .bind(&f.focus_id)
        .bind(&c.action_id)
        .bind(&c.block_stable_id)
        .bind(i as i64)
        .bind(&c.schedule_trigger_anchor_field)
        .bind(&c.schedule_trigger_offset)
        .bind(&c.guard_var)
        .bind(&c.guard_op)
        .bind(&c.guard_value)
        .bind(&c.guard_clauses)
        .bind(&c.store_op)
        .bind(&c.store_target)
        .bind(&c.store_args)
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO focus_block_customization
                (id, composition_id, persona_id, customization, created_at, updated_at)
             VALUES (?, ?, NULL, ?, datetime('now'), datetime('now'))",
        )
        .bind(format!("{comp}:default"))
        .bind(&comp)
        .bind(&c.customization)
        .execute(&mut *conn)
        .await?;
    }
    write_launch(
        conn,
        &f.focus_id,
        &seed.questions,
        seed.ui_one_liner.as_deref(),
    )
    .await
}

/// The launch tables of one Focus (questions, options, mode profiles, one-liner).
/// Separate so a caller that seeds the Focus rows first (the admission tests) can add a
/// question afterwards.
#[cfg(any(test, debug_assertions))]
pub async fn write_launch(
    conn: &mut sqlx::SqliteConnection,
    focus_id: &str,
    questions: &[LaunchQuestionSeed],
    ui_one_liner: Option<&str>,
) -> Result<(), sqlx::Error> {
    for (qi, q) in questions.iter().enumerate() {
        sqlx::query(
            "INSERT INTO focus_launch_questions
                (focus_id, question_id, slot, kind, sort_order, asked, default_option, prompt)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(focus_id)
        .bind(&q.question_id)
        .bind(&q.slot)
        .bind(&q.kind)
        .bind(qi as i64)
        .bind(i64::from(q.asked))
        .bind(&q.default_option)
        .bind(&q.prompt)
        .execute(&mut *conn)
        .await?;
        for (oi, o) in q.options.iter().enumerate() {
            sqlx::query(
                "INSERT INTO focus_launch_options
                    (focus_id, question_id, option_id, sort_order, label, hint)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(focus_id)
            .bind(&q.question_id)
            .bind(&o.option_id)
            .bind(oi as i64)
            .bind(&o.label)
            .bind(&o.hint)
            .execute(&mut *conn)
            .await?;
            if let Some(envelope) = &o.envelope {
                sqlx::query(
                    "INSERT INTO focus_mode_profiles (focus_id, option_id, envelope)
                     VALUES (?, ?, ?)",
                )
                .bind(focus_id)
                .bind(&o.option_id)
                .bind(envelope)
                .execute(&mut *conn)
                .await?;
            }
        }
    }
    if let Some(text) = ui_one_liner {
        sqlx::query("INSERT INTO focus_launch_display (focus_id, ui_one_liner) VALUES (?, ?)")
            .bind(focus_id)
            .bind(text)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Removes every row a Focus owns in shared.db. The launch tables have no foreign keys and
/// the composition tables are deleted explicitly too, so this does not depend on the
/// connection's foreign_keys pragma. Debug and test builds only.
#[cfg(any(test, debug_assertions))]
pub async fn delete_focus(
    conn: &mut sqlx::SqliteConnection,
    focus_id: &str,
) -> Result<(), sqlx::Error> {
    for sql in [
        "DELETE FROM focus_block_customization WHERE composition_id IN
            (SELECT id FROM focus_block_compositions WHERE focus_id = ?)",
        "DELETE FROM focus_block_compositions WHERE focus_id = ?",
        "DELETE FROM focus_launch_options WHERE focus_id = ?",
        "DELETE FROM focus_mode_profiles WHERE focus_id = ?",
        "DELETE FROM focus_launch_questions WHERE focus_id = ?",
        "DELETE FROM focus_launch_display WHERE focus_id = ?",
        "DELETE FROM focuses WHERE focus_id = ?",
    ] {
        sqlx::query(sql).bind(focus_id).execute(&mut *conn).await?;
    }
    Ok(())
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
                (id, focus_id, action_id, block_stable_id, sequence_index,
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
        assert_eq!(compositions[0].action_id, "step_a");
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
                (id, focus_id, action_id, block_stable_id, sequence_index,
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

    /// items.id=129: shared_038 takes the mirror from 9 rows to 10 (pc-03 in).
    /// items.id=894: shared_043 adds the three model blocks, 13 rows.
    /// items.id=387: shared_046 adds the render block cb-15, 14 rows.
    /// pc-04, cb-04 and cb-09 stay out, and pc-03 is a member with no handler.
    #[tokio::test]
    async fn mirror_has_fourteen_rows_with_pc03_in_and_pc04_cb04_cb09_out() {
        let env = setup().await;
        let pool = &env.pool;
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM building_blocks")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(n, 14);
        for id in ["cb-12", "cb-13", "cb-14", "cb-15"] {
            assert!(is_inline_composable_block(pool, id).await.unwrap(), "{id}");
        }
        assert!(is_inline_composable_block(pool, "pc-03").await.unwrap());
        for absent in ["pc-04", "cb-04", "cb-09"] {
            assert!(!is_inline_composable_block(pool, absent).await.unwrap());
            let any: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM building_blocks WHERE stable_id = ?")
                    .bind(absent)
                    .fetch_one(pool)
                    .await
                    .unwrap();
            assert_eq!(any, 0, "{absent} must not be in the mirror");
        }
        assert!(crate::conductor::block_dispatch::block_handler("pc-03").is_none());
    }

    /// items.id=129: every compiled-in block's stable_id (items.id=894: of any
    /// kind) must be a mirror member, or a Focus could never cite it (membership
    /// is checked first).
    #[tokio::test]
    async fn every_registered_handler_is_a_mirror_member() {
        let env = setup().await;
        for id in crate::conductor::block_dispatch::REGISTERED_STABLE_IDS {
            assert!(
                is_inline_composable_block(&env.pool, id).await.unwrap(),
                "{id} has a handler but is not an inline_composable mirror row"
            );
        }
    }
}
