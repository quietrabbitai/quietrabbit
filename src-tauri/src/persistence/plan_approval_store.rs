// src-tauri/src/persistence/plan_approval_store.rs
//
// items.id=901 (decisions.id=942 architecture point 4 and rulings Q2 and Q3): the typed
// store for plan_approvals (schema/outputs_017.sql) in the persona's outputs.db. It holds
// the approval a user gave to a Focus's RUN PLAN in one chat: the digest of the approved
// plan, the per-run cost ceiling the user chose, and the per-step choices. A document
// session therefore asks once.
//
// Typed API only: put_approval, get_approval and delete_approval. There is no other path
// to this table (a source scan pins it).
//
// CONNECTIONS: every function takes a caller-supplied `&mut SqliteConnection` to the
// persona's outputs.db. Callers open it with output_store::open_outputs_db_retrying (a
// failed read here fails a run start closed, as for launch answers, items.id=897). The
// persona is the file, so nothing here takes a persona id and a function can never read
// another persona's rows. A connection must not already be inside a transaction.
//
// THE LATCH (decisions.id=939 section 4, decisions.id=509). put_approval runs in ONE
// `BEGIN IMMEDIATE` transaction that takes SQLite's write lock BEFORE it reads the latch,
// and checks the latch (session_store::ensure_writable) BEFORE its first write, so no
// approval commits after the latch does and a latch set between a caller's earlier check
// and this write is still seen. A latched chat is refused (LatchError::Suppressed). A latch
// that cannot be read is refused too (LatchError::Unreadable): the caller then keeps the
// approval in process memory only (conductor::plan_memory), NEVER here. Approvals written
// before a latch was set stay and still read: reads are not latch-gated.
//
// delete_approval (a REVOCATION) is NOT latch-gated: it only withdraws a permission, so it
// can never widen anything, and a user must be able to withdraw an approval that was written
// before the chat became a one-time session.
//
// WHAT IS STORED: the digest (64 hex characters), the ceiling in micro-USD, and choices_json,
// a JSON array of per-step choices (action id, strategy, provider id and model id). Never a
// prompt, a draft, a persona fact or a model response. This module validates the shape; the
// meaning of a choice belongs to conductor::plan_approval.
//
// PRIVACY: the module logs nothing (a source scan pins it) and no error carries stored data.
//
// QUERY STYLE: runtime sqlx::query() only, never the query!() macros.

use std::fmt;

use sqlx::{Row, SqliteConnection};
use thiserror::Error;

use crate::persistence::session_store::{
    begin_write, ensure_writable, finish_write, validate_context_key, validate_text, LatchError,
    PutOutcome,
};

/// Largest choices_json in bytes. The schema CHECK repeats it.
pub const CHOICES_MAX_BYTES: usize = 8 * 1024;

const FOCUS_ID_MAX_CHARS: usize = 128;

/// Characters of a SHA-256 hex digest.
pub const DIGEST_HEX_CHARS: usize = 64;

/// No variant carries stored data.
#[derive(Debug, Error)]
pub enum ApprovalStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Validation(String),
    #[error("{0}")]
    Latch(#[from] LatchError),
}

/// One stored approval.
#[derive(Clone, PartialEq, Eq)]
pub struct PlanApproval {
    pub context_key: String,
    pub focus_id: String,
    /// Lowercase hex SHA-256 of the approved plan (conductor::run_plan).
    pub digest: String,
    /// The per-run ceiling the user approved, in micro-USD.
    pub ceiling_micros: i64,
    /// A JSON array of per-step choices.
    pub choices_json: String,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// Shows the digest and the ceiling, never the choices.
impl fmt::Debug for PlanApproval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlanApproval")
            .field("focus_id", &self.focus_id)
            .field("digest", &&self.digest[..self.digest.len().min(12)])
            .field("ceiling_micros", &self.ceiling_micros)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// One approval write.
#[derive(Clone)]
pub struct ApprovalWrite<'a> {
    pub context_key: &'a str,
    pub focus_id: &'a str,
    pub digest: &'a str,
    pub ceiling_micros: i64,
    pub choices_json: &'a str,
}

impl fmt::Debug for ApprovalWrite<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApprovalWrite")
            .field("focus_id", &self.focus_id)
            .field("ceiling_micros", &self.ceiling_micros)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate_scope(context_key: &str, focus_id: &str) -> Result<(), ApprovalStoreError> {
    validate_context_key(context_key).map_err(ApprovalStoreError::Validation)?;
    validate_text("focus_id", focus_id, FOCUS_ID_MAX_CHARS).map_err(ApprovalStoreError::Validation)
}

fn validate_write(write: &ApprovalWrite<'_>) -> Result<(), ApprovalStoreError> {
    validate_scope(write.context_key, write.focus_id)?;
    let digest_ok = write.digest.len() == DIGEST_HEX_CHARS
        && write
            .digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !digest_ok {
        return Err(ApprovalStoreError::Validation(
            "digest must be 64 lowercase hex characters".into(),
        ));
    }
    if write.ceiling_micros < 0 {
        return Err(ApprovalStoreError::Validation(
            "ceiling_micros must not be negative".into(),
        ));
    }
    if write.choices_json.len() > CHOICES_MAX_BYTES {
        return Err(ApprovalStoreError::Validation(format!(
            "choices are {} bytes, the limit is {CHOICES_MAX_BYTES}",
            write.choices_json.len()
        )));
    }
    if write.choices_json.contains('\0') {
        return Err(ApprovalStoreError::Validation(
            "choices must not contain a NUL character".into(),
        ));
    }
    match serde_json::from_str::<serde_json::Value>(write.choices_json) {
        Ok(v) if v.is_array() => Ok(()),
        _ => Err(ApprovalStoreError::Validation(
            "choices must be a JSON array".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

const COLUMNS: &str =
    "context_key, focus_id, digest, ceiling_micros, choices_json, version, created_at, updated_at";

fn row_to_approval(r: &sqlx::sqlite::SqliteRow) -> Result<PlanApproval, sqlx::Error> {
    Ok(PlanApproval {
        context_key: r.try_get("context_key")?,
        focus_id: r.try_get("focus_id")?,
        digest: r.try_get("digest")?,
        ceiling_micros: r.try_get("ceiling_micros")?,
        choices_json: r.try_get("choices_json")?,
        version: r.try_get("version")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

/// The approval of one Focus in one chat, or None when there is none. A read error is
/// an Err: callers must not treat it as absent. Not latch-gated.
pub async fn get_approval(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
) -> Result<Option<PlanApproval>, ApprovalStoreError> {
    validate_scope(context_key, focus_id)?;
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM plan_approvals WHERE context_key = ? AND focus_id = ?"
    ))
    .bind(context_key)
    .bind(focus_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(row_to_approval).transpose()?)
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Store the approval, replacing an earlier one (the latest approval wins, version + 1).
/// Refused once the chat is latched, and when the latch cannot be read, inside the same
/// transaction as the write. THE ONLY WRITER of plan_approvals.
pub async fn put_approval(
    conn: &mut SqliteConnection,
    write: &ApprovalWrite<'_>,
) -> Result<PutOutcome, ApprovalStoreError> {
    validate_write(write)?;
    begin_write(conn).await?;
    let result = put_inner(conn, write).await;
    finish_write(conn, result).await
}

async fn put_inner(
    conn: &mut SqliteConnection,
    write: &ApprovalWrite<'_>,
) -> Result<PutOutcome, ApprovalStoreError> {
    // The latch first, under the write lock already held by begin_write.
    ensure_writable(conn, write.context_key).await?;

    let current: Option<i64> = sqlx::query_scalar(
        "SELECT version FROM plan_approvals WHERE context_key = ? AND focus_id = ?",
    )
    .bind(write.context_key)
    .bind(write.focus_id)
    .fetch_optional(&mut *conn)
    .await?;
    let now = crate::providers::utils::now();

    match current {
        None => {
            sqlx::query(&format!(
                "INSERT INTO plan_approvals ({COLUMNS}) VALUES (?, ?, ?, ?, ?, 1, ?, ?)"
            ))
            .bind(write.context_key)
            .bind(write.focus_id)
            .bind(write.digest)
            .bind(write.ceiling_micros)
            .bind(write.choices_json)
            .bind(&now)
            .bind(&now)
            .execute(&mut *conn)
            .await?;
            Ok(PutOutcome::Created { version: 1 })
        }
        Some(version) => {
            let next = version + 1;
            sqlx::query(
                "UPDATE plan_approvals
                 SET digest = ?, ceiling_micros = ?, choices_json = ?, version = ?, updated_at = ?
                 WHERE context_key = ? AND focus_id = ?",
            )
            .bind(write.digest)
            .bind(write.ceiling_micros)
            .bind(write.choices_json)
            .bind(next)
            .bind(&now)
            .bind(write.context_key)
            .bind(write.focus_id)
            .execute(&mut *conn)
            .await?;
            Ok(PutOutcome::Updated { version: next })
        }
    }
}

/// Withdraw the approval of one Focus in one chat. Returns whether there was one. NOT
/// latch-gated (see the module comment): a revocation only removes a permission.
pub async fn delete_approval(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
) -> Result<bool, ApprovalStoreError> {
    validate_scope(context_key, focus_id)?;
    let done = sqlx::query("DELETE FROM plan_approvals WHERE context_key = ? AND focus_id = ?")
        .bind(context_key)
        .bind(focus_id)
        .execute(&mut *conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::session_store::{latch_write_suppressed, test_db::mem_outputs_conn};

    const CHAT: &str = "chat-1";
    const FOCUS: &str = "writing-assistant";

    fn digest(c: char) -> String {
        c.to_string().repeat(DIGEST_HEX_CHARS)
    }

    fn w<'a>(digest: &'a str, ceiling: i64, choices: &'a str) -> ApprovalWrite<'a> {
        ApprovalWrite {
            context_key: CHAT,
            focus_id: FOCUS,
            digest,
            ceiling_micros: ceiling,
            choices_json: choices,
        }
    }

    async fn count(conn: &mut SqliteConnection) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM plan_approvals")
            .fetch_one(&mut *conn)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_then_read_back_and_replace_moves_the_version() {
        let mut conn = mem_outputs_conn().await;
        assert!(get_approval(&mut conn, CHAT, FOCUS)
            .await
            .unwrap()
            .is_none());
        let d1 = digest('a');
        let out = put_approval(&mut conn, &w(&d1, 5_000, "[]")).await.unwrap();
        assert_eq!(out, PutOutcome::Created { version: 1 });
        let got = get_approval(&mut conn, CHAT, FOCUS).await.unwrap().unwrap();
        assert_eq!(
            (got.digest.as_str(), got.ceiling_micros, got.version),
            (d1.as_str(), 5_000, 1)
        );
        assert_eq!(got.choices_json, "[]");

        // The latest approval wins, and only for its own chat and Focus.
        let d2 = digest('b');
        let choices = r#"[{"action_id":"draft","strategy":"local"}]"#;
        let out = put_approval(&mut conn, &w(&d2, 9_000, choices))
            .await
            .unwrap();
        assert_eq!(out, PutOutcome::Updated { version: 2 });
        let got = get_approval(&mut conn, CHAT, FOCUS).await.unwrap().unwrap();
        assert_eq!(
            (got.digest.as_str(), got.ceiling_micros, got.version),
            (d2.as_str(), 9_000, 2)
        );
        assert_eq!(got.choices_json, choices);
        assert!(get_approval(&mut conn, "chat-2", FOCUS)
            .await
            .unwrap()
            .is_none());
        assert!(get_approval(&mut conn, CHAT, "tech-support")
            .await
            .unwrap()
            .is_none());
        assert_eq!(count(&mut conn).await, 1);
    }

    #[tokio::test]
    async fn the_store_refuses_a_malformed_write_before_touching_the_table() {
        let mut conn = mem_outputs_conn().await;
        let good = digest('c');
        let long = format!("[\"{}\"]", "x".repeat(CHOICES_MAX_BYTES));
        for (label, write) in [
            ("short digest", w("abc", 1, "[]")),
            ("uppercase digest", w(&"A".repeat(64), 1, "[]")),
            ("negative ceiling", w(&good, -1, "[]")),
            ("not an array", w(&good, 1, r#"{"a":1}"#)),
            ("not json", w(&good, 1, "nope")),
            ("too long", w(&good, 1, &long)),
        ] {
            let err = put_approval(&mut conn, &write).await.unwrap_err();
            assert!(matches!(err, ApprovalStoreError::Validation(_)), "{label}");
        }
        let mut bad_scope = w(&good, 1, "[]");
        bad_scope.context_key = "";
        assert!(put_approval(&mut conn, &bad_scope).await.is_err());
        bad_scope = w(&good, 1, "[]");
        bad_scope.focus_id = "";
        assert!(put_approval(&mut conn, &bad_scope).await.is_err());
        assert_eq!(count(&mut conn).await, 0);
    }

    /// Ruling R-A test 1: a submit in a latched chat writes no row.
    #[tokio::test]
    async fn a_latched_chat_writes_no_row() {
        let mut conn = mem_outputs_conn().await;
        latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-a")
            .await
            .unwrap();
        let d = digest('d');
        let err = put_approval(&mut conn, &w(&d, 1, "[]")).await.unwrap_err();
        assert!(
            matches!(err, ApprovalStoreError::Latch(LatchError::Suppressed)),
            "{err:?}"
        );
        assert_eq!(count(&mut conn).await, 0);
        // The latch is scoped to the chat: another chat still writes.
        let mut other = w(&d, 1, "[]");
        other.context_key = "chat-2";
        assert!(put_approval(&mut conn, &other).await.is_ok());
        assert_eq!(count(&mut conn).await, 1);
    }

    /// An approval written before the latch was set stays and still reads. A
    /// revocation still works in a latched chat, and nothing new can be written.
    #[tokio::test]
    async fn an_approval_written_before_the_latch_stays_and_can_still_be_revoked() {
        let mut conn = mem_outputs_conn().await;
        let d = digest('e');
        put_approval(&mut conn, &w(&d, 7, "[]")).await.unwrap();
        latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-a")
            .await
            .unwrap();
        let got = get_approval(&mut conn, CHAT, FOCUS).await.unwrap().unwrap();
        assert_eq!(got.ceiling_micros, 7, "setting the latch deletes nothing");
        // No replacement either.
        assert!(put_approval(&mut conn, &w(&digest('f'), 9, "[]"))
            .await
            .is_err());
        assert_eq!(
            get_approval(&mut conn, CHAT, FOCUS)
                .await
                .unwrap()
                .unwrap()
                .ceiling_micros,
            7
        );
        assert!(delete_approval(&mut conn, CHAT, FOCUS).await.unwrap());
        assert!(!delete_approval(&mut conn, CHAT, FOCUS).await.unwrap());
        assert_eq!(count(&mut conn).await, 0);
    }

    /// Ruling R-A test 3: an unreadable latch refuses the write (the caller then falls
    /// back to the memory-only home, never to this one).
    #[tokio::test]
    async fn an_unreadable_latch_writes_no_row() {
        let mut conn = mem_outputs_conn().await;
        sqlx::query("DROP TABLE session_variables")
            .execute(&mut conn)
            .await
            .unwrap();
        let err = put_approval(&mut conn, &w(&digest('1'), 1, "[]"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ApprovalStoreError::Latch(LatchError::Unreadable(_))),
            "{err:?}"
        );
        assert_eq!(count(&mut conn).await, 0);
    }

    /// Ruling R-A test 2: a latch set between the check and the write. The write lock is
    /// taken before the latch is read, so a latch writer cannot commit inside the
    /// approval's critical section: it is refused as busy until the approval commits, and
    /// then the approval is already stored. The other order (latch first) is refused, so
    /// no approval can commit after the latch does.
    #[tokio::test]
    async fn a_latch_cannot_slip_in_between_the_check_and_the_write() {
        use sqlx::sqlite::SqliteConnectOptions;
        use sqlx::ConnectOptions;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("outputs.db");
        let open = || {
            let path = path.clone();
            async move {
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true)
                    .foreign_keys(true)
                    .busy_timeout(std::time::Duration::from_millis(150))
                    .connect()
                    .await
                    .expect("file connection")
            }
        };
        let mut a = open().await;
        crate::persistence::migrations::run_migrations(&mut a, "outputs", None)
            .await
            .expect("outputs migrations");
        let mut b = open().await;

        // A is inside the approval's critical section.
        let d = digest('2');
        let write = w(&d, 11, "[]");
        begin_write(&mut a).await.unwrap();
        let landed = put_inner(&mut a, &write).await.unwrap();
        assert_eq!(landed, PutOutcome::Created { version: 1 });

        // B cannot set the latch while A holds the write lock.
        let blocked = latch_write_suppressed(&mut b, CHAT, FOCUS, "run-b").await;
        assert!(blocked.is_err(), "the latch must wait for the approval");

        finish_write::<_, ApprovalStoreError>(&mut a, Ok(()))
            .await
            .unwrap();
        // The approval landed before the latch; the latch then sets.
        latch_write_suppressed(&mut b, CHAT, FOCUS, "run-b")
            .await
            .unwrap();
        assert!(get_approval(&mut a, CHAT, FOCUS).await.unwrap().is_some());
        // And now nothing more is written.
        let err = put_approval(&mut a, &w(&digest('3'), 1, "[]"))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ApprovalStoreError::Latch(LatchError::Suppressed)
        ));
    }

    #[tokio::test]
    async fn the_table_itself_refuses_what_the_store_refuses() {
        let mut conn = mem_outputs_conn().await;
        let insert = |digest: String, ceiling: i64, key: &'static str| {
            sqlx::query(
                "INSERT INTO plan_approvals
                 (context_key, focus_id, digest, ceiling_micros, created_at, updated_at)
                 VALUES (?, 'f', ?, ?, 't', 't')",
            )
            .bind(key)
            .bind(digest)
            .bind(ceiling)
        };
        assert!(insert("a".repeat(64), 0, "k1")
            .execute(&mut conn)
            .await
            .is_ok());
        assert!(insert("a".repeat(63), 0, "k2")
            .execute(&mut conn)
            .await
            .is_err());
        assert!(insert("a".repeat(64), -1, "k3")
            .execute(&mut conn)
            .await
            .is_err());
        assert!(insert("a".repeat(64), 0, "")
            .execute(&mut conn)
            .await
            .is_err());
        // The primary key is the scope.
        assert!(insert("b".repeat(64), 0, "k1")
            .execute(&mut conn)
            .await
            .is_err());
        // The table has no foreign key and no trigger (items.id=633 rebuilds focus_runs).
        let fks: Vec<(String,)> =
            sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_list('plan_approvals')")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert!(fks.is_empty());
    }

    #[test]
    fn debug_shows_no_choices() {
        let a = PlanApproval {
            context_key: "c".into(),
            focus_id: "f".into(),
            digest: "0123456789abcdef".repeat(4),
            ceiling_micros: 5,
            choices_json: r#"[{"action_id":"SECRET-ACTION"}]"#.into(),
            version: 1,
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        let shown = format!("{a:?}");
        assert!(!shown.contains("SECRET-ACTION"), "{shown}");
        let write = ApprovalWrite {
            context_key: "c",
            focus_id: "f",
            digest: "d",
            ceiling_micros: 1,
            choices_json: "SECRET-CHOICES",
        };
        assert!(!format!("{write:?}").contains("SECRET-CHOICES"));
    }

    /// The approval write path is the only writer of plan_approvals, and it checks the
    /// latch before its first write. Source scans, because the rule is about what the
    /// code does (ruling R-A, items.id=901).
    #[test]
    fn put_approval_is_the_only_writer_and_checks_the_latch_first() {
        let src = include_str!("plan_approval_store.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        for banned in [
            "log::",
            "println!",
            "eprintln!",
            "dbg!",
            "tracing::",
            "sqlx::query!(",
            "query_as!(",
            "query_scalar!(",
        ] {
            assert!(!code.contains(banned), "plan_approval_store uses {banned}");
        }
        assert_eq!(code.matches("INSERT INTO plan_approvals").count(), 1);
        assert_eq!(code.matches("UPDATE plan_approvals").count(), 1);
        assert_eq!(code.matches("DELETE FROM plan_approvals").count(), 1);
        // Inside put_inner the latch check comes before either write.
        let inner = code
            .split("async fn put_inner")
            .nth(1)
            .unwrap()
            .split("/// Withdraw the approval")
            .next()
            .unwrap();
        let latch = inner.find("ensure_writable(").expect("latch check");
        assert!(latch < inner.find("INSERT INTO plan_approvals").unwrap());
        assert!(latch < inner.find("UPDATE plan_approvals").unwrap());
        // put_approval opens its transaction before it reads anything.
        let put = code
            .split("pub async fn put_approval")
            .nth(1)
            .unwrap()
            .split("async fn put_inner")
            .next()
            .unwrap();
        assert!(put.find("begin_write(").unwrap() < put.find("put_inner(").unwrap());

        // No other file in the crate writes the table.
        let mut offenders = Vec::new();
        walk(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut |p, text| {
                let name = p.to_string_lossy();
                if name.ends_with("persistence/plan_approval_store.rs")
                    || name.ends_with("persistence/migrations.rs")
                {
                    return;
                }
                // This scan names the table in its own strings, so skip the test module of
                // any file that is itself a scan (none yet), and look at code only.
                let code = text.split("#[cfg(test)]").next().unwrap_or(text);
                for stmt in [
                    "INSERT INTO plan_approvals",
                    "UPDATE plan_approvals",
                    "DELETE FROM plan_approvals",
                    "INSERT OR REPLACE INTO plan_approvals",
                ] {
                    if code.contains(stmt) {
                        offenders.push(format!("{name}: {stmt}"));
                    }
                }
            },
        );
        assert!(offenders.is_empty(), "{offenders:?}");
    }

    fn walk(dir: std::path::PathBuf, f: &mut dyn FnMut(&std::path::Path, &str)) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(path, f);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                f(&path, &text);
            }
        }
    }
}
