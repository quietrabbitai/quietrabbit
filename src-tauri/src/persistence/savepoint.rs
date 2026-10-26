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

use sqlx::SqliteConnection;

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
}
