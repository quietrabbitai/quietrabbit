// src-tauri/src/commands/shared_log_deck.rs
//
// items.id=957 unit 2 (decisions.id=967; design record items.id=956): the event
// deck and the field-label map of the "What QR shared" view. PURE: no I/O.
//
// WHY IT RUNS IN RUST: raw event strings and raw field names never cross the IPC
// boundary. The command (commands/shared_log.rs) returns a sentence KEY and
// field LABELS only; the words live in en.json under navShell.shared.*
// (placeholder copy, Chat-BRAND owns the wording).
//
// PLACEMENT (Chat-PM rulings on items.id=957, 2026-11-22):
//   Sent          what left this computer
//   KeptBack      something was stopped or held back, and nothing was sent
//   Choice        a consent record (consent_audit_records)
//   Hidden        background checks, shown only under "Show background checks"
//   Excluded      written at tier 1, so it never left this computer: not listed
// Decisions that differ from the first design deck:
//   gate3_source_spans_unreviewed   KeptBack (the content is blocked)
//   voice_profile_contamination     KeptBack (a hosted request is halted, and a
//                                   local step is excluded by the tier rule)
//   gate2_contamination_detected    Hidden (the reply is kept and flagged:
//                                   executor.rs reply_gate2)
//   output_scan_full                KeptBack only when override_declined = 1
//                                   (a blocked export), otherwise Hidden
//   output_scan_light, _light_legacy, _pf_error, _pf_unavailable,
//   _full_legacy                    Hidden
//   provenance_cross_persona_omitted  Excluded (written at execution_tier 1)
//   gate3_consent_pending           Hidden: found by the writer scan after the plan (it is
//                                   written when a review is raised, before the prompt;
//                                   a judgment call for Chat-PM to confirm)

use crate::conductor::personal_fact_fields;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Sent,
    KeptBack,
    Choice,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classified {
    /// Not listed: it never left this computer.
    Excluded,
    Item {
        tag: Tag,
        key: &'static str,
    },
}

/// Written at tier 1, so the tier rule drops them. Kept as an explicit list so the
/// every-event-has-a-sentence test can tell "excluded on purpose" from "forgotten".
pub const EXCLUDED_BY_TIER: [&str; 1] = ["provenance_cross_persona_omitted"];

/// Every disclosure_log event_type a writer emits (read from the writers and pinned
/// by a source scan: gate1, gate2, gate3, executor, output_scan, lifecycle).
pub const DISCLOSURE_EVENTS: [&str; 24] = [
    "gate1_pass",
    "gate2_contamination_detected",
    "gate3_tier_ceiling_block",
    "gate3_source_spans_unreviewed",
    "gate3_sensitivity_block",
    "gate3_promotion_approved",
    "gate3_pf_error_block",
    "gate3_pf_too_long",
    "gate3_pf_unavailable_block",
    "gate3_pf_no_spans",
    "gate3_pf_no_spans_forced_review",
    "gate3_consent_pending",
    "gate3_fact_reapplied",
    "gate_timeout",
    "floor_consent_auto",
    "voice_profile_contamination",
    "provenance_cross_persona_omitted",
    "output_scan_timeout",
    "output_scan_light",
    "output_scan_light_legacy",
    "output_scan_full",
    "output_scan_full_legacy",
    "output_scan_pf_error",
    "output_scan_pf_unavailable",
];

/// The sentence of an unknown or empty event type.
pub const UNKNOWN_KEY: &str = "bg.unknown";

pub fn classify_disclosure(event_type: &str, override_declined: bool) -> Classified {
    use Tag::*;
    let item = |tag, key| Classified::Item { tag, key };
    match event_type {
        // Sent
        "gate1_pass" => item(Sent, "sent.plain"),
        "gate3_promotion_approved" => item(Sent, "sent.reviewed"),
        "gate3_pf_no_spans" => item(Sent, "sent.noReview"),
        "gate3_fact_reapplied" => item(Sent, "sent.reapplied"),
        // Kept back
        "gate3_tier_ceiling_block" => item(KeptBack, "held.tierCeiling"),
        "gate3_sensitivity_block" => item(KeptBack, "held.sensitivity"),
        "gate3_pf_unavailable_block" => item(KeptBack, "held.pfUnavailable"),
        "gate3_pf_error_block" => item(KeptBack, "held.pfError"),
        "gate3_pf_too_long" => item(KeptBack, "held.tooLong"),
        "gate_timeout" => item(KeptBack, "held.timeout"),
        "gate3_source_spans_unreviewed" => item(KeptBack, "held.spansUnreviewed"),
        "voice_profile_contamination" => item(KeptBack, "held.voiceProfile"),
        "output_scan_full" if override_declined => item(KeptBack, "held.exportHeld"),
        // Hidden background checks
        "gate2_contamination_detected" => item(Hidden, "bg.replyFlagged"),
        "floor_consent_auto" => item(Hidden, "bg.floorApplied"),
        "output_scan_timeout" => item(Hidden, "bg.scanTimeout"),
        "gate3_pf_no_spans_forced_review" => item(Hidden, "bg.forcedReview"),
        "gate3_consent_pending" => item(Hidden, "bg.reviewRaised"),
        "output_scan_light"
        | "output_scan_light_legacy"
        | "output_scan_full"
        | "output_scan_full_legacy" => item(Hidden, "bg.scanRan"),
        "output_scan_pf_error" | "output_scan_pf_unavailable" => item(Hidden, "bg.scanNotRun"),
        e if EXCLUDED_BY_TIER.contains(&e) => Classified::Excluded,
        _ => item(Hidden, UNKNOWN_KEY),
    }
}

/// True for the keys that count as "a send" in a group's summary line. A reapplied
/// choice is shown as a Sent item but is not itself a send.
pub fn counts_as_send(key: &str) -> bool {
    matches!(key, "sent.plain" | "sent.reviewed" | "sent.noReview")
}

/// Every consent event type of consent_audit_store::ConsentEventType, plus the
/// history cleared marker (written by the purge, unit 3). A string this version
/// does not know gets the unknown background sentence.
pub const CONSENT_EVENTS: [&str; 9] = [
    "collection_consent_granted",
    "collection_consent_withdrawn",
    "document_ceiling_raised",
    "tier3_generation",
    "tier3_validation",
    "sensitivity_override",
    "standing_consent_use",
    "output_privacy_disclosure",
    HISTORY_CLEARED,
];

pub const HISTORY_CLEARED: &str = "history_cleared";

/// None for history_cleared (it is the permanent "cleared on" line, not an item).
pub fn classify_consent(event_type: &str, has_reference: bool) -> Option<(Tag, &'static str)> {
    let choice = |key| Some((Tag::Choice, key));
    match event_type {
        "collection_consent_granted" if has_reference => choice("choice.regranted"),
        "collection_consent_granted" => choice("choice.granted"),
        "collection_consent_withdrawn" => choice("choice.withdrawn"),
        "document_ceiling_raised" => choice("choice.ceilingRaised"),
        "standing_consent_use" => choice("choice.standingUsed"),
        "sensitivity_override" => choice("choice.sensitiveOverride"),
        "tier3_generation" => choice("choice.generated"),
        "tier3_validation" => choice("choice.checked"),
        "output_privacy_disclosure" => choice("choice.outputChecked"),
        HISTORY_CLEARED => None,
        _ => Some((Tag::Hidden, UNKNOWN_KEY)),
    }
}

// ---------------------------------------------------------------------------
// Field labels
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelKind {
    /// A catalogued fact field: `text` is its label from personal_fact_fields.json.
    Catalogued,
    Draft,
    Message,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub kind: LabelKind,
    pub text: Option<String>,
}

/// One raw field name to its label. A raw name is never returned: anything not in
/// the catalogue and not a content key is `Other` ("Other details").
pub fn label_for(name: &str) -> Label {
    if let Ok(cat) = personal_fact_fields::registry() {
        if let Some(def) = cat.field(name) {
            return Label {
                kind: LabelKind::Catalogued,
                text: Some(def.label.clone()),
            };
        }
    }
    let lower = name.to_lowercase();
    let kind = if lower.contains("draft") {
        LabelKind::Draft
    } else if lower.contains("message") {
        LabelKind::Message
    } else {
        LabelKind::Other
    };
    Label { kind, text: None }
}

/// Labels of a list of names, in order, without repeats ("Other details" at most
/// once).
pub fn labels_for(names: &[String]) -> Vec<Label> {
    let mut out: Vec<Label> = Vec::new();
    for n in names {
        let l = label_for(n);
        if !out.contains(&l) {
            out.push(l);
        }
    }
    out
}

/// The display name of a Writing Assistant document type, or None for an id this
/// version does not list.
pub fn document_type_name(id: &str) -> Option<String> {
    static NAMES: std::sync::OnceLock<std::collections::HashMap<String, String>> =
        std::sync::OnceLock::new();
    NAMES
        .get_or_init(|| {
            let v: serde_json::Value =
                serde_json::from_str(include_str!("../../block_data/document_types.json"))
                    .unwrap_or_default();
            v.get("types")
                .and_then(|t| t.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|t| {
                            Some((
                                t.get("id")?.as_str()?.to_owned(),
                                t.get("name")?.as_str()?.to_owned(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .get(id)
        .cloned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::consent_audit_store::ConsentEventType;

    /// Every string a writer emits as an event type, found by scanning the writers'
    /// own source: `event_type: "x"` and the `=> "output_scan_..."` match arms.
    fn writer_event_strings() -> std::collections::BTreeSet<String> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/conductor");
        let mut found = std::collections::BTreeSet::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&p).unwrap();
                    for line in text.lines() {
                        let t = line.trim_start();
                        if t.starts_with("//") {
                            continue;
                        }
                        if let Some(rest) = t.strip_prefix("event_type: \"") {
                            if let Some(end) = rest.find('"') {
                                found.insert(rest[..end].to_owned());
                            }
                        }
                        if let Some(rest) = t.split("=> \"output_scan_").nth(1) {
                            if let Some(end) = rest.find('"') {
                                found.insert(format!("output_scan_{}", &rest[..end]));
                            }
                        }
                        // A type chosen by an if/else is a bare literal on its own line.
                        if let Some(rest) = t.strip_prefix("\"gate3_") {
                            if let Some(end) = rest.find('"') {
                                found.insert(format!("gate3_{}", &rest[..end]));
                            }
                        }
                        if let Some(rest) = t.strip_prefix("\"output_scan_") {
                            if let Some(end) = rest.find('"') {
                                found.insert(format!("output_scan_{}", &rest[..end]));
                            }
                        }
                    }
                }
            }
        }
        found
    }

    #[test]
    fn every_event_a_writer_emits_has_a_sentence_or_an_explicit_exclusion() {
        let written = writer_event_strings();
        // The scan sees at least the known writers (guards the scan itself).
        assert!(written.contains("gate1_pass") && written.contains("output_scan_full"));
        for ev in &written {
            // Strings only a test uses are not writers: test code assigns the
            // same event names, never new ones, so any unknown here is a writer.
            if !DISCLOSURE_EVENTS.contains(&ev.as_str()) {
                panic!("a writer emits `{ev}`, which has no entry in DISCLOSURE_EVENTS");
            }
        }
        for ev in DISCLOSURE_EVENTS {
            for declined in [false, true] {
                match classify_disclosure(ev, declined) {
                    Classified::Excluded => assert!(
                        EXCLUDED_BY_TIER.contains(&ev),
                        "`{ev}` is excluded but not on the excluded-by-tier list"
                    ),
                    Classified::Item { key, .. } => {
                        assert_ne!(key, UNKNOWN_KEY, "`{ev}` falls to the unknown sentence")
                    }
                }
            }
        }
        // The deck lists nothing no writer emits.
        for ev in DISCLOSURE_EVENTS {
            assert!(
                written.contains(ev),
                "`{ev}` is in the deck but no writer emits it"
            );
        }
    }

    #[test]
    fn every_consent_type_has_a_sentence_and_the_unknown_one_is_never_silent() {
        for t in ConsentEventType::ALL {
            if t == ConsentEventType::HistoryCleared {
                continue; // the permanent cleared-on line, asserted as None below
            }
            let c = classify_consent(t.as_str(), false);
            assert!(
                matches!(c, Some((Tag::Choice, k)) if k != UNKNOWN_KEY),
                "{}",
                t.as_str()
            );
            assert!(CONSENT_EVENTS.contains(&t.as_str()));
        }
        assert_eq!(classify_consent(HISTORY_CLEARED, false), None);
        assert_eq!(
            classify_consent("something_new", false),
            Some((Tag::Hidden, UNKNOWN_KEY))
        );
        assert_eq!(
            classify_consent("collection_consent_granted", true),
            Some((Tag::Choice, "choice.regranted"))
        );
        assert_eq!(
            classify_disclosure("", false),
            Classified::Item {
                tag: Tag::Hidden,
                key: UNKNOWN_KEY
            }
        );
    }

    #[test]
    fn rulings_hold_for_the_contested_placements() {
        let tag = |e, d| match classify_disclosure(e, d) {
            Classified::Item { tag, .. } => Some(tag),
            Classified::Excluded => None,
        };
        assert_eq!(
            tag("gate3_source_spans_unreviewed", true),
            Some(Tag::KeptBack)
        );
        assert_eq!(
            tag("voice_profile_contamination", false),
            Some(Tag::KeptBack)
        );
        assert_eq!(
            tag("gate2_contamination_detected", false),
            Some(Tag::Hidden)
        );
        assert_eq!(tag("output_scan_full", true), Some(Tag::KeptBack));
        assert_eq!(tag("output_scan_full", false), Some(Tag::Hidden));
        assert_eq!(tag("provenance_cross_persona_omitted", false), None);
        assert!(counts_as_send("sent.plain") && !counts_as_send("sent.reapplied"));
    }

    #[test]
    fn every_field_name_gets_a_label_or_the_fallback_and_never_itself() {
        let l = label_for("location_city");
        assert_eq!(
            (l.kind, l.text.as_deref()),
            (LabelKind::Catalogued, Some("City"))
        );
        assert_eq!(label_for("job_title").text.as_deref(), Some("Job title"));
        assert_eq!(label_for("employer").text.as_deref(), Some("Employer"));
        assert_eq!(label_for("industry").text.as_deref(), Some("Industry"));
        assert_eq!(label_for("draft").kind, LabelKind::Draft);
        assert_eq!(label_for("cover_letter_draft").kind, LabelKind::Draft);
        assert_eq!(label_for("message").kind, LabelKind::Message);
        // Anything else, including an entity id pair or a device field, is Other.
        for raw in [
            "",
            "x9-secret-field",
            "entity-id:serial_number",
            "serial_number",
        ] {
            let l = label_for(raw);
            assert_eq!((l.kind, l.text), (LabelKind::Other, None), "{raw}");
        }
        // "Other details" appears once however many unknown names there are.
        let many = labels_for(&["a".into(), "b".into(), "location_city".into(), "c".into()]);
        assert_eq!(many.len(), 2);
    }

    #[test]
    fn a_document_type_resolves_to_its_name_or_none() {
        assert_eq!(
            document_type_name("cover_letter").as_deref(),
            Some("Cover letter")
        );
        assert_eq!(document_type_name("not-a-type"), None);
    }
}
