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
    SchemaFile { prefix: "domain_context", version: 1,
        sql: include_str!("../../schema/domain_context_001.sql") },
    SchemaFile { prefix: "keys", version: 1,
        sql: include_str!("../../schema/keys_001.sql") },
    SchemaFile { prefix: "outputs", version: 1,
        sql: include_str!("../../schema/outputs_001.sql") },
    SchemaFile { prefix: "outputs", version: 2,
        sql: include_str!("../../schema/outputs_002.sql") },
    SchemaFile { prefix: "outputs", version: 3,
        sql: include_str!("../../schema/outputs_003.sql") },
    SchemaFile { prefix: "outputs", version: 4,
        sql: include_str!("../../schema/outputs_004.sql") },
    SchemaFile { prefix: "outputs", version: 5,
        sql: include_str!("../../schema/outputs_005.sql") },
    SchemaFile { prefix: "personal", version: 1,
        sql: include_str!("../../schema/personal_001.sql") },
    SchemaFile { prefix: "personal", version: 2,
        sql: include_str!("../../schema/personal_002.sql") },
    SchemaFile { prefix: "personal", version: 3,
        sql: include_str!("../../schema/personal_003.sql") },
    SchemaFile { prefix: "personal", version: 4,
        sql: include_str!("../../schema/personal_004.sql") },
    SchemaFile { prefix: "plan_state", version: 1,
        sql: include_str!("../../schema/plan_state_001.sql") },
    SchemaFile { prefix: "plan_state", version: 2,
        sql: include_str!("../../schema/plan_state_002.sql") },
    SchemaFile { prefix: "scores", version: 1,
        sql: include_str!("../../schema/scores_001.sql") },
    SchemaFile { prefix: "shared", version: 1,
        sql: include_str!("../../schema/shared_001.sql") },
    SchemaFile { prefix: "shared", version: 2,
        sql: include_str!("../../schema/shared_002.sql") },
    SchemaFile { prefix: "shared", version: 3,
        sql: include_str!("../../schema/shared_003.sql") },
    SchemaFile { prefix: "shared", version: 4,
        sql: include_str!("../../schema/shared_004.sql") },
    SchemaFile { prefix: "shared", version: 5,
        sql: include_str!("../../schema/shared_005.sql") },
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
                f.prefix, f.version, prev
            );
        }
        max_versions.insert(f.prefix, f.version);
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

        if upper.starts_with("CREATE TRIGGER")
            || upper.starts_with("CREATE OR REPLACE TRIGGER")
        {
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
    let _ = sqlx::query(
        "UPDATE migration_lock SET locked_at = NULL, locked_by = NULL WHERE id = 1",
    )
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
async fn run_pending(
    conn: &mut SqliteConnection,
    prefix: &str,
) -> Result<u32, MigrationError> {
    let current_version = get_applied_version(conn).await;
    let migrations = get_migration_files(prefix);
    let mut applied: u32 = 0;

    for (version, sql) in migrations {
        if version <= current_version {
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
            sqlx::query("INSERT INTO schema_version (version) VALUES (?)")
                .bind(version as i64)
                .execute(&mut *conn)
                .await?;

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

        applied += 1;
    }

    let check: Option<(String,)> =
        sqlx::query_as("PRAGMA integrity_check")
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
    PathBuf::from(
        std::env::var("QR_DATA_ROOT")
            .expect("QR_DATA_ROOT environment variable not set"),
    )
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
        .join("users").join(user_id)
        .join("personas").join(persona_id)
        .join("personal.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "personal", Some(key_hex)).await
}

/// Migrate a user's outputs.db (encrypted). key_hex: bare hex bytes only.
pub async fn migrate_outputs_db(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<u32, MigrationError> {
    let db_path = get_data_root()
        .join("users").join(user_id)
        .join("personas").join(persona_id)
        .join("outputs.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "outputs", Some(key_hex)).await
}

/// Migrate a user's integration_keys.db (encrypted). key_hex: bare hex bytes only.
pub async fn migrate_keys_db(user_id: &str, key_hex: &str) -> Result<u32, MigrationError> {
    let db_path = get_data_root()
        .join("users").join(user_id)
        .join("integration_keys.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let mut conn = open_raw(&db_path).await?;
    run_migrations(&mut conn, "keys", Some(key_hex)).await
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
        .join("users").join(user_id)
        .join("personas").join(persona_id)
        .join("focuses").join(focus_id)
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
        .join("users").join(user_id)
        .join("personas").join(persona_id)
        .join("focuses").join(focus_id)
        .join("topics").join(topic_id)
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
        let sql = "-- comment\nCREATE TABLE a (id INTEGER);\n-- another\nCREATE TABLE b (id INTEGER);";
        let stmts = parse_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("CREATE TABLE a"));
        assert!(stmts[1].contains("CREATE TABLE b"));
    }

    #[test]
    fn test_parse_trigger_block() {
        let sql = "CREATE TRIGGER trg AFTER INSERT ON foo\nBEGIN\n  UPDATE bar SET x = 1;\nEND;";
        let stmts = parse_statements(sql);
        assert_eq!(stmts.len(), 1, "trigger must be one statement, got: {:?}", stmts);
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
                f.prefix, f.version
            );
        }
    }

    // -- validate_manifest --------------------------------------------------

    #[test]
    fn test_manifest_is_valid() {
        validate_manifest();
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
        bootstrap_lock_table(&mut conn).await.expect("first bootstrap failed");
        bootstrap_lock_table(&mut conn).await.expect("second bootstrap must be idempotent");
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM migration_lock WHERE id = 1")
                .fetch_optional(&mut conn)
                .await
                .unwrap();
        assert!(row.is_some(), "seed row must exist after bootstrap");
    }

    #[tokio::test]
    async fn test_acquire_and_release_lock() {
        let mut conn = make_test_conn().await;
        bootstrap_lock_table(&mut conn).await.unwrap();
        assert!(acquire_lock(&mut conn).await.unwrap(), "should acquire free lock");
        assert!(!acquire_lock(&mut conn).await.unwrap(), "should not acquire already-held lock");
        release_lock(&mut conn).await;
        assert!(acquire_lock(&mut conn).await.unwrap(), "should acquire after release");
    }
}
