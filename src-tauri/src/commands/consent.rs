// src-tauri/src/commands/consent.rs
//
// Group 2 — Consent and privacy gates.
// Commands: submit_consent_decision, submit_floor_consent_decision.
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
// Both commands are fire-and-respond: the lifecycle checks the consent_decisions
// table when the run is resumed. No direct signalling into the background task.

use serde::Deserialize;
use specta::Type;
use sqlx::ConnectOptions;
use sqlx::Row;

use crate::persistence::output_store;
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
    // in shared.db (D5-152). Scoped to abstraction_tier — not a blanket consent.
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write floor_consent_preference to personas.extra_metadata in shared.db.
/// D5-152: scoped to abstraction_tier. Schema:
///   {"mode": "modified", "abstraction_tier": N,
///    "consent_timestamp": "...", "consent_version": "1"}
/// shared.db is unencrypted (instance-level, not per-persona encrypted).
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

    // Merge into existing extra_metadata rather than overwriting the whole field.
    let existing_json: String = sqlx::query(
        "SELECT extra_metadata FROM personas WHERE id = ?",
    )
    .bind(persona_id)
    .fetch_one(&mut conn)
    .await?
    .try_get("extra_metadata")?;

    let mut meta: serde_json::Value =
        serde_json::from_str(&existing_json).unwrap_or(serde_json::json!({}));

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
