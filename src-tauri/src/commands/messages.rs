// src-tauri/src/commands/messages.rs
//
// Group 14 — Messages/transcript. Commands: send_message, list_messages.
//
// Backs ChatPane.tsx, the real component behind MiddleZone's chatPane prop
// for both Persona hub chat and Tier3AccessPane's starter-drafting pane.
// list_messages doubles as "get transcript" -- a context_key-scoped fetch
// already is the transcript, so no separate command exists for it.
//
// send_message is where this store meets Focus execution: it persists the
// user's turn, builds a bounded conversation-history prefix (see
// build_conversation_prompt) so a stateless-per-call Tier2Provider still
// gets turn-to-turn continuity, starts a real Focus run via
// commands::execution::load_and_authorize_run (the same LOAD+AUTHORIZE core
// submit_focus_run uses), and — unlike submit_focus_run, which fires
// execute_full() and forgets it — keeps the run to await completion in its
// own background task, so the placeholder assistant message row it writes
// immediately can be backfilled with real content once generation finishes.
// Staged/incremental reveal while that's in flight is a frontend concern
// (ChatPane listens to run-status-update's step_content field); this file
// only owns the final persisted backfill.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::commands::execution::{self, SubmitFocusRunRequest};
use crate::conductor::concurrency::ConductorScheduler;
use crate::persistence::{message_store, output_store};

// ---------------------------------------------------------------------------
// Request DTO
// ---------------------------------------------------------------------------

/// Bundled to keep send_message's own parameter count under specta's 10-arg
/// SpectaFn ceiling once confirmed_cross_persona_fact_ids (decisions.id=815,
/// items.id=27) is added — the 4 Tauri-injected params (app_handle,
/// scheduler, pool, key_registry) plus 7 business fields would otherwise be
/// 11. Mirrors SubmitFocusRunRequest's existing convention of one request DTO
/// per command rather than a growing positional-arg list.
#[derive(Debug, Deserialize, Type)]
pub struct SendMessageRequest {
    pub user_id: String,
    pub persona_id: String,
    pub context_key: String,
    pub content: String,
    pub focus_id: String,
    pub gate3_track: bool,
    /// entity_facts.id values the user already confirmed this session, via
    /// the frontend's pre-send commands::consent::get_pending_cross_persona_
    /// confirmations() query + confirmation UI, BEFORE calling send_message.
    /// Threaded straight into SubmitFocusRunRequest — previously hardcoded to
    /// vec![] here, which silently omitted every legitimate cross-Persona
    /// export on every QR Chat message (decisions.id=815's rescoping of this
    /// item). No is_quick_ask branch: decisions.id=815 holds Quick Ask to the
    /// identical standard as a named Focus run.
    pub confirmed_cross_persona_fact_ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// Response DTO
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Type)]
pub struct MessageInfo {
    pub id: String,
    pub context_key: String,
    pub sender: String,
    pub content: String,
    pub focus_run_id: Option<String>,
    pub gate3_review_status: Option<String>,
    pub created_at: String,
}

/// Push event payload for "message-content-ready" (items.id=320). Emitted
/// once, unconditionally, after send_message's background backfill task has
/// finished attempting to write real content into the placeholder assistant
/// row -- regardless of which branch fired (success, crisis block, Tier
/// 3/gate3 draft, or genuinely nothing to backfill). This is the only
/// reliable signal that a re-fetch via list_messages will see the real,
/// final content: every run-status-update status (including
/// "awaiting_feedback") is emitted from inside execute_full_inner(), which
/// completes and returns well before this file's backfill even starts.
#[derive(Debug, Clone, Serialize, Type)]
pub struct MessageContentReadyPayload {
    pub focus_run_id: String,
    pub message_id: String,
}

fn to_message_info(r: message_store::MessageRecord) -> MessageInfo {
    MessageInfo {
        id: r.id,
        context_key: r.context_key,
        sender: r.sender,
        content: r.content,
        focus_run_id: r.focus_run_id,
        gate3_review_status: r.gate3_review_status,
        created_at: r.created_at,
    }
}

// ---------------------------------------------------------------------------
// Conversation-history prefix
// ---------------------------------------------------------------------------

/// How many recent messages to fold into a send's user_input as context.
/// Tier2Provider is single-request/stateless -- no multi-turn state, no
/// tools, no memory (providers/tier2_base.rs) -- so turn-to-turn continuity
/// has to be threaded through the one prompt string each call gets, not
/// through the provider. Flat concatenation, bounded window: no
/// summarization or selective relevance, a real follow-up if this needs to
/// get smarter later.
const HISTORY_WINDOW: usize = 10;

/// Build the `User: ...\nAssistant: ...\n` prefix send_message passes as
/// SubmitFocusRunRequest.user_input, from the last HISTORY_WINDOW messages
/// in `history` (already includes the just-saved new user turn as the last
/// entry -- send_message calls this after save_message, not before).
///
/// user_input is opaque data substituted via a single str::replace() into
/// the {user_input} template token (executor.rs's token substitution) --
/// nothing further parses it, so plain-text prefixing here is safe.
///
/// Skips messages with empty content: an assistant placeholder row whose
/// generation hasn't finished/backfilled yet (see send_message) has nothing
/// useful to thread into context, and an empty "Assistant: \n" line would
/// just be noise.
/// Bounded, logged wrapper around message_store::update_message_content --
/// same rationale as lifecycle.rs's write_focus_run_record_logged (f615f8b),
/// applied to the one backfill write it doesn't cover: a stalled encrypted-DB
/// write here would otherwise silently block the "message-content-ready"
/// emit below forever. Non-fatal: logs and returns on both failure and
/// timeout, same as its lifecycle.rs sibling.
async fn update_message_content_logged(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    message_id: &str,
    content: &str,
) {
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        message_store::update_message_content(user_id, persona_id, key_hex, message_id, content),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::warn!("send_message: update_message_content failed (non-fatal): {e}"),
        Err(_) => {
            log::warn!("send_message: update_message_content timed out after 10s (non-fatal)")
        }
    }
}

fn build_conversation_prompt(history: &[message_store::MessageRecord]) -> String {
    let start = history.len().saturating_sub(HISTORY_WINDOW);
    let mut prompt = String::new();
    for m in &history[start..] {
        if m.content.is_empty() {
            continue;
        }
        let label = if m.sender == "user" {
            "User"
        } else {
            "Assistant"
        };
        prompt.push_str(label);
        prompt.push_str(": ");
        prompt.push_str(&m.content);
        prompt.push('\n');
    }
    prompt
}

/// R1 crisis-handling floor (items.id=297): the Ok(None) arm of send_message's
/// background backfill match (no saved `outputs` row -- true for every run
/// that paused or failed before output()) has only `execute_full()`'s
/// RunResult left to check for a crisis resource block. Pulled out as its own
/// pure function so this decision is directly unit-testable without spinning
/// up the full tokio::spawn/DB backfill path. Err(_) (execute_full() itself
/// failed) is treated the same as "no block" -- nothing to persist.
fn crisis_block_from_result(
    result: &Result<
        crate::conductor::lifecycle::RunResult,
        crate::conductor::lifecycle::LifecycleError,
    >,
) -> Option<&str> {
    result
        .as_ref()
        .ok()
        .and_then(|r| r.crisis_resource_block.as_deref())
}

/// items.id=317: same Ok(None) gap as crisis_block_from_result above, for the
/// ordinary (non-crisis) cloud_frontier pause -- the assistant placeholder's content
/// must be backfilled with the draft awaiting Gate3 review, or
/// request_tier3_gate3_review's content.is_empty() guard fails every time
/// (consent.rs). RunResult.output_content is only populated by lifecycle.rs
/// for a cloud_frontier boundary pause (status == "awaiting_user"); other paused/failed
/// statuses leave it None, so this stays a no-op for them.
fn draft_content_from_result(
    result: &Result<
        crate::conductor::lifecycle::RunResult,
        crate::conductor::lifecycle::LifecycleError,
    >,
) -> Option<&str> {
    result
        .as_ref()
        .ok()
        .filter(|r| r.status == "awaiting_user")
        .and_then(|r| r.output_content.as_deref())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn list_messages(
    user_id: String,
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
    context_key: String,
) -> Result<Vec<MessageInfo>, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let records = message_store::list_messages(&user_id, &persona_id, &key_hex_str, &context_key)
        .await
        .map_err(|e| e.to_string())?;
    Ok(records.into_iter().map(to_message_info).collect())
}

#[tauri::command]
#[specta::specta]
pub async fn send_message(
    app_handle: tauri::AppHandle,
    scheduler: tauri::State<'_, Arc<ConductorScheduler>>,
    pool: tauri::State<'_, sqlx::SqlitePool>,
    key_registry: State<'_, KeyRegistry>,
    request: SendMessageRequest,
) -> Result<Vec<MessageInfo>, String> {
    let SendMessageRequest {
        user_id,
        persona_id,
        context_key,
        content,
        focus_id,
        gate3_track,
        confirmed_cross_persona_fact_ids,
    } = request;

    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    // 1. Persist the user's turn.
    message_store::save_message(
        &user_id,
        &persona_id,
        &key_hex_str,
        &context_key,
        "user",
        &content,
        None,
        None,
    )
    .await
    .map_err(|e| e.to_string())?;

    // 1.5. R1 crisis-handling floor (decisions.id=607, items.id=265): local,
    // deterministic check on the raw fresh turn (`content`), NOT the blended
    // history window built in step 2 below -- detecting on the blended
    // window would re-fire the resource block on every following turn for
    // up to HISTORY_WINDOW messages after the actual disclosure, which is
    // exactly the "repeated check-in" behavior decisions.id=607 point 2
    // rules out.
    let crisis_detected = crate::conductor::crisis::detect(&content);

    // 2. Build the bounded conversation-history prefix (includes the turn
    // just saved above as the last entry).
    let history = message_store::list_messages(&user_id, &persona_id, &key_hex_str, &context_key)
        .await
        .map_err(|e| e.to_string())?;
    let user_input = build_conversation_prompt(&history);

    // 3. Start the real Focus run (LOAD + AUTHORIZE synchronously, same as
    // submit_focus_run), keeping ownership of `run` so step 5 can await its
    // completion.
    let request = SubmitFocusRunRequest {
        focus_id,
        user_input,
        user_id: user_id.clone(),
        persona_id: persona_id.clone(),
        topic_id: None,
        confirmed_cross_persona_fact_ids,
    };
    let mut run = execution::load_and_authorize_run(
        app_handle,
        scheduler,
        pool,
        key_hex_str.clone(),
        crisis_detected,
        request,
    )
    .await?;
    let run_id = run
        .focus_run_id
        .clone()
        .ok_or_else(|| "run_id not set after authorize".to_string())?;

    // 4. Reserve a placeholder assistant row now, so list_messages has
    // something to show (and Phase 3's staged reveal has a row to render
    // into) while generation is in flight.
    let gate3_review_status = if gate3_track { Some("drafted") } else { None };
    let assistant_record = message_store::save_message(
        &user_id,
        &persona_id,
        &key_hex_str,
        &context_key,
        "assistant",
        "",
        Some(&run_id),
        gate3_review_status,
    )
    .await
    .map_err(|e| e.to_string())?;

    // 5. Await completion in the background and backfill the placeholder's
    // content once the run's real output exists. Mirrors execute_full()'s
    // own "failures logged, not panicking" convention (execution.rs) --
    // errors here are lost sends, not crashes.
    let bg_user_id = user_id.clone();
    let bg_persona_id = persona_id.clone();
    let bg_key_hex = key_hex_str.clone();
    let bg_run_id = run_id.clone();
    let bg_message_id = assistant_record.id.clone();
    tokio::spawn(async move {
        let result = run.execute_full().await;
        match output_store::get_output_for_run(&bg_user_id, &bg_persona_id, &bg_key_hex, &bg_run_id)
            .await
        {
            Ok(Some(output)) => {
                // content is only NULL for an ingested-document row
                // (items.id=383) -- a Focus run's own output always has
                // real text content, so this is never actually reached here.
                update_message_content_logged(
                    &bg_user_id,
                    &bg_persona_id,
                    &bg_key_hex,
                    &bg_message_id,
                    output.content.as_deref().unwrap_or_default(),
                )
                .await;
            }
            Ok(None) => {
                // No saved `outputs` row -- true for every run that paused or
                // failed before reaching output() (cloud_frontier, consent gates,
                // Gate3 review, step failure). R1 crisis-handling floor
                // (items.id=297): if the run was crisis-flagged, persist the
                // resource block into the placeholder now, so it survives a
                // reload/reopen even if the live "run-status-update" event
                // that also carries it was missed by the frontend -- this
                // takes priority over an ordinary draft backfill below.
                // items.id=317: otherwise, an ordinary cloud_frontier pause backfills
                // the draft awaiting Gate3 review, so
                // request_tier3_gate3_review's content.is_empty() guard
                // (consent.rs) doesn't fail on every gate3_track message.
                // Any other paused/failed status keeps prior behavior -- the
                // placeholder stays empty, just logged.
                if let Some(block) = crisis_block_from_result(&result) {
                    update_message_content_logged(
                        &bg_user_id,
                        &bg_persona_id,
                        &bg_key_hex,
                        &bg_message_id,
                        block,
                    )
                    .await;
                } else if let Some(draft) = draft_content_from_result(&result) {
                    update_message_content_logged(
                        &bg_user_id,
                        &bg_persona_id,
                        &bg_key_hex,
                        &bg_message_id,
                        draft,
                    )
                    .await;
                } else {
                    log::warn!(
                        "send_message: run {bg_run_id} finished but produced no output to backfill"
                    );
                }
            }
            Err(e) => {
                log::warn!("send_message: failed to fetch output for run {bg_run_id}: {e}");
            }
        }

        // items.id=320: the sole reliable "safe to re-fetch now" signal --
        // every run-status-update status above (including
        // "awaiting_feedback") was already emitted from inside
        // execute_full_inner()/cleanup(), before this backfill attempt even
        // started. Fires unconditionally, whichever branch above ran,
        // including the genuine-no-output case: ChatPane still needs to know
        // the backfill attempt is over so it can stop waiting.
        if let Some(handle) = &run.app_handle {
            use tauri::Emitter;
            let payload = MessageContentReadyPayload {
                focus_run_id: bg_run_id.clone(),
                message_id: bg_message_id.clone(),
            };
            if let Err(e) = handle.emit("message-content-ready", &payload) {
                log::warn!("send_message: emit message-content-ready failed: {e}");
            }
        }
    });

    // 6. Return the transcript as it stands now (includes the just-reserved,
    // still-empty assistant placeholder — the caller renders staged/final
    // content via the run-status-update listener and a later refetch).
    let transcript =
        message_store::list_messages(&user_id, &persona_id, &key_hex_str, &context_key)
            .await
            .map_err(|e| e.to_string())?;
    Ok(transcript.into_iter().map(to_message_info).collect())
}

// ---------------------------------------------------------------------------
// DIAG_329 -- dev-only test scaffolding (items.id=329)
// ---------------------------------------------------------------------------

/// TEMPORARY dev-only test scaffolding (items.id=329, DIAG_329). Seeds a
/// synthetic "drafted" assistant message directly, skipping real Focus-run
/// execution/model generation, so a debug build can jump straight into
/// request_tier3_gate3_review without the several-seconds-per-iteration
/// manual type-a-message/wait-for-the-model dance. The seeded row still goes
/// through the REAL request_tier3_gate3_review -> gate3() path unmodified --
/// this only fabricates the drafted input Gate3 reviews, not Gate3's own
/// approve/deny decision or the tier-ceiling check ahead of it.
///
/// focus_run_id is a fresh synthetic id, not a real focus_runs.id -- both
/// places that store it (messages.focus_run_id, disclosure_log.focus_run_id)
/// are plain TEXT columns with no FK, and live in different SQLite files
/// than focus_runs anyway, so this is safe (confirmed 2026-08-28).
///
/// #[cfg(debug_assertions)]: compiled only into debug builds -- absent
/// entirely from a release binary, not just unreachable. See ipc.rs's
/// specta_builder for the matching debug-only command registration; both
/// halves must be removed together once items.id=329's Cloud Chat pane work no
/// longer needs fast iteration.
#[cfg(debug_assertions)]
#[tauri::command]
#[specta::specta]
pub async fn dev_seed_tier3_draft_message(
    key_registry: State<'_, KeyRegistry>,
    user_id: String,
    persona_id: String,
    context_key: String,
) -> Result<String, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let synthetic_run_id = format!("dev-seed-{}", uuid::Uuid::new_v4());
    let record = message_store::save_message(
        &user_id,
        &persona_id,
        &key_hex_str,
        &context_key,
        "assistant",
        "[DIAG_329 dev-seeded draft] Synthetic Tier 3 starter message for \
         pane-testing -- not real model output.",
        Some(&synthetic_run_id),
        Some("drafted"),
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(record.id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // build_conversation_prompt is a pure function -- no IO, no Tauri state --
    // so it's tested directly. send_message's own Focus-run integration
    // (load_and_authorize_run -> execute_full -> backfill) is not
    // unit-tested here: it requires the same live executor/provider test
    // infrastructure commands::execution::submit_focus_run itself has none
    // of today (execution.rs has zero tests). That path is exercised
    // instead via a real dev-server run against a provisioned key_hex, per
    // this item's verification pass, not skipped.

    fn msg(sender: &str, content: &str) -> message_store::MessageRecord {
        message_store::MessageRecord {
            id: uuid::Uuid::new_v4().to_string(),
            context_key: "ctx-1".to_owned(),
            sender: sender.to_owned(),
            content: content.to_owned(),
            focus_run_id: None,
            gate3_review_status: None,
            created_at: "2026-08-09T00:00:00Z".to_owned(),
            reviewed_at_risk_rating: None,
        }
    }

    #[test]
    fn build_conversation_prompt_formats_user_and_assistant_lines() {
        let history = vec![msg("user", "hi"), msg("assistant", "hello there")];
        let prompt = build_conversation_prompt(&history);
        assert_eq!(prompt, "User: hi\nAssistant: hello there\n");
    }

    #[test]
    fn build_conversation_prompt_skips_empty_placeholder_rows() {
        let history = vec![
            msg("user", "hi"),
            msg("assistant", ""), // not-yet-generated placeholder
            msg("user", "still there?"),
        ];
        let prompt = build_conversation_prompt(&history);
        assert_eq!(prompt, "User: hi\nUser: still there?\n");
    }

    #[test]
    fn build_conversation_prompt_bounds_to_the_last_ten_messages() {
        let history: Vec<message_store::MessageRecord> = (0..15)
            .map(|i| msg("user", &format!("message {i}")))
            .collect();
        let prompt = build_conversation_prompt(&history);
        let line_count = prompt.lines().count();
        assert_eq!(line_count, 10, "must bound to HISTORY_WINDOW messages");
        assert!(
            prompt.starts_with("User: message 5\n"),
            "must keep the most recent 10, not the earliest: {prompt}"
        );
        assert!(prompt.contains("User: message 14\n"));
    }

    #[test]
    fn build_conversation_prompt_on_empty_history_is_empty_string() {
        assert_eq!(build_conversation_prompt(&[]), "");
    }

    // -----------------------------------------------------------------
    // crisis_block_from_result (items.id=297) -- also a pure function, same
    // rationale as build_conversation_prompt above.
    // -----------------------------------------------------------------

    fn run_result(crisis_resource_block: Option<&str>) -> crate::conductor::lifecycle::RunResult {
        crate::conductor::lifecycle::RunResult {
            focus_run_id: "run-1".to_owned(),
            status: "awaiting_user".to_owned(),
            output_id: None,
            output_content: None,
            failure: None,
            crisis_resource_block: crisis_resource_block.map(|s| s.to_owned()),
        }
    }

    #[test]
    fn crisis_block_from_result_present_when_run_was_crisis_flagged() {
        let result = Ok(run_result(Some("call 988")));
        assert_eq!(crisis_block_from_result(&result), Some("call 988"));
    }

    #[test]
    fn crisis_block_from_result_absent_for_an_ordinary_pause_or_failure() {
        let result = Ok(run_result(None));
        assert_eq!(crisis_block_from_result(&result), None);
    }

    #[test]
    fn crisis_block_from_result_absent_when_execute_full_itself_errored() {
        let result: Result<_, crate::conductor::lifecycle::LifecycleError> = Err(
            crate::conductor::lifecycle::LifecycleError::FocusNotFound("quick-ask".to_owned()),
        );
        assert_eq!(crisis_block_from_result(&result), None);
    }

    // -----------------------------------------------------------------
    // draft_content_from_result (items.id=317) -- same pure-function
    // rationale as crisis_block_from_result above.
    // -----------------------------------------------------------------

    fn run_result_awaiting_user(
        output_content: Option<&str>,
    ) -> crate::conductor::lifecycle::RunResult {
        crate::conductor::lifecycle::RunResult {
            focus_run_id: "run-1".to_owned(),
            status: "awaiting_user".to_owned(),
            output_id: None,
            output_content: output_content.map(|s| s.to_owned()),
            failure: None,
            crisis_resource_block: None,
        }
    }

    #[test]
    fn draft_content_from_result_present_for_an_awaiting_user_pause_with_output() {
        let result = Ok(run_result_awaiting_user(Some("the draft text")));
        assert_eq!(draft_content_from_result(&result), Some("the draft text"));
    }

    #[test]
    fn draft_content_from_result_absent_when_no_prior_step_output_exists() {
        let result = Ok(run_result_awaiting_user(None));
        assert_eq!(draft_content_from_result(&result), None);
    }

    #[test]
    fn draft_content_from_result_absent_for_a_non_awaiting_user_status() {
        let mut r = run_result_awaiting_user(Some("should not surface"));
        r.status = "failed".to_owned();
        let result = Ok(r);
        assert_eq!(draft_content_from_result(&result), None);
    }

    #[test]
    fn draft_content_from_result_absent_when_execute_full_itself_errored() {
        let result: Result<_, crate::conductor::lifecycle::LifecycleError> = Err(
            crate::conductor::lifecycle::LifecycleError::FocusNotFound("quick-ask".to_owned()),
        );
        assert_eq!(draft_content_from_result(&result), None);
    }

    // -----------------------------------------------------------------
    // Real-encrypted-path test for the list_messages IPC command itself
    // (thin wrapper -- worth confirming the DTO mapping end to end
    // through a real messages.db, same TestEnv shape as
    // commands::library's tests / persistence::message_store's tests).
    // -----------------------------------------------------------------

    use crate::test_support::{mock_app_with_registry, populate_registry, ENV_MUTEX};
    use tauri::Manager;

    const USER_ID: &str = "user-msgcmd-test";
    const PERSONA_ID: &str = "persona-msgcmd-test";
    const MASTER_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
        [0xEFu8; crate::auth::kdf::MASTER_KEY_LEN];

    fn key_hex_str() -> String {
        key_hex(&MASTER_KEY)
    }

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn setup() -> TestEnv {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();

        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_messages_db(USER_ID, PERSONA_ID, &key_hex_str())
            .await
            .expect("messages.db migration must succeed in test setup");

        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
        }
    }

    #[tokio::test]
    async fn list_messages_command_returns_saved_messages_as_message_info() {
        let _env = setup().await;

        message_store::save_message(
            USER_ID,
            PERSONA_ID,
            &key_hex_str(),
            "persona-hub-persona-1",
            "user",
            "hello",
            None,
            None,
        )
        .await
        .expect("save_message must succeed");

        let app = mock_app_with_registry(sqlx::SqlitePool::connect_lazy_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(":memory:"),
        ));
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, USER_ID, MASTER_KEY).await;

        let results = list_messages(
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            registry,
            "persona-hub-persona-1".to_owned(),
        )
        .await
        .expect("list_messages command must succeed");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].sender, "user");
        assert_eq!(results[0].content, "hello");
    }

    #[tokio::test]
    async fn list_messages_command_returns_empty_vec_for_unknown_context_key() {
        let _env = setup().await;

        let app = mock_app_with_registry(sqlx::SqlitePool::connect_lazy_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(":memory:"),
        ));
        let registry = app.state::<KeyRegistry>();
        populate_registry(&registry, USER_ID, MASTER_KEY).await;

        let results = list_messages(
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            registry,
            "never-sent-to".to_owned(),
        )
        .await
        .expect("list_messages command must succeed");

        assert!(results.is_empty());
    }

    #[test]
    fn to_message_info_maps_every_field() {
        let record = message_store::MessageRecord {
            id: "id-1".to_owned(),
            context_key: "ctx-1".to_owned(),
            sender: "assistant".to_owned(),
            content: "drafted text".to_owned(),
            focus_run_id: Some("run-1".to_owned()),
            gate3_review_status: Some("drafted".to_owned()),
            created_at: "2026-08-09T00:00:00Z".to_owned(),
            reviewed_at_risk_rating: None,
        };
        let info = to_message_info(record);
        assert_eq!(info.id, "id-1");
        assert_eq!(info.focus_run_id.as_deref(), Some("run-1"));
        assert_eq!(info.gate3_review_status.as_deref(), Some("drafted"));
    }
}
