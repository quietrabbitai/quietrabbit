// src-tauri/src/commands/execution.rs
//
// Group 1 — Focus execution.
// Commands: submit_focus_run, get_run_output, cancel_run, resume_run.
//
// submit_focus_run: runs Phase 1 (LOAD) and Phase 2 (AUTHORIZE) synchronously
//   to obtain a valid run_id, then spawns execute_full() in a background task.
//   Progress arrives via run_status_update push events (fired from lifecycle.rs).
//   Failures inside execute_full() are logged and surfaced as push events there.
// get_run_output: polls output_store for a completed run's output.
//   Returns "not_found" if the run has not yet produced output.
// cancel_run: writes status='cancelled' to focus_runs via cancel_focus_run()
//   (D6-352). No-op if already terminal. Returns RunNotFound if run unknown.
// resume_run: routes by focus_run status. For awaiting_extract_confirm:
//   crash-recovery replay, then re-emit if pending > 0, or not_implemented
//   if pending == 0 (output() is owned by submit_extract_confirm, not here).
//   complete/cancelled/failed: distinct "run_already_finished" error (terminal,
//   nothing to resume). awaiting_feedback: distinct "no_resume_needed" error
//   (output already saved, Phase 6 feedback out of scope). awaiting_user/
//   paused/running/initializing: still not_implemented -- blocked on
//   user_input never being persisted, so a live FocusRun can't be
//   reconstructed to re-enter execute() (see resume_run()'s own doc comment).
//
// Lifecycle ownership for awaiting_extract_confirm (item 20):
//   resume_run() = crash recovery + UI rehydration only.
//   The transition awaiting_extract_confirm -> output() -> complete is owned
//   by submit_extract_confirm (commands/consent.rs) while the original FocusRun
//   actor is still resident. resume_run() never calls output().
//   Full snapshot replay for resume_run() is deferred post-Release-1.
//
// is_fast_lane: always false at IPC boundary (Phase 2 "Promote to Focus"
//   pre-population per CLAUDE.md). Oracle default: False.
// is_quick_ask: inferred from focus_id == "quick-ask" (oracle pattern).
//
// State injection:
//   app_handle: tauri::AppHandle — auto-injected by Tauri.
//   scheduler: tauri::State<Arc<ConductorScheduler>> — registered at startup.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::conductor::concurrency::ConductorScheduler;
use crate::conductor::lifecycle::FocusRun;
use crate::persistence::output_store;

// ---------------------------------------------------------------------------
// Request / response structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Type)]
pub struct SubmitFocusRunRequest {
    pub focus_id: String,
    pub user_input: String,
    pub user_id: String,
    pub persona_id: String,
    pub topic_id: Option<String>,
    /// entity_facts.id values the user approved via the pre-Focus-start
    /// cross-Persona confirmation flow (decisions.id=546, decisions.id=639,
    /// items.id=27) -- obtained by calling
    /// commands::consent::get_pending_cross_persona_confirmations() before
    /// this command. Any cross_persona_export=true fact whose id is absent
    /// here is omitted from context, not blocked (declined-or-unasked case).
    /// Empty vec is valid -- most runs have no pending cross-Persona facts.
    pub confirmed_cross_persona_fact_ids: Vec<String>,
}

#[derive(Debug, Serialize, Type)]
pub struct SubmitFocusRunResponse {
    pub run_id: String,
}

#[derive(Debug, Serialize, Type)]
pub struct GetRunOutputResponse {
    pub content: String,
    pub output_type: String,
    pub sensitivity: String,
}

#[derive(Debug, Deserialize, Type)]
pub struct ResumeRunRequest {
    pub run_id: String,
    pub user_id: String,
    pub persona_id: String,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Shared LOAD+AUTHORIZE core of submit_focus_run, factored out so callers
/// besides the plain submit_focus_run IPC command (namely
/// commands::messages::send_message, items.id=245-ish) can retain ownership
/// of the constructed, authorized `FocusRun` across the execute_full() await
/// instead of only getting back a bare run_id -- e.g. to write the run's
/// eventual output into a different store once generation completes.
/// submit_focus_run itself doesn't need this: it fires execute_full() and
/// forgets it, relying entirely on push events for progress.
///
/// key_hex is a separate parameter, not a SubmitFocusRunRequest field
/// (items.id=268): this fn isn't a #[tauri::command] itself, so it can't
/// take State<KeyRegistry> directly -- each of its two command-layer callers
/// (submit_focus_run, commands::messages::send_message) derives key_hex from
/// KeyRegistry and passes the owned String in here.
///
/// crisis_detected follows the same pattern for the same reason
/// (decisions.id=607, items.id=265): each caller runs
/// conductor::crisis::detect() on the actual fresh user turn it has on hand
/// (request.user_input here; commands::messages::send_message uses its raw
/// `content` param, not the blended history window it builds from it) and
/// passes the result in. It is deliberately NOT a SubmitFocusRunRequest
/// field -- that DTO is deserialized directly from the frontend, and this
/// safety floor must always be server-computed from the real text, never
/// trusted from the client.
pub(crate) async fn load_and_authorize_run(
    app_handle: tauri::AppHandle,
    scheduler: tauri::State<'_, Arc<ConductorScheduler>>,
    key_hex: String,
    crisis_detected: bool,
    request: SubmitFocusRunRequest,
) -> Result<FocusRun, String> {
    let is_quick_ask = request.focus_id == "quick-ask";
    let scheduler = Arc::clone(&*scheduler);
    let confirmed_cross_persona_fact_ids: std::collections::HashSet<String> = request
        .confirmed_cross_persona_fact_ids
        .into_iter()
        .collect();

    // Default type param (FocusRun<L = SqliteDisclosureLogger>) does not
    // resolve through full inference at a plain `let` binding -- explicit
    // annotation needed here (items.id=173). Production always wants the
    // concrete, disclosure_log-table-backed logger, so this IS the default;
    // the annotation exists to satisfy the type checker, not to make a
    // different choice than what the default already expresses.
    let mut run: FocusRun = FocusRun::new(
        request.user_id,
        request.persona_id,
        request.focus_id,
        scheduler,
        request.user_input,
        false, // is_fast_lane: always false at IPC boundary
        Some(key_hex),
        request.topic_id,
        is_quick_ask,
        confirmed_cross_persona_fact_ids,
        Some(app_handle),
    );
    run.crisis_floor_triggered = crisis_detected;

    // Phase 1 LOAD and Phase 2 AUTHORIZE run synchronously before returning,
    // guaranteeing a valid run_id is available to the caller.
    run.load().await.map_err(|e| e.to_string())?;
    run.authorize().await.map_err(|e| e.to_string())?;

    Ok(run)
}

#[tauri::command]
#[specta::specta]
pub async fn submit_focus_run(
    app_handle: tauri::AppHandle,
    scheduler: tauri::State<'_, Arc<ConductorScheduler>>,
    key_registry: State<'_, KeyRegistry>,
    request: SubmitFocusRunRequest,
) -> Result<SubmitFocusRunResponse, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    // R1 crisis-handling floor (decisions.id=607, items.id=265): local,
    // deterministic check on the actual fresh input for this run -- not
    // blended with any history here, so this is already the "fresh turn".
    let crisis_detected = crate::conductor::crisis::detect(&request.user_input);

    let mut run =
        load_and_authorize_run(app_handle, scheduler, key_hex_str, crisis_detected, request)
            .await?;

    let run_id = run
        .focus_run_id
        .clone()
        .ok_or_else(|| "run_id not set after authorize".to_string())?;

    // Spawn execute_full() in the background. Failures are logged and surfaced
    // as push events inside execute_full() -- they do not panic.
    tokio::spawn(async move {
        let _result = run.execute_full().await;
    });

    Ok(SubmitFocusRunResponse { run_id })
}

#[tauri::command]
#[specta::specta]
pub async fn get_run_output(
    run_id: String,
    user_id: String,
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<GetRunOutputResponse, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let record = output_store::get_output_for_run(&user_id, &persona_id, &key_hex_str, &run_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not_found".to_string())?;

    Ok(GetRunOutputResponse {
        content: record.content,
        output_type: record.output_type,
        sensitivity: record.sensitivity,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_run(
    run_id: String,
    user_id: String,
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<(), String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    output_store::cancel_focus_run(&user_id, &persona_id, &key_hex_str, &run_id)
        .await
        .map_err(|e| e.to_string())
}

/// Resume a paused focus run.
///
/// Routes by focus_runs.status (schema/outputs_001.sql CHECK — the full set
/// is 'initializing','running','paused','awaiting_user','awaiting_feedback',
/// 'awaiting_extract_confirm','complete','cancelled','failed').
///
/// awaiting_extract_confirm routing:
///   1. Crash-recovery replay: rows with status='confirmed' AND persisted_at IS NULL
///      -> replay persist_confirmed_field() + set_persisted_at() using persisted
///      sensitivity (never recomputed -- see load_unrecovered_rows() doc).
///   2. If pending > 0: re-emit extract_confirm_request push event and return.
///   3. If pending == 0: all candidates decided. output() is called by
///      submit_extract_confirm once the user submits decisions -- not from here.
///      Full snapshot replay for this path is deferred post-Release-1.
///
/// complete/cancelled/failed: terminal -- there is nothing to resume, so this
/// returns a distinct "already finished" error rather than not_implemented,
/// which would incorrectly imply resume itself is the missing piece.
///
/// awaiting_feedback: Phase 5 OUTPUT already ran and saved content (see
/// lifecycle.rs output()) -- Phase 6 FEEDBACK is explicitly out of scope of
/// the FocusRun module (lifecycle.rs module header, "out of scope for this
/// module (async paste-back)"). Nothing is pending execution; the caller
/// should fetch the already-produced output via get_run_output instead.
///
/// awaiting_user, paused, running, initializing: genuinely not_implemented
/// (unchanged stub behaviour). All four are blocked on the same structural
/// gap, not a missing switch statement: FocusRun::new() requires
/// `user_input`, and every step's prompt render threads it through
/// StepContext (lifecycle.rs execute_step()) -- but user_input is never
/// persisted anywhere (not on focus_runs, not in focus_run_snapshots). There
/// is currently no way to reconstruct a live FocusRun to re-enter execute()
/// for any of these four statuses without first adding somewhere to store
/// it, which is a schema change, not a resume_run fix. See each match arm
/// below for the state-specific detail on top of that shared blocker.
#[tauri::command]
#[specta::specta]
pub async fn resume_run(
    app_handle: tauri::AppHandle,
    _scheduler: tauri::State<'_, Arc<ConductorScheduler>>,
    key_registry: State<'_, KeyRegistry>,
    request: ResumeRunRequest,
) -> Result<String, String> {
    use crate::conductor::extract;
    use crate::conductor::privacy::types::ExtractedCandidate as IpcCandidate;
    use crate::persistence::output_store::get_focus_run_status;
    use tauri::Emitter;

    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let status = get_focus_run_status(
        &request.user_id,
        &request.persona_id,
        &key_hex_str,
        &request.run_id,
    )
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "not_found".to_string())?;

    match status.as_str() {
        "awaiting_extract_confirm" => {}

        // Terminal -- resuming a finished run is meaningless. Distinct from
        // not_implemented: nothing needs building here, the run is just done.
        "complete" | "cancelled" | "failed" => {
            return Err(format!("run_already_finished:{status}"));
        }

        // Phase 5 OUTPUT already completed and saved content; Phase 6
        // FEEDBACK (the only thing this status is waiting on) is out of
        // scope of this module. Nothing to resume -- direct the caller to
        // the output that already exists instead of claiming this is unbuilt.
        "awaiting_feedback" => {
            return Err("no_resume_needed:awaiting_feedback -- output already \
                         produced, call get_run_output"
                .to_string());
        }

        // Tier 3 boundary (lifecycle.rs execute(), routing_tier == 3) or a
        // consent-gate pause (handle_step_failure()'s AwaitUser/HoldForGate/
        // OfferTier2/OfferCompact/AwaitFloorConsent/AwaitConsent actions).
        // consent.rs's own module header ("lifecycle checks consent_decisions
        // when the run is resumed") names resume_run as the intended
        // re-attachment point once the user answers the consent_decisions
        // row those commands write -- but reattaching means rebuilding
        // PersonalTrack/TaskTrack/SharedStateTrack from focus_run_snapshots
        // and continuing execute() from current_step, which hits the
        // missing-user_input blocker described above.
        "awaiting_user" => return Err("not_implemented".to_string()),

        // Crash-demoted by demote_interrupted_runs() (stale 'running' or
        // 'initializing', see that function's own doc comment) -- the
        // FocusRun actor that held the live tracks is gone. Same
        // missing-user_input blocker; additionally needs the
        // focus_run_snapshots -> PersonalTrack/TaskTrack/SharedStateTrack
        // rehydration that reentry.rs explicitly declined to build (see its
        // module header: it only computes a plan, it does not resume).
        "paused" => return Err("not_implemented".to_string()),

        // Set at Phase 3 INITIALIZE (lifecycle.rs initialize(), before the
        // EXECUTE loop starts) or Phase 2 AUTHORIZE (authorize(), before
        // tracks even exist). A row seen in either state via resume_run
        // implies either a very recent crash (not yet demoted to 'paused' by
        // demote_interrupted_runs()) or that demotion never ran -- same
        // missing-user_input blocker as 'paused' either way.
        "running" | "initializing" => return Err("not_implemented".to_string()),

        other => return Err(format!("not_implemented:unrecognized_status:{other}")),
    }

    // Step 1: crash-recovery replay.
    // Sensitivity read from persisted state -- never recomputed.
    let unrecovered = extract::load_unrecovered_rows(
        &request.user_id,
        &request.persona_id,
        &key_hex_str,
        &request.run_id,
    )
    .await
    .map_err(|e| e.to_string())?;

    for (candidate_id, field_name, confirmed_value, sensitivity) in unrecovered {
        extract::persist_confirmed_field(
            &request.user_id,
            &request.persona_id,
            &key_hex_str,
            &field_name,
            &confirmed_value,
            &sensitivity,
        )
        .await
        .map_err(|e| e.to_string())?;

        extract::set_persisted_at(
            &request.user_id,
            &request.persona_id,
            &key_hex_str,
            candidate_id,
        )
        .await
        .map_err(|e| e.to_string())?;
    }

    // Step 2: check pending count.
    let pending = extract::count_pending(
        &request.user_id,
        &request.persona_id,
        &key_hex_str,
        &request.run_id,
    )
    .await
    .map_err(|e| e.to_string())?;

    if pending > 0 {
        // Re-emit extract_confirm_request with current pending candidates.
        let candidates = extract::load_pending_candidates(
            &request.user_id,
            &request.persona_id,
            &key_hex_str,
            &request.run_id,
        )
        .await
        .map_err(|e| e.to_string())?;

        let ipc_candidates: Vec<IpcCandidate> = candidates
            .into_iter()
            .map(|c| IpcCandidate {
                candidate_id: c.id,
                field_name: c.field_name,
                extracted_value: c.extracted_value,
                sensitivity: c.sensitivity,
                reason: c.reason,
                confidence: c.confidence,
                warn_flag: c.warn_flag,
            })
            .collect();

        let payload = serde_json::json!({
            "run_id": request.run_id,
            "candidates": serde_json::to_value(&ipc_candidates)
                .unwrap_or(serde_json::json!([])),
        });
        if let Err(e) = app_handle.emit("extract_confirm_request", &payload) {
            log::warn!("resume_run: emit extract_confirm_request failed: {e}");
        }
        return Ok(request.run_id);
    }

    // Step 3: pending == 0. All candidates decided.
    // output() transition is owned by submit_extract_confirm while the original
    // FocusRun actor is still resident. Snapshot replay for resume_run() on this
    // path is deferred post-Release-1.
    Err("not_implemented".to_string())
}
