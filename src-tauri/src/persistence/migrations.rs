// src-tauri/src/persistence/migrations.rs
//
// Database migration runner — faithful port of persistence/migrations.py.
//
// ATOMICITY: sqlx SqliteConnection operates in autocommit mode. SAVEPOINTs
// are used directly (no BEGIN/COMMIT wrappers) — SAVEPOINT outside a BEGIN
// acts as the outermost transaction; RELEASE commits it atomically. This
// matches the Python implementation which avoided executescript() for the
// same reason (implicit COMMIT breaks SAVEPOINT atomicity).
//
// SCHEMA EMBEDDING: SQL files are embedded at compile time via include_str!()
// from src-tauri/schema/. The crate owns its schema assets — no runtime path
// resolution required and no Tauri AppHandle dependency in the runner API.
//
// KEY FORMAT: callers pass bare hex bytes only (e.g. "deadbeef...64chars").
// The PRAGMA is constructed here as: PRAGMA key = "x'{key_hex}'"
// Callers must NOT wrap the value in x'...' themselves.
//
// SCHEMA AUTHORING RULE: no semicolons inside string literals in .sql files.
// parse_statements() is not a general-purpose SQL parser.
//
// LOCK IDENTITY: hostname:pid:uuid (uuid generated once per process startup
// via OnceLock). The UUID component eliminates PID-reuse false ownership.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::SqliteConnection;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum MigrationError {
    /// User-facing migration failure. plain_language is shown to the user;
    /// diagnostic carries the underlying sqlx error string for internal use.
    #[error("{plain_language}")]
    Failed {
        db_path: String,
        plain_language: String,
        diagnostic: Option<String>,
    },
    #[error("Migration lock held by another process — try again in a moment")]
    Locked,
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Embedded schema files
// ---------------------------------------------------------------------------
// KEY FORMAT INVARIANT: callers pass hex bytes only. This file constructs the
// full PRAGMA key = "x'...'" syntax. Do not change without updating all callers.
//
// Manifest ordering rule: versions MUST be strictly increasing within each
// prefix. Enforced at runtime by validate_manifest() on every run_migrations call.

struct SchemaFile {
    prefix: &'static str,
    version: u32,
    sql: &'static str,
}

static SCHEMA_FILES: &[SchemaFile] = &[
    SchemaFile {
        prefix: "domain_context",
        version: 1,
        sql: include_str!("../../schema/domain_context_001.sql"),
    },
    SchemaFile {
        prefix: "group",
        version: 1,
        sql: include_str!("../../schema/group_001.sql"),
    },
    SchemaFile {
        prefix: "keys",
        version: 1,
        sql: include_str!("../../schema/keys_001.sql"),
    },
    SchemaFile {
        prefix: "messages",
        version: 1,
        sql: include_str!("../../schema/messages_001.sql"),
    },
    SchemaFile {
        prefix: "outputs",
        version: 1,
        sql: include_str!("../../schema/outputs_001.sql"),
    },
    SchemaFile {
        prefix: "outputs",
        version: 2,
        sql: include_str!("../../schema/outputs_002.sql"),
    },
    SchemaFile {
        prefix: "outputs",
        version: 3,
        sql: include_str!("../../schema/outputs_003.sql"),
    },
    SchemaFile {
        prefix: "personal",
        version: 1,
        sql: include_str!("../../schema/personal_001.sql"),
    },
    SchemaFile {
        prefix: "personal",
        version: 2,
        sql: include_str!("../../schema/personal_002.sql"),
    },
    SchemaFile {
        prefix: "personal",
        version: 3,
        sql: include_str!("../../schema/personal_003.sql"),
    },
    SchemaFile {
        prefix: "personal",
        version: 4,
        sql: include_str!("../../schema/personal_004.sql"),
    },
    SchemaFile {
        prefix: "plan_state",
        version: 1,
        sql: include_str!("../../schema/plan_state_001.sql"),
    },
    SchemaFile {
        prefix: "scores",
        version: 1,
        sql: include_str!("../../schema/scores_001.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 1,
        sql: include_str!("../../schema/shared_001.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 2,
        sql: include_str!("../../schema/shared_002.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 3,
        sql: include_str!("../../schema/shared_003.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 4,
        sql: include_str!("../../schema/shared_004.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 5,
        sql: include_str!("../../schema/shared_005.sql"),
    },
    SchemaFile {
        prefix: "tier3_cookies",
        version: 1,
        sql: include_str!("../../schema/tier3_cookies_001.sql"),
    },
];

/// Validate manifest ordering on every run_migrations call.
/// O(19) — negligible cost. Manifest corruption is a build problem, not a
/// perf concern, so this runs unconditionally (not debug-only).
/// Walks SCHEMA_FILES in declaration order and tracks per-prefix max version
/// via HashMap — catches both non-contiguous interleaving and out-of-order
/// versions within a prefix block.
fn validate_manifest() {
    let mut max_versions: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for f in SCHEMA_FILES {
        if let Some(&prev) = max_versions.get(f.prefix) {
            assert!(
                f.version > prev,
                "SCHEMA_FILES: prefix '{}' version {} not strictly \
                 greater than previous version {}",
                f.prefix,
                f.version,
                prev
            );
        }
        max_versions.insert(f.prefix, f.version);
    }
}

/// v1 schema files are always re-run (see run_pending) to pick up in-place
/// amendments, so they must consist solely of idempotent statements. Scans
/// parsed statements (not raw source) so that explanatory comments in v2+
/// files mentioning "ALTER TABLE" can't false-positive a v1 file that merely
/// quotes them.
fn validate_v1_rerun_safety() {
    for f in SCHEMA_FILES.iter().filter(|f| f.version == 1) {
        for stmt in parse_statements(f.sql) {
            let upper = stmt.trim_start().to_uppercase();
            assert!(
                !upper.starts_with("ALTER TABLE") && !upper.starts_with("DROP "),
                "{}_001.sql (v1, always re-run every startup) contains a non-\
                 idempotent statement: {stmt:?} — use a new versioned migration \
                 file (v2+) instead of amending a v1 file in place for this change",
                f.prefix,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Return (version, sql) pairs for the given prefix, in version order.
fn get_migration_files(prefix: &str) -> Vec<(u32, &'static str)> {
    let mut files: Vec<(u32, &'static str)> = SCHEMA_FILES
        .iter()
        .filter(|f| f.prefix == prefix)
        .map(|f| (f.version, f.sql))
        .collect();
    files.sort_by_key(|(v, _)| *v);
    files
}

/// Return the highest migration version applied to this database.
/// Returns 0 on any error (including missing schema_version table).
async fn get_applied_version(conn: &mut SqliteConnection) -> u32 {
    let result: Result<Option<(Option<i64>,)>, _> =
        sqlx::query_as("SELECT MAX(version) FROM schema_version")
            .fetch_optional(conn)
            .await;
    match result {
        Ok(Some((Some(v),))) if v > 0 => v as u32,
        _ => 0,
    }
}

/// Split a SQL file into individual statements for execution.
/// Strips -- comment lines. Handles CREATE TRIGGER...END blocks atomically.
/// Faithful port of Python _parse_statements(sql).
/// Constraint: no semicolons inside string literals (see module header).
pub fn parse_statements(sql: &str) -> Vec<String> {
    let stripped: Vec<&str> = sql
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with("--")
        })
        .collect();
    let stripped_sql = stripped.join("\n");

    let mut statements: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut in_trigger = false;

    for line in stripped_sql.lines() {
        let upper = line.trim().to_uppercase();

        if upper.starts_with("CREATE TRIGGER") || upper.starts_with("CREATE OR REPLACE TRIGGER") {
            in_trigger = true;
        }

        current.push(line);

        if in_trigger {
            if upper == "END" || upper == "END;" {
                let stmt = current.join("\n").trim().to_owned();
                if !stmt.is_empty() {
                    statements.push(stmt);
                }
                current.clear();
                in_trigger = false;
            }
        } else if line.trim_end().ends_with(';') {
            let stmt = current
                .join("\n")
                .trim_end()
                .trim_end_matches(';')
                .trim()
                .to_owned();
            if !stmt.is_empty() {
                statements.push(stmt);
            }
            current.clear();
        }
    }

    let remainder = current.join("\n").trim().to_owned();
    if !remainder.is_empty() {
        statements.push(remainder);
    }

    statements
}

/// RFC3339 timestamp for migration_lock.locked_at.
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// "hostname:pid:uuid" lock identity string.
/// UUID is generated once per process startup via OnceLock — eliminates
/// PID-reuse false ownership. Stale locks (process died holding lock) are
/// not automatically recovered; they require manual intervention or a
/// future lock-expiry mechanism.
fn process_id() -> String {
    static UUID: OnceLock<String> = OnceLock::new();
    let uuid = UUID.get_or_init(|| uuid::Uuid::new_v4().to_string());
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    format!("{}:{}:{}", host, std::process::id(), uuid)
}

/// Open a raw SqliteConnection without key or journal configuration.
/// Callers apply key and journal_mode via run_migrations().
async fn open_raw(path: &Path) -> Result<SqliteConnection, MigrationError> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    Ok(opts.connect().await?)
}

/// Create migration_lock table and seed row atomically under a SAVEPOINT.
/// Safe to call on already-migrated databases — IF NOT EXISTS and
/// INSERT OR IGNORE are no-ops.
/// No COMMIT needed after RELEASE — SAVEPOINT outside a BEGIN is the
/// outermost transaction; RELEASE commits atomically in autocommit mode.
async fn bootstrap_lock_table(conn: &mut SqliteConnection) -> Result<(), MigrationError> {
    sqlx::query("SAVEPOINT bootstrap_lock")
        .execute(&mut *conn)
        .await?;

    let result = async {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS migration_lock (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                locked_at TEXT,
                locked_by TEXT
            )",
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query("INSERT OR IGNORE INTO migration_lock (id) VALUES (1)")
            .execute(&mut *conn)
            .await?;
        Ok::<_, sqlx::Error>(())
    }
    .await;

    match result {
        Ok(()) => {
            sqlx::query("RELEASE bootstrap_lock")
                .execute(&mut *conn)
                .await?;
        }
        Err(e) => {
            let _ = sqlx::query("ROLLBACK TO bootstrap_lock")
                .execute(&mut *conn)
                .await;
            return Err(MigrationError::Sqlx(e));
        }
    }

    Ok(())
}

/// Acquire migration_lock. Returns true if acquired, false if already locked.
/// Uses rows_affected from the UPDATE — atomically confirms this invocation
/// changed the lock state rather than checking ownership after the fact.
/// Predicate guards both columns: protects against a future bug that might
/// leave locked_at=NULL with stale locked_by metadata.
/// No COMMIT needed — autocommit fires immediately after each statement.
async fn acquire_lock(conn: &mut SqliteConnection) -> Result<bool, MigrationError> {
    let pid = process_id();
    let result = sqlx::query(
        "UPDATE migration_lock SET locked_at = ?, locked_by = ? \
         WHERE id = 1 AND locked_at IS NULL AND locked_by IS NULL",
    )
    .bind(now())
    .bind(&pid)
    .execute(&mut *conn)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Release migration_lock unconditionally. Errors are swallowed — mirrors
/// Python release_lock() which uses bare except pass.
async fn release_lock(conn: &mut SqliteConnection) {
    let _ =
        sqlx::query("UPDATE migration_lock SET locked_at = NULL, locked_by = NULL WHERE id = 1")
            .execute(&mut *conn)
            .await;
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Return true if schema_version table exists in the database at db_path.
/// Opens and closes its own short-lived connection.
/// Returns false if the file does not exist (fast path, no connection opened).
/// Returns false on any error (treated as uninitialised) — mirrors Python behavior.
///
/// key_hex: bare hex bytes only (no x'...' wrapper) — or None for unencrypted.
pub async fn schema_version_exists(db_path: &Path, key_hex: Option<&str>) -> bool {
    if !db_path.exists() {
        return false;
    }
    let mut conn = match open_raw(db_path).await {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(key) = key_hex {
        // PRAGMA key MUST be the first statement on an encrypted connection.
        let pragma = format!("PRAGMA key = \"x'{key}'\"");
        if sqlx::query(&pragma).execute(&mut conn).await.is_err() {
            return false;
        }
    }
    let result: Result<Option<(String,)>, _> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='schema_version'",
    )
    .fetch_optional(&mut conn)
    .await;
    matches!(result, Ok(Some(_)))
}

/// Apply all pending migrations for the given prefix to conn.
/// PRAGMA key (if provided) is applied before any other operation.
/// Returns number of migrations applied.
///
/// key_hex: bare hex bytes only (no x'...' wrapper) — or None for unencrypted.
pub async fn run_migrations(
    conn: &mut SqliteConnection,
    prefix: &str,
    key_hex: Option<&str>,
) -> Result<u32, MigrationError> {
    // Always validate manifest — O(19), negligible cost, catches hand-edit errors.
    validate_manifest();
    validate_v1_rerun_safety();

    // PRAGMA key MUST precede journal_mode — non-negotiable (CLAUDE.md).
    if let Some(key) = key_hex {
        let pragma = format!("PRAGMA key = \"x'{key}'\"");
        sqlx::query(&pragma).execute(&mut *conn).await?;
    }

    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);

    if network_storage {
        sqlx::query("PRAGMA journal_mode=DELETE")
            .execute(&mut *conn)
            .await?;
    } else {
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&mut *conn)
            .await?;
    }

    sqlx::query("PRAGMA busy_timeout=5000")
        .execute(&mut *conn)
        .await?;

    bootstrap_lock_table(conn).await?;

    if !acquire_lock(conn).await? {
        return Err(MigrationError::Locked);
    }

    let result = run_pending(conn, prefix).await;
    release_lock(conn).await;
    result
}

/// Inner migration loop — runs after lock is acquired.
async fn run_pending(conn: &mut SqliteConnection, prefix: &str) -> Result<u32, MigrationError> {
    let current_version = get_applied_version(conn).await;
    let migrations = get_migration_files(prefix);
    let mut applied: u32 = 0;

    for (version, sql) in migrations {
        let already_applied = version <= current_version;
        // v1 schema files are this project's amend-in-place surface (see
        // CLAUDE.md Schema Authoring convention + shared_001.sql's
        // tier3_providers precedent, items.id=228). They are required to
        // consist solely of idempotent statements (enforced by
        // validate_v1_rerun_safety() below), so re-running them on every
        // call is always safe and is how a v1 file amended after a database
        // already recorded version 1 gets picked up without deleting the
        // database. Versions 2+ are real incremental migrations (may
        // contain ALTER TABLE) and must still run exactly once.
        if already_applied && version != 1 {
            continue;
        }

        let savepoint = format!("migration_v{version}");
        let statements = parse_statements(sql);

        let step_result: Result<(), sqlx::Error> = async {
            sqlx::query(&format!("SAVEPOINT {savepoint}"))
                .execute(&mut *conn)
                .await?;

            for stmt in &statements {
                sqlx::query(stmt).execute(&mut *conn).await?;
            }

            // Record the applied version inside the SAVEPOINT so that schema
            // content and tracking record commit or rollback atomically.
            // Every current schema file's own trailing INSERT (see SCHEMA
            // AUTHORING convention) already seeds this row with a real
            // applied_at/description as one of the `statements` executed
            // just above -- so this existence check is expected to find a
            // row and skip every time today. It exists as a fallback for
            // the rare file that omits its own seed row: checking first
            // (rather than INSERT OR IGNORE unconditionally) avoids a
            // redundant second INSERT attempt against the same row on
            // every well-formed migration, while still catching the file
            // that forgot to seed itself.
            let already_recorded: Option<(i64,)> =
                sqlx::query_as("SELECT 1 FROM schema_version WHERE version = ?")
                    .bind(version as i64)
                    .fetch_optional(&mut *conn)
                    .await?;

            if already_recorded.is_none() {
                sqlx::query(
                    "INSERT INTO schema_version (version, applied_at, description) \
                     VALUES (?, ?, ?)",
                )
                .bind(version as i64)
                .bind(now())
                .bind(format!("{prefix} v{version}"))
                .execute(&mut *conn)
                .await?;
            }

            sqlx::query(&format!("RELEASE {savepoint}"))
                .execute(&mut *conn)
                .await?;

            Ok(())
        }
        .await;

        if let Err(e) = step_result {
            let _ = sqlx::query(&format!("ROLLBACK TO {savepoint}"))
                .execute(&mut *conn)
                .await;
            return Err(MigrationError::Failed {
                db_path: prefix.to_owned(),
                plain_language: "Quiet Rabbit couldn't finish setting up. \
                    Your data is safe. [Get help]"
                    .to_owned(),
                diagnostic: Some(e.to_string()),
            });
        }

        if !already_applied {
            applied += 1;
        }
    }

    let check: Option<(String,)> = sqlx::query_as("PRAGMA integrity_check")
        .fetch_optional(&mut *conn)
        .await?;

    if !matches!(check, Some((ref s,)) if s == "ok") {
        return Err(MigrationError::Failed {
            db_path: prefix.to_owned(),
            plain_language: "Quiet Rabbit found a problem with its database. \
                Your data may need attention. [Get help]"
                .to_owned(),
            diagnostic: None,
        });
    }

    Ok(applied)
}

// ---------------------------------------------------------------------------
// Data root helper
// ---------------------------------------------------------------------------

/// Returns the QR data root path from QR_DATA_ROOT env var.
/// Mirrors Python get_data_root() from providers/utils.py — panics if unset,
/// matching the Python behavior (raises RuntimeError if missing).
pub fn get_data_root() -> PathBuf {
    PathBuf::from(std::env::var("QR_DATA_ROOT").expect("QR_DATA_ROOT environment variable not set"))
}

// ---------------------------------------------------------------------------
// Typed migration helpers
// ---------------------------------------------------------------------------

/// Migrate instance/shared.db (unencrypted).
pub async fn migrate_shared_db() -> Result<u32, MigrationError> {
    let db_path = get_data_root().join("instance").join("shared.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "shared", None).await
}

/// Migrate a user's personal.db (encrypted). key_hex: bare hex bytes only.
pub async fn migrate_personal_db(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<u32, MigrationError> {
    let db_path = get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("personal.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "personal", Some(key_hex)).await
}

/// Migrate a group's group.db (encrypted). key_hex: bare hex bytes only.
///
/// PATH, deliberately NOT users/{user_id}/personas/{persona_id}/...: per
/// GROUP_DB_DESIGN_20260802.md Section 2.1, group.db is "not part of any
/// individual member's account tree" -- it lives under its own top-level
/// root instead, scoped by (persona_id, group_id) matching
/// GroupKeyRegistry's own key order (auth::registry). No user_id
/// parameter/path segment: group membership is per-Persona, not
/// per-account, and this function's own signature (persona_id, group_id,
/// key_hex) has no user_id to construct one from.
pub async fn migrate_group_db(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
) -> Result<u32, MigrationError> {
    let db_path = get_data_root()
        .join("groups")
        .join(persona_id)
        .join(group_id)
        .join("group.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "group", Some(key_hex)).await
}

/// Migrate a user's outputs.db (encrypted). key_hex: bare hex bytes only.
pub async fn migrate_outputs_db(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<u32, MigrationError> {
    let db_path = get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("outputs.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "outputs", Some(key_hex)).await
}

/// Migrate a user's messages.db (encrypted). key_hex: bare hex bytes only.
pub async fn migrate_messages_db(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<u32, MigrationError> {
    let db_path = get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("messages.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "messages", Some(key_hex)).await
}

/// Migrate a user's integration_keys.db (encrypted). key_hex: bare hex bytes only.
pub async fn migrate_keys_db(user_id: &str, key_hex: &str) -> Result<u32, MigrationError> {
    let db_path = get_data_root()
        .join("users")
        .join(user_id)
        .join("integration_keys.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "keys", Some(key_hex)).await
}

/// Migrate a user's tier3_cookies.db (encrypted). key_hex: bare hex bytes
/// only. Per-user, not per-persona -- mirrors migrate_keys_db's path shape
/// exactly (items.id=224 resolution, decisions.id=711: cookie identity is
/// keyed by (user, provider), matching KeyRegistry's own user_id-only
/// scoping -- see tier3_cookies_001.sql's own header).
pub async fn migrate_tier3_cookies_db(user_id: &str, key_hex: &str) -> Result<u32, MigrationError> {
    let db_path = get_data_root()
        .join("users")
        .join(user_id)
        .join("tier3_cookies.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "tier3_cookies", Some(key_hex)).await
}

/// Migrate models/scores.db (unencrypted).
pub async fn migrate_scores_db() -> Result<u32, MigrationError> {
    let db_path = get_data_root().join("models").join("scores.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "scores", None).await
}

/// Migrate a focus's domain_context.db (encrypted). key_hex: bare hex bytes only.
/// TODO: Unify with topic_store canonical paths once topic_store is ported.
pub async fn migrate_domain_context_db(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    key_hex: &str,
) -> Result<u32, MigrationError> {
    let db_path = get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("focuses")
        .join(focus_id)
        .join("domain_context.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "domain_context", Some(key_hex)).await
}

/// Migrate a topic's plan_state.db (encrypted). key_hex: bare hex bytes only.
/// TODO: Unify with topic_store canonical paths once topic_store is ported.
pub async fn migrate_plan_state_db(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
) -> Result<u32, MigrationError> {
    let db_path = get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("focuses")
        .join(focus_id)
        .join("topics")
        .join(topic_id)
        .join("plan_state.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "plan_state", Some(key_hex)).await
}

/// Migrate both focus-level databases in one call.
/// Returns (domain_context_applied, plan_state_applied).
pub async fn migrate_focus_storage(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
) -> Result<(u32, u32), MigrationError> {
    let dc = migrate_domain_context_db(user_id, persona_id, focus_id, key_hex).await?;
    let ps = migrate_plan_state_db(user_id, persona_id, focus_id, topic_id, key_hex).await?;
    Ok((dc, ps))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_statements parity tests --------------------------------------

    #[test]
    fn test_parse_simple_statements() {
        let sql = "CREATE TABLE a (id INTEGER);\nCREATE TABLE b (id INTEGER);";
        let stmts = parse_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "CREATE TABLE a (id INTEGER)");
        assert_eq!(stmts[1], "CREATE TABLE b (id INTEGER)");
    }

    #[test]
    fn test_parse_strips_comment_lines() {
        let sql =
            "-- comment\nCREATE TABLE a (id INTEGER);\n-- another\nCREATE TABLE b (id INTEGER);";
        let stmts = parse_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("CREATE TABLE a"));
        assert!(stmts[1].contains("CREATE TABLE b"));
    }

    #[test]
    fn test_parse_trigger_block() {
        let sql = "CREATE TRIGGER trg AFTER INSERT ON foo\nBEGIN\n  UPDATE bar SET x = 1;\nEND;";
        let stmts = parse_statements(sql);
        assert_eq!(
            stmts.len(),
            1,
            "trigger must be one statement, got: {:?}",
            stmts
        );
        assert!(stmts[0].contains("CREATE TRIGGER"));
        assert!(stmts[0].contains("END;"));
    }

    #[test]
    fn test_parse_trigger_followed_by_statement() {
        let sql = "CREATE TRIGGER trg AFTER INSERT ON foo\nBEGIN\n  UPDATE bar SET x = 1;\nEND;\nCREATE INDEX idx ON foo(id);";
        let stmts = parse_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("CREATE TRIGGER"));
        assert!(stmts[1].contains("CREATE INDEX"));
    }

    #[test]
    fn test_parse_skips_empty_lines() {
        let sql = "\n\nCREATE TABLE a (id INTEGER);\n\n\nCREATE TABLE b (id INTEGER);\n";
        let stmts = parse_statements(sql);
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn test_parse_remainder_without_semicolon() {
        let sql = "CREATE TABLE a (id INTEGER);\nCREATE TABLE b (id INTEGER)";
        let stmts = parse_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[1], "CREATE TABLE b (id INTEGER)");
    }

    #[test]
    fn test_parse_empty_input() {
        assert!(parse_statements("").is_empty());
        assert!(parse_statements("-- only a comment").is_empty());
        assert!(parse_statements("\n\n--comment\n").is_empty());
    }

    #[test]
    fn test_parse_all_schema_files_non_empty() {
        // Smoke test: every embedded SQL file must parse to at least one statement.
        // Full Python/Rust golden-vector diff is a follow-up item (Chat-PM log).
        for f in SCHEMA_FILES {
            let stmts = parse_statements(f.sql);
            assert!(
                !stmts.is_empty(),
                "parse_statements produced no statements for {}_{}",
                f.prefix,
                f.version
            );
        }
    }

    // -- validate_manifest --------------------------------------------------

    #[test]
    fn test_manifest_is_valid() {
        validate_manifest();
    }

    #[test]
    fn test_v1_schema_files_are_rerun_safe() {
        validate_v1_rerun_safety();
    }

    // -- migration runner integration tests ---------------------------------

    async fn make_test_conn() -> SqliteConnection {
        SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed")
    }

    #[tokio::test]
    async fn test_get_applied_version_empty_db() {
        let mut conn = make_test_conn().await;
        assert_eq!(get_applied_version(&mut conn).await, 0);
    }

    #[tokio::test]
    async fn test_bootstrap_lock_table_idempotent() {
        let mut conn = make_test_conn().await;
        bootstrap_lock_table(&mut conn)
            .await
            .expect("first bootstrap failed");
        bootstrap_lock_table(&mut conn)
            .await
            .expect("second bootstrap must be idempotent");
        let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM migration_lock WHERE id = 1")
            .fetch_optional(&mut conn)
            .await
            .unwrap();
        assert!(row.is_some(), "seed row must exist after bootstrap");
    }

    #[tokio::test]
    async fn test_acquire_and_release_lock() {
        let mut conn = make_test_conn().await;
        bootstrap_lock_table(&mut conn).await.unwrap();
        assert!(
            acquire_lock(&mut conn).await.unwrap(),
            "should acquire free lock"
        );
        assert!(
            !acquire_lock(&mut conn).await.unwrap(),
            "should not acquire already-held lock"
        );
        release_lock(&mut conn).await;
        assert!(
            acquire_lock(&mut conn).await.unwrap(),
            "should acquire after release"
        );
    }

    // -- items.id=205: auth foundation migration tests ---------------------
    //
    // shared_001.sql was edited directly to its final auth-foundation shape
    // (users/user_salts/user_capabilities) rather than layered on via a
    // separate shared_003.sql rebuild migration (Jason's direction,
    // 2026-08-01, mirroring shared_001.sql's own 2026-07-24 consolidation
    // precedent) -- these tests exercise the resulting schema via
    // run_migrations() itself, not just parse_statements() in isolation.

    #[tokio::test]
    async fn test_shared_migration_applies_cleanly() {
        let mut conn = make_test_conn().await;
        let applied = run_migrations(&mut conn, "shared", None)
            .await
            .expect("shared migration chain must apply cleanly on a fresh db");
        assert_eq!(
            applied, 5,
            "expected all five shared schema versions to apply"
        );

        let version: (i64,) = sqlx::query_as("SELECT MAX(version) FROM schema_version")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(version.0, 5);
    }

    #[tokio::test]
    async fn test_shared_migration_creates_pending_group_invitations() {
        // items.id=283: shared_003.sql must load cleanly via a real
        // migration run, not just parse as syntactically valid SQL.
        let mut conn = make_test_conn().await;
        run_migrations(&mut conn, "shared", None).await.unwrap();

        let exists: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='pending_group_invitations'",
        )
        .fetch_optional(&mut conn)
        .await
        .unwrap();
        assert!(
            exists.is_some(),
            "pending_group_invitations table must exist after migration"
        );
    }

    #[tokio::test]
    async fn test_shared_migration_creates_user_sharing_keys() {
        // items.id=289: shared_004.sql must load cleanly via a real
        // migration run, not just parse as syntactically valid SQL.
        let mut conn = make_test_conn().await;
        run_migrations(&mut conn, "shared", None).await.unwrap();

        let exists: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='user_sharing_keys'",
        )
        .fetch_optional(&mut conn)
        .await
        .unwrap();
        assert!(
            exists.is_some(),
            "user_sharing_keys table must exist after migration"
        );
    }

    #[tokio::test]
    async fn run_pending_heals_content_drift_in_stale_v1_database() {
        // Simulates items.id=228: a database that recorded schema_version=1
        // before tier3_providers was added in place to shared_001.sql.
        // Hand-builds the stale shape (schema_version row present,
        // tier3_providers absent) since SCHEMA_FILES is a compile-time
        // static and can't be swapped to an old shared_001.sql revision at
        // test time.
        let mut conn = make_test_conn().await;
        sqlx::query(
            "CREATE TABLE schema_version (
                version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL, description TEXT NOT NULL
            )",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO schema_version (version, applied_at, description) \
             VALUES (1, '2026-01-01T00:00:00Z', 'stale pre-tier3_providers shared v1')",
        )
        .execute(&mut conn)
        .await
        .unwrap();

        let applied = run_migrations(&mut conn, "shared", None)
            .await
            .expect("drift-healing run must succeed");

        assert_eq!(
            applied, 4,
            "shared v2, v3, v4, and v5 should count as newly applied from a stale v1 database"
        );

        let exists: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='tier3_providers'",
        )
        .fetch_optional(&mut conn)
        .await
        .unwrap();
        assert!(
            exists.is_some(),
            "tier3_providers must be healed into a database stale at schema_version=1, \
             without requiring the database to be deleted and recreated"
        );

        let seeded: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tier3_providers")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert!(
            seeded.0 > 0,
            "shared_001.sql's seeded provider rows must also be healed in"
        );
    }

    #[tokio::test]
    async fn test_shared_migration_users_final_shape() {
        // No pre-edit 'builder'-role fixture to migrate from -- shared_001.sql
        // was edited directly (items.id=205), so this asserts the final
        // shape a fresh install actually gets, not a translation step.
        let mut conn = make_test_conn().await;
        run_migrations(&mut conn, "shared", None).await.unwrap();

        sqlx::query(
            "INSERT INTO users (id, display_name, role, is_primary, auth_enabled, created_at) \
             VALUES ('u-test-1', 'Test User', 'user', 1, 1, '2026-01-01T00:00:00Z')",
        )
        .execute(&mut conn)
        .await
        .expect("a 'user'-role row must insert cleanly under the final role CHECK");

        let row: (String, String, i64) = sqlx::query_as(
            "SELECT id, role, idle_timeout_minutes FROM users WHERE id = 'u-test-1'",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(row.0, "u-test-1");
        assert_eq!(row.1, "user");
        assert_eq!(row.2, 15, "idle_timeout_minutes must default to 15");

        sqlx::query(
            "INSERT INTO user_salts (user_id, salt_hex, created_at) \
             VALUES ('u-test-1', 'deadbeef', '2026-01-01T00:00:00Z')",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        let salt_row: (String, i64, i64, i64) = sqlx::query_as(
            "SELECT kdf_algorithm, kdf_memory_kib, kdf_iterations, kdf_parallelism \
             FROM user_salts WHERE user_id = 'u-test-1'",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(salt_row.0, "argon2id");
        assert_eq!(salt_row.1, 65536);
        assert_eq!(salt_row.2, 3);
        assert_eq!(salt_row.3, 4);
    }

    #[tokio::test]
    async fn test_shared_migration_creates_user_capabilities() {
        let mut conn = make_test_conn().await;
        run_migrations(&mut conn, "shared", None).await.unwrap();

        let exists: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='user_capabilities'",
        )
        .fetch_optional(&mut conn)
        .await
        .unwrap();
        assert!(
            exists.is_some(),
            "user_capabilities table must exist after migration"
        );
    }

    #[tokio::test]
    async fn test_user_capabilities_rejects_duplicate_account_wide_rows() {
        // Regression test for the NULL-PK gap found and closed this session
        // (items.id=205): SQLite's composite PRIMARY KEY treats each NULL as
        // distinct, so (user_id, persona_id, capability) alone does not
        // prevent two account-wide (persona_id IS NULL) rows for the same
        // (user_id, capability) -- confirms the partial unique index added
        // alongside the table actually closes that gap.
        let mut conn = make_test_conn().await;
        run_migrations(&mut conn, "shared", None).await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, display_name, role, created_at) \
             VALUES ('u-cap-1', 'Cap Test', 'user', '2026-01-01T00:00:00Z')",
        )
        .execute(&mut conn)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO user_capabilities (user_id, persona_id, capability, allowed, created_at) \
             VALUES ('u-cap-1', NULL, 'create_persona', 0, '2026-01-01T00:00:00Z')",
        )
        .execute(&mut conn)
        .await
        .expect("first account-wide capability row must insert cleanly");

        let dup = sqlx::query(
            "INSERT INTO user_capabilities (user_id, persona_id, capability, allowed, created_at) \
             VALUES ('u-cap-1', NULL, 'create_persona', 1, '2026-01-01T00:00:00Z')",
        )
        .execute(&mut conn)
        .await;
        assert!(
            dup.is_err(),
            "a second account-wide row for the same (user_id, capability) must be rejected \
             by idx_user_capabilities_account_wide -- without it, the composite PK alone \
             would silently allow both rows to coexist"
        );
    }

    #[tokio::test]
    async fn test_shared_migration_users_role_check_rejects_old_values() {
        let mut conn = make_test_conn().await;
        run_migrations(&mut conn, "shared", None).await.unwrap();

        let result = sqlx::query(
            "INSERT INTO users (id, display_name, role, created_at) \
             VALUES ('u-bad', 'Bad Role', 'builder', '2026-01-01T00:00:00Z')",
        )
        .execute(&mut conn)
        .await;
        assert!(
            result.is_err(),
            "old 'builder' role value must be rejected by the new CHECK"
        );
    }

    // -- real on-disk SQLCipher migration tests -----------------------------
    //
    // The tests above all exercise run_migrations() with key_hex=None
    // against :memory:. None of them exercise the encrypted branch, and
    // none of the typed migrate_*_db path-construction helpers are called
    // by any test anywhere in the codebase. These tests close that gap by
    // running the real typed helpers against a real on-disk file under a
    // tempdir-backed QR_DATA_ROOT, following the pattern established in
    // plan_state_store.rs's tests (real migration call, not a hand-
    // bootstrapped dummy table).

    const TEST_KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";
    const WRONG_KEY_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddee";

    /// Opens a verification connection to an already-migrated real file,
    /// applying the key with the same builder shape run_migrations itself
    /// requires (key first, nothing else configured here).
    async fn open_verify_conn(db_path: &Path, key_hex: Option<&str>) -> SqliteConnection {
        let mut opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(false);
        if let Some(key) = key_hex {
            opts = opts.pragma("key", format!("\"x'{key}'\""));
        }
        opts.connect()
            .await
            .expect("verification connection to a real migrated file must open")
    }

    async fn table_exists(conn: &mut SqliteConnection, table: &str) -> bool {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' AND name = ?")
                .bind(table)
                .fetch_optional(conn)
                .await
                .unwrap();
        row.is_some()
    }

    #[tokio::test]
    async fn schema_version_exists_returns_false_for_nonexistent_path() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let db_path = tempdir.path().join("does-not-exist.db");
        assert!(
            !schema_version_exists(&db_path, None).await,
            "a path that doesn't exist must report false without opening a connection"
        );
    }

    #[tokio::test]
    async fn migrate_personal_db_applies_all_three_versions_to_real_encrypted_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";
        let persona_id = "test-persona";
        let db_path = tempdir
            .path()
            .join("users")
            .join(user_id)
            .join("personas")
            .join(persona_id)
            .join("personal.db");

        let result = migrate_personal_db(user_id, persona_id, TEST_KEY_HEX).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert_eq!(
            result.expect("migration must apply cleanly to a real encrypted file"),
            4,
            "personal_001 + personal_002 + personal_003 + personal_004 must all apply in one pass"
        );

        let mut conn = open_verify_conn(&db_path, Some(TEST_KEY_HEX)).await;
        for table in [
            "entities",
            "entity_facts",
            "voice_profiles",
            "disclosure_log",
            "source_registry",
            "dedup_candidates",
            "document_forks",
        ] {
            assert!(
                table_exists(&mut conn, table).await,
                "table {table} must exist after migration to a real encrypted file"
            );
        }

        let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(entities)")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        let column_names: Vec<&str> = columns.iter().map(|c| c.1.as_str()).collect();
        assert!(
            column_names.contains(&"redact_identification"),
            "personal_003's ALTER TABLE must have applied to the real file"
        );
        assert!(
            column_names.contains(&"hide_from_shared_surfaces"),
            "personal_003's ALTER TABLE must have applied to the real file"
        );
    }

    #[tokio::test]
    async fn migrate_personal_db_is_idempotent_on_real_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";
        let persona_id = "test-persona";

        let first = migrate_personal_db(user_id, persona_id, TEST_KEY_HEX).await;
        let second = migrate_personal_db(user_id, persona_id, TEST_KEY_HEX).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert_eq!(first.expect("first migration must succeed"), 4);
        assert_eq!(
            second.expect("second migration on an already-migrated real file must not error"),
            0,
            "re-running the migration on an already-migrated real file must be a no-op"
        );
    }

    #[tokio::test]
    async fn migrate_personal_db_rejects_wrong_key_on_real_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";
        let persona_id = "test-persona";

        migrate_personal_db(user_id, persona_id, TEST_KEY_HEX)
            .await
            .expect("initial migration with the correct key must succeed");

        let result = migrate_personal_db(user_id, persona_id, WRONG_KEY_HEX).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        let err =
            result.expect_err("reopening a real encrypted file with the wrong key must error");
        let msg = err.to_string();
        assert!(
            msg.contains("not a database"),
            "wrong-key error must be classifiable the same way this codebase already \
             classifies it elsewhere (commands/auth.rs, personal_store.rs): {msg}"
        );
    }

    // -- items.id=283: migrate_group_db real on-disk tests --------------------

    #[tokio::test]
    async fn migrate_group_db_applies_to_real_encrypted_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let persona_id = "test-persona";
        let group_id = "test-group";
        let db_path = tempdir
            .path()
            .join("groups")
            .join(persona_id)
            .join(group_id)
            .join("group.db");

        let result = migrate_group_db(persona_id, group_id, TEST_KEY_HEX).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert_eq!(
            result.expect("migration must apply cleanly to a real encrypted file"),
            1,
            "group_001 must apply in one pass"
        );

        let mut conn = open_verify_conn(&db_path, Some(TEST_KEY_HEX)).await;
        for table in ["documents", "document_permissions"] {
            assert!(
                table_exists(&mut conn, table).await,
                "table {table} must exist after migration to a real encrypted file"
            );
        }
    }

    #[tokio::test]
    async fn migrate_group_db_is_idempotent_on_real_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let persona_id = "test-persona";
        let group_id = "test-group";

        let first = migrate_group_db(persona_id, group_id, TEST_KEY_HEX).await;
        let second = migrate_group_db(persona_id, group_id, TEST_KEY_HEX).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert_eq!(first.expect("first migration must succeed"), 1);
        assert_eq!(
            second.expect("second migration on an already-migrated real file must not error"),
            0,
            "re-running the migration on an already-migrated real file must be a no-op"
        );
    }

    #[tokio::test]
    async fn schema_version_exists_true_for_real_encrypted_file_with_correct_key() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";
        let persona_id = "test-persona";
        let db_path = tempdir
            .path()
            .join("users")
            .join(user_id)
            .join("personas")
            .join(persona_id)
            .join("personal.db");

        migrate_personal_db(user_id, persona_id, TEST_KEY_HEX)
            .await
            .expect("migration must succeed");

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert!(schema_version_exists(&db_path, Some(TEST_KEY_HEX)).await);
    }

    #[tokio::test]
    async fn schema_version_exists_false_for_real_encrypted_file_with_wrong_key() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";
        let persona_id = "test-persona";
        let db_path = tempdir
            .path()
            .join("users")
            .join(user_id)
            .join("personas")
            .join(persona_id)
            .join("personal.db");

        migrate_personal_db(user_id, persona_id, TEST_KEY_HEX)
            .await
            .expect("migration must succeed");

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert!(
            !schema_version_exists(&db_path, Some(WRONG_KEY_HEX)).await,
            "the wrong key against a real encrypted file must be swallowed to false, not panic"
        );
    }

    #[tokio::test]
    async fn migrate_outputs_db_applies_to_real_encrypted_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";
        let persona_id = "test-persona";
        let db_path = tempdir
            .path()
            .join("users")
            .join(user_id)
            .join("personas")
            .join(persona_id)
            .join("outputs.db");

        let result = migrate_outputs_db(user_id, persona_id, TEST_KEY_HEX).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert_eq!(
            result.expect("migration must apply cleanly"),
            3,
            "outputs_001 + outputs_002 + outputs_003 must all apply in one pass"
        );

        let mut conn = open_verify_conn(&db_path, Some(TEST_KEY_HEX)).await;
        for table in [
            "outputs",
            "focus_runs",
            "outputs_fts",
            "topics",
            "run_history",
        ] {
            assert!(
                table_exists(&mut conn, table).await,
                "table {table} must exist after migration to a real encrypted file"
            );
        }

        let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(extract_confirm_candidates)")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert!(
            columns.iter().any(|c| c.1 == "source"),
            "outputs_002's ALTER TABLE must have applied to the real file"
        );

        // Prove the FTS5 trigger fires for real, not just that outputs_fts
        // was created as an empty virtual table.
        sqlx::query(
            "INSERT INTO focus_runs (id, focus_id, started_at) \
             VALUES ('fr-1', 'focus-1', '2026-01-01T00:00:00Z')",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO outputs (id, focus_run_id, output_type, content, created_at, updated_at) \
             VALUES ('out-1', 'fr-1', 'quick_ask', 'hello world', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&mut conn)
        .await
        .unwrap();

        let matched: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM outputs_fts WHERE outputs_fts MATCH 'hello'")
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(
            matched.0, 1,
            "outputs_fts_insert trigger must index the row on insert into a real encrypted file"
        );
    }

    #[tokio::test]
    async fn migrate_keys_db_applies_to_real_encrypted_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";
        let db_path = tempdir
            .path()
            .join("users")
            .join(user_id)
            .join("integration_keys.db");

        let result = migrate_keys_db(user_id, TEST_KEY_HEX).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert_eq!(result.expect("migration must apply cleanly"), 1);

        let mut conn = open_verify_conn(&db_path, Some(TEST_KEY_HEX)).await;
        assert!(table_exists(&mut conn, "integration_keys").await);

        let invalid = sqlx::query(
            "INSERT INTO integration_keys \
                (id, provider, key_type, credential_label, credential, auth_type, created_at) \
             VALUES ('k-bad', 'groq', 'tier2', 'groq', 'secret', 'bogus_type', '2026-01-01T00:00:00Z')",
        )
        .execute(&mut conn)
        .await;
        assert!(
            invalid.is_err(),
            "auth_type CHECK must be enforced against a real encrypted file"
        );

        let valid = sqlx::query(
            "INSERT INTO integration_keys \
                (id, provider, key_type, credential_label, credential, auth_type, created_at) \
             VALUES ('k-good', 'groq', 'tier2', 'groq', 'secret', 'api_key', '2026-01-01T00:00:00Z')",
        )
        .execute(&mut conn)
        .await;
        assert!(valid.is_ok(), "a valid auth_type must insert cleanly");
    }

    #[tokio::test]
    async fn migrate_scores_db_creates_real_unencrypted_file_on_disk() {
        // scores.db is intentionally unencrypted (key_hex=None) -- no
        // SQLCipher key is involved here by design. This test still uses a
        // real tempdir-backed file (not :memory:) to exercise the real
        // path-construction/file-creation code, which no existing test does.
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let db_path = tempdir.path().join("models").join("scores.db");

        let result = migrate_scores_db().await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert_eq!(result.expect("migration must apply cleanly"), 1);
        assert!(
            db_path.exists(),
            "scores.db must exist as a real file on disk"
        );

        let mut conn = open_verify_conn(&db_path, None).await;
        assert!(table_exists(&mut conn, "model_hardware_scores").await);
    }

    #[tokio::test]
    async fn migrate_domain_context_db_applies_to_real_encrypted_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";
        let persona_id = "test-persona";
        let focus_id = "test-focus";
        let db_path = tempdir
            .path()
            .join("users")
            .join(user_id)
            .join("personas")
            .join(persona_id)
            .join("focuses")
            .join(focus_id)
            .join("domain_context.db");

        let result = migrate_domain_context_db(user_id, persona_id, focus_id, TEST_KEY_HEX).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert_eq!(result.expect("migration must apply cleanly"), 1);

        let mut conn = open_verify_conn(&db_path, Some(TEST_KEY_HEX)).await;
        for table in [
            "domain_context_blocks",
            "standing_summary",
            "pending_extractions",
            "provenance_log",
        ] {
            assert!(
                table_exists(&mut conn, table).await,
                "table {table} must exist after migration to a real encrypted file"
            );
        }

        let seeded: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM standing_summary")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(
            seeded.0, 1,
            "domain_context_001.sql's seeded standing_summary row must land in the real file"
        );
    }

    #[tokio::test]
    async fn migrate_focus_storage_migrates_both_real_encrypted_files() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "test-user";
        let persona_id = "test-persona";
        let focus_id = "test-focus";
        let topic_id = "test-topic";
        let focus_dir = tempdir
            .path()
            .join("users")
            .join(user_id)
            .join("personas")
            .join(persona_id)
            .join("focuses")
            .join(focus_id);
        let dc_path = focus_dir.join("domain_context.db");
        let ps_path = focus_dir
            .join("topics")
            .join(topic_id)
            .join("plan_state.db");

        let result =
            migrate_focus_storage(user_id, persona_id, focus_id, topic_id, TEST_KEY_HEX).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert_eq!(result.expect("migration must apply cleanly"), (1, 1));
        assert!(
            dc_path.exists(),
            "domain_context.db must exist as its own real file"
        );
        assert!(
            ps_path.exists(),
            "plan_state.db must exist as its own real file"
        );

        let mut dc_conn = open_verify_conn(&dc_path, Some(TEST_KEY_HEX)).await;
        assert!(table_exists(&mut dc_conn, "domain_context_blocks").await);

        let mut ps_conn = open_verify_conn(&ps_path, Some(TEST_KEY_HEX)).await;
        assert!(table_exists(&mut ps_conn, "topic_header").await);
        assert!(table_exists(&mut ps_conn, "handoff_tokens").await);
    }
}
