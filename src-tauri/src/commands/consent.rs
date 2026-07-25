// src-tauri/src/commands/consent.rs
//
// Group 2 — Consent and privacy gates.
// Commands: submit_consent_decision, submit_floor_consent_decision,
//           submit_element_consent_decision, submit_extract_confirm.
//
// submit_consent_decision: records a Gate 3 cross-tier promotion decision.
//   The frontend receives a consent_request push event, presents the UI,
//   then calls this command with the user's decision.
//   decision: "approved" | "declined"
//   Writes to consent_decisions table via write_consent_decision() (D6-352).
//
// submit_floor_consent_decision: records a floor abstraction clamping decision.
//   The frontend receives a floor_consent_request push event, presents the UI,
//   then calls this command with the user's decision and tier.
//   decision: "proceed" | "cancel"
//   If save_preference is true, writes floor_consent_preference to
//   personas.extra_metadata in shared.db (D5-152 scoped consent record).
//   Writes to consent_decisions table via write_floor_consent_decision() (D6-352).
//
// submit_element_consent_decision: records per-element Privacy Guardian decisions
//   (D6-362). The frontend serializes Vec<ElementDecision> to JSON and passes it
//   as decisions_json -- the IPC boundary carries a plain String, keeping the
//   command layer free of Conductor type imports.
//   BLOCKED until outputs_007 migration (CHECK constraint extension). Returns Err.
//
// submit_extract_confirm: processes user decisions for extracted personal field
//   candidates (item 20). Validates, writes confirmed fields to personal.db,
//   marks candidates decided, then sets status='complete'.
//
// get_pending_cross_persona_confirmations: pre-Focus-start query for the
//   Cross-Persona Data Provenance confirmation flow (decisions.id=546,
//   decisions.id=639, items.id=27). Called by the frontend BEFORE
//   FocusRun::new() -- entirely outside FocusRun/Conductor, per decisions.id=639's
//   explicit rejection of new pause/resume machinery inside the engine for what
//   is fundamentally a pre-run gate. Returns the persona's pending
//   cross_persona_export=true facts (IPC-safe projection -- field_value is
//   never included, mirrors commands/personal.rs's no-raw-values convention).
//   The frontend shows a confirmation UI, then passes the user's confirmed
//   fact IDs to submit_focus_run's confirmed_cross_persona_fact_ids field.
//   This decision is per-session and NOT persisted to any table (decisions.id=546:
//   "per-session, non-persisted") -- there is no submit_ command here writing
//   a decision record, unlike the other consent commands in this file.
//
// All commands are fire-and-respond: lifecycle checks consent_decisions when
// the run is resumed. No direct signalling into the background task.
// (get_pending_cross_persona_confirmations is a plain read query -- no
// consent_decisions write, since the decision it informs is carried as data
// into submit_focus_run's request rather than persisted.)

use serde::{Deserialize, Serialize};
use specta::Type;
use sqlx::ConnectOptions;
use sqlx::Row;

use crate::conductor::extract;
use crate::conductor::privacy::types::ExtractConfirmDecision;
use crate::persistence::output_store;
use crate::persistence::output_store::{get_focus_run_status, set_focus_run_status};
use crate::persistence::personal_store;
use crate::providers::utils::{connect_options_unencrypted, db_path_shared, now};

// ---------------------------------------------------------------------------
// Request / response structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Type)]
pub struct SubmitConsentDecisionRequest {
    pub run_id: String,
    pub user_id: String,
    pub persona_id: String,
    pub key_hex: String,
    /// "approved" | "declined"
    pub decision: String,
}

#[derive(Debug, Deserialize, Type)]
pub struct SubmitFloorConsentDecisionRequest {
    pub run_id: String,
    pub user_id: String,
    pub persona_id: String,
    pub key_hex: String,
    pub abstraction_tier: i32,
    /// "proceed" | "cancel"
    pub decision: String,
    /// If true, saves as standing floor consent preference (D5-152).
    pub save_preference: bool,
}

#[derive(Debug, Deserialize, Type)]
pub struct SubmitElementConsentDecisionRequest {
    pub run_id:         String,
    pub user_id:        String,
    pub persona_id:     String,
    pub key_hex:        String,
    /// JSON-serialized Vec<ElementDecision> produced by the frontend.
    /// Expected shape per element:
    ///   { "span_id": string, "decision": "generalize"|"keep_private"|"release_original",
    ///     "suggestion_text": string|null, "user_modified_text": string|null }
    /// The command layer does not deserialize this -- it is passed through to
    /// write_element_consent_decisions() as-is (D6-362 IPC boundary rule).
    pub decisions_json: String,
}

#[derive(Debug, Deserialize, Type)]
pub struct SubmitExtractConfirmRequest {
    pub run_id:         String,
    pub user_id:        String,
    pub persona_id:     String,
    pub key_hex:        String,
    /// JSON-serialized Vec<ExtractConfirmDecision>.
    /// Shape per element:
    ///   { "candidate_id": i64, "confirmed": bool,
    ///     "extracted_value": string, "confirmed_value": string|null }
    /// confirmed_value must be non-null when confirmed==true.
    pub decisions_json: String,
}

#[derive(Debug, Deserialize, Type)]
pub struct GetPendingCrossPersonaConfirmationsRequest {
    pub user_id: String,
    pub persona_id: String,
    pub key_hex: String,
}

/// IPC-safe projection of an entity_facts row pending cross-Persona
/// confirmation (decisions.id=546, decisions.id=639, items.id=27).
/// field_value is intentionally absent -- mirrors PersonalFieldInfo's
/// no-raw-values convention in commands/personal.rs.
#[derive(Debug, Serialize, Type)]
pub struct PendingCrossPersonaFact {
    /// entity_facts.id -- pass this back in
    /// SubmitFocusRunRequest.confirmed_cross_persona_fact_ids for any fact
    /// the user approves.
    pub fact_id: String,
    pub entity_id: Option<String>,
    pub field_name: String,
    pub sensitivity: String,
    /// The Persona instance this fact originated in (decisions.id=546).
    /// Always Some for a row with cross_persona_export=true -- the DB CHECK
    /// constraint on entity_facts (personal_001.sql) guarantees this.
    pub origin_persona_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn submit_consent_decision(
    request: SubmitConsentDecisionRequest,
) -> Result<(), String> {
    output_store::write_consent_decision(
        &request.user_id,
        &request.persona_id,
        &request.key_hex,
        &request.run_id,
        &request.decision,
    )
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn submit_floor_consent_decision(
    request: SubmitFloorConsentDecisionRequest,
) -> Result<(), String> {
    // Write the decision record to consent_decisions in outputs.db.
    output_store::write_floor_consent_decision(
        &request.user_id,
        &request.persona_id,
        &request.key_hex,
        &request.run_id,
        request.abstraction_tier,
        &request.decision,
        request.save_preference,
    )
    .await
    .map_err(|e| e.to_string())?;

    // If user chose to save, write standing preference to personas.extra_metadata
    // in shared.db (D5-152). Scoped to abstraction_tier -- not a blanket consent.
    if request.save_preference && request.decision == "proceed" {
        write_floor_consent_preference(
            &request.persona_id,
            request.abstraction_tier,
        )
        .await
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[tauri::command]
pub async fn submit_element_consent_decision(
    request: SubmitElementConsentDecisionRequest,
) -> Result<(), String> {
    output_store::write_element_consent_decisions(
        &request.user_id,
        &request.persona_id,
        &request.key_hex,
        &request.run_id,
        &request.decisions_json,
    )
    .await
    .map_err(|e| e.to_string())
}

/// Process user decisions for extract-and-confirm candidates (item 20).
///
/// Validation: confirmed==true with confirmed_value==None rejects entire call
/// before any DB mutation (IPC contract violation).
///
/// Processing set: pending candidates + confirmed-but-unpersisted (crash-recovery
/// targets). Both are included so retries can complete interrupted work.
///
/// Write sequence per confirmed pending candidate:
///   1. get_candidate_fields()    -> read field_name + sensitivity from outputs.db
///   2. persist_confirmed_field() -> COMMIT personal.db (idempotent upsert)
///   3. mark_candidate_decided()  -> COMMIT outputs.db (sets confirmed_at)
///   4. set_persisted_at()        -> COMMIT outputs.db
///
/// Ordering rationale: personal.db write first so that on retry, if step 3/4
/// failed, we re-attempt an idempotent upsert rather than leaving candidate
/// marked decided with no persisted field.
///
/// After all decisions: verify no confirmed-but-unpersisted rows remain,
/// then set focus_runs.status='complete' and emit run_status_update.
#[tauri::command]
pub async fn submit_extract_confirm(
    app_handle: tauri::AppHandle,
    request: SubmitExtractConfirmRequest,
) -> Result<(), String> {
    use tauri::Emitter;

    // Verify run is in the expected state.
    let status = get_focus_run_status(
        &request.user_id,
        &request.persona_id,
        &request.key_hex,
        &request.run_id,
    )
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "not_found".to_string())?;

    if status != "awaiting_extract_confirm" {
        return Err(format!(
            "run {} is not awaiting_extract_confirm (status: {})",
            request.run_id, status
        ));
    }

    // Deserialize decisions.
    let decisions: Vec<ExtractConfirmDecision> =
        serde_json::from_str(&request.decisions_json)
            .map_err(|e| format!("decisions_json parse error: {e}"))?;

    // Validation pass: reject entire call before any DB mutation.
    for d in &decisions {
        if d.confirmed && d.confirmed_value.is_none() {
            return Err(format!(
                "candidate {} has confirmed=true but confirmed_value is None \
                 -- call rejected (IPC contract violation)",
                d.candidate_id
            ));
        }
    }

    // Build processing set: pending candidates + confirmed-but-unpersisted
    // (crash-recovery targets). Both need work; idempotency handles safe retry.
    let pending_candidates = extract::load_pending_candidates(
        &request.user_id,
        &request.persona_id,
        &request.key_hex,
        &request.run_id,
    )
    .await
    .map_err(|e| e.to_string())?;

    let unrecovered_before = extract::load_unrecovered_rows(
        &request.user_id,
        &request.persona_id,
        &request.key_hex,
        &request.run_id,
    )
    .await
    .map_err(|e| e.to_string())?;

    let mut needs_work_ids: std::collections::HashSet<i64> =
        pending_candidates.iter().map(|c| c.id).collect();
    // Also include confirmed-but-unpersisted so retries can complete them.
    for (candidate_id, _, _, _) in &unrecovered_before {
        needs_work_ids.insert(*candidate_id);
    }

    // Write sequence for each decision.
    for d in &decisions {
        if !needs_work_ids.contains(&d.candidate_id) {
            log::debug!(
                "submit_extract_confirm: candidate {} already complete, skipping",
                d.candidate_id
            );
            continue;
        }

        if d.confirmed {
            // Step 1: read field_name + sensitivity from DB (never trust frontend).
            let fields = extract::get_candidate_fields(
                &request.user_id,
                &request.persona_id,
                &request.key_hex,
                d.candidate_id,
            )
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!(
                "candidate {} not found during confirmation",
                d.candidate_id
            ))?;

            // Step 2: write to personal.db first (idempotent upsert).
            // Ordered before mark_decided so retry after step 3/4 failure
            // re-attempts an idempotent upsert, not a ghost decided state.
            extract::persist_confirmed_field(
                &request.user_id,
                &request.persona_id,
                &request.key_hex,
                &fields.field_name,
                d.confirmed_value
                    .as_deref()
                    .ok_or_else(|| format!(
                        "candidate {} confirmed_value is None after validation \
                         -- invariant violated",
                        d.candidate_id
                    ))?,
                &fields.sensitivity,
            )
            .await
            .map_err(|e| e.to_string())?;

            // Step 3: mark decided in outputs.db.
            extract::mark_candidate_decided(
                &request.user_id,
                &request.persona_id,
                &request.key_hex,
                d.candidate_id,
                true,
                d.confirmed_value.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())?;

            // Step 4: mark persisted in outputs.db.
            extract::set_persisted_at(
                &request.user_id,
                &request.persona_id,
                &request.key_hex,
                d.candidate_id,
            )
            .await
            .map_err(|e| e.to_string())?;
        } else {
            // Declined: mark decided only, no personal.db write.
            extract::mark_candidate_decided(
                &request.user_id,
                &request.persona_id,
                &request.key_hex,
                d.candidate_id,
                false,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        }
    }

    // Post-loop guard: verify no confirmed-but-unpersisted rows remain.
    // If any exist, a step 3/4 failure occurred. Return Err so frontend can
    // retry -- status stays awaiting_extract_confirm.
    let unrecovered_after = extract::load_unrecovered_rows(
        &request.user_id,
        &request.persona_id,
        &request.key_hex,
        &request.run_id,
    )
    .await
    .map_err(|e| e.to_string())?;

    if !unrecovered_after.is_empty() {
        return Err(format!(
            "submit_extract_confirm: {} candidate(s) confirmed but not persisted \
             -- retry required",
            unrecovered_after.len()
        ));
    }

    // All decisions written and verified. Set run status to complete.
    set_focus_run_status(
        &request.user_id,
        &request.persona_id,
        &request.key_hex,
        &request.run_id,
        "complete",
    )
    .await
    .map_err(|e| e.to_string())?;

    // Emit run_status_update so frontend knows the run is done.
    let payload = serde_json::json!({
        "focus_run_id": request.run_id,
        "status": "complete",
        "current_step": 0,
        "total_steps": 0,
        "step_display_name": serde_json::Value::Null,
    });
    if let Err(e) = app_handle.emit("run-status-update", &payload) {
        log::warn!("submit_extract_confirm: emit run-status-update failed: {e}");
    }

    Ok(())
}

/// Pre-Focus-start query: which cross_persona_export=true entity_facts rows
/// exist for this persona and require per-session confirmation before a
/// Focus run may include them (decisions.id=546, decisions.id=639, items.id=27).
///
/// Called by the frontend BEFORE FocusRun::new() is constructed. Read-only --
/// this command does not write a decision anywhere. The frontend shows a
/// confirmation UI for the returned facts, then passes the fact_ids the user
/// approved into SubmitFocusRunRequest.confirmed_cross_persona_fact_ids.
/// Facts not included there are treated as declined by
/// apply_entity_fact_provenance_check() -- omitted from context, not a
/// hard block (Jason, this session: declining one fact should not block
/// the whole run).
#[tauri::command]
pub async fn get_pending_cross_persona_confirmations(
    request: GetPendingCrossPersonaConfirmationsRequest,
) -> Result<Vec<PendingCrossPersonaFact>, String> {
    let facts = personal_store::list_pending_cross_persona_exports(
        &request.user_id,
        &request.persona_id,
        &request.key_hex,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(facts
        .into_iter()
        .map(|f| PendingCrossPersonaFact {
            fact_id: f.id,
            entity_id: f.entity_id,
            field_name: f.field_name,
            sensitivity: f.sensitivity,
            origin_persona_id: f.origin_persona_id,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write floor_consent_preference to personas.extra_metadata in shared.db.
/// D5-152: scoped to abstraction_tier. Schema:
///   {"mode": "modified", "abstraction_tier": N,
///    "consent_timestamp": "...", "consent_version": "1"}
/// shared.db is unencrypted (instance-level, not per-persona encrypted).
///
/// extra_metadata is fetched as Option<String> -- the column may be NULL for
/// newly created personas. A NULL or malformed value is treated as an empty
/// object so the preference write proceeds without data loss.
async fn write_floor_consent_preference(
    persona_id: &str,
    abstraction_tier: i32,
) -> Result<(), sqlx::Error> {
    let db_path = db_path_shared();
    let mut conn = connect_options_unencrypted(&db_path)
        .connect()
        .await?;

    let timestamp = now();
    let preference = serde_json::json!({
        "mode": "modified",
        "abstraction_tier": abstraction_tier,
        "consent_timestamp": timestamp,
        "consent_version": "1"
    });

    // Fetch as Option<String> -- NULL extra_metadata is valid for new personas.
    let existing_json: Option<String> = sqlx::query(
        "SELECT extra_metadata FROM personas WHERE id = ?",
    )
    .bind(persona_id)
    .fetch_one(&mut conn)
    .await?
    .try_get("extra_metadata")?;

    // Merge into existing metadata rather than overwriting the whole field.
    // Falls back to empty object if the column is NULL or contains invalid JSON.
    let mut meta: serde_json::Value = existing_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    meta["floor_consent_preference"] = preference;

    sqlx::query(
        "UPDATE personas SET extra_metadata = ? WHERE id = ?",
    )
    .bind(meta.to_string())
    .bind(persona_id)
    .execute(&mut conn)
    .await?;

    Ok(())
}
