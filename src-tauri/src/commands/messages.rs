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

use serde::Serialize;
use specta::Type;

use crate::commands::execution::{self, SubmitFocusRunRequest};
use crate::conductor::concurrency::ConductorScheduler;
use crate::persistence::{message_store, output_store};

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
fn build_conversation_prompt(history: &[message_store::MessageRecord]) -> String {
    let start = history.len().saturating_sub(HISTORY_WINDOW);
    let mut prompt = String::new();
    for m in &history[start..] {
        if m.content.is_empty() {
            continue;
        }
        let label = if m.sender == "user" { "User" } else { "Assistant" };
        prompt.push_str(label);
        prompt.push_str(": ");
        prompt.push_str(&m.content);
        prompt.push('\n');
    }
    prompt
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn list_messages(
    user_id: String,
    persona_id: String,
    key_hex: String,
    context_key: String,
) -> Result<Vec<MessageInfo>, String> {
    let records = message_store::list_messages(&user_id, &persona_id, &key_hex, &context_key)
        .await
        .map_err(|e| e.to_string())?;
    Ok(records.into_iter().map(to_message_info).collect())
}

#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
#[tauri::command]
#[specta::specta]
pub async fn send_message(
    app_handle: tauri::AppHandle,
    scheduler: tauri::State<'_, Arc<ConductorScheduler>>,
    user_id: String,
    persona_id: String,
    key_hex: String,
    context_key: String,
    content: String,
    focus_id: String,
    gate3_track: bool,
) -> Result<Vec<MessageInfo>, String> {
    // 1. Persist the user's turn.
    message_store::save_message(
        &user_id,
        &persona_id,
        &key_hex,
        &context_key,
        "user",
        &content,
        None,
        None,
    )
    .await
    .map_err(|e| e.to_string())?;

    // 2. Build the bounded conversation-history prefix (includes the turn
    // just saved above as the last entry).
    let history = message_store::list_messages(&user_id, &persona_id, &key_hex, &context_key)
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
        key_hex: key_hex.clone(),
        topic_id: None,
        confirmed_cross_persona_fact_ids: vec![],
    };
    let mut run = execution::load_and_authorize_run(app_handle, scheduler, request).await?;
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
        &key_hex,
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
    let bg_key_hex = key_hex.clone();
    let bg_run_id = run_id.clone();
    let bg_message_id = assistant_record.id.clone();
    tokio::spawn(async move {
        let _result = run.execute_full().await;
        match output_store::get_output_for_run(&bg_user_id, &bg_persona_id, &bg_key_hex, &bg_run_id)
            .await
        {
            Ok(Some(output)) => {
                if let Err(e) = message_store::update_message_content(
                    &bg_user_id,
                    &bg_persona_id,
                    &bg_key_hex,
                    &bg_message_id,
                    &output.content,
                )
                .await
                {
                    log::warn!("send_message: failed to backfill assistant message content: {e}");
                }
            }
            Ok(None) => {
                log::warn!(
                    "send_message: run {bg_run_id} finished but produced no output to backfill"
                );
            }
            Err(e) => {
                log::warn!("send_message: failed to fetch output for run {bg_run_id}: {e}");
            }
        }
    });

    // 6. Return the transcript as it stands now (includes the just-reserved,
    // still-empty assistant placeholder — the caller renders staged/final
    // content via the run-status-update listener and a later refetch).
    let transcript = message_store::list_messages(&user_id, &persona_id, &key_hex, &context_key)
        .await
        .map_err(|e| e.to_string())?;
    Ok(transcript.into_iter().map(to_message_info).collect())
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
    // Real-encrypted-path test for the list_messages IPC command itself
    // (thin wrapper -- worth confirming the DTO mapping end to end
    // through a real messages.db, same TestEnv shape as
    // commands::library's tests / persistence::message_store's tests).
    // -----------------------------------------------------------------

    use crate::test_support::ENV_MUTEX;

    const USER_ID: &str = "user-msgcmd-test";
    const PERSONA_ID: &str = "persona-msgcmd-test";
    const KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
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
        let lock = ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();

        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_messages_db(USER_ID, PERSONA_ID, KEY_HEX)
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
            KEY_HEX,
            "persona-hub-persona-1",
            "user",
            "hello",
            None,
            None,
        )
        .await
        .expect("save_message must succeed");

        let results = list_messages(
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            KEY_HEX.to_owned(),
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

        let results = list_messages(
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            KEY_HEX.to_owned(),
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
        };
        let info = to_message_info(record);
        assert_eq!(info.id, "id-1");
        assert_eq!(info.focus_run_id.as_deref(), Some("run-1"));
        assert_eq!(info.gate3_review_status.as_deref(), Some("drafted"));
    }
}
