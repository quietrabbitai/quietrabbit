// src-tauri/src/conductor/privacy/gate2.rs
//
// PG_GATE_2: inbound response scan for personal field value leakage.
// Mirrors Python PrivacyGateway.gate2() exactly.
//
// Unicode case-folding parity (Hotspot):
//   Python: value.lower() in response_content.lower()
//   Rust:   value.to_lowercase() in response_lower
//   Oracle-confirmed behaviours -- do NOT alter:
//   - German eszett: "Straße".to_lowercase() == "straße"; "STRASSE" does NOT match.
//   - NFC/NFD mismatch: neither Python nor Rust normalises -- forms must match.
//   - Turkish I: Python and Rust both use Unicode tables, not locale.
//
// MIN_MATCH_LENGTH = 4: fields with value.chars().count() < 4 are not scanned.
// Note: Python uses len() which is byte count for ASCII but char count for ASCII;
//   for the values in scope (names, cities, streets) char count == byte count.
//   Using chars().count() is the correct Rust equivalent.
//
// Disclosure log written ONLY when flagged=true (mirrors Python).
// Fatality split (matches gate1): T1 log write failure is non-fatal (swallow,
// continue). T2+ log write failure is fatal (halt execution). The split lives
// here in the gate function, not in the logger implementation.

use indexmap::IndexMap;

use super::{
    errors::DisclosureLogWriteError,
    logger::{DisclosureLogEntry, DisclosureLogger},
    types::{Gate2Result, PersonalTrack},
};

const MIN_MATCH_LENGTH: usize = 4;

#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn gate2<L: DisclosureLogger>(
    logger: &L,
    step_id: &str,
    focus_run_id: &str,
    response_content: &str,
    personal_track: &PersonalTrack,
    execution_tier: u8,
    provider: Option<String>,
    fields_shared: Option<&[String]>,
) -> Result<Gate2Result, DisclosureLogWriteError> {
    let response_lower = response_content.to_lowercase();

    // Build shared-field exclusion set (None = no exclusion, all fields scanned).
    let shared_set: Option<std::collections::HashSet<&str>> =
        fields_shared.map(|s| s.iter().map(|x| x.as_str()).collect());

    let mut matched: Vec<String> = Vec::new();

    for (field_name, personal_field) in personal_track.fields() {
        // Skip fields already intentionally shared.
        if let Some(ref set) = shared_set {
            if set.contains(field_name.as_str()) {
                continue;
            }
        }

        let value = &personal_field.field_value;

        // Skip values below minimum match length.
        if value.chars().count() < MIN_MATCH_LENGTH {
            continue;
        }

        // Case-insensitive substring match.
        if response_lower.contains(&value.to_lowercase()) {
            matched.push(field_name.clone());
        }
    }

    let flagged = !matched.is_empty();

    // Disclosure log written only on flagged result.
    // Fatality split: T1 non-fatal (swallow), T2+ fatal (propagate).
    if flagged {
        let write_result = logger
            .write(DisclosureLogEntry {
                step_id: step_id.to_string(),
                focus_run_id: focus_run_id.to_string(),
                execution_tier,
                abstraction_tier: None,
                provider,
                fields_shared: vec![],
                fields_abstracted: IndexMap::new(),
                fields_withheld: matched.clone(),
                override_declined: false,
                event_type: "gate2_contamination_detected".to_string(),
            })
            .await;
        if let Err(e) = write_result {
            if execution_tier > 1 {
                return Err(e); // FATAL at tier 2+
            }
            // Non-fatal at tier 1: swallow, continue.
        }
    }

    Ok(Gate2Result {
        flagged,
        matched_field_names: matched,
    })
}
