// src-tauri/src/persistence/savepoint.rs
//
// The one place a failed SAVEPOINT is undone (items.id=615).
//
// `ROLLBACK TO x` undoes the savepoint's writes but leaves the savepoint --
// and the transaction it opened, with its write lock -- open. A pooled
// connection (shared.db) goes back to the pool in that state and the next
// user cannot BEGIN. Every failed savepoint therefore does ROLLBACK TO and
// then RELEASE, through `rollback_to_and_release`. The
// `no_rollback_to_outside_the_savepoint_helper` test fails the build if the
// statement appears anywhere else under src/.
//
// A failed RELEASE is the same bug one step later: RELEASE of the outermost
// savepoint is the commit, and a deferred constraint can fail it, leaving the
// savepoint open. A pooled connection therefore finishes through
// `end_savepoint`, which also undoes after a failed RELEASE, proves the
// connection clean, and detaches it if it cannot be made clean (items.id=822).
// The `every_savepoint_opener_is_allowlisted_or_uses_the_helper` test makes a
// new file that opens a savepoint choose between the two.

use sqlx::pool::PoolConnection;
use sqlx::{Sqlite, SqliteConnection};

/// Undo savepoint `name` and close it: ROLLBACK TO, then RELEASE. RELEASE is
/// attempted even when ROLLBACK TO fails (it can fail legitimately when the
/// SAVEPOINT statement itself never ran). Failures are logged and never
/// propagated, so a caller keeps returning the error that sent it here.
/// Returns true iff both statements succeeded.
///
/// `name` is interpolated into the SQL: pass a literal or an internal
/// identifier, never user input.
pub(crate) async fn rollback_to_and_release(conn: &mut SqliteConnection, name: &str) -> bool {
    debug_assert!(
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "savepoint name must be a plain identifier: {name:?}"
    );
    let rolled = match sqlx::query(&format!("ROLLBACK TO {name}"))
        .execute(&mut *conn)
        .await
    {
        Ok(_) => true,
        Err(e) => {
            log::error!("Savepoint rollback failed for `{name}`: {e}");
            false
        }
    };
    let released = match sqlx::query(&format!("RELEASE {name}"))
        .execute(&mut *conn)
        .await
    {
        Ok(_) => true,
        Err(e) => {
            log::warn!("Savepoint release after rollback failed for `{name}`: {e}");
            false
        }
    };
    rolled && released
}

/// Undo a savepoint and prove the connection is outside any transaction.
/// Returns false if it could not be made clean.
async fn undo_savepoint(conn: &mut SqliteConnection, name: &str) -> bool {
    if !rollback_to_and_release(conn, name).await {
        return false;
    }
    // Authoritative probe: BEGIN fails inside a transaction.
    if sqlx::query("BEGIN").execute(&mut *conn).await.is_err() {
        return false;
    }
    sqlx::query("ROLLBACK").execute(&mut *conn).await.is_ok()
}

/// Finish a SAVEPOINT on a pooled connection: RELEASE on success,
/// rollback_to_and_release on error. Takes the connection by value because a
/// connection that cannot be proven to be outside a transaction must never go
/// back to the pool: a failed RELEASE (for example a deferred constraint
/// failing at commit) is undone with rollback_to_and_release, and if THAT
/// fails too the connection is detached from the pool and dropped (closing
/// it, which ends whatever transaction it held).
///
/// `E: From<sqlx::Error>` is how a failed RELEASE becomes the caller's own
/// error type; every caller's error enum already has `Database(#[from]
/// sqlx::Error)`, so no error type changes (items.id=822). Map the step's own
/// error to `E` before calling.
pub(crate) async fn end_savepoint<T, E: From<sqlx::Error>>(
    mut conn: PoolConnection<Sqlite>,
    name: &str,
    step: Result<T, E>,
) -> Result<T, E> {
    let outcome = match step {
        Ok(v) => match sqlx::query(&format!("RELEASE {name}"))
            .execute(&mut *conn)
            .await
        {
            Ok(_) => return Ok(v),
            Err(e) => E::from(e),
        },
        Err(e) => e,
    };
    if !undo_savepoint(&mut conn, name).await {
        drop(conn.detach());
    }
    Err(outcome)
}

/// Test-only failure injection: make the outermost RELEASE (the commit) of
/// any savepoint that INSERTs into `table` fail on this connection, while the
/// function's own writes succeed. A TEMP trigger inserts a row into a TEMP
/// child table whose foreign key is DEFERRABLE INITIALLY DEFERRED and points
/// at a parent row that does not exist, so the violation only surfaces at
/// commit. TEMP objects are per connection and vanish with it; `table` is
/// interpolated, so pass a literal.
#[cfg(test)]
pub(crate) async fn arm_release_failure(conn: &mut SqliteConnection, table: &str) {
    for stmt in [
        "CREATE TEMP TABLE IF NOT EXISTS arm_parent (id TEXT PRIMARY KEY)".to_string(),
        "CREATE TEMP TABLE IF NOT EXISTS arm_child (parent_id TEXT \
         REFERENCES arm_parent(id) DEFERRABLE INITIALLY DEFERRED)"
            .to_string(),
        "DROP TRIGGER IF EXISTS temp.arm_release_failure".to_string(),
        // The body names arm_child unqualified: older SQLite builds (CI's
        // SQLCipher; 3.45/3.46 reproduce it) reject a qualified table name on
        // INSERT/UPDATE/DELETE inside any trigger body. A TEMP trigger resolves
        // the bare name to the TEMP table first, so this reaches temp.arm_child.
        format!(
            "CREATE TEMP TRIGGER arm_release_failure AFTER INSERT ON main.{table} \
             BEGIN INSERT INTO arm_child VALUES ('missing'); END"
        ),
    ] {
        sqlx::query(&stmt)
            .execute(&mut *conn)
            .await
            .unwrap_or_else(|e| panic!("arming failed: {e}\n{stmt}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;
    use std::path::{Path, PathBuf};

    async fn memory_conn() -> SqliteConnection {
        SqliteConnectOptions::new()
            .in_memory(true)
            .connect()
            .await
            .expect("in-memory connection must open")
    }

    #[tokio::test]
    async fn closes_the_savepoint_and_the_transaction_it_opened() {
        let mut conn = memory_conn().await;
        sqlx::query("CREATE TABLE t (x INTEGER)")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("SAVEPOINT sp")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t VALUES (1)")
            .execute(&mut conn)
            .await
            .unwrap();

        assert!(rollback_to_and_release(&mut conn, "sp").await);

        // BEGIN fails inside a transaction, so this proves none is open.
        sqlx::query("BEGIN").execute(&mut conn).await.unwrap();
        sqlx::query("ROLLBACK").execute(&mut conn).await.unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM t")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(n, 0, "the savepoint's write must be undone");
    }

    #[tokio::test]
    async fn an_unknown_savepoint_reports_false_and_does_not_panic() {
        let mut conn = memory_conn().await;
        assert!(!rollback_to_and_release(&mut conn, "never_opened").await);
        // The connection is still usable.
        sqlx::query("SELECT 1").execute(&mut conn).await.unwrap();
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("src dir must be readable") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Every failed savepoint must go through `rollback_to_and_release`, which
    /// always RELEASEs. Comments count: reword them rather than exempt them.
    #[test]
    fn no_rollback_to_outside_the_savepoint_helper() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        assert!(
            files.len() > 50,
            "scan found too few files: {}",
            files.len()
        );

        let mut offenders = Vec::new();
        for path in files {
            if path.ends_with("persistence/savepoint.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("source file must be readable");
            for (i, line) in text.lines().enumerate() {
                let norm = line
                    .to_lowercase()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if norm.contains("rollback to") || norm.contains("rollback transaction to") {
                    offenders.push(format!(
                        "{}:{}",
                        path.strip_prefix(&src).unwrap().display(),
                        i + 1
                    ));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "use persistence::savepoint::rollback_to_and_release instead of ROLLBACK TO \
             (it also RELEASEs; items.id=615): {offenders:?}"
        );
    }

    /// Files that open a SAVEPOINT but do not finish it through
    /// `end_savepoint`, each with why that is acceptable. Paths are relative
    /// to src/.
    const SAVEPOINT_ALLOWLIST: &[(&str, &str)] = &[
        ("persistence/entity_store.rs", "one-shot: open_personal_db"),
        ("persistence/domain_context_store.rs", "one-shot: open_domain_context_db"),
        ("persistence/topic_store.rs", "one-shot: open_outputs_db"),
        ("persistence/personal_store.rs", "one-shot: open_personal_db (the _conn fn's only caller is on it)"),
        ("persistence/plan_state_store.rs", "one-shot: open_plan_state_db"),
        ("persistence/output_store.rs", "one-shot: open_outputs_db (the _conn fns' only production callers pass it)"),
        ("persistence/source_registry_store.rs", "caller-supplied connection; the only production caller, import_record, passes a one-shot personal.db connection"),
        ("persistence/dedup_store.rs", "caller-supplied connection; the only production caller, resolve_candidate, passes a one-shot personal.db connection"),
        ("persistence/cloud_chat_cookie_store.rs", "caller-supplied connection; the only production caller, upsert_cookies, passes a one-shot cookies db connection"),
        ("persistence/group_store.rs", "caller-supplied connection; the only production caller, apply_synced_permissions, passes a one-shot group db connection"),
        ("persistence/migrations.rs", "caller-supplied connection; every production path is migrate_db_file, which opens a one-shot connection"),
        ("persona_sync/engine.rs", "one-shot: open_personal_db"),
        ("persona_view_sync/engine.rs", "one-shot: open_view_cache_db"),
    ];

    /// True if the line, outside a comment, executes SAVEPOINT SQL.
    fn opens_savepoint(line: &str) -> bool {
        let code = line.trim_start();
        !code.starts_with("//")
            && (code.contains("\"SAVEPOINT ") || code.contains("format!(\"SAVEPOINT"))
    }

    /// A file that opens a SAVEPOINT must either finish it through
    /// `end_savepoint` (required whenever the connection can be a pooled
    /// one, i.e. shared.db) or be on SAVEPOINT_ALLOWLIST with a reason
    /// (items.id=822).
    ///
    /// LIMITS, deliberately: the check is per FILE and textual. It cannot
    /// catch (a) a bad or pooled opener added to a file that is already on
    /// the allowlist, or to a file that already calls `end_savepoint` in
    /// another function, and (b) it does not tell pooled connections from
    /// one-shot ones: that judgement is the human's, made when adding an
    /// allowlist entry. All it does is force that decision whenever a file
    /// that did not open savepoints before starts to.
    #[test]
    fn every_savepoint_opener_is_allowlisted_or_uses_the_helper() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        assert!(
            files.len() > 50,
            "scan found too few files: {}",
            files.len()
        );

        let mut unlisted = Vec::new();
        let mut stale = Vec::new();
        let mut seen_listed = std::collections::HashSet::new();
        for path in files {
            if path.ends_with("persistence/savepoint.rs") {
                continue;
            }
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).expect("source file must be readable");
            let opens = text.lines().any(opens_savepoint);
            let uses_helper = text.contains("end_savepoint(");
            let listed = SAVEPOINT_ALLOWLIST.iter().any(|(p, _)| *p == rel);
            if listed {
                seen_listed.insert(rel.clone());
                if !opens {
                    stale.push(format!("{rel}: no longer opens a SAVEPOINT"));
                } else if uses_helper {
                    stale.push(format!("{rel}: now calls end_savepoint"));
                }
            } else if opens && !uses_helper {
                unlisted.push(rel);
            }
        }
        for (p, _) in SAVEPOINT_ALLOWLIST {
            if !seen_listed.contains(*p) {
                stale.push(format!("{p}: no such file under src/"));
            }
        }

        assert!(
            unlisted.is_empty(),
            "these files open a SAVEPOINT but are not on SAVEPOINT_ALLOWLIST and do not call \
             savepoint::end_savepoint: {unlisted:?}. If the function can run on a pooled \
             connection (shared.db), or you are not sure, finish the savepoint with \
             persistence::savepoint::end_savepoint (it also handles a failed RELEASE). If every \
             connection it can run on is one-shot (open_*_db), or caller-supplied with only \
             one-shot production callers, add the file to SAVEPOINT_ALLOWLIST in \
             persistence/savepoint.rs with a one-line reason (items.id=822)."
        );
        assert!(
            stale.is_empty(),
            "SAVEPOINT_ALLOWLIST has stale entries, remove or fix them: {stale:?}"
        );
    }
}
