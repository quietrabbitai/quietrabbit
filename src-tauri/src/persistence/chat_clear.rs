// src-tauri/src/persistence/chat_clear.rs
//
// items.id=888 stage 6 (decisions.id=939 sections 6 and 7): clear_chat, the archive hook.
//
// commands::chats::archive_chat calls it, best effort, after the chat is archived. In ONE
// BEGIN IMMEDIATE transaction on the persona's outputs.db it removes what QR saved FOR the
// chat (context_key):
//   1. the chat's PARKED runs (awaiting_user, awaiting_schedule, paused) are cancelled
//      through the same guarded cancel the expiry sweep uses (output_store::
//      cancel_parked_in_tx, reason "chat archived"): their snapshots, park reason and a
//      pending step review go with them;
//   2. user_input and external_input become NULL on EVERY run of the chat (focus_runs.
//      context_key, outputs_019), whatever its status, a live one included;
//   3. the park reason of every run of the chat (run_park_store::clear_park);
//   4. the chat's plan approvals (plan_approval_store::delete_chat_in_tx); the memory home
//      is cleared by the caller (PlanMemory::clear_chat);
//   5. the chat's pass artifacts (pass_artifact_store::clear_chat_in_tx);
//   6. the reviews of the chat's ENDED runs (complete, failed, cancelled, after step 1:
//      step_review_store::delete_reviews_of_ended_chat_runs_in_tx, its one delete). A run
//      that has not ended keeps its reviews: review_prompt reuses an earlier one;
//   7. the chat's session variables (session_store::clear_chat_in_tx). THE ONE-TIME LATCH
//      goes too ONLY when no run of the chat is still in a non-terminal status after step
//      1 (running, initializing, awaiting_feedback, awaiting_extract_confirm): such a run
//      may still write, and the latch is what stops it. This is the only place the latch
//      is ever removed, other than the removal of the persona's database.
// Each delete lives in the store that owns its table. Nothing here is latch-gated: it only
// removes.
//
// NOT REACHED (stated limits): a run with no context_key (a row from before outputs_019, or
// a run with no chat); the reviews of a run that has not ended, and a latch kept for one;
// a live run is not stopped (no lease is bound to a chat), and whatever it writes after the
// clear stays (the 30-day sweep, items.id=888 stage 7, takes the session rows later); the Library's hand-off replies (durable user
// documents, items.id=387 ruling); chat messages (messages.db, the archive keeps them).

use sqlx::SqliteConnection;

/// The reason a parked run cancelled by the archive hook carries.
pub const CHAT_ARCHIVED: &str = "chat archived";

/// What clear_chat removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChatCleared {
    pub runs_cancelled: u64,
    pub inputs_cleared: u64,
    pub reviews_deleted: u64,
    /// A run of the chat had not ended, so the one-time latch was kept.
    pub latch_kept: bool,
}

/// Clear what QR saved for the chat (see the module comment).
pub async fn clear_chat(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    context_key: &str,
) -> Result<ChatCleared, crate::persistence::output_store::OutputStoreError> {
    let mut conn =
        crate::persistence::output_store::open_outputs_db(user_id, persona_id, key_hex).await?;
    clear_chat_on(&mut conn, context_key).await
}

/// The same, on a connection the caller holds (tests, and any caller that already has one).
pub async fn clear_chat_on(
    conn: &mut SqliteConnection,
    context_key: &str,
) -> Result<ChatCleared, crate::persistence::output_store::OutputStoreError> {
    use crate::persistence::output_store::OutputStoreError;
    crate::persistence::session_store::validate_context_key(context_key)
        .map_err(|e| OutputStoreError::Validation(e.to_string()))?;
    crate::persistence::session_store::begin_write(conn).await?;
    let result: Result<ChatCleared, OutputStoreError> = async {
        let runs: Vec<(String, String)> =
            sqlx::query_as("SELECT id, status FROM focus_runs WHERE context_key = ? ORDER BY id")
                .bind(context_key)
                .fetch_all(&mut *conn)
                .await?;
        let mut report = ChatCleared::default();
        // 1. Parked runs: the guarded cancel (snapshots, park, pending review).
        for (id, status) in &runs {
            if crate::conductor::run_retention::PARKED_STATUSES.contains(&status.as_str())
                && crate::persistence::output_store::cancel_parked_in_tx(conn, id, CHAT_ARCHIVED)
                    .await?
            {
                report.runs_cancelled += 1;
            }
        }
        // 2. Both inputs of every run of the chat.
        report.inputs_cleared = sqlx::query(
            "UPDATE focus_runs SET user_input = NULL, external_input = NULL
             WHERE context_key = ? AND (user_input IS NOT NULL OR external_input IS NOT NULL)",
        )
        .bind(context_key)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        // 3. The park reason of every run of the chat.
        for (id, _) in &runs {
            crate::persistence::run_park_store::clear_park(conn, id)
                .await
                .map_err(|e| OutputStoreError::Validation(e.to_string()))?;
        }
        // 4 to 7. Each store removes its own rows of the chat.
        crate::persistence::plan_approval_store::delete_chat_in_tx(conn, context_key)
            .await
            .map_err(|e| OutputStoreError::Validation(e.to_string()))?;
        crate::persistence::pass_artifact_store::clear_chat_in_tx(conn, context_key)
            .await
            .map_err(|e| OutputStoreError::Validation(e.to_string()))?;
        report.reviews_deleted =
            crate::persistence::step_review_store::delete_reviews_of_ended_chat_runs_in_tx(
                conn,
                context_key,
            )
            .await
            .map_err(|e| OutputStoreError::Validation(e.to_string()))?;
        let open_runs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM focus_runs
             WHERE context_key = ? AND status NOT IN ('complete', 'failed', 'cancelled')",
        )
        .bind(context_key)
        .fetch_one(&mut *conn)
        .await?;
        report.latch_kept = open_runs > 0;
        crate::persistence::session_store::clear_chat_in_tx(conn, context_key, report.latch_kept)
            .await
            .map_err(|e| OutputStoreError::Validation(e.to_string()))?;
        Ok(report)
    }
    .await;
    crate::persistence::session_store::finish_write(conn, result).await
}

#[cfg(test)]
#[path = "chat_clear_tests.rs"]
mod tests;
