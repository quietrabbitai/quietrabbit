// src-tauri/src/commands/messages.rs
//
// Group 14 — Messages/transcript. Commands: send_message, list_messages.
//
// Backs ChatPane.tsx, the real component behind MiddleZone's chatPane prop
// for both Persona hub chat and CloudChatAccessPane's starter-drafting pane.
// list_messages doubles as "get transcript" -- a context_key-scoped fetch
// already is the transcript, so no separate command exists for it.
//
// send_message is where this store meets Focus execution: it persists the
// user's turn, builds a bounded conversation-history prefix (see
// build_conversation_prompt) so a stateless-per-call QrHostedProvider still
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
use crate::conductor::lifecycle::{LifecycleError, RunResult};
use crate::persistence::{chat_store, message_store, output_store};

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

/// Push event payload for "chat-activity-updated" (items.id=546). Emitted
/// once, right after send_message's call to
/// chat_store::ensure_chat_and_bump_activity successfully creates or bumps
/// a real `chats` row for the message just sent -- never for a
/// context_key with no backing chats row (the legacy flat
/// "tier3-access-{persona_id}" pseudo-conversation). HistoryScreen's
/// ChatHistoryAction listens for this to refresh its chat list live,
/// without needing to remount.
#[derive(Debug, Clone, Serialize, Type)]
pub struct ChatActivityUpdatedPayload {
    pub persona_id: String,
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
/// QrHostedProvider is single-request/stateless -- no multi-turn state, no
/// tools, no memory (providers/qr_hosted_base.rs) -- so turn-to-turn continuity
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
    is_error: bool,
) {
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        message_store::update_message_content(
            user_id, persona_id, key_hex, message_id, content, is_error,
        ),
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

/// items.id=546: auto-title candidate for a chat's first message, derived
/// from the raw user turn -- whitespace-collapsed to a single line, bounded
/// so a long first message doesn't blow out History's row layout. None for
/// empty/whitespace-only content, so ensure_chat_and_bump_activity's
/// `COALESCE(chats.title, excluded.title)` leaves title NULL
/// (HistoryScreen falls back to its own "untitled" string) rather than
/// persisting an empty title forever.
const CHAT_TITLE_MAX_CHARS: usize = 60;

fn derive_chat_title(content: &str) -> Option<String> {
    let collapsed: String = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() > CHAT_TITLE_MAX_CHARS {
        let truncated: String = collapsed.chars().take(CHAT_TITLE_MAX_CHARS).collect();
        Some(format!("{truncated}…"))
    } else {
        Some(collapsed)
    }
}

fn build_conversation_prompt(history: &[message_store::MessageRecord]) -> String {
    let start = history.len().saturating_sub(HISTORY_WINDOW);
    let mut prompt = String::new();
    for m in &history[start..] {
        // items.id=587: an is_error row is a plain-language failure message
        // shown to the user, never something the model actually said --
        // replaying it back as an "Assistant:" turn would be fabricating
        // history, not describing it.
        if m.content.is_empty() || m.is_error {
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
/// request_cloud_frontier_gate3_review's content.is_empty() guard fails every time
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

/// items.id=587: the Ok(None) arm's last real source of a human-readable
/// outcome -- every `handle_step_failure()` exit (lifecycle.rs) populates
/// `RunResult.failure` with a `FailureResult` carrying a plain_language
/// string (OllamaUnavailable's "The local AI isn't responding...", the new
/// OllamaModelMissing's "...local models aren't installed...", context-
/// exceeded, privacy blocks, an exhausted-retry escalation, etc.) --
/// send_message never read it before this item, so any run that failed
/// without ever reaching output() left its placeholder silently empty. Same
/// pure-function/directly-unit-testable shape as crisis_block_from_result/
/// draft_content_from_result above.
fn failure_message_from_result(
    result: &Result<
        crate::conductor::lifecycle::RunResult,
        crate::conductor::lifecycle::LifecycleError,
    >,
) -> Option<&str> {
    result
        .as_ref()
        .ok()
        .and_then(|r| r.failure.as_ref())
        .map(|f| f.plain_language.as_str())
}

/// items.id=587: the last-resort fallback for the one case
/// failure_message_from_result can't cover -- `execute_full()`/
/// `resume_execution()` returning `Err(LifecycleError)` directly (a
/// LifecycleError variant execute_full()'s own F_SYSTEM catch didn't wrap
/// into a FailureResult; see that function's doc comment). Rare, but
/// previously left the placeholder empty with only a log line -- this is
/// deliberately generic rather than echoing the raw LifecycleError string,
/// which is a developer-facing message, not a user-facing one.
const GENERIC_FAILURE_MESSAGE: &str =
    "Quiet Rabbit ran into an unexpected problem and couldn't finish that reply. [Try again] [Get help]";

// ---------------------------------------------------------------------------
// finalize_chat_reply -- the single point every run completion/resumption
// path passes through (items.id=587)
// ---------------------------------------------------------------------------

/// What `finalize_chat_reply` actually did -- lets each of its three callers
/// (send_message's own backfill, resume_run, scheduled_sweep.rs) decide
/// whether to emit `chat-activity-updated` without needing to re-derive the
/// same "was this a real reply, on a chat-shaped context_key" logic
/// themselves, and gives tests something concrete to assert on without
/// spying on a real Tauri `emit()` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyOutcome {
    /// A real reply (success output, crisis block, or an awaiting_user
    /// Gate3 draft) was written, and it belongs to a "chat-" context_key --
    /// `ensure_chat_and_bump_activity` created or bumped a row. Caller
    /// should emit `chat-activity-updated`.
    ChatFiled,
    /// A real reply was written, but its context_key isn't chat-shaped
    /// (the flat tier3-access-*/persona-hub-* pseudo-conversations) --
    /// `ensure_chat_and_bump_activity` is a documented no-op there
    /// (items.id=501's boundary). No History emit.
    RealReplyNoChat,
    /// A plain-language failure/error message was written (`is_error=1`).
    /// Per decisions.id=844, this never files or bumps a chats row.
    ErrorShown,
    /// Nothing was written -- a status this function doesn't have copy for
    /// (e.g. a hypothetical "cancelled" with no failure attached). The
    /// placeholder stays empty, same as before this item.
    NothingToBackfill,
    /// `run_id` has no owning assistant message row at all -- a
    /// submit_focus_run/Board-originated run, not a chat send. Safe no-op;
    /// this is what makes it safe to call this function from resume_run and
    /// scheduled_sweep.rs, which resume ANY paused run regardless of origin.
    NotAChatMessage,
}

/// Backfills the assistant placeholder owned by `run_id` (looked up by
/// focus_run_id, not passed in -- see find_assistant_message_by_focus_run_id's
/// own doc comment for why) with whatever `result` produced, and -- only for
/// a genuine reply -- files/bumps its owning chat in History
/// (decisions.id=844). Emits `message-content-ready` whenever a message row
/// was found at all (matching this event's original "fires unconditionally,
/// whichever branch ran" contract), and `chat-activity-updated` only when a
/// chats row was actually created or bumped.
///
/// Self-contained by design: every caller (send_message's background task,
/// resume_run, scheduled_sweep.rs) can call this with nothing but the
/// run_id and its own Result<RunResult, LifecycleError> -- none of them
/// need to separately track message_id/context_key/title_candidate across
/// a pause-and-resume boundary that may be a completely different command
/// invocation (and, for scheduled_sweep.rs, no user in the loop at all).
pub(crate) async fn finalize_chat_reply(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    run_id: &str,
    result: &Result<RunResult, LifecycleError>,
    app_handle: Option<&tauri::AppHandle>,
) -> ReplyOutcome {
    let msg = match message_store::find_assistant_message_by_focus_run_id(
        user_id, persona_id, key_hex, run_id,
    )
    .await
    {
        Ok(Some(m)) => m,
        Ok(None) => return ReplyOutcome::NotAChatMessage,
        Err(e) => {
            log::warn!(
                "finalize_chat_reply: find_assistant_message_by_focus_run_id failed for run {run_id}: {e}"
            );
            return ReplyOutcome::NotAChatMessage;
        }
    };

    let mut is_real_reply = false;
    let content: Option<String> =
        match output_store::get_output_for_run(user_id, persona_id, key_hex, run_id).await {
            Ok(Some(output)) => {
                // content is only NULL for an ingested-document row
                // (items.id=383) -- a Focus run's own output always has real
                // text content, so this default is never actually reached here.
                is_real_reply = true;
                Some(output.content.unwrap_or_default())
            }
            Ok(None) => {
                if let Some(block) = crisis_block_from_result(result) {
                    is_real_reply = true;
                    Some(block.to_owned())
                } else if let Some(draft) = draft_content_from_result(result) {
                    is_real_reply = true;
                    Some(draft.to_owned())
                } else if let Some(failure) = failure_message_from_result(result) {
                    Some(failure.to_owned())
                } else if let Err(e) = result {
                    log::warn!("finalize_chat_reply: run {run_id} failed unexpectedly: {e}");
                    Some(GENERIC_FAILURE_MESSAGE.to_owned())
                } else {
                    log::warn!(
                    "finalize_chat_reply: run {run_id} finished but produced no output to backfill"
                );
                    None
                }
            }
            Err(e) => {
                log::warn!("finalize_chat_reply: failed to fetch output for run {run_id}: {e}");
                None
            }
        };

    if let Some(c) = &content {
        update_message_content_logged(user_id, persona_id, key_hex, &msg.id, c, !is_real_reply)
            .await;
    }

    let outcome = if is_real_reply {
        // items.id=587 (title candidate): derived from the context_key's
        // first stored user message, not from whatever text the triggering
        // send happened to have in scope -- the only way a first reply that
        // completes via resume_run (a different command invocation
        // entirely, with no access to the original send's `content`) still
        // gets a correct title. Harmless/ignored on every later call for
        // the same context_key: ensure_chat_and_bump_activity's own
        // COALESCE keeps whichever title won on the row-creating call.
        let title_candidate =
            message_store::list_messages(user_id, persona_id, key_hex, &msg.context_key)
                .await
                .ok()
                .and_then(|msgs| msgs.into_iter().find(|m| m.sender == "user"))
                .and_then(|m| derive_chat_title(&m.content));

        match chat_store::ensure_chat_and_bump_activity(
            user_id,
            persona_id,
            key_hex,
            &msg.context_key,
            title_candidate.as_deref(),
        )
        .await
        {
            Ok(true) => ReplyOutcome::ChatFiled,
            Ok(false) => ReplyOutcome::RealReplyNoChat,
            Err(e) => {
                log::warn!(
                    "finalize_chat_reply: ensure_chat_and_bump_activity failed (non-fatal): {e}"
                );
                ReplyOutcome::RealReplyNoChat
            }
        }
    } else if content.is_some() {
        ReplyOutcome::ErrorShown
    } else {
        ReplyOutcome::NothingToBackfill
    };

    if let Some(handle) = app_handle {
        use tauri::Emitter;
        if outcome == ReplyOutcome::ChatFiled {
            let payload = ChatActivityUpdatedPayload {
                persona_id: persona_id.to_owned(),
            };
            if let Err(e) = handle.emit("chat-activity-updated", &payload) {
                log::warn!("finalize_chat_reply: emit chat-activity-updated failed: {e}");
            }
        }
        let payload = MessageContentReadyPayload {
            focus_run_id: run_id.to_owned(),
            message_id: msg.id.clone(),
        };
        if let Err(e) = handle.emit("message-content-ready", &payload) {
            log::warn!("finalize_chat_reply: emit message-content-ready failed: {e}");
        }
    }

    outcome
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

    // 1. Persist the user's turn. items.id=587 (decisions.id=844): the
    // owning chat row is no longer created/bumped here -- that now happens
    // only once a real reply is actually saved, in finalize_chat_reply
    // below (step 5), so a run that fails before ever producing a reply
    // leaves no History entry for it.
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
    // into) while generation is in flight. Its own id isn't needed beyond
    // this point -- finalize_chat_reply (step 5) finds this row again by
    // focus_run_id, not by id.
    let gate3_review_status = if gate3_track { Some("drafted") } else { None };
    message_store::save_message(
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

    // 5. Await completion in the background, then hand off to
    // finalize_chat_reply -- the single point every run completion or
    // resumption path (this one, resume_run, scheduled_sweep.rs) passes
    // through to backfill the placeholder and, only for a genuine reply,
    // file/bump the owning chat in History (items.id=587). Mirrors
    // execute_full()'s own "failures logged, not panicking" convention
    // (execution.rs) -- errors here are lost sends, not crashes.
    let bg_user_id = user_id.clone();
    let bg_persona_id = persona_id.clone();
    let bg_key_hex = key_hex_str.clone();
    let bg_run_id = run_id.clone();
    tokio::spawn(async move {
        let result = run.execute_full().await;
        finalize_chat_reply(
            &bg_user_id,
            &bg_persona_id,
            &bg_key_hex,
            &bg_run_id,
            &result,
            run.app_handle.as_ref(),
        )
        .await;
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
            is_error: false,
        }
    }

    fn error_msg(content: &str) -> message_store::MessageRecord {
        message_store::MessageRecord {
            is_error: true,
            ..msg("assistant", content)
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
    fn build_conversation_prompt_skips_error_rows() {
        let history = vec![
            msg("user", "hi"),
            error_msg("Quiet Rabbit's local models aren't installed yet."),
            msg("user", "still there?"),
        ];
        let prompt = build_conversation_prompt(&history);
        assert_eq!(
            prompt, "User: hi\nUser: still there?\n",
            "an is_error row must never be replayed back as a real Assistant: turn"
        );
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
    // failure_message_from_result (items.id=587) -- same pure-function
    // rationale as crisis_block_from_result/draft_content_from_result above.
    // -----------------------------------------------------------------

    fn run_result_with_failure(plain_language: &str) -> crate::conductor::lifecycle::RunResult {
        crate::conductor::lifecycle::RunResult {
            focus_run_id: "run-1".to_owned(),
            status: "failed".to_owned(),
            output_id: None,
            output_content: None,
            failure: Some(crate::conductor::failure::FailureResult {
                action: crate::conductor::failure::FailureAction::Stop,
                failure_mode: Some("F1".to_owned()),
                plain_language: plain_language.to_owned(),
                is_recoverable: false,
                severity: crate::conductor::failure::FailureSeverity::Stop,
                step_id: None,
                focus_id: None,
                metadata: None,
            }),
            crisis_resource_block: None,
        }
    }

    #[test]
    fn failure_message_from_result_present_for_a_failed_run() {
        let result = Ok(run_result_with_failure(
            "Quiet Rabbit's local models aren't installed yet. [Get help]",
        ));
        assert_eq!(
            failure_message_from_result(&result),
            Some("Quiet Rabbit's local models aren't installed yet. [Get help]")
        );
    }

    #[test]
    fn failure_message_from_result_absent_when_no_failure_is_attached() {
        let result = Ok(run_result_awaiting_user(None));
        assert_eq!(failure_message_from_result(&result), None);
    }

    #[test]
    fn failure_message_from_result_absent_when_execute_full_itself_errored() {
        let result: Result<_, crate::conductor::lifecycle::LifecycleError> = Err(
            crate::conductor::lifecycle::LifecycleError::FocusNotFound("quick-ask".to_owned()),
        );
        assert_eq!(failure_message_from_result(&result), None);
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

    // -----------------------------------------------------------------
    // finalize_chat_reply (items.id=587) -- integration-style, against a
    // real temp messages.db with the real message_store/chat_store, not
    // mocked -- per this item's own requirement that the chats-row-timing
    // behavior (no row on failure, one row on success, bump on a
    // follow-up) be verified against real DB state, not just asserted on
    // a pure decision function. No Tauri `emit()` is exercised here
    // (app_handle: None) -- this codebase has no existing pattern for
    // asserting a real emitted event, so the returned ReplyOutcome is
    // what's asserted instead: a real caller only emits
    // chat-activity-updated when it sees ReplyOutcome::ChatFiled, so
    // asserting the outcome directly tests the condition that gates it.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn finalize_chat_reply_on_failure_leaves_no_chats_row_and_marks_is_error() {
        let _env = setup().await;
        let key = key_hex_str();

        message_store::save_message(
            USER_ID,
            PERSONA_ID,
            &key,
            "chat-fail-test",
            "user",
            "hi",
            None,
            None,
        )
        .await
        .expect("save_message (user) must succeed");
        message_store::save_message(
            USER_ID,
            PERSONA_ID,
            &key,
            "chat-fail-test",
            "assistant",
            "",
            Some("run-fail-1"),
            None,
        )
        .await
        .expect("save_message (assistant placeholder) must succeed");

        let result = Ok(run_result_with_failure(
            "Quiet Rabbit's local models aren't installed yet. [Get help]",
        ));

        let outcome =
            finalize_chat_reply(USER_ID, PERSONA_ID, &key, "run-fail-1", &result, None).await;

        assert_eq!(outcome, ReplyOutcome::ErrorShown);

        let chats = chat_store::list_chats(USER_ID, PERSONA_ID, &key)
            .await
            .expect("list_chats must succeed");
        assert!(chats.is_empty(), "a failed run must leave no chats row");

        let placeholder = message_store::find_assistant_message_by_focus_run_id(
            USER_ID,
            PERSONA_ID,
            &key,
            "run-fail-1",
        )
        .await
        .expect("find_assistant_message_by_focus_run_id must succeed")
        .expect("the placeholder row must still exist");
        assert_eq!(
            placeholder.content,
            "Quiet Rabbit's local models aren't installed yet. [Get help]"
        );
        assert!(
            placeholder.is_error,
            "the backfilled error text must be flagged is_error"
        );
    }

    #[tokio::test]
    async fn finalize_chat_reply_on_success_files_the_chat_exactly_once() {
        let _env = setup().await;
        let key = key_hex_str();

        message_store::save_message(
            USER_ID,
            PERSONA_ID,
            &key,
            "chat-success-test",
            "user",
            "first message",
            None,
            None,
        )
        .await
        .expect("save_message (user) must succeed");
        message_store::save_message(
            USER_ID,
            PERSONA_ID,
            &key,
            "chat-success-test",
            "assistant",
            "",
            Some("run-ok-1"),
            None,
        )
        .await
        .expect("save_message (assistant placeholder) must succeed");

        // Enters via the awaiting_user/draft branch rather than a real
        // outputs.db row -- finalize_chat_reply treats both identically
        // once is_real_reply is true (the thing this test actually cares
        // about: does a real reply file the chat), and this avoids standing
        // up a second encrypted DB (outputs.db) just for this assertion.
        let result = Ok(run_result_awaiting_user(Some("the real reply content")));

        let outcome =
            finalize_chat_reply(USER_ID, PERSONA_ID, &key, "run-ok-1", &result, None).await;

        assert_eq!(outcome, ReplyOutcome::ChatFiled);

        let chats = chat_store::list_chats(USER_ID, PERSONA_ID, &key)
            .await
            .expect("list_chats must succeed");
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].title.as_deref(), Some("first message"));
    }

    #[tokio::test]
    async fn finalize_chat_reply_follow_up_bumps_without_filing_a_second_row() {
        let _env = setup().await;
        let key = key_hex_str();

        message_store::save_message(
            USER_ID,
            PERSONA_ID,
            &key,
            "chat-followup-test",
            "user",
            "first message",
            None,
            None,
        )
        .await
        .expect("save_message (user 1) must succeed");
        message_store::save_message(
            USER_ID,
            PERSONA_ID,
            &key,
            "chat-followup-test",
            "assistant",
            "",
            Some("run-a"),
            None,
        )
        .await
        .expect("save_message (assistant placeholder 1) must succeed");
        let first_result = Ok(run_result_awaiting_user(Some("reply one")));
        let first_outcome =
            finalize_chat_reply(USER_ID, PERSONA_ID, &key, "run-a", &first_result, None).await;
        assert_eq!(first_outcome, ReplyOutcome::ChatFiled);

        // Force a real ordering difference the same way a real later
        // message would -- same technique chat_store.rs's own bump test
        // uses, needed since now()-derived timestamps are second-resolution.
        let mut conn = message_store::open_messages_db(USER_ID, PERSONA_ID, &key)
            .await
            .expect("open_messages_db must succeed");
        sqlx::query("UPDATE chats SET last_message_at = ? WHERE context_key = ?")
            .bind("2026-09-01T00:00:01Z")
            .bind("chat-followup-test")
            .execute(&mut conn)
            .await
            .expect("forcing last_message_at back must succeed");

        message_store::save_message(
            USER_ID,
            PERSONA_ID,
            &key,
            "chat-followup-test",
            "user",
            "second message",
            None,
            None,
        )
        .await
        .expect("save_message (user 2) must succeed");
        message_store::save_message(
            USER_ID,
            PERSONA_ID,
            &key,
            "chat-followup-test",
            "assistant",
            "",
            Some("run-b"),
            None,
        )
        .await
        .expect("save_message (assistant placeholder 2) must succeed");
        let second_result = Ok(run_result_awaiting_user(Some("reply two")));
        let second_outcome =
            finalize_chat_reply(USER_ID, PERSONA_ID, &key, "run-b", &second_result, None).await;
        assert_eq!(
            second_outcome,
            ReplyOutcome::ChatFiled,
            "a follow-up's real reply must also report ChatFiled, so the caller emits chat-activity-updated"
        );

        let chats = chat_store::list_chats(USER_ID, PERSONA_ID, &key)
            .await
            .expect("list_chats must succeed");
        assert_eq!(
            chats.len(),
            1,
            "a follow-up must bump the existing row, not create a second one"
        );
        assert_eq!(
            chats[0].title.as_deref(),
            Some("first message"),
            "title must stay from the first message, never overwritten by a follow-up"
        );
        assert_ne!(
            chats[0].last_message_at, "2026-09-01T00:00:01Z",
            "a follow-up's real reply must bump last_message_at"
        );
    }

    #[tokio::test]
    async fn finalize_chat_reply_is_a_noop_for_a_run_with_no_owning_message() {
        let _env = setup().await;
        let key = key_hex_str();

        // No save_message call at all for this run_id -- simulates a
        // submit_focus_run/Board-originated run, which never writes a
        // messages.db row in the first place.
        let result = Ok(run_result_awaiting_user(Some("irrelevant")));
        let outcome = finalize_chat_reply(
            USER_ID,
            PERSONA_ID,
            &key,
            "run-with-no-message",
            &result,
            None,
        )
        .await;

        assert_eq!(outcome, ReplyOutcome::NotAChatMessage);
        let chats = chat_store::list_chats(USER_ID, PERSONA_ID, &key)
            .await
            .expect("list_chats must succeed");
        assert!(chats.is_empty());
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
            is_error: false,
        };
        let info = to_message_info(record);
        assert_eq!(info.id, "id-1");
        assert_eq!(info.focus_run_id.as_deref(), Some("run-1"));
        assert_eq!(info.gate3_review_status.as_deref(), Some("drafted"));
    }
}
