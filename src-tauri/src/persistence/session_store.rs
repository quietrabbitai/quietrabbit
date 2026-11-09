// src-tauri/src/persistence/session_store.rs
//
// items.id=881 -- typed store for session_variables (schema/outputs_013.sql,
// decisions.id=939) in the persona's outputs.db, plus the ONE-TIME WRITE LATCH
// that gates every write of this store and of case_store.
//
// Typed API only. Nothing here is wired into the executor: seeding and
// bound_names() are items.id=886, the store_op writers are items.id=887,
// clear_chat, the retention sweep and the resume pin are items.id=888.
//
// CONNECTIONS: every function takes a caller-supplied `&mut SqliteConnection`
// to the persona's outputs.db, opened by output_store::open_outputs_db (a
// one-shot connection, as everywhere in this persistence layer). The persona
// is the file, so nothing here takes a persona id and a function can never
// read another persona's rows: isolation is the database boundary and the
// per-persona subkey (auth::subkeys::DbPurpose::Outputs). A connection must
// not already be inside a transaction: each write opens its own.
//
// CONCURRENCY (decisions.id=939, findings held): nothing serialises runs per
// chat, so two passes in one chat can overlap. Every write therefore runs in
// ONE `BEGIN IMMEDIATE` transaction that takes SQLite's write lock before it
// reads anything. That makes two things atomic:
//   - version compare-and-set: read the current version and write the next one
//     with no other writer in between, so exactly one of two writers holding
//     the same expected version wins and the other gets VersionConflict;
//   - latch check and write: the writer reads the latch and writes under the
//     same lock the latch writer needs, so no write commits after the latch
//     does.
//
// THE LATCH (decisions.id=939 section 4, decisions.id=509 one-time session):
// one row of session_variables named write_mode with the value suppressed,
// scoped to the chat (context_key, any focus_id). Rules, all enforced here:
//   - monotonic: latch_write_suppressed only ever inserts. There is no function
//     that updates or deletes the row, and the schema CHECK lets the row hold
//     only the value suppressed at version 1. (Removal on archive or retention
//     is items.id=888.)
//   - the name write_mode is reserved: put_session_variable refuses it.
//   - every other write of session_store and case_store calls ensure_writable
//     inside its transaction and is refused once the latch exists.
//   - FAIL CLOSED: if the latch cannot be read, the write is refused
//     (LatchError::Unreadable) and write_mode() returns an error. An absent row
//     is the only thing that means open. Any row named write_mode in the chat,
//     whatever it holds, means suppressed.
// The latch is one control row with no user content, so it is compatible with
// decisions.id=509's "nothing persists", which concerns content.
//
// QUERY STYLE: runtime sqlx::query() only, never the query!() macros.

use sqlx::{Row, SqliteConnection};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Largest session variable value in bytes. The schema CHECK repeats it.
pub const SESSION_VALUE_MAX_BYTES: usize = 2048;

/// Reserved variable name of the one-time write latch.
pub const WRITE_MODE_NAME: &str = "write_mode";

/// The only value the latch row can hold.
pub const WRITE_MODE_SUPPRESSED: &str = "suppressed";

const CONTEXT_KEY_MAX_CHARS: usize = 128;
const FOCUS_ID_MAX_CHARS: usize = 128;
const NAME_MAX_CHARS: usize = 64;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a write was refused by the latch.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LatchError {
    /// The chat is a one-time session: nothing may be saved.
    #[error("this chat is a one-time session, nothing is saved")]
    Suppressed,
    /// The latch could not be read, so the write is refused (fail closed).
    #[error("the one-time session latch could not be read, so nothing is saved: {0}")]
    Unreadable(String),
}

#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Validation(String),
    /// `write_mode` can only be set through latch_write_suppressed.
    #[error("session variable name '{0}' is reserved")]
    ReservedName(String),
    /// The row's version is not the one the writer expected. `current_version`
    /// is None when the row does not exist.
    #[error("session variable version conflict (current version {current_version:?})")]
    VersionConflict { current_version: Option<i64> },
    #[error("{0}")]
    Latch(#[from] LatchError),
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionVariable {
    pub context_key: String,
    pub focus_id: String,
    pub name: String,
    pub value: String,
    pub version: i64,
    pub written_by_run: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One session write. `expected_version` is the version the writer read:
/// None means "I expect the variable to be absent".
#[derive(Debug, Clone)]
pub struct SessionWrite<'a> {
    pub context_key: &'a str,
    pub focus_id: &'a str,
    pub name: &'a str,
    pub value: &'a str,
    pub written_by_run: &'a str,
    pub expected_version: Option<i64>,
}

/// What a successful put did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The variable did not exist and was created at version 1.
    Created { version: i64 },
    /// The expected version matched and the value moved to `version`.
    Updated { version: i64 },
    /// The row already holds this value. Nothing was written. Either the
    /// writer's expected version matched an identical value, or this is a
    /// replay of the same run's earlier write (idempotent retry).
    Unchanged { version: i64 },
}

impl PutOutcome {
    pub fn version(self) -> i64 {
        match self {
            PutOutcome::Created { version }
            | PutOutcome::Updated { version }
            | PutOutcome::Unchanged { version } => version,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    Open,
    Suppressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatchOutcome {
    /// This call set the latch.
    Set,
    /// The chat was already latched. Nothing was written.
    AlreadySet,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

pub(crate) fn validate_text(label: &str, s: &str, max_chars: usize) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if s.chars().count() > max_chars {
        return Err(format!("{label} is longer than {max_chars} characters"));
    }
    if s.contains('\0') {
        return Err(format!("{label} must not contain a NUL character"));
    }
    Ok(())
}

pub(crate) fn validate_context_key(s: &str) -> Result<(), String> {
    validate_text("context_key", s, CONTEXT_KEY_MAX_CHARS)
}

fn validate_name(s: &str) -> Result<(), String> {
    validate_text("name", s, NAME_MAX_CHARS)?;
    let mut chars = s.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    if !first_ok
        || !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(
            "name must be lowercase letters, digits and underscores, starting with a letter".into(),
        );
    }
    Ok(())
}

fn validate_value(s: &str) -> Result<(), String> {
    if s.len() > SESSION_VALUE_MAX_BYTES {
        return Err(format!(
            "value is {} bytes, the limit is {SESSION_VALUE_MAX_BYTES}",
            s.len()
        ));
    }
    if s.contains('\0') {
        return Err("value must not contain a NUL character".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Write transactions (shared with case_store)
// ---------------------------------------------------------------------------

/// Take SQLite's write lock before reading anything. See the module header.
pub(crate) async fn begin_write(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    Ok(())
}

/// Commit on Ok, undo on Err. A failed COMMIT is undone and reported as the
/// error, so a caller never sees success for a write that did not land.
pub(crate) async fn finish_write<T, E>(
    conn: &mut SqliteConnection,
    result: Result<T, E>,
) -> Result<T, E>
where
    E: From<sqlx::Error>,
{
    match result {
        Ok(value) => match sqlx::query("COMMIT").execute(&mut *conn).await {
            Ok(_) => Ok(value),
            Err(e) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(E::from(e))
            }
        },
        Err(e) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// The latch
// ---------------------------------------------------------------------------

/// Refuse the write if the chat is latched or the latch cannot be read. Call
/// it INSIDE the write transaction, before the first write. Fails closed.
pub(crate) async fn ensure_writable(
    conn: &mut SqliteConnection,
    context_key: &str,
) -> Result<(), LatchError> {
    let row =
        sqlx::query("SELECT 1 FROM session_variables WHERE context_key = ? AND name = ? LIMIT 1")
            .bind(context_key)
            .bind(WRITE_MODE_NAME)
            .fetch_optional(&mut *conn)
            .await;
    match row {
        Ok(None) => Ok(()),
        Ok(Some(_)) => Err(LatchError::Suppressed),
        Err(e) => Err(LatchError::Unreadable(e.to_string())),
    }
}

/// The chat's write mode. A read error is an Err, never Open: a caller that
/// cannot read the latch must fail its pass closed (decisions.id=939 section 2).
pub async fn write_mode(
    conn: &mut SqliteConnection,
    context_key: &str,
) -> Result<WriteMode, LatchError> {
    if let Err(msg) = validate_context_key(context_key) {
        return Err(LatchError::Unreadable(msg));
    }
    match ensure_writable(conn, context_key).await {
        Ok(()) => Ok(WriteMode::Open),
        Err(LatchError::Suppressed) => Ok(WriteMode::Suppressed),
        Err(e) => Err(e),
    }
}

/// Set the one-time latch for the chat. Monotonic: it only ever inserts, a
/// second call is AlreadySet, and no function in this store lifts it. `focus_id`
/// is the Focus that declared the one-time session. The latch covers the chat,
/// not that Focus alone.
pub async fn latch_write_suppressed(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
    written_by_run: &str,
) -> Result<LatchOutcome, SessionStoreError> {
    validate_context_key(context_key).map_err(SessionStoreError::Validation)?;
    validate_text("focus_id", focus_id, FOCUS_ID_MAX_CHARS)
        .map_err(SessionStoreError::Validation)?;
    validate_text("written_by_run", written_by_run, 256).map_err(SessionStoreError::Validation)?;

    begin_write(conn).await?;
    let result = latch_inner(conn, context_key, focus_id, written_by_run).await;
    finish_write(conn, result).await
}

async fn latch_inner(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
    written_by_run: &str,
) -> Result<LatchOutcome, SessionStoreError> {
    // An unreadable latch is an error here too: the caller learns the latch
    // is not confirmed set and must not assume it is.
    match ensure_writable(conn, context_key).await {
        Err(LatchError::Suppressed) => return Ok(LatchOutcome::AlreadySet),
        Err(e @ LatchError::Unreadable(_)) => return Err(e.into()),
        Ok(()) => {}
    }
    let now = crate::providers::utils::now();
    sqlx::query(
        "INSERT INTO session_variables
         (context_key, focus_id, name, value, version, written_by_run, created_at, updated_at)
         VALUES (?, ?, ?, ?, 1, ?, ?, ?)",
    )
    .bind(context_key)
    .bind(focus_id)
    .bind(WRITE_MODE_NAME)
    .bind(WRITE_MODE_SUPPRESSED)
    .bind(written_by_run)
    .bind(&now)
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    Ok(LatchOutcome::Set)
}

// ---------------------------------------------------------------------------
// Session variables
// ---------------------------------------------------------------------------

fn row_to_variable(r: &sqlx::sqlite::SqliteRow) -> Result<SessionVariable, sqlx::Error> {
    Ok(SessionVariable {
        context_key: r.try_get("context_key")?,
        focus_id: r.try_get("focus_id")?,
        name: r.try_get("name")?,
        value: r.try_get("value")?,
        version: r.try_get("version")?,
        written_by_run: r.try_get("written_by_run")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

const VARIABLE_COLUMNS: &str =
    "context_key, focus_id, name, value, version, written_by_run, created_at, updated_at";

/// Idempotent upsert with version compare-and-set. Refused once the chat is
/// latched, and for the reserved name write_mode.
///
/// Outcomes, with `expected_version` as the version the writer read:
///   - absent row, expected None: created at version 1.
///   - present row, expected equals its version: updated to version + 1, or
///     Unchanged (nothing written) when the value is already the same.
///   - any other combination is VersionConflict, EXCEPT a replay: if the row
///     already holds this exact value and this same run wrote it, the write
///     already happened (a retry after a lost reply) and the result is
///     Unchanged with the row's version.
pub async fn put_session_variable(
    conn: &mut SqliteConnection,
    write: &SessionWrite<'_>,
) -> Result<PutOutcome, SessionStoreError> {
    validate_context_key(write.context_key).map_err(SessionStoreError::Validation)?;
    validate_text("focus_id", write.focus_id, FOCUS_ID_MAX_CHARS)
        .map_err(SessionStoreError::Validation)?;
    validate_name(write.name).map_err(SessionStoreError::Validation)?;
    validate_value(write.value).map_err(SessionStoreError::Validation)?;
    validate_text("written_by_run", write.written_by_run, 256)
        .map_err(SessionStoreError::Validation)?;
    if write.name == WRITE_MODE_NAME {
        return Err(SessionStoreError::ReservedName(write.name.to_owned()));
    }
    if let Some(v) = write.expected_version {
        if v < 1 {
            return Err(SessionStoreError::Validation(
                "expected_version must be at least 1".into(),
            ));
        }
    }

    begin_write(conn).await?;
    let result = put_inner(conn, write).await;
    finish_write(conn, result).await
}

async fn put_inner(
    conn: &mut SqliteConnection,
    write: &SessionWrite<'_>,
) -> Result<PutOutcome, SessionStoreError> {
    ensure_writable(conn, write.context_key).await?;

    let current =
        get_session_variable_inner(conn, write.context_key, write.focus_id, write.name).await?;
    let now = crate::providers::utils::now();

    let Some(current) = current else {
        if write.expected_version.is_some() {
            return Err(SessionStoreError::VersionConflict {
                current_version: None,
            });
        }
        sqlx::query(&format!(
            "INSERT INTO session_variables ({VARIABLE_COLUMNS}) VALUES (?, ?, ?, ?, 1, ?, ?, ?)"
        ))
        .bind(write.context_key)
        .bind(write.focus_id)
        .bind(write.name)
        .bind(write.value)
        .bind(write.written_by_run)
        .bind(&now)
        .bind(&now)
        .execute(&mut *conn)
        .await?;
        return Ok(PutOutcome::Created { version: 1 });
    };

    let replay = current.value == write.value && current.written_by_run == write.written_by_run;
    match write.expected_version {
        Some(v) if v == current.version => {
            if current.value == write.value {
                return Ok(PutOutcome::Unchanged {
                    version: current.version,
                });
            }
            let next = current.version + 1;
            let done = sqlx::query(
                "UPDATE session_variables
                 SET value = ?, version = ?, written_by_run = ?, updated_at = ?
                 WHERE context_key = ? AND focus_id = ? AND name = ? AND version = ?",
            )
            .bind(write.value)
            .bind(next)
            .bind(write.written_by_run)
            .bind(&now)
            .bind(write.context_key)
            .bind(write.focus_id)
            .bind(write.name)
            .bind(current.version)
            .execute(&mut *conn)
            .await?;
            if done.rows_affected() != 1 {
                return Err(SessionStoreError::VersionConflict {
                    current_version: Some(current.version),
                });
            }
            Ok(PutOutcome::Updated { version: next })
        }
        _ if replay => Ok(PutOutcome::Unchanged {
            version: current.version,
        }),
        _ => Err(SessionStoreError::VersionConflict {
            current_version: Some(current.version),
        }),
    }
}

async fn get_session_variable_inner(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
    name: &str,
) -> Result<Option<SessionVariable>, sqlx::Error> {
    let row = sqlx::query(&format!(
        "SELECT {VARIABLE_COLUMNS} FROM session_variables
         WHERE context_key = ? AND focus_id = ? AND name = ?"
    ))
    .bind(context_key)
    .bind(focus_id)
    .bind(name)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(row_to_variable).transpose()
}

/// One variable, or None when it was never written. A read error is an Err:
/// callers must not treat it as absent.
pub async fn get_session_variable(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
    name: &str,
) -> Result<Option<SessionVariable>, SessionStoreError> {
    validate_context_key(context_key).map_err(SessionStoreError::Validation)?;
    validate_text("focus_id", focus_id, FOCUS_ID_MAX_CHARS)
        .map_err(SessionStoreError::Validation)?;
    validate_name(name).map_err(SessionStoreError::Validation)?;
    Ok(get_session_variable_inner(conn, context_key, focus_id, name).await?)
}

/// Every variable of one Focus in one chat, by name. The latch row is a
/// control row, not a variable, and is not listed: read it with write_mode.
pub async fn list_session_variables(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
) -> Result<Vec<SessionVariable>, SessionStoreError> {
    validate_context_key(context_key).map_err(SessionStoreError::Validation)?;
    validate_text("focus_id", focus_id, FOCUS_ID_MAX_CHARS)
        .map_err(SessionStoreError::Validation)?;
    let rows = sqlx::query(&format!(
        "SELECT {VARIABLE_COLUMNS} FROM session_variables
         WHERE context_key = ? AND focus_id = ? AND name <> ?
         ORDER BY name"
    ))
    .bind(context_key)
    .bind(focus_id)
    .bind(WRITE_MODE_NAME)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .iter()
        .map(row_to_variable)
        .collect::<Result<Vec<_>, _>>()?)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_db {
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{ConnectOptions, SqliteConnection};

    /// A fresh in-memory outputs.db with every real migration applied.
    pub(crate) async fn mem_outputs_conn() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .foreign_keys(true)
            .connect()
            .await
            .expect("in-memory connection");
        crate::persistence::migrations::run_migrations(&mut conn, "outputs", None)
            .await
            .expect("outputs migrations");
        conn
    }
}

#[cfg(test)]
mod tests {
    use super::test_db::mem_outputs_conn;
    use super::*;

    const CHAT: &str = "chat-1";
    const FOCUS: &str = "tech-support";

    fn w<'a>(
        name: &'a str,
        value: &'a str,
        run: &'a str,
        expected: Option<i64>,
    ) -> SessionWrite<'a> {
        SessionWrite {
            context_key: CHAT,
            focus_id: FOCUS,
            name,
            value,
            written_by_run: run,
            expected_version: expected,
        }
    }

    async fn count(conn: &mut SqliteConnection, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&mut *conn)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_then_read_back() {
        let mut conn = mem_outputs_conn().await;
        let out = put_session_variable(&mut conn, &w("rung", "2", "run-a", None))
            .await
            .unwrap();
        assert_eq!(out, PutOutcome::Created { version: 1 });
        let got = get_session_variable(&mut conn, CHAT, FOCUS, "rung")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((got.value.as_str(), got.version), ("2", 1));
        assert_eq!(got.written_by_run, "run-a");
        assert!(get_session_variable(&mut conn, CHAT, FOCUS, "absent")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn compare_and_set_accepts_the_current_version_and_refuses_a_stale_one() {
        let mut conn = mem_outputs_conn().await;
        put_session_variable(&mut conn, &w("rung", "1", "run-a", None))
            .await
            .unwrap();
        let out = put_session_variable(&mut conn, &w("rung", "2", "run-a", Some(1)))
            .await
            .unwrap();
        assert_eq!(out, PutOutcome::Updated { version: 2 });

        // A second writer that still holds version 1 loses.
        let err = put_session_variable(&mut conn, &w("rung", "3", "run-b", Some(1)))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SessionStoreError::VersionConflict {
                    current_version: Some(2)
                }
            ),
            "{err:?}"
        );
        // Expecting absence of a present row is a conflict too.
        let err = put_session_variable(&mut conn, &w("rung", "3", "run-b", None))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SessionStoreError::VersionConflict {
                current_version: Some(2)
            }
        ));
        // Expecting a version of an absent row is a conflict.
        let err = put_session_variable(&mut conn, &w("other", "x", "run-b", Some(1)))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SessionStoreError::VersionConflict {
                current_version: None
            }
        ));

        let got = get_session_variable(&mut conn, CHAT, FOCUS, "rung")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (got.value.as_str(), got.version),
            ("2", 2),
            "the loser changed nothing"
        );
    }

    #[tokio::test]
    async fn a_replayed_write_is_idempotent_and_does_not_bump_the_version() {
        let mut conn = mem_outputs_conn().await;
        put_session_variable(&mut conn, &w("rung", "1", "run-a", None))
            .await
            .unwrap();
        // Same create again (a retry after a lost reply).
        let again = put_session_variable(&mut conn, &w("rung", "1", "run-a", None))
            .await
            .unwrap();
        assert_eq!(again, PutOutcome::Unchanged { version: 1 });

        put_session_variable(&mut conn, &w("rung", "2", "run-a", Some(1)))
            .await
            .unwrap();
        // The same update again: the row is at version 2 now, the writer's
        // expected version is stale, but the row holds this run's value.
        let again = put_session_variable(&mut conn, &w("rung", "2", "run-a", Some(1)))
            .await
            .unwrap();
        assert_eq!(again, PutOutcome::Unchanged { version: 2 });

        // A different run writing the same value on a stale version is not a replay.
        let err = put_session_variable(&mut conn, &w("rung", "2", "run-b", Some(1)))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionStoreError::VersionConflict { .. }));

        // An identical value at the matching version writes nothing.
        let same = put_session_variable(&mut conn, &w("rung", "2", "run-b", Some(2)))
            .await
            .unwrap();
        assert_eq!(same, PutOutcome::Unchanged { version: 2 });
        let got = get_session_variable(&mut conn, CHAT, FOCUS, "rung")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((got.version, got.written_by_run.as_str()), (2, "run-a"));
    }

    #[tokio::test]
    async fn value_is_length_capped_in_the_store_and_in_the_schema() {
        let mut conn = mem_outputs_conn().await;
        let at_cap = "x".repeat(SESSION_VALUE_MAX_BYTES);
        put_session_variable(&mut conn, &w("v", &at_cap, "r", None))
            .await
            .unwrap();

        let over = "x".repeat(SESSION_VALUE_MAX_BYTES + 1);
        let err = put_session_variable(&mut conn, &w("v2", &over, "r", None))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionStoreError::Validation(_)), "{err:?}");
        // Multi-byte text is capped by bytes, not characters.
        let wide = "é".repeat(SESSION_VALUE_MAX_BYTES / 2 + 1);
        assert!(wide.chars().count() <= SESSION_VALUE_MAX_BYTES);
        let err = put_session_variable(&mut conn, &w("v3", &wide, "r", None))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionStoreError::Validation(_)));

        // The schema refuses it independently of the Rust check.
        let direct = sqlx::query(
            "INSERT INTO session_variables
             (context_key, focus_id, name, value, written_by_run, created_at, updated_at)
             VALUES ('c', 'f', 'n', ?, 'r', 't', 't')",
        )
        .bind(&over)
        .execute(&mut conn)
        .await;
        assert!(direct.is_err(), "the CHECK must cap the value");
    }

    #[tokio::test]
    async fn names_and_keys_are_validated() {
        let mut conn = mem_outputs_conn().await;
        for bad in ["", "Upper", "has space", "1lead", "a-b"] {
            let err = put_session_variable(&mut conn, &w(bad, "v", "r", None))
                .await
                .unwrap_err();
            assert!(
                matches!(err, SessionStoreError::Validation(_)),
                "name {bad:?}: {err:?}"
            );
        }
        let mut bad = w("ok", "v", "r", None);
        bad.context_key = "";
        assert!(matches!(
            put_session_variable(&mut conn, &bad).await.unwrap_err(),
            SessionStoreError::Validation(_)
        ));
        assert_eq!(count(&mut conn, "session_variables").await, 0);
    }

    #[tokio::test]
    async fn variables_are_scoped_to_chat_and_focus() {
        let mut conn = mem_outputs_conn().await;
        put_session_variable(&mut conn, &w("rung", "1", "r", None))
            .await
            .unwrap();
        let mut other_chat = w("rung", "9", "r", None);
        other_chat.context_key = "chat-2";
        put_session_variable(&mut conn, &other_chat).await.unwrap();
        let mut other_focus = w("rung", "7", "r", None);
        other_focus.focus_id = "writing-assistant";
        put_session_variable(&mut conn, &other_focus).await.unwrap();

        let mine = list_session_variables(&mut conn, CHAT, FOCUS)
            .await
            .unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].value, "1");
        let theirs = list_session_variables(&mut conn, "chat-2", FOCUS)
            .await
            .unwrap();
        assert_eq!(theirs[0].value, "9");
    }

    // -- the latch ----------------------------------------------------------

    #[tokio::test]
    async fn write_mode_is_open_until_latched_and_the_latch_is_monotonic() {
        let mut conn = mem_outputs_conn().await;
        assert_eq!(write_mode(&mut conn, CHAT).await.unwrap(), WriteMode::Open);

        let first = latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-a")
            .await
            .unwrap();
        assert_eq!(first, LatchOutcome::Set);
        assert_eq!(
            write_mode(&mut conn, CHAT).await.unwrap(),
            WriteMode::Suppressed
        );

        // Setting it again writes nothing.
        let second = latch_write_suppressed(&mut conn, CHAT, "other-focus", "run-b")
            .await
            .unwrap();
        assert_eq!(second, LatchOutcome::AlreadySet);
        assert_eq!(
            count(&mut conn, "session_variables").await,
            1,
            "one control row"
        );

        // No sanctioned path lifts it: the reserved name is refused by put.
        let err = put_session_variable(&mut conn, &w(WRITE_MODE_NAME, "open", "r", Some(1)))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionStoreError::ReservedName(_)), "{err:?}");
        assert_eq!(
            write_mode(&mut conn, CHAT).await.unwrap(),
            WriteMode::Suppressed
        );

        // And the schema lets the row hold only the suppressed value at version 1.
        for stmt in [
            "UPDATE session_variables SET value = 'open' WHERE name = 'write_mode'",
            "UPDATE session_variables SET version = 2 WHERE name = 'write_mode'",
            "INSERT INTO session_variables
             (context_key, focus_id, name, value, written_by_run, created_at, updated_at)
             VALUES ('c2', 'f', 'write_mode', 'open', 'r', 't', 't')",
        ] {
            assert!(
                sqlx::query(stmt).execute(&mut conn).await.is_err(),
                "schema must refuse: {stmt}"
            );
        }
        assert_eq!(
            write_mode(&mut conn, CHAT).await.unwrap(),
            WriteMode::Suppressed
        );
    }

    #[tokio::test]
    async fn a_latched_chat_refuses_every_session_write_and_other_chats_are_unaffected() {
        let mut conn = mem_outputs_conn().await;
        put_session_variable(&mut conn, &w("rung", "1", "r", None))
            .await
            .unwrap();
        latch_write_suppressed(&mut conn, CHAT, FOCUS, "r")
            .await
            .unwrap();

        let before = count(&mut conn, "session_variables").await;
        for write in [w("rung", "2", "r", Some(1)), w("new_name", "x", "r", None)] {
            let err = put_session_variable(&mut conn, &write).await.unwrap_err();
            assert!(
                matches!(err, SessionStoreError::Latch(LatchError::Suppressed)),
                "{err:?}"
            );
        }
        // The latch is scoped to the chat, not to the Focus that declared it.
        let mut other_focus = w("new_name", "x", "r", None);
        other_focus.focus_id = "writing-assistant";
        assert!(matches!(
            put_session_variable(&mut conn, &other_focus)
                .await
                .unwrap_err(),
            SessionStoreError::Latch(LatchError::Suppressed)
        ));
        assert_eq!(
            count(&mut conn, "session_variables").await,
            before,
            "nothing was written"
        );
        let kept = get_session_variable(&mut conn, CHAT, FOCUS, "rung")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((kept.value.as_str(), kept.version), ("1", 1));

        // Reads still work, and another chat still writes.
        assert_eq!(
            list_session_variables(&mut conn, CHAT, FOCUS)
                .await
                .unwrap()
                .len(),
            1
        );
        let mut other_chat = w("rung", "5", "r", None);
        other_chat.context_key = "chat-2";
        put_session_variable(&mut conn, &other_chat).await.unwrap();
        assert_eq!(
            write_mode(&mut conn, "chat-2").await.unwrap(),
            WriteMode::Open
        );
    }

    #[tokio::test]
    async fn an_unreadable_latch_fails_closed_for_reads_and_writes() {
        let mut conn = mem_outputs_conn().await;
        put_session_variable(&mut conn, &w("rung", "1", "r", None))
            .await
            .unwrap();
        // Make the latch unreadable: the table it lives in is gone.
        sqlx::query("ALTER TABLE session_variables RENAME TO session_variables_gone")
            .execute(&mut conn)
            .await
            .unwrap();

        let mode = write_mode(&mut conn, CHAT).await;
        assert!(
            matches!(mode, Err(LatchError::Unreadable(_))),
            "must not default to open: {mode:?}"
        );

        let err = put_session_variable(&mut conn, &w("rung", "2", "r", Some(1)))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SessionStoreError::Latch(LatchError::Unreadable(_))),
            "{err:?}"
        );
        // Setting the latch also reports the failure instead of claiming success.
        assert!(latch_write_suppressed(&mut conn, CHAT, FOCUS, "r")
            .await
            .is_err());
        // The failed write left the connection clean (no open transaction).
        sqlx::query("BEGIN")
            .execute(&mut conn)
            .await
            .expect("no transaction left open");
        sqlx::query("ROLLBACK").execute(&mut conn).await.unwrap();
    }

    #[tokio::test]
    async fn a_refused_write_leaves_the_connection_clean_and_changes_nothing() {
        let mut conn = mem_outputs_conn().await;
        put_session_variable(&mut conn, &w("a", "1", "r", None))
            .await
            .unwrap();
        // A conflict is raised inside the write transaction, so it must be undone.
        let err = put_session_variable(&mut conn, &w("a", "2", "r2", Some(5))).await;
        assert!(matches!(
            err,
            Err(SessionStoreError::VersionConflict { .. })
        ));
        assert_eq!(count(&mut conn, "session_variables").await, 1);
        let kept = get_session_variable(&mut conn, CHAT, FOCUS, "a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(kept.value, "1");
        sqlx::query("BEGIN")
            .execute(&mut conn)
            .await
            .expect("no transaction left open");
        sqlx::query("ROLLBACK").execute(&mut conn).await.unwrap();
    }
}
