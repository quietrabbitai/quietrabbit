// src-tauri/src/persistence/run_park_store.rs
//
// items.id=901 (decisions.id=942 point 4): the typed store for run_parks
// (schema/outputs_017.sql) in the persona's outputs.db. A run that parks (a resolver stop,
// a spend block, a plan stop, an outbound-review stop) leaves its reason here so a screen
// can read it with get_run_park, because RunStatusPayload carries none: the park lives only
// in FailureResult.metadata, which no IPC path carries to the frontend today.
//
// ONE ROW PER RUN, the latest park, replaced by a later one. The row is a registered code
// (spend::codes::Code, checked here against Code::ALL) and its structured parameters (opaque
// ids, counts and reason codes), never text. The stage that writes it (conductor::lifecycle,
// handle_step_failure) passes only the parameters a SpendBlock or a park result carries.
//
// THE LATCH. run_parks is written exactly when the run's own focus_runs row is written
// (lifecycle::write_focus_run_record writes that row in a latched chat too and suppresses
// only the run's input), so this store makes no write a latched chat does not already make.
// It therefore takes no latch check of its own. If a future change ever suppresses the run
// row, the park must not be written here but held in the run-scoped memory-only home
// (conductor::plan_memory) and read by get_run_park from there until logout. A test in the
// writing stage ties the two writes together.
//
// CONNECTIONS: every function takes a caller-supplied `&mut SqliteConnection` to the
// persona's outputs.db, opened by output_store::open_outputs_db_retrying.
//
// PRIVACY: the module logs nothing (a source scan pins it) and no error carries stored data.
//
// QUERY STYLE: runtime sqlx::query() only, never the query!() macros.

use sqlx::{Row, SqliteConnection};
use thiserror::Error;

use crate::spend::codes::Code;

/// Largest params_json in bytes. The schema CHECK repeats it.
pub const PARAMS_MAX_BYTES: usize = 4096;

const RUN_ID_MAX_CHARS: usize = 128;
const ACTION_ID_MAX_CHARS: usize = 256;

#[derive(Debug, Error)]
pub enum ParkStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Validation(String),
}

/// One stored park.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPark {
    pub run_id: String,
    pub action_id: String,
    /// A registered code string (Code::as_str).
    pub code: String,
    /// A JSON object of structured parameters.
    pub params_json: String,
    pub created_at: String,
}

/// One park write.
#[derive(Debug, Clone)]
pub struct ParkWrite<'a> {
    pub run_id: &'a str,
    pub action_id: &'a str,
    pub code: &'a str,
    pub params_json: &'a str,
}

fn validate_text(label: &str, s: &str, max_chars: usize) -> Result<(), ParkStoreError> {
    if s.is_empty() || s.chars().count() > max_chars || s.contains('\0') {
        return Err(ParkStoreError::Validation(format!(
            "{label} must be 1 to {max_chars} characters with no NUL"
        )));
    }
    Ok(())
}

/// Whether a code string is one the registry emits.
pub fn is_registered_code(code: &str) -> bool {
    Code::ALL.iter().any(|c| c.as_str() == code)
}

fn validate_write(write: &ParkWrite<'_>) -> Result<(), ParkStoreError> {
    validate_text("run_id", write.run_id, RUN_ID_MAX_CHARS)?;
    validate_text("action_id", write.action_id, ACTION_ID_MAX_CHARS)?;
    // Only a registered code is stored, so a free-text failure can never reach the table.
    if !is_registered_code(write.code) {
        return Err(ParkStoreError::Validation(
            "code is not a registered code".into(),
        ));
    }
    if write.params_json.len() > PARAMS_MAX_BYTES || write.params_json.contains('\0') {
        return Err(ParkStoreError::Validation(format!(
            "params must be at most {PARAMS_MAX_BYTES} bytes with no NUL"
        )));
    }
    match serde_json::from_str::<serde_json::Value>(write.params_json) {
        Ok(v) if v.is_object() => Ok(()),
        _ => Err(ParkStoreError::Validation(
            "params must be a JSON object".into(),
        )),
    }
}

/// Record the run's park, replacing an earlier one. THE ONLY WRITER of run_parks.
pub async fn put_park(
    conn: &mut SqliteConnection,
    write: &ParkWrite<'_>,
) -> Result<(), ParkStoreError> {
    validate_write(write)?;
    sqlx::query(
        "INSERT OR REPLACE INTO run_parks (run_id, action_id, code, params_json, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(write.run_id)
    .bind(write.action_id)
    .bind(write.code)
    .bind(write.params_json)
    .bind(crate::providers::utils::now())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The run's latest park, or None. A read error is an Err, never absent.
pub async fn get_park(
    conn: &mut SqliteConnection,
    run_id: &str,
) -> Result<Option<RunPark>, ParkStoreError> {
    validate_text("run_id", run_id, RUN_ID_MAX_CHARS)?;
    let row = sqlx::query(
        "SELECT run_id, action_id, code, params_json, created_at FROM run_parks WHERE run_id = ?",
    )
    .bind(run_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(RunPark {
            run_id: r.try_get("run_id")?,
            action_id: r.try_get("action_id")?,
            code: r.try_get("code")?,
            params_json: r.try_get("params_json")?,
            created_at: r.try_get("created_at")?,
        })
    })
    .transpose()
    .map_err(ParkStoreError::Database)
}

/// Remove the run's park (the run moved on). Returns whether there was one.
pub async fn clear_park(conn: &mut SqliteConnection, run_id: &str) -> Result<bool, ParkStoreError> {
    validate_text("run_id", run_id, RUN_ID_MAX_CHARS)?;
    let done = sqlx::query("DELETE FROM run_parks WHERE run_id = ?")
        .bind(run_id)
        .execute(&mut *conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::session_store::test_db::mem_outputs_conn;

    fn w<'a>(code: &'a str, params: &'a str) -> ParkWrite<'a> {
        ParkWrite {
            run_id: "run-1",
            action_id: "draft",
            code,
            params_json: params,
        }
    }

    #[tokio::test]
    async fn a_park_round_trips_and_a_later_park_replaces_it() {
        let mut conn = mem_outputs_conn().await;
        assert!(get_park(&mut conn, "run-1").await.unwrap().is_none());
        put_park(
            &mut conn,
            &w("resolver.too_weak", r#"{"required":"standard"}"#),
        )
        .await
        .unwrap();
        let got = get_park(&mut conn, "run-1").await.unwrap().unwrap();
        assert_eq!(got.code, "resolver.too_weak");
        assert_eq!(got.action_id, "draft");
        assert_eq!(got.params_json, r#"{"required":"standard"}"#);

        put_park(
            &mut conn,
            &w("step_review.blocked", r#"{"reason":"timeout"}"#),
        )
        .await
        .unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM run_parks")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(n, 1, "one row per run");
        assert_eq!(
            get_park(&mut conn, "run-1").await.unwrap().unwrap().code,
            "step_review.blocked"
        );
        assert!(clear_park(&mut conn, "run-1").await.unwrap());
        assert!(!clear_park(&mut conn, "run-1").await.unwrap());
    }

    #[tokio::test]
    async fn only_a_registered_code_and_an_object_are_stored() {
        let mut conn = mem_outputs_conn().await;
        for (label, write) in [
            ("free text", w("Something went wrong for Jane Doe", "{}")),
            ("empty code", w("", "{}")),
            ("array params", w("resolver.too_weak", "[]")),
            ("not json", w("resolver.too_weak", "nope")),
        ] {
            assert!(put_park(&mut conn, &write).await.is_err(), "{label}");
        }
        let long = format!(r#"{{"k":"{}"}}"#, "x".repeat(PARAMS_MAX_BYTES));
        assert!(put_park(&mut conn, &w("resolver.too_weak", &long))
            .await
            .is_err());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM run_parks")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn every_registry_code_is_storable_and_nothing_else() {
        for c in Code::ALL {
            assert!(is_registered_code(c.as_str()));
        }
        assert!(!is_registered_code("plan.unknown"));
    }

    #[tokio::test]
    async fn the_table_has_no_foreign_key_and_no_trigger() {
        let mut conn = mem_outputs_conn().await;
        let fks: Vec<(String,)> =
            sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_list('run_parks')")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert!(fks.is_empty());
        let triggers: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'run_parks'",
        )
        .fetch_all(&mut conn)
        .await
        .unwrap();
        assert!(triggers.is_empty());
    }

    /// The store logs nothing and is the only writer of run_parks.
    #[test]
    fn the_module_logs_nothing_and_is_the_only_writer() {
        let src = include_str!("run_park_store.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        for banned in [
            "log::",
            "println!",
            "eprintln!",
            "dbg!",
            "tracing::",
            "sqlx::query!(",
        ] {
            assert!(!code.contains(banned), "run_park_store uses {banned}");
        }
        assert_eq!(code.matches("INSERT OR REPLACE INTO run_parks").count(), 1);
        assert_eq!(code.matches("DELETE FROM run_parks").count(), 1);
    }
}
