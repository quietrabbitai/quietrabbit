// src-tauri/src/persistence/domain_context_store.rs
//
// Domain Context CRUD for domain_context.db.
// Per-user, per-persona, per-focus encrypted database.
// Path: /users/{user_id}/personas/{persona_id}/focuses/{focus_id}/domain_context.db
//
// Extraction authority invariant (ADR-013 Section 6.6):
//   The generalisation pass is the ONLY mechanism that writes Domain Context.
//   Users review and approve extraction cards — they do not author blocks directly.
//   All writes: Phase 5B → pending_extractions → user review
//   → write_approved_extraction() → domain_context_blocks.
//
// Circular FK insertion order (see domain_context_001.sql):
//   1. INSERT provenance_log (approved_block_id = NULL)
//   2. INSERT domain_context_blocks (extraction_event_id = provenance_log.id)
//   3. UPDATE provenance_log SET approved_block_id = block.id
//   write_approved_extraction() enforces this atomically under a SAVEPOINT.
//
// KEY FORMAT: callers pass bare hex bytes only (e.g. "deadbeef...64chars").
// The x'...' wrapper is applied inside open_domain_context_db().
// Callers must NOT wrap the value in x'...' themselves.
//
// QUERY STYLE: all queries use sqlx::query() (runtime) — not query!() (compile-time).
// The many-small-encrypted-DBs topology has no static DATABASE_URL, so compile-time
// query verification is unavailable. Row extraction uses sqlx::Row::try_get().

use std::path::{Path, PathBuf};

use sqlx::sqlite::{SqliteConnectOptions, SqliteRow};
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum DomainContextStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Migration error: {0}")]
    Migration(#[from] crate::persistence::migrations::MigrationError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DomainContextBlock {
    pub id: String,
    pub content: String,
    pub visibility_scope: String,
    pub transformation: String,
    pub sensitivity_preset: String,
    pub source_topic_id: String,
    pub extraction_event_id: String,
    pub token_estimate: i32,
    pub inferred_by_system: bool,
    pub standing_summary_eligible: bool,
    pub created_at: String,
    pub updated_at: String,
    pub relevance_tags: Vec<String>,
    pub dependency_refs: Vec<String>,
    pub revoked_at: Option<String>,
    pub revocation_reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StandingSummary {
    pub content: String,
    pub token_count: i32,
    pub source_block_ids: Vec<String>,
    pub generated_at: String,
    pub invalidated_at: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingExtraction {
    pub id: String,
    pub source_topic_id: String,
    pub source_focus_run_id: String,
    pub proposed_content: String,
    pub proposed_preset: String,
    pub status: String,
    pub created_at: String,
    pub reviewed_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Tier ceiling constants
// ---------------------------------------------------------------------------

// Visibility scopes eligible for each execution tier.
// Structural access control only — Gate1 applies content-level abstraction separately.
struct TierScopes {
    tier: i32,
    scopes: &'static [&'static str],
}

static ELIGIBLE_SCOPES_BY_TIER: &[TierScopes] = &[
    TierScopes {
        tier: 1,
        scopes: &["tier_1_only", "anonymous_tier2", "tier2_permitted", "tier3_permitted"],
    },
    TierScopes {
        tier: 2,
        scopes: &["anonymous_tier2", "tier2_permitted", "tier3_permitted"],
    },
    TierScopes {
        tier: 3,
        scopes: &["tier3_permitted"],
    },
];

fn get_eligible_scopes(execution_tier: i32) -> &'static [&'static str] {
    ELIGIBLE_SCOPES_BY_TIER
        .iter()
        .find(|t| t.tier == execution_tier)
        .map(|t| t.scopes)
        .unwrap_or(&[])
}

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

fn get_domain_context_path(user_id: &str, persona_id: &str, focus_id: &str) -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("focuses")
        .join(focus_id)
        .join("domain_context.db")
}

// ---------------------------------------------------------------------------
// DB opener
// ---------------------------------------------------------------------------

/// Open domain_context.db with key and journal_mode applied via SqliteConnectOptions.
/// PRAGMA key fires before journal_mode in the pragma batch — ordering guaranteed
/// by SqliteConnectOptions.pragma() insertion order (D6-346).
/// key_hex: bare hex bytes only (no x'...' wrapper).
async fn open_domain_context_db(
    db_path: &Path,
    key_hex: &str,
) -> Result<SqliteConnection, DomainContextStoreError> {
    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let journal_mode = if network_storage { "DELETE" } else { "WAL" };

    // PRAGMA key MUST be registered before journal_mode (D6-346).
    // SqliteConnectOptions.pragma() fires pragmas in insertion order after
    // sqlite3_open_v2 — key is first, journal_mode second.
    let conn = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(false)
        .pragma("key", format!("x'{key_hex}'"))
        .pragma("journal_mode", journal_mode)
        .connect()
        .await?;

    Ok(conn)
}

// ---------------------------------------------------------------------------
// Row extraction helpers
// ---------------------------------------------------------------------------

/// Extract a boolean from an INTEGER column. Treats NULL as false.
fn bool_col(row: &SqliteRow, col: &str) -> Result<bool, sqlx::Error> {
    let v: Option<i64> = row.try_get(col)?;
    Ok(v.unwrap_or(0) != 0)
}

/// Extract an i32 from an INTEGER column (SQLite returns i64).
fn i32_col(row: &SqliteRow, col: &str) -> Result<i32, sqlx::Error> {
    let v: i64 = row.try_get(col)?;
    Ok(v as i32)
}

/// Parse a JSON TEXT column into Vec<String>. Returns empty vec on NULL or parse failure.
fn json_vec_col(row: &SqliteRow, col: &str) -> Result<Vec<String>, sqlx::Error> {
    let raw: Option<String> = row.try_get(col)?;
    let s = raw.unwrap_or_else(|| "[]".to_owned());
    // TODO: log parse failure for forensic visibility
    Ok(serde_json::from_str(&s).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Migration
// ---------------------------------------------------------------------------

/// Ensure domain_context.db exists and is migrated.
/// Creates the focus directory if needed (lazy initialisation).
/// Returns the database path.
pub async fn ensure_domain_context_db(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    key_hex: &str,
) -> Result<PathBuf, DomainContextStoreError> {
    let db_path = get_domain_context_path(user_id, persona_id, focus_id);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::persistence::migrations::migrate_domain_context_db(
        user_id, persona_id, focus_id, key_hex,
    )
    .await?;
    Ok(db_path)
}

// ---------------------------------------------------------------------------
// Retrieval
// ---------------------------------------------------------------------------

/// Retrieval Eligibility Check — pre-filter by visibility_scope vs tier ceiling.
/// Returns non-revoked blocks eligible for the given execution_tier,
/// ordered by token_estimate ASC (smallest-first greedy packing).
/// Stops at first overflow (break) — rows are sorted ASC so all subsequent
/// rows are the same size or larger.
/// Gate1 applies content-level abstraction policy separately.
pub async fn get_eligible_blocks(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    key_hex: &str,
    execution_tier: i32,
    max_tokens: Option<i32>,
) -> Result<Vec<DomainContextBlock>, DomainContextStoreError> {
    let db_path = get_domain_context_path(user_id, persona_id, focus_id);
    if !db_path.exists() {
        return Ok(vec![]);
    }

    let eligible_scopes = get_eligible_scopes(execution_tier);
    if eligible_scopes.is_empty() {
        return Ok(vec![]);
    }

    let mut conn = open_domain_context_db(&db_path, key_hex).await?;

    // Use QueryBuilder to construct the IN (...) clause safely.
    // push_bind() handles placeholder insertion and binding atomically —
    // no manual ? counting required.
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, content, visibility_scope, transformation, sensitivity_preset,
         source_topic_id, extraction_event_id, token_estimate, inferred_by_system,
         standing_summary_eligible, relevance_tags, dependency_refs, revoked_at,
         revocation_reason, created_at, updated_at
         FROM domain_context_blocks
         WHERE revoked_at IS NULL AND visibility_scope IN (",
    );

    let mut separated = qb.separated(", ");
    for scope in eligible_scopes {
        separated.push_bind(*scope);
    }
    qb.push(") ORDER BY token_estimate ASC");

    let rows = qb.build().fetch_all(&mut conn).await?;

    let mut blocks = Vec::new();
    let mut accumulated = 0i32;

    for row in rows {
        let token_estimate = i32_col(&row, "token_estimate")?;
        if let Some(max) = max_tokens {
            if accumulated + token_estimate > max {
                break; // sorted ASC — all subsequent rows are same size or larger
            }
            accumulated += token_estimate;
        }
        blocks.push(DomainContextBlock {
            id: row.try_get("id")?,
            content: row.try_get("content")?,
            visibility_scope: row.try_get("visibility_scope")?,
            transformation: row.try_get("transformation")?,
            sensitivity_preset: row.try_get("sensitivity_preset")?,
            source_topic_id: row.try_get("source_topic_id")?,
            extraction_event_id: row.try_get("extraction_event_id")?,
            token_estimate,
            inferred_by_system: bool_col(&row, "inferred_by_system")?,
            standing_summary_eligible: bool_col(&row, "standing_summary_eligible")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            relevance_tags: json_vec_col(&row, "relevance_tags")?,
            dependency_refs: json_vec_col(&row, "dependency_refs")?,
            revoked_at: row.try_get("revoked_at")?,
            revocation_reason: row.try_get("revocation_reason")?,
        });
    }

    Ok(blocks)
}

/// Fetch the current standing summary.
/// Returns None if domain_context.db does not exist or no summary row exists.
/// invalidated_at non-null signals summary needs regeneration.
pub async fn get_standing_summary(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    key_hex: &str,
) -> Result<Option<StandingSummary>, DomainContextStoreError> {
    let db_path = get_domain_context_path(user_id, persona_id, focus_id);
    if !db_path.exists() {
        return Ok(None);
    }

    let mut conn = open_domain_context_db(&db_path, key_hex).await?;

    let row = sqlx::query(
        "SELECT content, token_count, source_block_ids, generated_at, invalidated_at
         FROM standing_summary WHERE id = 1",
    )
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(StandingSummary {
            content: r.try_get("content")?,
            token_count: i32_col(&r, "token_count")?,
            source_block_ids: json_vec_col(&r, "source_block_ids")?,
            generated_at: r.try_get("generated_at")?,
            invalidated_at: r.try_get("invalidated_at")?,
        })),
    }
}

/// List all pending_review extraction cards for user review.
pub async fn list_pending_extractions(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    key_hex: &str,
) -> Result<Vec<PendingExtraction>, DomainContextStoreError> {
    let db_path = get_domain_context_path(user_id, persona_id, focus_id);
    if !db_path.exists() {
        return Ok(vec![]);
    }

    let mut conn = open_domain_context_db(&db_path, key_hex).await?;

    let rows = sqlx::query(
        "SELECT id, source_topic_id, source_focus_run_id, proposed_content,
         proposed_preset, status, created_at, reviewed_at
         FROM pending_extractions WHERE status = 'pending_review'
         ORDER BY created_at ASC",
    )
    .fetch_all(&mut conn)
    .await?;

    let mut extractions = Vec::new();
    for r in rows {
        extractions.push(PendingExtraction {
            id: r.try_get("id")?,
            source_topic_id: r.try_get("source_topic_id")?,
            source_focus_run_id: r.try_get("source_focus_run_id")?,
            proposed_content: r.try_get("proposed_content")?,
            proposed_preset: r.try_get("proposed_preset")?,
            status: r.try_get("status")?,
            created_at: r.try_get("created_at")?,
            reviewed_at: r.try_get("reviewed_at")?,
        });
    }
    Ok(extractions)
}

// ---------------------------------------------------------------------------
// Write operations
// ---------------------------------------------------------------------------

/// Write a Phase 5B generalisation pass output to pending_extractions staging.
/// Returns the pending extraction id.
pub async fn write_pending_extraction(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    key_hex: &str,
    source_topic_id: &str,
    source_focus_run_id: &str,
    proposed_content: &str,
    proposed_preset: &str,
) -> Result<String, DomainContextStoreError> {
    let entry_id = uuid::Uuid::new_v4().to_string();
    let db_path = get_domain_context_path(user_id, persona_id, focus_id);
    let timestamp = crate::providers::utils::now();

    let mut conn = open_domain_context_db(&db_path, key_hex).await?;

    sqlx::query(
        "INSERT INTO pending_extractions
         (id, source_topic_id, source_focus_run_id, proposed_content,
          proposed_preset, status, created_at)
         VALUES (?, ?, ?, ?, ?, 'pending_review', ?)",
    )
    .bind(&entry_id)
    .bind(source_topic_id)
    .bind(source_focus_run_id)
    .bind(proposed_content)
    .bind(proposed_preset)
    .bind(&timestamp)
    .execute(&mut conn)
    .await?;

    Ok(entry_id)
}

/// Write an approved extraction from pending_extractions to domain_context_blocks.
///
/// Circular FK insertion order enforced atomically under a SAVEPOINT:
///   1. INSERT provenance_log (approved_block_id = NULL)
///   2. INSERT domain_context_blocks (extraction_event_id = provenance_log.id)
///   3. UPDATE provenance_log SET approved_block_id = block.id
///   4. UPDATE pending_extractions → 'approved'
///   5. UPDATE standing_summary invalidated_at
///
/// Returns the new domain_context_blocks.id.
pub async fn write_approved_extraction(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    key_hex: &str,
    pending_id: &str,
    final_content: &str,
    visibility_scope: &str,
    transformation: &str,
    sensitivity_preset: &str,
    source_topic_id: &str,
    source_focus_run_id: &str,
    inferred_by_system: bool,
    relevance_tags: Option<Vec<String>>,
    token_estimate: i32,
) -> Result<String, DomainContextStoreError> {
    let timestamp = crate::providers::utils::now();
    let provenance_id = uuid::Uuid::new_v4().to_string();
    let block_id = uuid::Uuid::new_v4().to_string();
    let tags_json = serde_json::to_string(&relevance_tags.unwrap_or_default())
        .unwrap_or_else(|_| "[]".to_owned());
    let db_path = get_domain_context_path(user_id, persona_id, focus_id);
    let inferred_flag: i32 = if inferred_by_system { 1 } else { 0 };

    let mut conn = open_domain_context_db(&db_path, key_hex).await?;

    sqlx::query("SAVEPOINT write_approved")
        .execute(&mut conn)
        .await?;

    let step: Result<(), sqlx::Error> = async {
        // Step 1: insert provenance_log with approved_block_id = NULL
        sqlx::query(
            "INSERT INTO provenance_log
             (id, source_topic_id, source_focus_run_id, proposed_content,
              approval_action, edited_content, approved_block_id, approved_at)
             VALUES (?, ?, ?, ?, 'approved', ?, NULL, ?)",
        )
        .bind(&provenance_id)
        .bind(source_topic_id)
        .bind(source_focus_run_id)
        .bind(final_content)
        .bind(final_content)
        .bind(&timestamp)
        .execute(&mut conn)
        .await?;

        // Step 2: insert domain_context_blocks
        sqlx::query(
            "INSERT INTO domain_context_blocks
             (id, content, visibility_scope, transformation, sensitivity_preset,
              relevance_tags, token_estimate, dependency_refs,
              source_topic_id, extraction_event_id, inferred_by_system,
              standing_summary_eligible, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, '[]', ?, ?, ?, 1, ?, ?)",
        )
        .bind(&block_id)
        .bind(final_content)
        .bind(visibility_scope)
        .bind(transformation)
        .bind(sensitivity_preset)
        .bind(&tags_json)
        .bind(token_estimate)
        .bind(source_topic_id)
        .bind(&provenance_id)
        .bind(inferred_flag)
        .bind(&timestamp)
        .bind(&timestamp)
        .execute(&mut conn)
        .await?;

        // Step 3: update provenance_log with the new block id
        sqlx::query("UPDATE provenance_log SET approved_block_id = ? WHERE id = ?")
            .bind(&block_id)
            .bind(&provenance_id)
            .execute(&mut conn)
            .await?;

        // Step 4: mark pending extraction as approved
        sqlx::query(
            "UPDATE pending_extractions SET status = 'approved', reviewed_at = ? WHERE id = ?",
        )
        .bind(&timestamp)
        .bind(pending_id)
        .execute(&mut conn)
        .await?;

        // Step 5: invalidate standing summary
        sqlx::query("UPDATE standing_summary SET invalidated_at = ? WHERE id = 1")
            .bind(&timestamp)
            .execute(&mut conn)
            .await?;

        Ok(())
    }
    .await;

    match step {
        Ok(()) => {
            sqlx::query("RELEASE write_approved")
                .execute(&mut conn)
                .await?;
        }
        Err(e) => {
            // ROLLBACK failure is swallowed — mirrors Python except: pass.
            // TODO: log rollback failure for forensic visibility.
            let _ = sqlx::query("ROLLBACK TO write_approved")
                .execute(&mut conn)
                .await;
            return Err(DomainContextStoreError::Database(e));
        }
    }

    Ok(block_id)
}

/// Mark a pending extraction as discarded and write provenance record.
/// pending_extractions row marked discarded (purged later).
/// provenance_log entry written and retained permanently (D6-198).
pub async fn discard_pending_extraction(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    key_hex: &str,
    pending_id: &str,
    source_topic_id: &str,
    source_focus_run_id: &str,
    proposed_content: &str,
) -> Result<(), DomainContextStoreError> {
    let timestamp = crate::providers::utils::now();
    let provenance_id = uuid::Uuid::new_v4().to_string();
    let db_path = get_domain_context_path(user_id, persona_id, focus_id);

    let mut conn = open_domain_context_db(&db_path, key_hex).await?;

    sqlx::query(
        "INSERT INTO provenance_log
         (id, source_topic_id, source_focus_run_id, proposed_content,
          approval_action, edited_content, approved_block_id, approved_at)
         VALUES (?, ?, ?, ?, 'discarded', NULL, NULL, ?)",
    )
    .bind(&provenance_id)
    .bind(source_topic_id)
    .bind(source_focus_run_id)
    .bind(proposed_content)
    .bind(&timestamp)
    .execute(&mut conn)
    .await?;

    sqlx::query(
        "UPDATE pending_extractions SET status = 'discarded', reviewed_at = ? WHERE id = ?",
    )
    .bind(&timestamp)
    .bind(pending_id)
    .execute(&mut conn)
    .await?;

    Ok(())
}

/// Revoke a domain_context_blocks entry.
/// Sets revoked_at and revocation_reason — row is NOT deleted (audit trail).
/// Invalidates standing summary after revocation.
/// Returns true if found and revoked, false if not found or already revoked.
pub async fn revoke_block(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    key_hex: &str,
    block_id: &str,
    reason: &str,
) -> Result<bool, DomainContextStoreError> {
    let timestamp = crate::providers::utils::now();
    let db_path = get_domain_context_path(user_id, persona_id, focus_id);
    if !db_path.exists() {
        return Ok(false);
    }

    let mut conn = open_domain_context_db(&db_path, key_hex).await?;

    let result = sqlx::query(
        "UPDATE domain_context_blocks
         SET revoked_at = ?, revocation_reason = ?, updated_at = ?
         WHERE id = ? AND revoked_at IS NULL",
    )
    .bind(&timestamp)
    .bind(reason)
    .bind(&timestamp)
    .bind(block_id)
    .execute(&mut conn)
    .await?;

    let revoked = result.rows_affected() > 0;
    if revoked {
        sqlx::query("UPDATE standing_summary SET invalidated_at = ? WHERE id = 1")
            .bind(&timestamp)
            .execute(&mut conn)
            .await?;
    }

    Ok(revoked)
}

/// Regenerate the standing summary from all eligible blocks.
/// Called after any domain_context_blocks write or revocation.
/// Enforces max_tokens ceiling — rows sorted ASC, first overflow triggers break.
/// Returns the new StandingSummary.
pub async fn regenerate_standing_summary(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    key_hex: &str,
    max_tokens: i32,
) -> Result<StandingSummary, DomainContextStoreError> {
    let db_path = get_domain_context_path(user_id, persona_id, focus_id);
    let timestamp = crate::providers::utils::now();

    let mut conn = open_domain_context_db(&db_path, key_hex).await?;

    let rows = sqlx::query(
        "SELECT id, content, token_estimate FROM domain_context_blocks
         WHERE revoked_at IS NULL AND standing_summary_eligible = 1
         ORDER BY token_estimate ASC",
    )
    .fetch_all(&mut conn)
    .await?;

    let mut assembled: Vec<String> = Vec::new();
    let mut source_ids: Vec<String> = Vec::new();
    let mut total_tokens = 0i32;

    for row in rows {
        let token_estimate = i32_col(&row, "token_estimate")?;
        if total_tokens + token_estimate > max_tokens {
            break; // sorted ASC — all subsequent rows are same size or larger
        }
        assembled.push(row.try_get("content")?);
        source_ids.push(row.try_get("id")?);
        total_tokens += token_estimate;
    }

    let summary_content = assembled.join("\n\n");
    let source_ids_json =
        serde_json::to_string(&source_ids).unwrap_or_else(|_| "[]".to_owned());

    sqlx::query(
        "UPDATE standing_summary SET content = ?, token_count = ?,
         source_block_ids = ?, generated_at = ?, invalidated_at = NULL
         WHERE id = 1",
    )
    .bind(&summary_content)
    .bind(total_tokens)
    .bind(&source_ids_json)
    .bind(&timestamp)
    .execute(&mut conn)
    .await?;

    Ok(StandingSummary {
        content: summary_content,
        token_count: total_tokens,
        source_block_ids: source_ids,
        generated_at: timestamp,
        invalidated_at: None,
    })
}
