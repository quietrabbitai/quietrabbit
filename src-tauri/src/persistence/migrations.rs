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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

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
        prefix: "group",
        version: 2,
        sql: include_str!("../../schema/group_002.sql"),
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
        prefix: "messages",
        version: 2,
        sql: include_str!("../../schema/messages_002.sql"),
    },
    SchemaFile {
        prefix: "messages",
        version: 3,
        sql: include_str!("../../schema/messages_003.sql"),
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
        prefix: "outputs",
        version: 4,
        sql: include_str!("../../schema/outputs_004.sql"),
    },
    SchemaFile {
        prefix: "outputs",
        version: 5,
        sql: include_str!("../../schema/outputs_005.sql"),
    },
    SchemaFile {
        prefix: "outputs",
        version: 6,
        sql: include_str!("../../schema/outputs_006.sql"),
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
        prefix: "personal",
        version: 5,
        sql: include_str!("../../schema/personal_005.sql"),
    },
    SchemaFile {
        prefix: "personal",
        version: 6,
        sql: include_str!("../../schema/personal_006.sql"),
    },
    SchemaFile {
        prefix: "personal",
        version: 7,
        sql: include_str!("../../schema/personal_007.sql"),
    },
    SchemaFile {
        prefix: "personal",
        version: 8,
        sql: include_str!("../../schema/personal_008.sql"),
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
        prefix: "shared",
        version: 6,
        sql: include_str!("../../schema/shared_006.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 7,
        sql: include_str!("../../schema/shared_007.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 8,
        sql: include_str!("../../schema/shared_008.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 9,
        sql: include_str!("../../schema/shared_009.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 10,
        sql: include_str!("../../schema/shared_010.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 11,
        sql: include_str!("../../schema/shared_011.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 12,
        sql: include_str!("../../schema/shared_012.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 13,
        sql: include_str!("../../schema/shared_013.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 14,
        sql: include_str!("../../schema/shared_014.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 15,
        sql: include_str!("../../schema/shared_015.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 16,
        sql: include_str!("../../schema/shared_016.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 17,
        sql: include_str!("../../schema/shared_017.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 18,
        sql: include_str!("../../schema/shared_018.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 19,
        sql: include_str!("../../schema/shared_019.sql"),
    },
    SchemaFile {
        prefix: "shared",
        version: 20,
        sql: include_str!("../../schema/shared_020.sql"),
    },
    SchemaFile {
        prefix: "tier3_cookies",
        version: 1,
        sql: include_str!("../../schema/tier3_cookies_001.sql"),
    },
    SchemaFile {
        prefix: "view_cache",
        version: 1,
        sql: include_str!("../../schema/view_cache_001.sql"),
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
        validate_v1_file_rerun_safety(f.prefix, f.sql);
    }
}

/// Returns true if `upper` is a CREATE TABLE/VIRTUAL TABLE/INDEX/UNIQUE
/// INDEX/TRIGGER statement missing its required IF NOT EXISTS guard.
/// `upper` must already be the uppercased, trimmed-start statement text.
fn create_stmt_missing_if_not_exists(upper: &str) -> bool {
    const GUARDED: &[&str] = &[
        "CREATE TABLE IF NOT EXISTS",
        "CREATE VIRTUAL TABLE IF NOT EXISTS",
        "CREATE INDEX IF NOT EXISTS",
        "CREATE UNIQUE INDEX IF NOT EXISTS",
        "CREATE TRIGGER IF NOT EXISTS",
    ];
    if GUARDED.iter().any(|g| upper.starts_with(g)) {
        return false;
    }
    const UNGUARDED_KINDS: &[&str] = &[
        "CREATE TABLE ",
        "CREATE VIRTUAL TABLE ",
        "CREATE INDEX ",
        "CREATE UNIQUE INDEX ",
        "CREATE TRIGGER ",
    ];
    UNGUARDED_KINDS.iter().any(|k| upper.starts_with(k))
}

/// Statement-level checks behind validate_v1_rerun_safety(), factored out so
/// tests can feed it arbitrary SQL without adding fake entries to the real
/// SCHEMA_FILES manifest.
fn validate_v1_file_rerun_safety(prefix: &str, sql: &str) {
    for stmt in parse_statements(sql) {
        let upper = stmt.trim_start().to_uppercase();

        assert!(
            !upper.starts_with("ALTER TABLE") && !upper.starts_with("DROP "),
            "{prefix}_001.sql (v1, always re-run every startup) contains a non-\
             idempotent statement: {stmt:?} — use a new versioned migration \
             file (v2+) instead of amending a v1 file in place for this change",
        );

        assert!(
            !create_stmt_missing_if_not_exists(&upper),
            "{prefix}_001.sql (v1, always re-run every startup) contains a \
             CREATE TABLE/INDEX/TRIGGER statement missing its IF NOT EXISTS \
             guard: {stmt:?} — every v1 CREATE must be idempotent on rerun",
        );

        // parse_statements folds an entire CREATE TRIGGER...END block into
        // one statement, so a trigger's upper here is the whole body, not
        // just its header. Statements inside that body only execute when a
        // real row change fires the trigger, never on migration replay
        // itself, so they're exempt from the bare-DML check below --
        // skip past this statement without inspecting its body's INSERTs.
        let is_trigger_def =
            upper.starts_with("CREATE TRIGGER") || upper.starts_with("CREATE OR REPLACE TRIGGER");
        if is_trigger_def {
            continue;
        }

        let is_safe_insert =
            upper.starts_with("INSERT OR IGNORE") || upper.starts_with("INSERT OR REPLACE");
        let is_bare_dml = (upper.starts_with("INSERT ") && !is_safe_insert)
            || upper.starts_with("UPDATE ")
            || upper.starts_with("DELETE ");
        assert!(
            !is_bare_dml,
            "{prefix}_001.sql (v1, always re-run every startup) contains a \
             bare top-level INSERT/UPDATE/DELETE outside a trigger body: \
             {stmt:?} — use INSERT OR IGNORE / INSERT OR REPLACE for \
             idempotent seeding, or move non-idempotent DML into a new \
             versioned migration file (v2+)",
        );
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

/// Lock rows older than this are treated as abandoned (holder crashed before
/// reaching release_lock()) and become reclaimable by a new acquisition
/// attempt. items.id=473.
///
/// Sized from a real measurement, not a guess: test_shared_migration_
/// applies_cleanly (below) runs the entire "shared" chain — 17 migration
/// versions, including this app's largest single schema file (shared_001.sql,
/// ~35KB) — end to end against an in-memory connection in ~20ms. Real
/// deployments add SQLCipher's PRAGMA key KDF cost and disk I/O, and
/// QR_NETWORK_STORAGE=true forces single-writer journal_mode=DELETE, but
/// none of that plausibly closes a 4-5 order-of-magnitude gap to this
/// threshold. 300s (5 minutes) is therefore chosen to be effectively
/// unreachable by any genuinely still-running migration against this app's
/// current or near-future schemas -- so it should never fire concurrently
/// with a real one -- while still bounding a crash-recovery wait to
/// something a person can sit through on a retry, instead of a permanent
/// lockout that needs manual DB surgery to clear.
const STALE_LOCK_THRESHOLD_SECS: i64 = 300;

/// "hostname:pid:uuid" lock identity string.
/// UUID is generated once per process startup via OnceLock — eliminates
/// PID-reuse false ownership.
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
        .create_if_missing(true)
        // Pinned explicitly rather than left to sqlx's default (items.id=
        // 185/305/352) -- zero behavior change today, removes the latent
        // risk of a future sqlx upgrade or an unpinned connection path
        // silently changing the default this codebase's FK-declared tables
        // (personal_002.sql, outputs_001.sql) rely on.
        .foreign_keys(true);
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
///
/// If the row is already held, falls through to try_reclaim_stale_lock() to
/// recover from a holder that crashed before ever calling release_lock().
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

    if result.rows_affected() == 1 {
        return Ok(true);
    }

    try_reclaim_stale_lock(conn, &pid).await
}

/// If migration_lock is held but locked_at is older than
/// STALE_LOCK_THRESHOLD_SECS, reclaim it -- the holder almost certainly
/// crashed mid-migration without reaching release_lock(). Returns true if
/// this call reclaimed the lock.
///
/// The reclaim UPDATE's WHERE clause pins locked_at to the exact value just
/// read (a compare-and-swap), not to an inequality against a cutoff
/// timestamp string: two lock rows' RFC3339 strings are only safe to compare
/// with `<` when both have the same fractional-second digit count, which
/// chrono's to_rfc3339() (SecondsFormat::AutoSi) does not guarantee. Pinning
/// to the exact previously-read string sidesteps that entirely and still
/// gives the same atomicity guarantee as acquire_lock's own UPDATE: if a
/// second process races this one to reclaim the same stale row, only the
/// first UPDATE to land will affect a row -- the loser's locked_at no longer
/// matches and it simply gets rows_affected() == 0.
async fn try_reclaim_stale_lock(
    conn: &mut SqliteConnection,
    pid: &str,
) -> Result<bool, MigrationError> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT locked_at FROM migration_lock WHERE id = 1")
            .fetch_optional(&mut *conn)
            .await?;

    let Some((Some(locked_at),)) = row else {
        return Ok(false);
    };

    let held_since = match chrono::DateTime::parse_from_rfc3339(&locked_at) {
        Ok(ts) => ts.with_timezone(&chrono::Utc),
        Err(_) => return Ok(false),
    };

    let stale =
        chrono::Utc::now() - held_since > chrono::Duration::seconds(STALE_LOCK_THRESHOLD_SECS);
    if !stale {
        return Ok(false);
    }

    let result = sqlx::query(
        "UPDATE migration_lock SET locked_at = ?, locked_by = ? \
         WHERE id = 1 AND locked_at = ?",
    )
    .bind(now())
    .bind(pid)
    .bind(&locked_at)
    .execute(&mut *conn)
    .await?;

    let reclaimed = result.rows_affected() == 1;
    if reclaimed {
        log::warn!(
            "migration_lock reclaimed: prior holder's lock was acquired at {} \
             (more than {}s ago) and never released -- it likely crashed \
             mid-migration. Proceeding as {}.",
            locked_at,
            STALE_LOCK_THRESHOLD_SECS,
            pid
        );
    }
    Ok(reclaimed)
}

/// Acquire migration_lock, retrying with a short bounded backoff if it's
/// currently held. items.id=391: React 18 StrictMode (frontend/src/main.tsx)
/// double-invokes mount effects in dev builds -- ChatPane's own
/// `listMessages` effect is one of them, and since items.id=384/389 made
/// every DB open call migrate_messages_db/migrate_personal_db
/// unconditionally (previously gated on the file not yet existing), two
/// concurrent opens of the SAME db file now reliably race on this lock:
/// confirmed live, 2026-09-01/02 verification passes ("database is
/// locked" on message loading, "Migration lock held by another process"
/// on the Tier3 dev-force-escalation path -- both are the same race
/// hitting different callers of open_messages_db). Before 384/389 this
/// never mattered: the loser's migration path was skipped entirely once
/// the file already existed, so nothing ever contended for this lock in
/// the same millisecond.
///
/// acquire_lock's own UPDATE is a single atomic statement -- it never
/// blocks, it just reports whether THIS call won. The lock is only ever
/// held for the duration of an in-process migration run (sub-millisecond
/// once a database is already at its latest version, since every
/// already-applied migration is a cheap version-number skip -- see
/// run_pending), so a short bounded retry resolves both this in-process
/// race and genuine cross-process contention (the lock's own
/// "hostname:pid:uuid" identity, this file's header comment, already
/// anticipates the latter) without masking a real stuck lock: exhausting
/// every retry still returns false, same as before, and run_migrations
/// still surfaces MigrationError::Locked's existing "try again in a
/// moment" message in that case.
async fn acquire_lock_with_retry(conn: &mut SqliteConnection) -> Result<bool, MigrationError> {
    const MAX_ATTEMPTS: u32 = 8;
    const INITIAL_DELAY_MS: u64 = 50;
    const MAX_DELAY_MS: u64 = 500;

    let mut delay_ms = INITIAL_DELAY_MS;
    for attempt in 0..MAX_ATTEMPTS {
        if acquire_lock(conn).await? {
            return Ok(true);
        }
        if attempt + 1 == MAX_ATTEMPTS {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        delay_ms = (delay_ms * 2).min(MAX_DELAY_MS);
    }
    Ok(false)
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
        // Pin SQLCipher 4.x KDF/page/HMAC defaults, right after key.
        if sqlx::query("PRAGMA cipher_compatibility = 4")
            .execute(&mut conn)
            .await
            .is_err()
        {
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

/// Per-process, in-memory-only "already checked this run" gate for the
/// trailing PRAGMA integrity_check/quick_check in run_pending (items.id=484,
/// Option D approved via items.id=475's Plan Mode investigation,
/// chat_session_handoffs.id=333). Never persisted — resets on every process
/// launch — so a fresh app start (including recovery from a crash or
/// unclean shutdown, exactly when real corruption is most likely) still
/// always checks every file at least once. What this gate removes is the
/// redundant re-scan on every subsequent open of a file THIS process has
/// already opened and verified, which is what made integrity_check fire
/// hundreds of times per session before this change.
static CHECKED_FILES: OnceLock<std::sync::Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn checked_files_set() -> &'static std::sync::Mutex<HashSet<PathBuf>> {
    CHECKED_FILES.get_or_init(|| std::sync::Mutex::new(HashSet::new()))
}

/// Returns true if `path` was already recorded as checked by an earlier
/// call in this process run, marking it checked as a side effect if not.
/// Deliberately check-and-set in one lock acquisition (like path_lock's own
/// get-or-create above) so two racing callers can't both observe "not yet
/// checked" for the same path.
fn already_checked_this_run(path: &Path) -> bool {
    let mut guard = checked_files_set()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    !guard.insert(path.to_path_buf())
}

#[cfg(test)]
fn is_marked_checked_for_test(path: &Path) -> bool {
    checked_files_set()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(path)
}

/// Pure decision for run_pending's trailing consistency check — factored
/// out of the actual PRAGMA execution so the branching itself is directly
/// unit-testable without spinning up a real connection. `Some(true)` means
/// run the full PRAGMA integrity_check; `Some(false)` means run the cheaper
/// PRAGMA quick_check; `None` means skip the check entirely.
///
/// db_path is None for every caller with no real file identity to gate
/// against (this module's and provider_store.rs's :memory:-backed tests,
/// which call the public run_migrations() directly) — those keep the
/// unconditional full check this module always ran before items.id=484.
///
/// DELIBERATELY does not gate on version-parity alone (migrations_applied
/// == 0): that was explicitly rejected during items.id=475's investigation
/// as a real corruption-detection regression, since it would stop catching
/// disk errors, unclean shutdowns, or hardware faults unrelated to schema
/// version — self-hosted software with no ops team watching for silent
/// corruption can't afford that gap. The first-touch-no-migration case
/// still gets quick_check, never a silent skip.
fn integrity_check_decision(
    db_path: Option<&Path>,
    migrations_applied_this_call: u32,
) -> Option<bool> {
    match db_path {
        None => Some(true),
        Some(path) => {
            if already_checked_this_run(path) {
                None
            } else {
                Some(migrations_applied_this_call > 0)
            }
        }
    }
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
    run_migrations_at(conn, prefix, key_hex, None).await
}

/// Same as run_migrations, but threads a real db file path through to
/// run_pending's trailing consistency check for the items.id=484 per-
/// process-per-file integrity_check gate (Option D). Only migrate_db_file
/// (below) has genuine file identity to gate against — every other caller
/// goes through the public run_migrations() above with db_path=None, which
/// always runs the full check exactly as this module did before
/// items.id=484.
async fn run_migrations_at(
    conn: &mut SqliteConnection,
    prefix: &str,
    key_hex: Option<&str>,
    db_path: Option<&Path>,
) -> Result<u32, MigrationError> {
    // Always validate manifest — O(19), negligible cost, catches hand-edit errors.
    validate_manifest();
    validate_v1_rerun_safety();

    // PRAGMA key MUST precede journal_mode — non-negotiable (CLAUDE.md).
    if let Some(key) = key_hex {
        let pragma = format!("PRAGMA key = \"x'{key}'\"");
        sqlx::query(&pragma).execute(&mut *conn).await?;
        // Pin SQLCipher 4.x KDF/page/HMAC defaults, right after key.
        sqlx::query("PRAGMA cipher_compatibility = 4")
            .execute(&mut *conn)
            .await?;
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

    if !acquire_lock_with_retry(conn).await? {
        return Err(MigrationError::Locked);
    }

    let result = run_pending(conn, prefix, db_path).await;
    release_lock(conn).await;
    result
}

/// Inner migration loop — runs after lock is acquired.
async fn run_pending(
    conn: &mut SqliteConnection,
    prefix: &str,
    db_path: Option<&Path>,
) -> Result<u32, MigrationError> {
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

    // items.id=484 (Option D): gate the trailing consistency check so a
    // file this process already opened and verified isn't rescanned on
    // every subsequent open — see integrity_check_decision's own doc
    // comment for why this doesn't weaken corruption detection.
    if let Some(full_check) = integrity_check_decision(db_path, applied) {
        let pragma = if full_check {
            "PRAGMA integrity_check"
        } else {
            "PRAGMA quick_check"
        };
        let check: Option<(String,)> = sqlx::query_as(pragma).fetch_optional(&mut *conn).await?;

        if !matches!(check, Some((ref s,)) if s == "ok") {
            return Err(MigrationError::Failed {
                db_path: prefix.to_owned(),
                plain_language: "Quiet Rabbit found a problem with its database. \
                    Your data may need attention. [Get help]"
                    .to_owned(),
                diagnostic: None,
            });
        }
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
// Per-file migration serialization
// ---------------------------------------------------------------------------

/// Per-db-file async mutex, created on first use and reused thereafter.
///
/// items.id=391: the real fix for the StrictMode double-mount race
/// (React 18's dev-mode double-invoked mount effects -- see
/// ChatPane.tsx's `listMessages` effect -- calling e.g. migrate_messages_db
/// twice within the same file nearly simultaneously, reliably since
/// items.id=384/389 made every DB open call it unconditionally). This
/// session's first attempt (acquire_lock_with_retry, above) only wrapped
/// the app-level `migration_lock` row check and turned out NOT to be
/// enough: confirmed live (Jason, 2026-09-02) still hitting
/// `(code: 5) database is locked` -- a raw SQLite SQLITE_BUSY, not this
/// module's own `MigrationError::Locked`, meaning the actual contention
/// is happening somewhere earlier/elsewhere in the sequence:
/// bootstrap_lock_table's own CREATE TABLE/SAVEPOINT, or run_pending's
/// per-migration SAVEPOINT (v1 files are ALWAYS re-run in full, even on
/// an already-migrated database -- see run_pending's own comment), or the
/// unconditional trailing `PRAGMA integrity_check` -- none of which
/// acquire_lock_with_retry's loop ever touches. This matters more than it
/// would under WAL: `QR_NETWORK_STORAGE=true` in this dev environment
/// forces `journal_mode=DELETE` for every migration connection regardless
/// of whether that particular file is actually on network storage (the
/// choice is a single global env var, not per-path), and DELETE mode
/// allows only one writer at a time file-wide.
///
/// Retrying around a specific query assumes we know where contention can
/// occur; serializing in-process callers up front doesn't need that
/// assumption and closes the gap for every current and future statement
/// in the sequence at once. This does NOT replace acquire_lock_with_retry
/// -- that still matters for genuine cross-process contention (a second
/// QR process instance, which the lock table's own "hostname:pid:uuid"
/// identity already anticipates and this in-process mutex can't see at
/// all) -- the two are complementary, not redundant.
///
/// A std::sync::Mutex guards the HashMap itself (held only for the brief
/// entry lookup/insert, never across an await); each entry is a
/// tokio::sync::Mutex so a waiting caller yields instead of blocking a
/// worker thread.
///
/// items.id=470: the map stores Weak, not Arc, so it never keeps a path's
/// mutex alive on its own. The only strong reference is the local `lock`
/// binding in migrate_db_file, held for exactly the duration of that one
/// migration (it outlives the `.lock().await` guard, which borrows from
/// it). Once migrate_db_file returns, that Arc drops; if it was the last
/// strong reference, the Mutex<()> is freed immediately -- no sweep, no
/// size threshold, nothing to schedule. The next call for that path finds
/// a dead Weak (upgrade() -> None) and allocates a fresh entry.
///
/// This is race-free for the exact hazard path_lock exists to prevent:
/// the get-or-create-or-replace decision (upgrade attempt, and the insert
/// if it fails) all happens while holding `guard`, a single std::sync::
/// Mutex over the whole map, synchronously with no await in between. A
/// second caller can't observe or act on the map mid-decision -- it either
/// sees the old live entry (and upgrades it, extending the same Arc a
/// migration is currently holding) or sees the just-inserted fresh one.
/// Nothing removes an entry out from under a strong holder: a Weak can
/// only fail to upgrade once every Arc referencing it (including any
/// migration's `lock` local) has already been dropped.
fn path_lock(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static PATH_LOCKS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let map = PATH_LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = guard.get(path).and_then(Weak::upgrade) {
        return existing;
    }
    let fresh = Arc::new(tokio::sync::Mutex::new(()));
    guard.insert(path.to_path_buf(), Arc::downgrade(&fresh));
    fresh
}

/// Shared body for every typed migrate_*_db helper below: ensures the
/// parent directory exists, serializes concurrent in-process callers
/// against this exact file (see path_lock's own doc comment), opens a
/// fresh connection, and runs the migration chain. One place for this
/// sequence rather than each typed helper repeating it (which is also
/// how items.id=391's fix reaches every one of them at once).
async fn migrate_db_file(
    db_path: &Path,
    prefix: &str,
    key_hex: Option<&str>,
) -> Result<u32, MigrationError> {
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let lock = path_lock(db_path);
    let _guard = lock.lock().await;
    let mut conn = open_raw(db_path).await?;
    run_migrations_at(&mut conn, prefix, key_hex, Some(db_path)).await
}

// ---------------------------------------------------------------------------
// Typed migration helpers
// ---------------------------------------------------------------------------

/// Migrate instance/shared.db (unencrypted).
pub async fn migrate_shared_db() -> Result<u32, MigrationError> {
    let db_path = get_data_root().join("instance").join("shared.db");
    migrate_db_file(&db_path, "shared", None).await
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
    migrate_db_file(&db_path, "personal", Some(key_hex)).await
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
    migrate_db_file(&db_path, "group", Some(key_hex)).await
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
    migrate_db_file(&db_path, "outputs", Some(key_hex)).await
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
    migrate_db_file(&db_path, "messages", Some(key_hex)).await
}

/// Migrate a VIEW-ONLY persona share's recipient-side read-only cache
/// (encrypted). key_hex: bare hex bytes only.
///
/// PATH shape combines two existing precedents: migrate_keys_db /
/// migrate_tier3_cookies_db already key a per-account (not per-persona) file
/// directly with the account's own master-key hex at
/// users/{user_id}/{name}.db; migrate_group_db already adds a second
/// identifier as an extra path segment. items.id=304 (decisions.id=723): a
/// VIEW-ONLY share never materializes a Persona, so this cannot live under
/// personas/{persona_id}/ the way personal.db does -- it is scoped to
/// (user_id, share_id) instead, encrypted with the same account master key
/// personal.db already uses (no new key derivation, no new KeyRegistry
/// plumbing).
pub async fn migrate_view_cache_db(
    user_id: &str,
    share_id: &str,
    key_hex: &str,
) -> Result<u32, MigrationError> {
    let db_path = get_data_root()
        .join("users")
        .join(user_id)
        .join("persona_view_shares")
        .join(share_id)
        .join("view_cache.db");
    migrate_db_file(&db_path, "view_cache", Some(key_hex)).await
}

/// Migrate a user's integration_keys.db (encrypted). key_hex: bare hex bytes only.
pub async fn migrate_keys_db(user_id: &str, key_hex: &str) -> Result<u32, MigrationError> {
    let db_path = get_data_root()
        .join("users")
        .join(user_id)
        .join("integration_keys.db");
    migrate_db_file(&db_path, "keys", Some(key_hex)).await
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
    migrate_db_file(&db_path, "tier3_cookies", Some(key_hex)).await
}

/// Migrate models/scores.db (unencrypted).
pub async fn migrate_scores_db() -> Result<u32, MigrationError> {
    let db_path = get_data_root().join("models").join("scores.db");
    migrate_db_file(&db_path, "scores", None).await
}

/// Migrate a focus's domain_context.db (encrypted). key_hex: bare hex bytes only.
/// Path is the canonical domain_context_store::get_domain_context_path
/// (items.id=222, items.id=466).
pub async fn migrate_domain_context_db(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    key_hex: &str,
) -> Result<u32, MigrationError> {
    let db_path = crate::persistence::domain_context_store::get_domain_context_path(
        user_id, persona_id, focus_id,
    );
    migrate_db_file(&db_path, "domain_context", Some(key_hex)).await
}

/// Migrate a topic's plan_state.db (encrypted). key_hex: bare hex bytes only.
/// Path is the canonical topic_store::get_plan_state_path (items.id=94, items.id=222).
pub async fn migrate_plan_state_db(
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
    topic_id: &str,
    key_hex: &str,
) -> Result<u32, MigrationError> {
    let db_path = crate::persistence::topic_store::get_plan_state_path(
        user_id, persona_id, focus_id, topic_id,
    );
    migrate_db_file(&db_path, "plan_state", Some(key_hex)).await
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

    #[test]
    #[should_panic(expected = "missing its IF NOT EXISTS guard")]
    fn test_v1_rejects_create_table_missing_if_not_exists() {
        validate_v1_file_rerun_safety("fake", "CREATE TABLE foo (id INTEGER);");
    }

    #[test]
    #[should_panic(expected = "missing its IF NOT EXISTS guard")]
    fn test_v1_rejects_create_index_missing_if_not_exists() {
        validate_v1_file_rerun_safety(
            "fake",
            "CREATE TABLE IF NOT EXISTS foo (id INTEGER);\n\
             CREATE INDEX idx_foo ON foo(id);",
        );
    }

    #[test]
    #[should_panic(expected = "missing its IF NOT EXISTS guard")]
    fn test_v1_rejects_create_trigger_missing_if_not_exists() {
        validate_v1_file_rerun_safety(
            "fake",
            "CREATE TABLE IF NOT EXISTS foo (id INTEGER);\n\
             CREATE TRIGGER trg AFTER INSERT ON foo\nBEGIN\n  \
             INSERT INTO bar(id) VALUES (1);\nEND;",
        );
    }

    #[test]
    #[should_panic(expected = "bare top-level INSERT/UPDATE/DELETE")]
    fn test_v1_rejects_bare_top_level_insert() {
        validate_v1_file_rerun_safety(
            "fake",
            "CREATE TABLE IF NOT EXISTS foo (id INTEGER);\n\
             INSERT INTO foo (id) VALUES (1);",
        );
    }

    #[test]
    #[should_panic(expected = "bare top-level INSERT/UPDATE/DELETE")]
    fn test_v1_rejects_bare_top_level_update() {
        validate_v1_file_rerun_safety(
            "fake",
            "CREATE TABLE IF NOT EXISTS foo (id INTEGER);\n\
             UPDATE foo SET id = 1;",
        );
    }

    #[test]
    #[should_panic(expected = "bare top-level INSERT/UPDATE/DELETE")]
    fn test_v1_rejects_bare_top_level_delete() {
        validate_v1_file_rerun_safety(
            "fake",
            "CREATE TABLE IF NOT EXISTS foo (id INTEGER);\n\
             DELETE FROM foo;",
        );
    }

    #[test]
    fn test_v1_allows_insert_or_ignore_and_trigger_body_dml() {
        // Both real-world patterns already present in the current v1 files
        // (schema_version/instance_config seed rows, and outputs_001.sql's
        // outputs_fts triggers) must keep passing.
        validate_v1_file_rerun_safety(
            "fake",
            "CREATE TABLE IF NOT EXISTS foo (id INTEGER);\n\
             INSERT OR IGNORE INTO foo (id) VALUES (1);\n\
             CREATE TRIGGER IF NOT EXISTS trg AFTER INSERT ON foo\nBEGIN\n  \
             UPDATE foo SET id = 1;\n  DELETE FROM foo WHERE id = 0;\nEND;",
        );
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
    async fn sqlite_build_supports_fts5() {
        // Standalone capability probe, independent of ENV_MUTEX/setup(): if the
        // linked SQLite/SQLCipher build lacks SQLITE_ENABLE_FTS5, this fails by
        // itself with a direct error instead of surfacing as a poisoned-mutex
        // cascade through unrelated tests (see outputs_001.sql's outputs_fts table).
        let mut conn = make_test_conn().await;
        sqlx::query("CREATE VIRTUAL TABLE fts5_probe USING fts5(x)")
            .execute(&mut conn)
            .await
            .expect("linked SQLite/SQLCipher build must support FTS5 (SQLITE_ENABLE_FTS5)");
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

    #[tokio::test]
    async fn test_acquire_lock_reclaims_abandoned_lock_past_staleness_threshold() {
        // items.id=473: simulates a process that acquired the lock and then
        // crashed before ever reaching release_lock() -- the row is left
        // permanently held (locked_at/locked_by both set, no live process
        // behind them). Hand-writes that row shape directly rather than
        // going through acquire_lock(), since the whole point is that no
        // release ever happened.
        let mut conn = make_test_conn().await;
        bootstrap_lock_table(&mut conn).await.unwrap();

        let abandoned_at = (chrono::Utc::now()
            - chrono::Duration::seconds(STALE_LOCK_THRESHOLD_SECS + 60))
        .to_rfc3339();
        sqlx::query("UPDATE migration_lock SET locked_at = ?, locked_by = ? WHERE id = 1")
            .bind(&abandoned_at)
            .bind("dead-host:12345:stale-uuid")
            .execute(&mut conn)
            .await
            .unwrap();

        assert!(
            acquire_lock(&mut conn).await.unwrap(),
            "acquire_lock must reclaim a lock whose locked_at is past the \
             staleness threshold, since no release_lock() call is ever \
             coming for an abandoned row"
        );

        let row: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT locked_at, locked_by FROM migration_lock WHERE id = 1")
                .fetch_one(&mut conn)
                .await
                .unwrap();
        let (locked_at, locked_by) = row;
        assert_ne!(
            locked_at.as_deref(),
            Some(abandoned_at.as_str()),
            "reclaim must stamp a fresh locked_at, not keep the abandoned one"
        );
        assert_eq!(
            locked_by.as_deref(),
            Some(process_id().as_str()),
            "reclaim must record this process as the new holder"
        );

        // Second acquire attempt must now fail -- this process holds the
        // lock it just reclaimed, same as any other successful acquire.
        assert!(
            !acquire_lock(&mut conn).await.unwrap(),
            "lock must be held by this process after reclaiming it"
        );
    }

    #[tokio::test]
    async fn test_acquire_lock_does_not_reclaim_lock_under_staleness_threshold() {
        // Companion regression test for the hazard this feature must not
        // introduce: a lock held by a genuinely still-running migration
        // (locked_at recent, well under the threshold) must NOT be
        // reclaimable, or two processes could migrate the same database
        // concurrently -- exactly what migration_lock exists to prevent.
        let mut conn = make_test_conn().await;
        bootstrap_lock_table(&mut conn).await.unwrap();

        let recently_locked_at = (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
        sqlx::query("UPDATE migration_lock SET locked_at = ?, locked_by = ? WHERE id = 1")
            .bind(&recently_locked_at)
            .bind("live-host:99999:live-uuid")
            .execute(&mut conn)
            .await
            .unwrap();

        assert!(
            !acquire_lock(&mut conn).await.unwrap(),
            "a lock held well under the staleness threshold must not be reclaimed"
        );
    }

    #[tokio::test]
    async fn test_acquire_lock_with_retry_succeeds_after_contender_releases() {
        // items.id=391: regression test for the StrictMode double-mount-
        // effect race (this function's own doc comment) -- two real
        // connections to the SAME on-disk file (a :memory: connection
        // can't model this: each :memory: connection is its own isolated
        // database), one holding the lock while the other retries.
        // Confirms acquire_lock_with_retry actually waits out a transient
        // holder instead of failing on first contact the way plain
        // acquire_lock (test_acquire_and_release_lock above) does.
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let db_path = tempdir.path().join("lock_retry_test.db");

        let mut conn1 = open_raw(&db_path).await.expect("conn1 open failed");
        bootstrap_lock_table(&mut conn1).await.unwrap();
        assert!(
            acquire_lock(&mut conn1).await.unwrap(),
            "conn1 must win the initial acquire"
        );

        let mut conn2 = open_raw(&db_path).await.expect("conn2 open failed");
        bootstrap_lock_table(&mut conn2).await.unwrap();

        let retry_handle = tokio::spawn(async move { acquire_lock_with_retry(&mut conn2).await });

        // Give the retry loop a couple of failed attempts before releasing
        // conn1's hold, so this test actually exercises the retry path
        // rather than winning on a lucky first attempt.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        release_lock(&mut conn1).await;

        let acquired = retry_handle
            .await
            .expect("retry task panicked")
            .expect("acquire_lock_with_retry must not error");
        assert!(
            acquired,
            "conn2 must eventually acquire the lock after conn1 releases"
        );
    }

    #[tokio::test]
    async fn test_concurrent_migrate_calls_to_the_same_file_do_not_race() {
        // items.id=391: reproduces the actual StrictMode double-mount
        // scenario -- two truly concurrent calls into the SAME typed
        // helper (migrate_personal_db, chosen since it already exercises
        // three real migration versions) against the SAME on-disk file,
        // fired via tokio::join! rather than sequenced. Before path_lock
        // (this module's migrate_db_file), this occasionally raced --
        // confirmed live (Jason, 2026-09-02) as a raw SQLITE_BUSY
        // ("(code: 5) database is locked"), not MigrationError::Locked,
        // meaning acquire_lock_with_retry's own fix (which only wraps the
        // app-level lock row -- see test above) doesn't cover it on its
        // own. Forces journal_mode=DELETE via QR_NETWORK_STORAGE=true,
        // matching this dev environment's real config -- WAL's
        // readers-don't-block-writers behavior would mask the bug this
        // test exists to catch.
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let saved_network = std::env::var("QR_NETWORK_STORAGE").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        std::env::set_var("QR_NETWORK_STORAGE", "true");

        let user_id = "concurrent-test-user";
        let persona_id = "concurrent-test-persona";

        // Prime the file once so both concurrent calls below hit the
        // "already migrated, v1 always re-runs + trailing integrity_check"
        // path -- the actual steady-state shape of every real ChatPane
        // mount, not a fresh-file race that would resolve differently.
        let primed = migrate_personal_db(user_id, persona_id, TEST_KEY_HEX).await;

        let (r1, r2) = tokio::join!(
            migrate_personal_db(user_id, persona_id, TEST_KEY_HEX),
            migrate_personal_db(user_id, persona_id, TEST_KEY_HEX),
        );

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        if let Some(v) = saved_network {
            std::env::set_var("QR_NETWORK_STORAGE", v);
        } else {
            std::env::remove_var("QR_NETWORK_STORAGE");
        }

        primed.expect("priming migration must succeed");
        r1.expect("first concurrent call must not hit a database-locked race");
        r2.expect("second concurrent call must not hit a database-locked race");
    }

    #[test]
    fn test_path_lock_entry_is_reclaimed_once_unused() {
        // items.id=470: path_lock stores Weak, not Arc, specifically so an
        // entry with no active migration doesn't pin its Mutex<()> forever.
        // Drop the only strong reference and confirm the next lookup can't
        // upgrade it -- i.e. the memory is actually freed, not just eligible
        // for some future sweep to find.
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let db_path = tempdir.path().join("reclaim_test.db");

        let first = path_lock(&db_path);
        let weak = Arc::downgrade(&first);
        drop(first);

        assert!(
            weak.upgrade().is_none(),
            "dropping the only strong ref must free the Mutex<()> immediately, \
             not leave it pinned by the map"
        );
    }

    #[tokio::test]
    async fn test_path_lock_still_serializes_after_a_reclaim_cycle() {
        // items.id=470: the fix must not reopen items.id=391's race. Run one
        // migration to completion (so its Arc drops and the map's Weak goes
        // dead -- a reclaim cycle), THEN start two concurrent migrations
        // against that same path. If the post-cleanup path_lock() ever
        // handed out two different mutexes for the same file, these two
        // calls would race exactly like the pre-items.id=391 bug.
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let saved_network = std::env::var("QR_NETWORK_STORAGE").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        std::env::set_var("QR_NETWORK_STORAGE", "true");

        let user_id = "reclaim-cycle-test-user";
        let persona_id = "reclaim-cycle-test-persona";

        // First migration runs and completes, dropping its Arc and leaving
        // a dead Weak behind in PATH_LOCKS for this path.
        let primed = migrate_personal_db(user_id, persona_id, TEST_KEY_HEX).await;

        // Now race two more against the same (already-migrated) file. Each
        // must observe the dead Weak, allocate a fresh Arc, and still
        // serialize against each other via that fresh mutex.
        let (r1, r2) = tokio::join!(
            migrate_personal_db(user_id, persona_id, TEST_KEY_HEX),
            migrate_personal_db(user_id, persona_id, TEST_KEY_HEX),
        );

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        if let Some(v) = saved_network {
            std::env::set_var("QR_NETWORK_STORAGE", v);
        } else {
            std::env::remove_var("QR_NETWORK_STORAGE");
        }

        primed.expect("priming migration must succeed");
        r1.expect("first post-reclaim call must not hit a database-locked race");
        r2.expect("second post-reclaim call must not hit a database-locked race");
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
            applied, 20,
            "expected all twenty shared schema versions to apply"
        );

        let version: (i64,) = sqlx::query_as("SELECT MAX(version) FROM schema_version")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(version.0, 20);
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
            applied, 19,
            "shared v2 through v20 should count as newly applied from a stale v1 database"
        );

        // items.id=427: shared_013.sql drops tier3_providers (generalized
        // into providers) -- a stale v1 database heals straight through to
        // the current providers shape, never stopping at the retired table.
        let old_table: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='tier3_providers'",
        )
        .fetch_optional(&mut conn)
        .await
        .unwrap();
        assert!(
            old_table.is_none(),
            "tier3_providers must not survive healing past shared_013.sql -- it's generalized \
             into providers, not left dangling as a second source of truth"
        );

        let exists: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='providers'",
        )
        .fetch_optional(&mut conn)
        .await
        .unwrap();
        assert!(
            exists.is_some(),
            "providers must be healed into a database stale at schema_version=1, \
             without requiring the database to be deleted and recreated"
        );

        let seeded: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM providers")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert!(
            seeded.0 > 0,
            "shared_001.sql's seeded provider rows, migrated into providers by shared_013.sql, \
             must also be healed in"
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
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
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
            8,
            "personal_001 + personal_002 + personal_003 + personal_004 + personal_005 + personal_006 + personal_007 + personal_008 must all apply in one pass"
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
            "group_keys",
            "group_fact_sources",
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

        let voice_profile_columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(voice_profiles)")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert!(
            voice_profile_columns
                .iter()
                .any(|c| c.1 == "modification_state"),
            "personal_007's ALTER TABLE must have applied to the real file"
        );
    }

    #[tokio::test]
    async fn migrate_personal_db_is_idempotent_on_real_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
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

        assert_eq!(first.expect("first migration must succeed"), 8);
        assert_eq!(
            second.expect("second migration on an already-migrated real file must not error"),
            0,
            "re-running the migration on an already-migrated real file must be a no-op"
        );
    }

    #[tokio::test]
    async fn migrate_personal_db_rejects_wrong_key_on_real_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
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
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
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
            2,
            "group_001 + group_002 must both apply in one pass"
        );

        let mut conn = open_verify_conn(&db_path, Some(TEST_KEY_HEX)).await;
        for table in ["documents", "document_permissions", "group_facts"] {
            assert!(
                table_exists(&mut conn, table).await,
                "table {table} must exist after migration to a real encrypted file"
            );
        }
    }

    #[tokio::test]
    async fn migrate_group_db_is_idempotent_on_real_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
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

        assert_eq!(first.expect("first migration must succeed"), 2);
        assert_eq!(
            second.expect("second migration on an already-migrated real file must not error"),
            0,
            "re-running the migration on an already-migrated real file must be a no-op"
        );
    }

    #[tokio::test]
    async fn schema_version_exists_true_for_real_encrypted_file_with_correct_key() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
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
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
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
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
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
            6,
            "outputs_001 + outputs_002 + outputs_003 + outputs_004 + outputs_005 + \
             outputs_006 must all apply in one pass"
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
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
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
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
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
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
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
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
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

    // -- items.id=484: integrity_check gate (Option D) tests ---------------
    //
    // integrity_check_decision is tested directly as pure branching logic
    // first (fast, deterministic, no real connection needed), then the real
    // migrate_personal_db path is exercised end-to-end to confirm the gate
    // is actually wired up. Each pure-logic test uses its own UUID-suffixed
    // fake path -- CHECKED_FILES is one process-wide static shared by every
    // test in this binary, so a fixed literal path would let unrelated
    // tests interfere with each other depending on run order.

    #[test]
    fn integrity_check_decision_without_a_path_always_runs_full_check() {
        // Direct run_migrations() callers with no file-path identity (every
        // :memory:-backed test in this module, plus provider_store.rs's own
        // direct run_migrations() calls) have nothing to gate against --
        // they must keep getting the unconditional full check this module
        // always ran before items.id=484, on every single call.
        assert_eq!(integrity_check_decision(None, 0), Some(true));
        assert_eq!(integrity_check_decision(None, 5), Some(true));
    }

    #[test]
    fn integrity_check_decision_runs_full_check_on_first_open_with_a_migration_applied() {
        let fake_path = PathBuf::from(format!("/fake/gate-test-{}.db", uuid::Uuid::new_v4()));
        assert_eq!(
            integrity_check_decision(Some(&fake_path), 8),
            Some(true),
            "first open of a file that applied a real migration must run the full \
             integrity_check -- this is exactly the crash/unclean-shutdown detection \
             Option D must not weaken"
        );
    }

    #[test]
    fn integrity_check_decision_runs_quick_check_on_first_open_with_nothing_pending() {
        let fake_path = PathBuf::from(format!("/fake/gate-test-{}.db", uuid::Uuid::new_v4()));
        assert_eq!(
            integrity_check_decision(Some(&fake_path), 0),
            Some(false),
            "first-touch-no-migration must still run the cheaper quick_check, not skip \
             entirely -- every file must be checked at least once per process launch, \
             not gated on version-parity alone"
        );
    }

    #[test]
    fn integrity_check_decision_skips_a_same_run_reopen_of_an_already_checked_file() {
        let fake_path = PathBuf::from(format!("/fake/gate-test-{}.db", uuid::Uuid::new_v4()));
        // First open records this path as checked, regardless of outcome.
        assert_eq!(integrity_check_decision(Some(&fake_path), 3), Some(true));
        // Any subsequent open of the SAME path in this process run must
        // skip entirely -- even one that itself has further migrations to
        // apply, per items.id=484's own "on every subsequent open... skip
        // the check entirely" spec (no carve-out for a later migration).
        assert_eq!(integrity_check_decision(Some(&fake_path), 0), None);
        assert_eq!(integrity_check_decision(Some(&fake_path), 2), None);
    }

    #[tokio::test]
    async fn migrate_personal_db_forces_full_check_on_first_open_then_skips_same_run_reopen() {
        // End-to-end proof that migrate_personal_db's real call path
        // actually reaches the Option D gate. A brand-new file's first
        // migration always applies all 8 real personal_*.sql versions --
        // nothing needs to be hand-seeded to force that -- which is exactly
        // the "applied a real migration" case the gate must run the full
        // integrity_check for on this, its first open.
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "gate-test-user";
        let persona_id = "gate-test-persona";
        let db_path = tempdir
            .path()
            .join("users")
            .join(user_id)
            .join("personas")
            .join(persona_id)
            .join("personal.db");

        assert!(
            !is_marked_checked_for_test(&db_path),
            "a path never opened by this process must not already be in the gate"
        );

        let first = migrate_personal_db(user_id, persona_id, TEST_KEY_HEX).await;
        let marked_after_first = is_marked_checked_for_test(&db_path);
        let second = migrate_personal_db(user_id, persona_id, TEST_KEY_HEX).await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert_eq!(
            first.expect("first migration, which applies all 8 real versions, must succeed"),
            8,
            "a brand-new personal.db must apply every real migration version on first open"
        );
        assert!(
            marked_after_first,
            "the first open, having applied a real migration and run the full \
             integrity_check, must record this path as checked so a same-run reopen \
             can skip it"
        );
        assert_eq!(
            second.expect("a same-run reopen of an already-checked file must not error"),
            0,
            "no migrations are pending on the second open of an already-migrated file"
        );
    }

    // -- schema shape drift detection (items.id=477) -------------------------
    //
    // validate_manifest()/validate_v1_rerun_safety() above only look at
    // statement *kind* and ordering -- neither catches a plain typo or
    // accidental column rename inside a CREATE TABLE/ALTER TABLE statement,
    // which would otherwise only surface as a runtime "no such column" error
    // against a real user's encrypted DB, long after the schema file was
    // written. This test runs every prefix's full migration chain against a
    // fresh in-memory connection, introspects the resulting schema via
    // sqlite_master/PRAGMA table_info, and diffs the result against a
    // checked-in golden snapshot (tests/golden/schema_shape.txt) -- any
    // accidental drift in table/column shape fails cargo test immediately.
    //
    // Prefixes are read directly from SCHEMA_FILES rather than a separate
    // hand-maintained list, so a newly added prefix is picked up
    // automatically instead of silently going unchecked.
    //
    // An intentional schema change updates the snapshot the same way an
    // intentional Gate1-4 golden-vector fixture update does (see
    // tests/golden_vectors.rs's own header): re-run with
    // UPDATE_SCHEMA_SNAPSHOT=1 set, then review and commit the diff.

    fn all_schema_prefixes() -> Vec<&'static str> {
        let mut prefixes: Vec<&'static str> = SCHEMA_FILES.iter().map(|f| f.prefix).collect();
        prefixes.sort_unstable();
        prefixes.dedup();
        prefixes
    }

    /// Deterministic textual description of every user table's columns in
    /// the connection's current schema. Table order is sorted; column order
    /// is left as PRAGMA table_info returns it (declaration order) -- a
    /// genuine column reorder is exactly the kind of drift this should catch.
    async fn describe_schema_shape(conn: &mut SqliteConnection) -> String {
        let tables: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .fetch_all(&mut *conn)
        .await
        .expect("sqlite_master query must succeed against a freshly migrated in-memory db");

        let mut out = String::new();
        for (table,) in tables {
            out.push_str(&format!("TABLE {table}\n"));
            // PRAGMA table_info's table-name argument isn't a normal bindable
            // query parameter in SQLite -- safe to interpolate directly since
            // `table` came from sqlite_master itself, never external input.
            let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
                sqlx::query_as(&format!("PRAGMA table_info({table})"))
                    .fetch_all(&mut *conn)
                    .await
                    .unwrap_or_else(|e| panic!("PRAGMA table_info({table}) failed: {e}"));
            for (_cid, name, col_type, notnull, dflt, pk) in columns {
                out.push_str(&format!(
                    "  {name} {col_type} notnull={notnull} pk={pk} default={dflt:?}\n"
                ));
            }
        }
        out
    }

    fn schema_shape_golden_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("golden")
            .join("schema_shape.txt")
    }

    #[tokio::test]
    async fn test_schema_shape_matches_golden_snapshot() {
        let mut actual = String::new();
        for prefix in all_schema_prefixes() {
            let mut conn = make_test_conn().await;
            run_migrations(&mut conn, prefix, None)
                .await
                .unwrap_or_else(|e| panic!("migrating prefix '{prefix}' failed: {e}"));
            actual.push_str(&format!("=== {prefix} ===\n"));
            actual.push_str(&describe_schema_shape(&mut conn).await);
        }

        let golden_path = schema_shape_golden_path();

        if std::env::var("UPDATE_SCHEMA_SNAPSHOT").is_ok() {
            std::fs::write(&golden_path, &actual)
                .unwrap_or_else(|e| panic!("failed to write {golden_path:?}: {e}"));
            return;
        }

        let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|e| {
            panic!(
                "could not read {golden_path:?}: {e} -- if this is a brand-new schema \
                 file/prefix, generate the snapshot by re-running this test with \
                 UPDATE_SCHEMA_SNAPSHOT=1 set, then review and commit the diff"
            )
        });

        assert_eq!(
            actual,
            expected,
            "schema shape drift detected against {}: if this change is intentional, \
             regenerate the snapshot by re-running this test with \
             UPDATE_SCHEMA_SNAPSHOT=1 set, then review and commit the diff",
            golden_path.display()
        );
    }
}
