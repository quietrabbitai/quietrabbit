// src-tauri/src/persistence/plan_state_store.rs
//
// Plan State CRUD for plan_state.db.
// Per-user, per-persona, per-focus, per-topic encrypted database.
// Path: /users/{user_id}/personas/{persona_id}/focuses/{focus_id}/topics/{topic_id}/plan_state.db
//
// Source of truth declaration (ADR-013 Section 8.9):
//   outputs.db topics table = authoritative source of truth for topic metadata.
//   topic_header in plan_state.db = cache copy for offline coherence (backup/restore).
//   On conflict between topic_header and outputs.db, outputs.db governs.
//   topic_header updated by Phase 5A and Reconciliation Boot Check.
//
// Cross-database FK note: plan_state_blocks.focus_run_id references
//   focus_runs.id in outputs.db — cross-db FK by value only, application-enforced.
//   handoff_tokens.topic_id references topics.id in outputs.db — same.
//   SQLite cannot enforce FKs across separate database files.
//
// Block-level sensitivity model (ADR-013 Section 5.7):
//   Each block carries its own visibility_scope and sensitivity_preset.
//   A medical block does NOT elevate general research blocks.
//   Output blocks inherit highest sensitivity from dependency_refs lineage.
//   get_sensitivity_ceiling() computes the ceiling across referenced blocks.
//
// Soft ceiling notification (ADR-013 Section 3.3):
//   System surfaces a calm consolidation prompt when token estimate exceeds threshold.
//   NEVER automatically closes or consolidates — user controls response.
//
// Python oracle deviation (flagged for Chat-PM):
//   Python plan_state_store.py uses open_db() (unencrypted opener) for all
//   plan_state.db access. This is a bug — plan_state.db is SQLCipher encrypted
//   per migrations.py and the schema header. Rust port uses the encrypted
//   opener with key_hex, which is architecturally correct.
//
// Future consideration (flagged for Chat-PM):
//   consume_handoff_token checks expired_at IS NULL but does not verify
//   expiry_at >= now() at the query layer. A token past its expiry_at but
//   not yet swept by Boot Check can still be consumed. Adding
//   AND expiry_at >= ? would close this window — deferred, not a Phase 1 blocker.
//
// QUERY STYLE: runtime sqlx::query() only — no query!() macros.
// PRAGMA key applied via SqliteConnectOptions (D6-346).
// Caller supplies bare hex; store wraps it in SQLCipher x'...' syntax.

use std::path::PathBuf;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

#[allow(dead_code)]
const DEFAULT_CEILING_THRESHOLD: i32 = 32000;

// ---------------------------------------------------------------------------
// Sensitivity preset helpers
// ---------------------------------------------------------------------------

fn preset_rank(preset: &str) -> i32 {
    match preset {
        "sensitive" => 1,
        "private"   => 2,
        "locked"    => 3,
        _           => 0,
    }
}

fn rank_to_preset(rank: i32) -> &'static str {
    match rank {
        1 => "sensitive",
        2 => "private",
        3 => "locked",
        _ => "standard",
    }
}

// ---------------------------------------------------------------------------
// Eligible scopes by execution tier
// ---------------------------------------------------------------------------

fn eligible_scopes_for_tier(execution_tier: i32) -> &'static [&'static str] {
    debug_assert!(
        (1..=3).contains(&execution_tier),
        "execution_tier must be 1, 2, or 3 -- got {execution_tier}"
    );
    match execution_tier {
        1 => &["tier_1_only", "anonymous_tier2", "tier2_permitted", "tier3_permitted"],
        2 => &["anonymous_tier2", "tier2_permitted", "tier3_permitted"],
        3 => &["tier3_permitted"],
        _ => &[],
    }
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PlanStateBlock {
    pub id: String,
    pub block_type: String,
    pub content: String,
    pub visibility_scope: String,
    pub transformation: String,
    pub token_estimate: i32,
    pub inferred_by_system: bool,
    pub focus_run_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub sensitivity_preset: Option<String>,
    pub relevance_tags: Vec<String>,
    pub dependency_refs: Vec<String>,
    pub archived_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TopicHeader {
    pub topic_id: String,
    pub focus_id: String,
    pub persona_id: String,
    pub placeholder_name: String,
    pub lifecycle_state: String,
    pub session_count: i32,
    pub created_at: String,
    pub updated_at: String,
    pub name: Option<String>,
    pub current_phase: Option<String>,
}

impl TopicHeader {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.placeholder_name)
    }
}

#[derive(Debug, Clone)]
pub struct StateCeilingStatus {
    pub current_token_estimate: i32,
    pub ceiling_threshold: i32,
    pub notification_sent_at: Option<String>,
    pub user_response: Option<String>,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum PlanStateStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Migration error: {0}")]
    Migration(String),
}

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

fn get_plan_state_db_path(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
) -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("focuses")
        .join(focus_id)
        .join("topics")
        .join(topic_id)
        .join("plan_state.db")
}

// ---------------------------------------------------------------------------
// DB opener
// ---------------------------------------------------------------------------

/// Open plan_state.db with SQLCipher key.
/// Caller supplies bare hex; store wraps it in SQLCipher x'...' syntax.
/// PRAGMA key fires before journal_mode via SqliteConnectOptions (D6-346).
/// busy_timeout=5000ms guards against transient SQLITE_BUSY.
///
/// Python oracle deviation: Python uses open_db() (unencrypted) for
/// plan_state.db -- this is a bug. Rust uses the encrypted opener, which
/// is correct per migrations.py and the schema header.
async fn open_plan_state_db(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, PlanStateStoreError> {
    let db_path = get_plan_state_db_path(user_id, persona_id, focus_id, topic_id);

    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let journal_mode = if network_storage { "DELETE" } else { "WAL" };

    let conn = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .pragma("key", format!("x'{key_hex}'"))
        .pragma("journal_mode", journal_mode)
        .pragma("busy_timeout", "5000")
        .connect()
        .await?;

    Ok(conn)
}

// ---------------------------------------------------------------------------
// Migration / ensure
// ---------------------------------------------------------------------------

/// Ensure plan_state.db exists and is migrated for this topic.
/// Directory structure created by migrate_plan_state_db (safety mkdir).
/// Returns the database path.
pub async fn ensure_plan_state_db(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
) -> Result<PathBuf, PlanStateStoreError> {
    crate::persistence::migrations::migrate_plan_state_db(
        user_id, persona_id, focus_id, topic_id, key_hex,
    )
    .await
    .map_err(|e| PlanStateStoreError::Migration(e.to_string()))?;
    Ok(get_plan_state_db_path(user_id, persona_id, focus_id, topic_id))
}

// ---------------------------------------------------------------------------
// Topic header
// ---------------------------------------------------------------------------

/// Read the topic header cache copy from plan_state.db.
/// Returns None if plan_state.db does not exist.
/// Source of truth is outputs.db -- this is a cache copy only.
pub async fn get_topic_header(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
) -> Result<Option<TopicHeader>, PlanStateStoreError> {
    let db_path = get_plan_state_db_path(user_id, persona_id, focus_id, topic_id);
    if !db_path.exists() {
        return Ok(None);
    }

    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT topic_id, focus_id, persona_id, name, placeholder_name,
                lifecycle_state, current_phase, session_count, created_at, updated_at
         FROM topic_header WHERE id = 1",
    )
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(TopicHeader {
            topic_id:         r.try_get("topic_id")?,
            focus_id:         r.try_get("focus_id")?,
            persona_id:       r.try_get("persona_id")?,
            name:             r.try_get("name")?,
            placeholder_name: r.try_get("placeholder_name")?,
            lifecycle_state:  r.try_get("lifecycle_state")?,
            current_phase:    r.try_get("current_phase")?,
            session_count:    r.try_get::<i64, _>("session_count")? as i32,
            created_at:       r.try_get("created_at")?,
            updated_at:       r.try_get("updated_at")?,
        })),
    }
}

/// Write the initial topic_header row on first plan_state.db creation.
/// Uses INSERT OR IGNORE -- safe to call multiple times.
pub async fn initialise_topic_header(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
    name: Option<&str>,
    placeholder_name: &str,
) -> Result<(), PlanStateStoreError> {
    let timestamp = crate::providers::utils::now();
    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    sqlx::query(
        "INSERT OR IGNORE INTO topic_header
         (id, topic_id, focus_id, persona_id, name, placeholder_name,
          lifecycle_state, session_count, created_at, updated_at)
         VALUES (1, ?, ?, ?, ?, ?, 'active', 0, ?, ?)",
    )
    .bind(topic_id)
    .bind(focus_id)
    .bind(persona_id)
    .bind(name)
    .bind(placeholder_name)
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(&mut conn)
    .await?;

    Ok(())
}

/// Update topic_header cache copy.
/// Called by Phase 5A and Reconciliation Boot Check.
/// Source of truth is outputs.db -- this mirrors it.
/// Any combination of fields may be updated per call; all on one connection.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn update_topic_header(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
    lifecycle_state: Option<&str>,
    current_phase: Option<&str>,
    name: Option<&str>,
    increment_session: bool,
) -> Result<(), PlanStateStoreError> {
    let db_path = get_plan_state_db_path(user_id, persona_id, focus_id, topic_id);
    if !db_path.exists() {
        return Ok(());
    }

    let timestamp = crate::providers::utils::now();
    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    if let Some(state) = lifecycle_state {
        sqlx::query(
            "UPDATE topic_header SET lifecycle_state = ?, updated_at = ? WHERE id = 1",
        )
        .bind(state)
        .bind(&timestamp)
        .execute(&mut conn)
        .await?;
    }

    if let Some(phase) = current_phase {
        sqlx::query(
            "UPDATE topic_header SET current_phase = ?, updated_at = ? WHERE id = 1",
        )
        .bind(phase)
        .bind(&timestamp)
        .execute(&mut conn)
        .await?;
    }

    if let Some(n) = name {
        sqlx::query(
            "UPDATE topic_header SET name = ?, updated_at = ? WHERE id = 1",
        )
        .bind(n)
        .bind(&timestamp)
        .execute(&mut conn)
        .await?;
    }

    if increment_session {
        sqlx::query(
            "UPDATE topic_header \
             SET session_count = session_count + 1, updated_at = ? WHERE id = 1",
        )
        .bind(&timestamp)
        .execute(&mut conn)
        .await?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Block reads
// ---------------------------------------------------------------------------

/// Retrieval Eligibility Check -- pre-filter by visibility_scope vs tier ceiling.
/// Returns non-archived blocks eligible for the given execution_tier,
/// ordered by updated_at DESC (most recent first).
/// Gate1 applies content-level abstraction policy separately.
/// block_types: if provided, filters to specified types only.
/// max_tokens: stops accumulating when budget is reached. Non-fitting blocks
/// are skipped but remaining blocks are still checked -- intentional Python parity.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn get_eligible_blocks(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
    execution_tier: i32,
    max_tokens: Option<i32>,
    block_types: Option<Vec<String>>,
) -> Result<Vec<PlanStateBlock>, PlanStateStoreError> {
    let db_path = get_plan_state_db_path(user_id, persona_id, focus_id, topic_id);
    if !db_path.exists() {
        return Ok(vec![]);
    }

    let scopes = eligible_scopes_for_tier(execution_tier);
    if scopes.is_empty() {
        return Ok(vec![]);
    }

    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, block_type, content, visibility_scope, transformation,
                sensitivity_preset, relevance_tags, token_estimate, dependency_refs,
                inferred_by_system, focus_run_id, created_at, updated_at, archived_at
         FROM plan_state_blocks
         WHERE archived_at IS NULL AND visibility_scope IN (",
    );
    let mut sep = qb.separated(", ");
    for scope in scopes {
        sep.push_bind(*scope);
    }
    sep.push_unseparated(")");

    if let Some(ref types) = block_types {
        if !types.is_empty() {
            qb.push(" AND block_type IN (");
            let mut sep2 = qb.separated(", ");
            for t in types {
                sep2.push_bind(t.as_str());
            }
            sep2.push_unseparated(")");
        }
    }

    qb.push(" ORDER BY updated_at DESC");

    let rows = qb.build().fetch_all(&mut conn).await?;

    let mut blocks = Vec::new();
    let mut accumulated: i32 = 0;

    for row in rows {
        let token_estimate = row.try_get::<i64, _>("token_estimate")? as i32;

        if let Some(max) = max_tokens {
            if accumulated + token_estimate > max {
                continue;
            }
            accumulated += token_estimate;
        }

        // JSON parse fallback: .unwrap_or_default() returns [] on malformed data.
        // Intentional resilience -- a crashed read is worse than missing tags.
        // Diverges from Python (which would raise on invalid JSON).
        let relevance_tags: Vec<String> = serde_json::from_str(
            &row.try_get::<String, _>("relevance_tags")
                .unwrap_or_else(|_| "[]".to_owned()),
        )
        .unwrap_or_default();

        let dependency_refs: Vec<String> = serde_json::from_str(
            &row.try_get::<String, _>("dependency_refs")
                .unwrap_or_else(|_| "[]".to_owned()),
        )
        .unwrap_or_default();

        blocks.push(PlanStateBlock {
            id:                 row.try_get("id")?,
            block_type:         row.try_get("block_type")?,
            content:            row.try_get("content")?,
            visibility_scope:   row.try_get("visibility_scope")?,
            transformation:     row.try_get("transformation")?,
            sensitivity_preset: row.try_get("sensitivity_preset")?,
            token_estimate,
            inferred_by_system: row.try_get::<i64, _>("inferred_by_system")? != 0,
            focus_run_id:       row.try_get("focus_run_id")?,
            created_at:         row.try_get("created_at")?,
            updated_at:         row.try_get("updated_at")?,
            archived_at:        row.try_get("archived_at")?,
            relevance_tags,
            dependency_refs,
        });
    }

    Ok(blocks)
}

/// Compute the highest sensitivity_preset across a set of dependency block IDs.
/// Used by output block sensitivity inheritance (ADR-013 Section 5.7).
/// Sensitive data cannot be laundered through iterative summarisation.
/// Returns "standard" if no blocks found or no sensitivity set.
/// Preset ordering: standard(0) < sensitive(1) < private(2) < locked(3).
pub async fn get_sensitivity_ceiling(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
    block_ids: &[String],
) -> Result<String, PlanStateStoreError> {
    if block_ids.is_empty() {
        return Ok("standard".to_owned());
    }

    let db_path = get_plan_state_db_path(user_id, persona_id, focus_id, topic_id);
    if !db_path.exists() {
        return Ok("standard".to_owned());
    }

    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT sensitivity_preset FROM plan_state_blocks WHERE id IN (",
    );
    let mut sep = qb.separated(", ");
    for id in block_ids {
        sep.push_bind(id.as_str());
    }
    sep.push_unseparated(") AND sensitivity_preset IS NOT NULL");

    let rows = qb.build().fetch_all(&mut conn).await?;

    if rows.is_empty() {
        return Ok("standard".to_owned());
    }

    let max_rank = rows
        .iter()
        .filter_map(|r| {
            r.try_get::<Option<String>, _>("sensitivity_preset")
                .ok()
                .flatten()
        })
        .map(|preset| preset_rank(&preset))
        .max()
        .unwrap_or(0);

    Ok(rank_to_preset(max_rank).to_owned())
}

// ---------------------------------------------------------------------------
// Block writes
// ---------------------------------------------------------------------------

/// Write a plan state block. Returns the block id.
/// Updates state_ceiling_status.current_token_estimate after write.
///
/// Sensitivity inheritance invariant (ADR-013 Section 5.7):
/// If dependency_refs provided, sensitivity_preset is overridden by the
/// ceiling of referenced blocks if that ceiling is higher.
/// Sensitive data cannot be laundered through iterative summarisation.
///
/// PHASE 1 NOTE: write_block opens two connections total when dependency_refs
/// is non-empty: one for get_sensitivity_ceiling, one for INSERT+UPDATE.
/// SQLCipher PBKDF2 runs on each open. Target for shared connection in Layer 8+.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn write_block(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
    block_type: &str,
    content: &str,
    focus_run_id: &str,
    visibility_scope: &str,
    transformation: &str,
    sensitivity_preset: Option<&str>,
    relevance_tags: Option<&[String]>,
    token_estimate: i32,
    dependency_refs: Option<&[String]>,
    inferred_by_system: bool,
) -> Result<String, PlanStateStoreError> {
    let effective_preset: String = if let Some(refs) = dependency_refs {
        if !refs.is_empty() {
            let inherited = get_sensitivity_ceiling(
                user_id, persona_id, focus_id, topic_id, key_hex, refs,
            )
            .await?;
            let current_rank = preset_rank(sensitivity_preset.unwrap_or("standard"));
            let inherited_rank = preset_rank(&inherited);
            if inherited_rank > current_rank { inherited }
            else { sensitivity_preset.unwrap_or("standard").to_owned() }
        } else {
            sensitivity_preset.unwrap_or("standard").to_owned()
        }
    } else {
        sensitivity_preset.unwrap_or("standard").to_owned()
    };

    let block_id = uuid::Uuid::new_v4().to_string();
    let timestamp = crate::providers::utils::now();
    let tags_json = serde_json::to_string(relevance_tags.unwrap_or(&[]))
        .unwrap_or_else(|_| "[]".to_owned());
    let refs_json = serde_json::to_string(dependency_refs.unwrap_or(&[]))
        .unwrap_or_else(|_| "[]".to_owned());
    let inferred_flag: i64 = if inferred_by_system { 1 } else { 0 };

    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    // SAVEPOINT: INSERT and token counter UPDATE are a logical unit.
    // If UPDATE fails after INSERT, token estimate diverges from actual block count.
    // ROLLBACK TO used (not conn.begin()) to match the SAVEPOINT pattern in migrations.rs.
    sqlx::query("SAVEPOINT write_block_sp")
        .execute(&mut conn)
        .await?;

    let insert_result = sqlx::query(
        "INSERT INTO plan_state_blocks
         (id, block_type, content, visibility_scope, transformation,
          sensitivity_preset, relevance_tags, token_estimate, dependency_refs,
          inferred_by_system, focus_run_id, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&block_id)
    .bind(block_type)
    .bind(content)
    .bind(visibility_scope)
    .bind(transformation)
    .bind(&effective_preset)
    .bind(&tags_json)
    .bind(token_estimate)
    .bind(&refs_json)
    .bind(inferred_flag)
    .bind(focus_run_id)
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(&mut conn)
    .await;

    if let Err(e) = insert_result {
        // Rollback failure means the connection is already broken;
        // the upstream ? on the next operation will surface it.
        let _ = sqlx::query("ROLLBACK TO write_block_sp")
            .execute(&mut conn)
            .await;
        return Err(PlanStateStoreError::Database(e));
    }

    let update_result = sqlx::query(
        "UPDATE state_ceiling_status
         SET current_token_estimate = current_token_estimate + ? WHERE id = 1",
    )
    .bind(token_estimate)
    .execute(&mut conn)
    .await;

    if let Err(e) = update_result {
        let _ = sqlx::query("ROLLBACK TO write_block_sp")
            .execute(&mut conn)
            .await;
        return Err(PlanStateStoreError::Database(e));
    }

    sqlx::query("RELEASE write_block_sp")
        .execute(&mut conn)
        .await?;

    Ok(block_id)
}

/// Archive all active blocks when a topic closes.
/// Blocks retained -- not deleted. User preference determines archive vs discard.
/// Returns count of archived blocks.
pub async fn archive_all_blocks(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
) -> Result<u64, PlanStateStoreError> {
    let db_path = get_plan_state_db_path(user_id, persona_id, focus_id, topic_id);
    if !db_path.exists() {
        return Ok(0);
    }

    let timestamp = crate::providers::utils::now();
    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    let result = sqlx::query(
        "UPDATE plan_state_blocks SET archived_at = ? WHERE archived_at IS NULL",
    )
    .bind(&timestamp)
    .execute(&mut conn)
    .await?;

    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------
// Handoff tokens
// ---------------------------------------------------------------------------

/// Create a handoff token for an Awaiting state dependency.
/// Awaiting invariant: topic_id must be non-null -- enforced by callers.
/// Cross-db FK: topic_id references topics.id in outputs.db -- app-enforced.
/// Returns the token id.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn create_handoff_token(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
    focus_run_id: &str,
    action_id: &str,
    expiry_at: &str,
    expected_return_schema: Option<&serde_json::Value>,
) -> Result<String, PlanStateStoreError> {
    let token_id = uuid::Uuid::new_v4().to_string();
    let schema_json = serde_json::to_string(
        expected_return_schema.unwrap_or(&serde_json::json!({})),
    )
    .unwrap_or_else(|_| "{}".to_owned());
    let timestamp = crate::providers::utils::now();

    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    sqlx::query(
        "INSERT INTO handoff_tokens
         (id, topic_id, focus_run_id, action_id,
          expected_return_schema, created_at, expiry_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&token_id)
    .bind(topic_id)
    .bind(focus_run_id)
    .bind(action_id)
    .bind(&schema_json)
    .bind(&timestamp)
    .bind(expiry_at)
    .execute(&mut conn)
    .await?;

    Ok(token_id)
}

/// Mark a handoff token as consumed after a valid return result.
/// Returns true if found and consumed, false if not found or already consumed/expired.
///
/// Note: does not check expiry_at >= now() at the query layer -- a token past its
/// expiry_at but not yet swept by Boot Check can still be consumed. See module
/// header comment for future enhancement rationale.
pub async fn consume_handoff_token(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
    token_id: &str,
) -> Result<bool, PlanStateStoreError> {
    let db_path = get_plan_state_db_path(user_id, persona_id, focus_id, topic_id);
    if !db_path.exists() {
        return Ok(false);
    }

    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    let result = sqlx::query(
        "UPDATE handoff_tokens SET consumed_at = ?
         WHERE id = ? AND consumed_at IS NULL AND expired_at IS NULL",
    )
    .bind(crate::providers::utils::now())
    .bind(token_id)
    .execute(&mut conn)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Called by Reconciliation Boot Check. Marks all overdue unconsumed tokens as expired.
/// Returns count of expired tokens.
/// Boot Check then transitions topic to paused, reason: dependency_timeout.
/// Dashboard shows: "External update timed out -- retry or resume manually."
pub async fn expire_overdue_handoff_tokens(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
) -> Result<u64, PlanStateStoreError> {
    let db_path = get_plan_state_db_path(user_id, persona_id, focus_id, topic_id);
    if !db_path.exists() {
        return Ok(0);
    }

    let timestamp = crate::providers::utils::now();
    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    let result = sqlx::query(
        "UPDATE handoff_tokens SET expired_at = ?
         WHERE consumed_at IS NULL AND expired_at IS NULL AND expiry_at < ?",
    )
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(&mut conn)
    .await?;

    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------
// Soft ceiling
// ---------------------------------------------------------------------------

/// Read the soft ceiling notification state.
/// Returns None if plan_state.db does not exist.
/// System NEVER automatically closes or consolidates -- user controls response.
pub async fn get_state_ceiling_status(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
) -> Result<Option<StateCeilingStatus>, PlanStateStoreError> {
    let db_path = get_plan_state_db_path(user_id, persona_id, focus_id, topic_id);
    if !db_path.exists() {
        return Ok(None);
    }

    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT current_token_estimate, ceiling_threshold,
                notification_sent_at, user_response
         FROM state_ceiling_status WHERE id = 1",
    )
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(StateCeilingStatus {
            current_token_estimate: r.try_get::<i64, _>("current_token_estimate")? as i32,
            ceiling_threshold:      r.try_get::<i64, _>("ceiling_threshold")? as i32,
            notification_sent_at:   r.try_get("notification_sent_at")?,
            user_response:          r.try_get("user_response")?,
        })),
    }
}

/// Record that the soft ceiling consolidation prompt was surfaced to the user.
pub async fn record_ceiling_notification_sent(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
) -> Result<(), PlanStateStoreError> {
    let db_path = get_plan_state_db_path(user_id, persona_id, focus_id, topic_id);
    if !db_path.exists() {
        return Ok(());
    }

    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    sqlx::query(
        "UPDATE state_ceiling_status
         SET notification_sent_at = ?, user_response = NULL WHERE id = 1",
    )
    .bind(crate::providers::utils::now())
    .execute(&mut conn)
    .await?;

    Ok(())
}

/// Record the user's response to the soft ceiling consolidation prompt.
/// response must be "consolidate" or "continue" -- enforced by SQL CHECK constraint.
pub async fn record_ceiling_user_response(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
    response: &str,
) -> Result<(), PlanStateStoreError> {
    let db_path = get_plan_state_db_path(user_id, persona_id, focus_id, topic_id);
    if !db_path.exists() {
        return Ok(());
    }

    let mut conn = open_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;

    sqlx::query("UPDATE state_ceiling_status SET user_response = ? WHERE id = 1")
        .bind(response)
        .execute(&mut conn)
        .await?;

    Ok(())
}
