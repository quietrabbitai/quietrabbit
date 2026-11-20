// src-tauri/src/persistence/store_sweep.rs
//
// items.id=888 stage 7 (decisions.id=939 sections 6 and 7): the 30-day sweep of the chat
// stores. One pass per persona in the hourly retention tick and in the login pass
// (conductor::run_retention, the supervised idiom of items.id=681), only while the owner's
// key is resident (decisions.id=847).
//
// WHAT. Rows not written for STORE_RETENTION_DAYS (run_retention.rs), by updated_at:
//   session_variables  (session_store::sweep_older_than_in_tx; idx_session_variables_updated_at)
//                      EXCEPT the one-time latch row (write_mode): it is never swept, so a
//                      one-time chat stays one-time; only the archive hook removes it;
//   pass_artifacts     (pass_artifact_store::sweep_older_than_in_tx; idx_pass_artifacts_updated_at);
//   plan_approvals     (plan_approval_store::sweep_older_than_in_tx; idx_plan_approvals_updated_at).
// Each delete lives in the store that owns its table. The cutoff is now minus the period in
// the writers' own text format (providers::utils::now, RFC 3339), so the comparison uses
// each table's updated_at index. The comparison is TEXTUAL, so what it promises rests on one
// fact: all three stores stamp updated_at through providers::utils::now() (RFC 3339, UTC).
// A value in any other format is outside what the sweep promises: text that sorts after the
// cutoff (a word) is kept, text that sorts before it (a number) is swept.
//
// FAILURE DIRECTION: suppression. The three deletes run in ONE BEGIN IMMEDIATE transaction;
// any error rolls all of them back, so a sweep that fails deletes nothing and the next pass
// tries again.
//
// NOT SWEPT HERE (stated): step_reviews (only the archive hook deletes them, for ended runs),
// run_parks (removed with their run's status), focus_runs and snapshots (items.id=681's
// sweep and the post-output settle). A chat that is still in use loses a value nobody wrote
// for 30 days; a parked run that pinned such a value is then refused on resume (stage 5).

use sqlx::SqliteConnection;

use crate::persistence::output_store::OutputStoreError;

/// What one persona's sweep removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StoreSweep {
    pub session_variables: u64,
    pub pass_artifacts: u64,
    pub plan_approvals: u64,
}

/// The cutoff text: rows with updated_at below it are swept.
pub fn cutoff(now: chrono::DateTime<chrono::Utc>) -> String {
    (now - chrono::Duration::days(crate::conductor::run_retention::STORE_RETENTION_DAYS))
        .to_rfc3339()
}

/// Sweep one persona's chat stores at `now`.
pub async fn sweep_stores(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<StoreSweep, OutputStoreError> {
    let mut conn =
        crate::persistence::output_store::open_outputs_db(user_id, persona_id, key_hex).await?;
    sweep_stores_on(&mut conn, now).await
}

/// The same on a connection the caller holds.
pub async fn sweep_stores_on(
    conn: &mut SqliteConnection,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<StoreSweep, OutputStoreError> {
    let before = cutoff(now);
    crate::persistence::session_store::begin_write(conn).await?;
    let result: Result<StoreSweep, OutputStoreError> = async {
        Ok(StoreSweep {
            session_variables: crate::persistence::session_store::sweep_older_than_in_tx(
                conn, &before,
            )
            .await?,
            pass_artifacts: crate::persistence::pass_artifact_store::sweep_older_than_in_tx(
                conn, &before,
            )
            .await
            .map_err(|e| OutputStoreError::Validation(e.to_string()))?,
            plan_approvals: crate::persistence::plan_approval_store::sweep_older_than_in_tx(
                conn, &before,
            )
            .await
            .map_err(|e| OutputStoreError::Validation(e.to_string()))?,
        })
    }
    .await;
    crate::persistence::session_store::finish_write(conn, result).await
}

#[cfg(test)]
#[path = "store_sweep_tests.rs"]
mod tests;
