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
// Fatality split (matches gate1): qr_local log write failure is non-fatal (swallow,
// continue). External-access-live log write failure is fatal (halt execution).
// The split lives here in the gate function, not in the logger implementation.
// items.id=528 Phase 2: re-keyed from execution_tier > 1 to effective_access
// != LocalOnly -- execution_tier itself is kept as a separate parameter
// purely for the disclosure log's own numeric audit field below, not for
// this decision.
//
// items.id=735 (decisions.id=883): the Err above is the GATE's contract and is
// unchanged, but the executor no longer fails a step on it. A reply from an
// external provider is flagged, never held, discarded or edited, so
// executor.rs::reply_gate2 turns that Err into "flagged, audit unavailable".

use indexmap::IndexMap;

use crate::conductor::tokens::ExternalAccess;

use super::{
    errors::DisclosureLogWriteError,
    logger::{DisclosureLogEntry, DisclosureLogger},
    types::{Gate2Result, PersonalTrack},
};

const MIN_MATCH_LENGTH: usize = 4;

/// The pure scan behind gate2(): every field not in `fields_shared` whose
/// value (4+ characters) appears in the response, case-insensitively, as
/// (field name, sensitivity_severity). No I/O. items.id=735 split it out so a
/// caller whose disclosure-log write failed can still learn the matched
/// severity; gate2() itself is behaviour-identical.
pub fn scan_matches(
    response_content: &str,
    personal_track: &PersonalTrack,
    fields_shared: Option<&[String]>,
) -> Vec<(String, u8)> {
    let response_lower = response_content.to_lowercase();

    // Build shared-field exclusion set (None = no exclusion, all fields scanned).
    let shared_set: Option<std::collections::HashSet<&str>> =
        fields_shared.map(|s| s.iter().map(|x| x.as_str()).collect());

    let mut matched: Vec<(String, u8)> = Vec::new();

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
            matched.push((field_name.clone(), personal_field.sensitivity_severity));
        }
    }

    matched
}

#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn gate2<L: DisclosureLogger>(
    logger: &L,
    step_id: &str,
    focus_run_id: &str,
    response_content: &str,
    personal_track: &PersonalTrack,
    execution_tier: u8,
    effective_access: ExternalAccess,
    provider: Option<String>,
    fields_shared: Option<&[String]>,
) -> Result<Gate2Result, DisclosureLogWriteError> {
    let scanned = scan_matches(response_content, personal_track, fields_shared);
    let matched: Vec<String> = scanned.iter().map(|(name, _)| name.clone()).collect();
    let matched_max_severity = scanned.iter().map(|(_, sev)| *sev).max().unwrap_or(0);

    let flagged = !matched.is_empty();

    // Disclosure log written only on flagged result.
    // Fatality split: qr_local non-fatal (swallow), external access live fatal (propagate).
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
                category: None,
                fact_key: None,
            })
            .await;
        if let Err(e) = write_result {
            if effective_access != ExternalAccess::LocalOnly {
                return Err(e); // FATAL when external access is live
            }
            // Non-fatal at qr_local: swallow, continue.
        }
    }

    Ok(Gate2Result {
        flagged,
        matched_field_names: matched,
        matched_max_severity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::privacy::types::{AbstractionPolicy, PersonalField, Sensitivity};

    fn track(fields: &[(&str, &str, u8)]) -> PersonalTrack {
        let mut t = PersonalTrack::new();
        for (name, value, severity) in fields {
            t.add_field(PersonalField {
                field_name: (*name).to_owned(),
                field_value: (*value).to_owned(),
                sensitivity: Sensitivity::General,
                sensitivity_severity: *severity,
                source_id: "test".to_owned(),
                abstraction_tier2: AbstractionPolicy::Omit,
                abstraction_tier3: AbstractionPolicy::Omit,
            })
            .unwrap();
        }
        t.seal();
        t
    }

    #[test]
    fn scan_matches_reports_each_matched_field_with_its_severity() {
        let t = track(&[
            ("home_city", "Springfield", 2),
            ("diagnosis", "Hypertension", 3),
            ("balance", "$12000", 4),
            ("nickname", "Bo", 1),
        ]);
        let hits = scan_matches(
            "You live in springfield and have HYPERTENSION. Bo!",
            &t,
            None,
        );
        assert_eq!(
            hits,
            vec![("home_city".to_owned(), 2), ("diagnosis".to_owned(), 3)]
        );
    }

    #[test]
    fn scan_matches_skips_shared_fields_and_finds_nothing_in_a_clean_reply() {
        let t = track(&[("home_city", "Springfield", 2)]);
        let shared = vec!["home_city".to_owned()];
        assert!(scan_matches("Springfield", &t, Some(&shared)).is_empty());
        assert!(scan_matches("Nothing personal here", &t, None).is_empty());
    }
}
