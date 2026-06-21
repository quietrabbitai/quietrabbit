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
// resume_run: STUB — snapshot replay not yet wired. Returns not_implemented.
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
    pub key_hex: String,
    pub topic_id: Option<String>,
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

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn submit_focus_run(
    app_handle: tauri::AppHandle,
    scheduler: tauri::State<'_, Arc<ConductorScheduler>>,
    request: SubmitFocusRunRequest,
) -> Result<SubmitFocusRunResponse, String> {
    let is_quick_ask = request.focus_id == "quick-ask";
    let scheduler = Arc::clone(&*scheduler);

    let mut run = FocusRun::new(
        request.user_id,
        request.persona_id,
        request.focus_id,
        scheduler,
        request.user_input,
        false, // is_fast_lane: always false at IPC boundary
        Some(request.key_hex),
        request.topic_id,
        is_quick_ask,
        Some(app_handle),
    );

    // Phase 1 LOAD and Phase 2 AUTHORIZE run synchronously before returning,
    // guaranteeing a valid run_id is available in the response.
    run.load().await.map_err(|e| e.to_string())?;
    run.authorize().await.map_err(|e| e.to_string())?;

    let run_id = run
        .focus_run_id
        .clone()
        .ok_or_else(|| "run_id not set after authorize".to_string())?;

    // Spawn execute_full() in the background. Failures are logged and surfaced
    // as push events inside execute_full() — they do not panic.
    tokio::spawn(async move {
        let _result = run.execute_full().await;
    });

    Ok(SubmitFocusRunResponse { run_id })
}

#[tauri::command]
pub async fn get_run_output(
    run_id: String,
    user_id: String,
    persona_id: String,
    key_hex: String,
) -> Result<GetRunOutputResponse, String> {
    let record = output_store::get_output_for_run(&user_id, &persona_id, &key_hex, &run_id)
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
pub async fn cancel_run(
    run_id: String,
    user_id: String,
    persona_id: String,
    key_hex: String,
) -> Result<(), String> {
    output_store::cancel_focus_run(&user_id, &persona_id, &key_hex, &run_id)
        .await
        .map_err(|e| e.to_string())
}

/// STUB — resume requires snapshot replay, not yet wired.
#[tauri::command]
pub async fn resume_run(_run_id: String) -> Result<String, String> {
    Err("not_implemented".to_string())
}
