// src-tauri/src/conductor/privacy/gate1.rs
//
// PG_GATE_1: field approval and abstraction.
// Mirrors Python PrivacyGateway.gate1() exactly.
//
// INVARIANT: Always writes disclosure_log, even for empty PersonalTrack (D5-073).
// INVARIANT: Raw personal field values NEVER written to disclosure log.
// INVARIANT: Write-before-send ordering enforced -- logger.write().await completes
//            before this function returns. Caller cannot proceed without log.
//
// Fatality split (ADR-012, D5-091):
//   execution_tier == 1 -> disclosure log write failure is non-fatal.
//                          Gate swallows error, returns empty log id, continues.
//   execution_tier >  1 -> disclosure log write failure is FATAL.
//                          Gate propagates DisclosureLogWriteError, run halts.
// The split lives here in the gate function, not in the logger implementation.

use indexmap::IndexMap;

use super::{
    abstraction::apply_abstraction,
    errors::DisclosureLogWriteError,
    logger::{DisclosureLogEntry, DisclosureLogger},
    types::{Gate1Result, PersonalTrack},
};

#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn gate1<L: DisclosureLogger>(
    logger: &L,
    step_id: &str,
    focus_run_id: &str,
    personal_track: &PersonalTrack,
    abstraction_tier: u8,
    raw_abstraction: u8,
    execution_tier: u8,
    provider: Option<String>,
) -> Result<Gate1Result, DisclosureLogWriteError> {
    let mut approved: IndexMap<String, String> = IndexMap::new();
    let mut withheld: Vec<String>              = Vec::new();

    // Evaluate every field in insertion order (IndexMap preserves order).
    for (field_name, personal_field) in personal_track.fields() {
        match apply_abstraction(personal_field, abstraction_tier) {
            Some(abstracted) => { approved.insert(field_name.clone(), abstracted); }
            None             => { withheld.push(field_name.clone()); }
        }
    }

    // Floor-clamp detection (ADR-012 Amendment 3).
    // Only runs when raw_abstraction differs from abstraction_tier.
    let mut floor_clamped: Vec<String> = Vec::new();
    if raw_abstraction != abstraction_tier {
        for (field_name, personal_field) in personal_track.fields() {
            let raw_result     = apply_abstraction(personal_field, raw_abstraction);
            let clamped_result = approved.get(field_name).cloned();
            if raw_result != clamped_result {
                floor_clamped.push(field_name.clone());
            }
        }
    }

    // fields_shared: approved fields whose value equals the raw field_value
    // (pass-through -- no transformation applied at this tier).
    let fields_shared: Vec<String> = personal_track
        .fields()
        .iter()
        .filter(|(name, pf)| {
            approved.get(*name).map(|v| v == &pf.field_value).unwrap_or(false)
        })
        .map(|(name, _)| name.clone())
        .collect();

    // fields_abstracted: approved fields that are NOT pass-through.
    let fields_abstracted: IndexMap<String, String> = approved
        .iter()
        .filter(|(name, _)| !fields_shared.contains(name))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Write disclosure log BEFORE returning result (write-before-send invariant).
    // Fatality split applied here: tier 1 swallows, tier 2+ propagates.
    let log_id = {
        let result = logger
            .write(DisclosureLogEntry {
                step_id:           step_id.to_string(),
                focus_run_id:      focus_run_id.to_string(),
                execution_tier,
                abstraction_tier:  Some(abstraction_tier),
                provider,
                fields_shared:     fields_shared.clone(),
                fields_abstracted: fields_abstracted.clone(),
                fields_withheld:   withheld.clone(),
                override_declined: false,
                event_type:        "gate1_pass".to_string(),
            })
            .await;

        match result {
            Ok(id) => id,
            Err(e) => {
                if execution_tier > 1 {
                    return Err(e);   // FATAL at tier 2+
                }
                // Non-fatal at tier 1: swallow, use empty sentinel.
                String::new()
            }
        }
    };

    Ok(Gate1Result {
        approved_fields:      approved,
        withheld_fields:      withheld,
        fields_shared,
        floor_clamped_fields: floor_clamped,
        disclosure_log_id:    log_id,
        blocked:              false,
    })
}
