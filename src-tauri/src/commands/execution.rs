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
// cancel_run: stops a run that is live in this process (trips its lease's
//   stop token, so it ends cancelled with no further model call, items.id=816),
//   then writes status='cancelled' to focus_runs and deletes the run's
//   snapshots via cancel_focus_run() (D6-352). The owner is the signed-in
//   session user, never the client-supplied user_id; a foreign and an unknown
//   run give the same RunNotFound error and stop nothing. No-op if already
//   terminal. A run not live here (parked awaiting_user) is only marked.
// resume_run: routes by focus_run status. For awaiting_extract_confirm:
//   crash-recovery replay, then re-emit if pending > 0, or not_implemented
//   if pending == 0 (output() is owned by submit_extract_confirm, not here).
//   complete/cancelled/failed: distinct "run_already_finished" error (terminal,
//   nothing to resume). awaiting_feedback: distinct "no_resume_needed" error
//   (output already saved, Phase 6 feedback out of scope). awaiting_user/
//   paused/running/initializing (items.id=245): rehydrate_focus_run() rebuilds
//   a live FocusRun from focus_runs.user_input + the latest focus_run_snapshots
//   row + a fresh PersonalTrack fetch, then resume_execution() promotes it to
//   'running' and continues execute() from current_step -- spawned in the
//   background, same fire-and-forget shape as submit_focus_run (see resume_run()'s
//   own doc comment).
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
use crate::conductor::lifecycle::{rehydrate_focus_run, FocusRun, LifecycleError};
use crate::conductor::reentry::ReentryError;
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

#[derive(Debug, Deserialize, Type)]
pub struct ReenterStepRequest {
    pub run_id: String,
    pub target_step_id: String,
    pub user_id: String,
    pub persona_id: String,
}

#[derive(Debug, Serialize, Type)]
pub struct ReenterStepResponse {
    pub run_id: String,
    // u32, not usize: specta-typescript forbids exporting BigInt-style
    // types (usize/isize/i64/u64/i128/u128) across the IPC boundary to
    // avoid TS precision loss. A step index safely fits u32.
    pub resume_from_index: u32,
    pub discarded_step_ids: Vec<String>,
    pub stale_output_vars: Vec<String>,
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
///
/// `bound` (items.id=716) is the provider and model an API Chat run is bound
/// to. It is set before AUTHORIZE because authorize builds the failure handler
/// from provider resolution. Every other caller passes None.
///
/// items.id=778: the caller holds the run's writer lease (taken before anything
/// was written) across this call and moves it into `run.lease` afterwards, so
/// the run keeps it until it is dropped. If LOAD or AUTHORIZE fails the lease
/// is dropped with the error.
pub(crate) async fn load_and_authorize_run(
    app_handle: tauri::AppHandle,
    scheduler: tauri::State<'_, Arc<ConductorScheduler>>,
    pool: tauri::State<'_, sqlx::SqlitePool>,
    key_hex: String,
    crisis_detected: bool,
    bound: Option<crate::api_chat::BoundDestination>,
    request: SubmitFocusRunRequest,
) -> Result<FocusRun, String> {
    let is_quick_ask = is_quick_ask_focus(&request.focus_id);
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
        pool.inner().clone(),
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
    run.chat_binding = bound;
    // A user started this run from the app (items.id=710): attended.
    run.attendance = crate::spend::gate::RunAttendance::Attended;

    // Phase 1 LOAD and Phase 2 AUTHORIZE run synchronously before returning,
    // guaranteeing a valid run_id is available to the caller.
    run.load().await.map_err(|e| e.to_string())?;
    run.authorize().await.map_err(|e| e.to_string())?;

    Ok(run)
}

/// Whether a run's Focus id is the retired Quick Ask Focus. Pulled out so a
/// test can pin that an API Chat run (`api-chat`) never counts as one
/// (items.id=716).
fn is_quick_ask_focus(focus_id: &str) -> bool {
    focus_id == "quick-ask"
}

#[tauri::command]
#[specta::specta]
pub async fn submit_focus_run(
    app_handle: tauri::AppHandle,
    scheduler: tauri::State<'_, Arc<ConductorScheduler>>,
    pool: tauri::State<'_, sqlx::SqlitePool>,
    key_registry: State<'_, KeyRegistry>,
    request: SubmitFocusRunRequest,
) -> Result<SubmitFocusRunResponse, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.data_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;
    // items.id=778: the run's lease, held by the run until it ends.
    let lease = crate::auth::writer_gate::lease_for_session(
        &key_registry,
        crate::auth::writer_gate::Kind::FocusRun,
    )
    .await?;

    // R1 crisis-handling floor (decisions.id=607, items.id=265): local,
    // deterministic check on the actual fresh input for this run -- not
    // blended with any history here, so this is already the "fresh turn".
    let crisis_detected = crate::conductor::crisis::detect(&request.user_input);

    let mut run = load_and_authorize_run(
        app_handle,
        scheduler,
        pool,
        key_hex_str,
        crisis_detected,
        None,
        request,
    )
    .await?;
    run.attach_lease(lease).map_err(|e| e.to_string())?;

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
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.data_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let record = output_store::get_output_for_run(&user_id, &persona_id, &key_hex_str, &run_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not_found".to_string())?;

    Ok(GetRunOutputResponse {
        // content is only NULL for an ingested-document row with an opaque
        // binary upload (items.id=383) -- a Focus run's own output always
        // has real text content set by save_output, so this is never
        // actually reached for a normal run. unwrap_or_default() rather
        // than an error, matching this endpoint's existing "return what we
        // have" posture.
        content: record.content.unwrap_or_default(),
        output_type: record.output_type,
        sensitivity: record.sensitivity,
    })
}

/// items.id=587 (late-reply recovery): the frontend's only other read,
/// get_run_output above, answers "does real output exist yet" -- it has no
/// way to tell a run that's still genuinely being worked on apart from one
/// that's paused, cancelled, or crashed. ChatPane needs exactly that
/// distinction when it resumes tracking an empty assistant placeholder
/// after a remount (e.g. reopening a chat from History while its reply is
/// still generating): resume polling only for a run that is actually
/// active, and stop/show a plain "didn't finish" for one that's dead,
/// without resuming for one that's merely paused at a consent/Gate3 step
/// (that has its own UI elsewhere -- this must not interfere with it).
///
/// Thin wrapper -- output_store::get_focus_run_status already exists and is
/// already used for exactly this read by resume_run and
/// submit_extract_confirm above; this just exposes it to the frontend.
/// Returns Ok(None) if run_id doesn't exist in focus_runs at all (never an
/// error -- "unknown" is a normal, expected answer here, not a failure).
#[tauri::command]
#[specta::specta]
pub async fn get_run_status(
    run_id: String,
    user_id: String,
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<Option<String>, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.data_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    output_store::get_focus_run_status(&user_id, &persona_id, &key_hex_str, &run_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_run(
    run_id: String,
    user_id: String,
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<(), String> {
    let lease = crate::auth::writer_gate::command(&key_registry).await?;
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.data_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    // The lease was taken for the signed-in session, so its user is the
    // session's (items.id=816).
    cancel_run_for_session(
        lease.user_id(),
        &user_id,
        &persona_id,
        &key_hex_str,
        &run_id,
    )
    .await
}

/// The body of `cancel_run` (items.id=816), apart from the Tauri state so it can
/// be tested. `session_user` is the signed-in user; `user_id` is what the client
/// sent and must match it.
///
/// 1. A client `user_id` that is not the session's gets the same error as an
///    unknown run, before anything is touched: no existence leak, and no open
///    of another account's database.
/// 2. A run live in this process is stopped FIRST by tripping its lease's stop
///    token, so it takes no further step or model call while the row is written
///    (it ends `cancelled` through the same path as a rotation's 'Stop and
///    save'). Only the session user's own runs can match.
/// 3. The row is marked cancelled and its snapshots deleted. A run not live
///    here (parked awaiting_user) gets just this; a finished or already
///    cancelled run is a no-op; an unknown run is RunNotFound.
async fn cancel_run_for_session(
    session_user: &str,
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    run_id: &str,
) -> Result<(), String> {
    if user_id != session_user {
        return Err(output_store::OutputStoreError::RunNotFound(run_id.to_owned()).to_string());
    }
    crate::auth::writer_gate::request_stop_for_run(session_user, run_id);
    output_store::cancel_focus_run(session_user, persona_id, key_hex, run_id)
        .await
        .map_err(|e| e.to_string())
}

/// Resume a paused focus run.
///
/// Routes by focus_runs.status (schema/outputs_001.sql CHECK, extended by
/// outputs_011.sql — the full set is 'initializing','running','paused',
/// 'awaiting_user','awaiting_feedback','awaiting_extract_confirm',
/// 'awaiting_schedule','complete','cancelled','failed').
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
/// awaiting_user, awaiting_schedule, paused, running, initializing (items.id=245, items.id=496): the
/// structural gap the previous revision of this doc comment described here
/// -- FocusRun::new() requires `user_input`, but user_input was never
/// persisted anywhere, so a live FocusRun could not be reconstructed to
/// re-enter execute() -- is closed: focus_runs.user_input (outputs_010.sql)
/// plus lifecycle::rehydrate_focus_run() (items.id=574 Part 1) now do
/// exactly that reconstruction, from the focus_runs row + the latest
/// focus_run_snapshots row + a fresh PersonalTrack fetch. All four statuses
/// collapse to identical handling here: rehydrate_focus_run() itself derives
/// the correct resume index from the row's own persisted status (AT the
/// checkpointed step for awaiting_user's consent-gate/handoff pause, AFTER
/// it for paused/running/initializing's periodic-or-crash checkpoint -- see
/// that function's own doc comment), so there is no per-status branching
/// left for this command layer to do. Execution resumes via
/// FocusRun::resume_execution(), spawned in the background exactly like
/// submit_focus_run's execute_full() -- progress arrives via the same
/// run_status_update push events, not this command's return value.
#[tauri::command]
#[specta::specta]
pub async fn resume_run(
    app_handle: tauri::AppHandle,
    scheduler: tauri::State<'_, Arc<ConductorScheduler>>,
    pool: tauri::State<'_, sqlx::SqlitePool>,
    key_registry: State<'_, KeyRegistry>,
    request: ResumeRunRequest,
) -> Result<String, String> {
    use crate::conductor::extract;
    use crate::conductor::privacy::types::ExtractedCandidate as IpcCandidate;
    use crate::persistence::output_store::get_focus_run_status;
    use tauri::Emitter;

    // items.id=778: this command's own lease; the resumed run takes its own
    // below, which outlives the command (it moves into the spawned task).
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.data_key))
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

        // cloud_frontier boundary (lifecycle.rs execute(), routing_tier == 3), a
        // consent-gate pause (handle_step_failure()'s AwaitUser/HoldForGate/
        // OfferTier2/OfferCompact/AwaitFloorConsent/AwaitConsent actions -- each
        // now checkpointed at the pausing step, items.id=245), a crash-demoted
        // run (demote_interrupted_runs() sweeps stale 'running'/'initializing'
        // to 'paused'), or a very recent crash not yet swept: all four collapse
        // to the same handling. rehydrate_focus_run() rebuilds a live FocusRun
        // from the focus_runs row (focus_id/user_input/topic_id/is_quick_ask)
        // plus the latest focus_run_snapshots row plus a fresh PersonalTrack
        // fetch, deriving the correct resume index from the row's own
        // persisted status itself -- see that function's doc comment for why
        // no per-status branching is needed here. confirmed_cross_persona_fact_ids
        // is empty here (ResumeRunRequest carries no such field, unlike
        // SubmitFocusRunRequest): any cross-Persona fact the original run had
        // confirmed is re-omitted on resume, same "declined-or-unasked" fallback
        // build_personal_track()'s provenance check already applies -- it does
        // not hard-block, so the resumed run still completes on its other,
        // permitted facts.
        // items.id=496: awaiting_schedule joins this same collapsed
        // handling -- rehydrate_focus_run()'s resume-index derivation
        // already treats it like awaiting_user (both checkpoint the step
        // BEFORE it runs), so no new branch is needed here either.
        "awaiting_user" | "awaiting_schedule" | "paused" | "running" | "initializing" => {
            let run_lease = crate::auth::writer_gate::lease_for_session(
                &key_registry,
                crate::auth::writer_gate::Kind::FocusRun,
            )
            .await?;
            let scheduler = Arc::clone(&*scheduler);
            let mut run: FocusRun = rehydrate_focus_run(
                request.user_id.clone(),
                request.persona_id.clone(),
                request.run_id.clone(),
                pool.inner().clone(),
                scheduler,
                Some(key_hex_str.clone()),
                std::collections::HashSet::new(),
                Some(app_handle),
            )
            .await
            .map_err(|e| e.to_string())?;
            // A user pressed resume (items.id=710): attended.
            run.attendance = crate::spend::gate::RunAttendance::Attended;
            run.attach_lease(run_lease).map_err(|e| e.to_string())?;

            // Spawn resume_execution() in the background, matching
            // submit_focus_run's fire-and-forget shape -- progress arrives via
            // run_status_update push events, not this command's return value.
            // items.id=587: unlike submit_focus_run's own discard, a resumed
            // run may be a chat's run that paused mid-reply -- finalize_chat_reply
            // is a safe no-op for any run_id with no owning messages.db row
            // (e.g. a Board-originated resume), so it's always correct to
            // call it here too.
            let bg_user_id = request.user_id.clone();
            let bg_persona_id = request.persona_id.clone();
            let bg_key_hex = key_hex_str.clone();
            let bg_run_id = request.run_id.clone();
            tokio::spawn(async move {
                let result = run.resume_execution().await;
                crate::commands::messages::finalize_chat_reply(
                    &bg_user_id,
                    &bg_persona_id,
                    &bg_key_hex,
                    &bg_run_id,
                    &result,
                    run.app_handle.as_ref(),
                )
                .await;
            });

            return Ok(request.run_id);
        }

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
                source: c.source,
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

/// Jump back to an earlier step in an already-executed run with new
/// information, discarding the now-stale downstream steps (items.id=574
/// follow-up, cb-07). Rewinds and checkpoints only -- it does NOT resume
/// execution itself; call resume_run() afterward to actually continue
/// forward, exactly like a crash-recovered run would.
///
/// Status gating mirrors resume_run()'s own terminal check plus one more
/// carve-out: 'complete'/'cancelled'/'failed' are rejected because
/// cleanup() purges focus_run_snapshots for exactly those three statuses
/// (lifecycle.rs cleanup()) -- there is no snapshot left to rehydrate from.
/// 'awaiting_extract_confirm' is also rejected: ownership of finishing that
/// run has already passed to submit_extract_confirm (commands/consent.rs),
/// and mutating task_track/current_step concurrently with that in-flight
/// handoff is a real race with no reentry use case that needs it -- the
/// user can confirm/cancel the extraction first and reenter from the
/// settled status that follows. Every other status (awaiting_user,
/// awaiting_schedule, awaiting_feedback, paused, running, initializing)
/// keeps its snapshot on disk and is accepted, including awaiting_feedback
/// so a user can revise an earlier step after seeing the final output
/// (items.id=574's own job-hunting motivating example). awaiting_schedule
/// (items.id=496) is not purged by cleanup() either, same as the others --
/// jumping back to an earlier step while a later one waits on its own
/// schedule_trigger is ordinary reentry, no special-case needed.
#[tauri::command]
#[specta::specta]
pub async fn reenter_step(
    app_handle: tauri::AppHandle,
    scheduler: tauri::State<'_, Arc<ConductorScheduler>>,
    pool: tauri::State<'_, sqlx::SqlitePool>,
    key_registry: State<'_, KeyRegistry>,
    request: ReenterStepRequest,
) -> Result<ReenterStepResponse, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    use crate::persistence::output_store::get_focus_run_status;

    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.data_key))
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
        "complete" | "cancelled" | "failed" => {
            return Err(format!("run_already_finished:{status}"));
        }
        "awaiting_extract_confirm" => {
            return Err("not_resumable:awaiting_extract_confirm".to_string());
        }
        "awaiting_user" | "awaiting_schedule" | "awaiting_feedback" | "paused" | "running"
        | "initializing" => {}
        other => return Err(format!("not_implemented:unrecognized_status:{other}")),
    }

    let scheduler = Arc::clone(&*scheduler);
    let mut run: FocusRun = rehydrate_focus_run(
        request.user_id.clone(),
        request.persona_id.clone(),
        request.run_id.clone(),
        pool.inner().clone(),
        scheduler,
        Some(key_hex_str),
        std::collections::HashSet::new(),
        Some(app_handle),
    )
    .await
    .map_err(|e| e.to_string())?;
    // A user asked to revisit a step (items.id=710): attended.
    run.attendance = crate::spend::gate::RunAttendance::Attended;

    // This run's resolved step list -- already customization-applied at
    // LOAD time (focus_def.steps), not the Focus type's raw catalog.
    let focus_step_order: Vec<String> = run
        .focus_def
        .as_ref()
        .map(|fd| fd.steps.iter().map(|s| s.step_id.clone()).collect())
        .unwrap_or_default();

    let target_step = run
        .focus_def
        .as_ref()
        .and_then(|fd| {
            fd.steps
                .iter()
                .find(|s| s.step_id == request.target_step_id)
        })
        .ok_or_else(|| format!("step_not_found:{}", request.target_step_id))?;

    if !target_step.revisitable {
        return Err(format!("step_not_revisitable:{}", request.target_step_id));
    }

    let plan = run
        .reenter_step(&focus_step_order, &request.target_step_id)
        .await
        .map_err(|e| match e {
            LifecycleError::Reentry(ReentryError::StepNotFound(id)) => {
                format!("step_not_found:{id}")
            }
            LifecycleError::Reentry(ReentryError::NothingToDiscard(id)) => {
                format!("nothing_to_discard:{id}")
            }
            other => other.to_string(),
        })?;

    Ok(ReenterStepResponse {
        run_id: request.run_id,
        resume_from_index: plan.resume_from_index as u32,
        discarded_step_ids: plan.discarded_step_ids,
        stale_output_vars: plan.stale_output_vars,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// items.id=716: an API Chat run never counts as Quick Ask (a retired
    /// concept, decisions.id=804), so `is_quick_ask` is never set for it.
    #[test]
    fn only_the_quick_ask_focus_id_is_quick_ask() {
        assert!(is_quick_ask_focus("quick-ask"));
        assert!(!is_quick_ask_focus(crate::api_chat::API_CHAT_FOCUS_ID));
        assert!(!is_quick_ask_focus("writing-assistant"));
        assert!(!is_quick_ask_focus(""));
    }

    // -- items.id=816: cancel_run ----------------------------------------------
    //
    // `cancel_run` itself takes Tauri State, which has no construction path in a
    // plain #[tokio::test] (see commands/api_chat.rs); everything it decides is
    // in `cancel_run_for_session`, tested here against a real SQLCipher
    // outputs.db. Not tested: the command wrapper (the Command lease and the
    // session lookup) and a real in-flight model call being dropped.

    use crate::auth::writer_gate::{self, Kind};
    use crate::persistence::output_store::open_outputs_db;
    use sqlx::Row;

    const KEY: &str = "0606060606060606060606060606060606060606060606060606060606060606";
    const PERSONA: &str = "cancel-persona";

    /// Points QR_DATA_ROOT at a fresh directory for one test and restores it.
    struct DataRoot {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved: Option<String>,
        _dir: tempfile::TempDir,
    }

    impl DataRoot {
        async fn new() -> Self {
            let lock = crate::test_support::ENV_MUTEX.lock().await;
            let saved = std::env::var("QR_DATA_ROOT").ok();
            let dir = tempfile::tempdir().unwrap();
            std::env::set_var("QR_DATA_ROOT", dir.path());
            Self {
                _lock: lock,
                saved,
                _dir: dir,
            }
        }
    }

    impl Drop for DataRoot {
        fn drop(&mut self) {
            match &self.saved {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn seed_run(user: &str, run_id: &str, status: &str, snapshots: usize) {
        let mut conn = open_outputs_db(user, PERSONA, KEY).await.unwrap();
        sqlx::query(
            "INSERT INTO focus_runs (id, focus_id, status, started_at)
             VALUES (?, 'writing-assistant', ?, '2026-10-26T00:00:00Z')",
        )
        .bind(run_id)
        .bind(status)
        .execute(&mut conn)
        .await
        .unwrap();
        for i in 0..snapshots {
            sqlx::query(
                "INSERT INTO focus_run_snapshots
                 (id, focus_run_id, step_id, phase, checkpoint_hash, created_at)
                 VALUES (?, ?, 's1', 4, 'h', '2026-10-26T00:00:00Z')",
            )
            .bind(format!("{run_id}-snap-{i}"))
            .bind(run_id)
            .execute(&mut conn)
            .await
            .unwrap();
        }
    }

    async fn status_of(user: &str, run_id: &str) -> String {
        let mut conn = open_outputs_db(user, PERSONA, KEY).await.unwrap();
        sqlx::query("SELECT status FROM focus_runs WHERE id = ?")
            .bind(run_id)
            .fetch_one(&mut conn)
            .await
            .unwrap()
            .get("status")
    }

    async fn snapshots_of(user: &str, run_id: &str) -> i64 {
        let mut conn = open_outputs_db(user, PERSONA, KEY).await.unwrap();
        sqlx::query("SELECT COUNT(*) AS n FROM focus_run_snapshots WHERE focus_run_id = ?")
            .bind(run_id)
            .fetch_one(&mut conn)
            .await
            .unwrap()
            .get("n")
    }

    #[tokio::test]
    async fn a_parked_run_that_is_not_live_is_marked_cancelled_and_loses_its_snapshots() {
        let _root = DataRoot::new().await;
        let user = "cancel-parked";
        seed_run(user, "run-1", "awaiting_user", 2).await;
        assert_eq!(snapshots_of(user, "run-1").await, 2);

        cancel_run_for_session(user, user, PERSONA, KEY, "run-1")
            .await
            .unwrap();
        assert_eq!(status_of(user, "run-1").await, "cancelled");
        assert_eq!(snapshots_of(user, "run-1").await, 0);

        // Idempotent: a second cancel is Ok and changes nothing.
        cancel_run_for_session(user, user, PERSONA, KEY, "run-1")
            .await
            .unwrap();
        assert_eq!(status_of(user, "run-1").await, "cancelled");
    }

    #[tokio::test]
    async fn a_finished_run_is_a_no_op_and_keeps_its_status() {
        let _root = DataRoot::new().await;
        let user = "cancel-finished";
        for (id, status) in [("c", "complete"), ("f", "failed")] {
            seed_run(user, id, status, 1).await;
            cancel_run_for_session(user, user, PERSONA, KEY, id)
                .await
                .unwrap();
            assert_eq!(status_of(user, id).await, status);
            // Nothing was changed, so nothing was purged.
            assert_eq!(snapshots_of(user, id).await, 1);
        }
    }

    #[tokio::test]
    async fn a_live_run_is_stopped_and_its_row_reads_cancelled() {
        let _root = DataRoot::new().await;
        let user = "cancel-live";
        seed_run(user, "run-1", "running", 1).await;
        let lease = writer_gate::acquire(user, Kind::FocusRun).unwrap();
        lease.bind_run("run-1");
        let other = writer_gate::acquire(user, Kind::FocusRun).unwrap();
        other.bind_run("run-2");

        cancel_run_for_session(user, user, PERSONA, KEY, "run-1")
            .await
            .unwrap();
        assert!(lease.stop_token().is_cancelled(), "the run is told to stop");
        assert!(!other.stop_token().is_cancelled(), "another run is not");
        assert_eq!(status_of(user, "run-1").await, "cancelled");
        assert_eq!(snapshots_of(user, "run-1").await, 0);
    }

    #[tokio::test]
    async fn an_unknown_run_and_a_foreign_user_get_the_same_error_and_stop_nothing() {
        let _root = DataRoot::new().await;
        let owner = "cancel-owner";
        seed_run(owner, "run-1", "running", 0).await;
        let lease = writer_gate::acquire(owner, Kind::FocusRun).unwrap();
        lease.bind_run("run-1");

        let unknown = cancel_run_for_session(owner, owner, PERSONA, KEY, "no-such-run")
            .await
            .unwrap_err();
        assert_eq!(unknown, "Run not found: no-such-run");

        // Another account's session, naming the owner's run: it opens its own
        // (empty) database, finds no such run, and the owner's lease is not tripped.
        let foreign =
            cancel_run_for_session("cancel-intruder", "cancel-intruder", PERSONA, KEY, "run-1")
                .await
                .unwrap_err();
        assert_eq!(foreign, "Run not found: run-1");
        assert!(!lease.stop_token().is_cancelled());

        // A client that names the owner while signed in as someone else: the
        // same error, before any database is opened, and nothing is stopped.
        let spoofed = cancel_run_for_session("cancel-intruder", owner, PERSONA, KEY, "run-1")
            .await
            .unwrap_err();
        assert_eq!(spoofed, "Run not found: run-1");
        assert!(!lease.stop_token().is_cancelled());
        assert_eq!(status_of(owner, "run-1").await, "running");
    }
}
