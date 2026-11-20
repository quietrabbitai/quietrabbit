// src-tauri/src/conductor/carry_scan.rs
//
// items.id=933 (M2, ruled by Chat-PM 2026-11-20): the carry scan of a hosted step.
//
// Since items.id=933 a LOCAL step receives its declared personal fields raw (abstraction
// tier 1). Its output can then carry a value into a later HOSTED step through an output
// variable or {previous_output}, where the user's tier-2 policy (omit by default) would
// otherwise never let that value go. Gate 3 at the local step (severity 2) approves it,
// and the outbound review sees only what the Privacy Filter detects (it has no label for
// an employer, an industry or a job title). This scan closes that path for the bridged
// personal facts (items.id=931): it is PURE (no I/O) and the executor calls it once per
// hosted attempt, after the render and before the outbound review.
//
// RULES:
// - TRIGGER: gate2::scan_matches (Gate 2's match rule: case-insensitive substring, at
//   least 4 characters) over each CARRIED value the template names (an output variable,
//   {previous_output}), never {user_input} and never the step's own projected fields.
//   Only bridged fields (personal_fact_bridge::is_bridged_source) count, and a field this step
//   released RAW (g1.fields_shared: an approved value equal to the raw value; a
//   summarize label is not in it) is skipped.
// - ACTION, by the field's policy at the step's tier (tier 2 when the step's abstraction
//   tier is 1): not_permitted -> every occurrence is REMOVED before the review ("[removed]")
//   and never shown, so no answer can release it; anything else -> every occurrence becomes
//   a span raised by source (gate3::SourceSpan), with the field's abstraction at that tier
//   as its suggestion when it differs from the raw value (a summarize label). The review
//   merges the spans before its partition, so a remembered answer applies.
// - EVERY OCCURRENCE in the final prompt is covered, the user's own typed text included:
//   leaving one in place would send the value anyway.
// - LIMITS (stated plainly): a text match misses a paraphrase ("the Leeds area" is caught,
//   "West Yorkshire" is not); a value under 4 characters is not scanned and so not caught;
//   a substring inside a longer word is caught; case folding is per character.
// - Device facts, group facts and persona_context are out of scope (items.id=924).

use crate::api_chat::redact::REMOVED_PLACEHOLDER;
use crate::conductor::personal_fact_bridge::is_bridged_source;
use crate::conductor::privacy::abstraction::{apply_abstraction, AbstractionLevel};
use crate::conductor::privacy::gate2::scan_matches;
use crate::conductor::privacy::gate3::{SourceSpan, CARRIED_FIELD_CATEGORY};
use crate::conductor::privacy::privacy_filter::PfEntityDecoded;
use crate::conductor::privacy::types::{AbstractionPolicy, PersonalTrack as GateTrack};

/// Gate 2's minimum match length (gate2::MIN_MATCH_LENGTH, private there; a test pins
/// that scan_matches ignores a 3-character value and matches a 4-character one).
pub const MIN_VALUE_CHARS: usize = 4;

/// One bridged field whose value an earlier step's output carried into this prompt.
/// Debug never prints the value.
#[derive(Clone, PartialEq, Eq)]
pub struct CarryHit {
    pub field_name: String,
    pub value: String,
    pub suggestion: Option<String>,
}

impl std::fmt::Debug for CarryHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CarryHit")
            .field("field_name", &self.field_name)
            .field("has_suggestion", &self.suggestion.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CarryHits {
    /// Raised as spans in the outbound review.
    pub ask: Vec<CarryHit>,
    /// not_permitted at the step's tier: removed before the review, never shown.
    pub remove: Vec<CarryHit>,
}

impl CarryHits {
    pub fn is_empty(&self) -> bool {
        self.ask.is_empty() && self.remove.is_empty()
    }
}

/// Classify the bridged fields whose values `carried` holds (see the header).
pub fn classify(
    carried: &[&str],
    gate_track: &GateTrack,
    fields_shared: &[String],
    abstraction_tier: u8,
) -> CarryHits {
    let mut names: Vec<String> = Vec::new();
    for text in carried {
        for (name, _) in scan_matches(text, gate_track, Some(fields_shared)) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names.sort();
    let level = AbstractionLevel::from_tier(abstraction_tier.clamp(2, 3));
    let mut hits = CarryHits::default();
    for name in names {
        let Some(field) = gate_track.fields().get(&name) else {
            continue;
        };
        if !is_bridged_source(&field.source_id) {
            continue;
        }
        let policy = match level {
            AbstractionLevel::Level3 => &field.abstraction_tier3,
            _ => &field.abstraction_tier2,
        };
        let suggestion =
            apply_abstraction(field, level).filter(|abstracted| abstracted != &field.field_value);
        let hit = CarryHit {
            field_name: name,
            value: field.field_value.clone(),
            suggestion,
        };
        if *policy == AbstractionPolicy::NotPermitted {
            hits.remove.push(CarryHit {
                suggestion: None,
                ..hit
            });
        } else {
            hits.ask.push(hit);
        }
    }
    hits
}

/// Byte ranges of every non-overlapping, case-insensitive (per character) occurrence of
/// `needle` in `hay`, left to right. A needle under MIN_VALUE_CHARS has none.
pub fn occurrences(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    let n: Vec<char> = needle.chars().collect();
    if n.len() < MIN_VALUE_CHARS {
        return Vec::new();
    }
    let idx: Vec<(usize, char)> = hay.char_indices().collect();
    let eq = |a: char, b: char| a == b || a.to_lowercase().eq(b.to_lowercase());
    let mut out = Vec::new();
    let mut i = 0;
    while i + n.len() <= idx.len() {
        if (0..n.len()).all(|k| eq(idx[i + k].1, n[k])) {
            let start = idx[i].0;
            let end = idx.get(i + n.len()).map_or(hay.len(), |(b, _)| *b);
            out.push((start, end));
            i += n.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Every occurrence of every hit's value in `prompt`, longest value first, an
/// occurrence that overlaps one already taken dropped (overlapping spans could not be
/// applied). Ordered by start.
fn ranges_for(prompt: &str, hits: &[CarryHit]) -> Vec<(usize, usize, usize)> {
    let mut order: Vec<usize> = (0..hits.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(hits[i].value.chars().count()));
    let mut taken: Vec<(usize, usize, usize)> = Vec::new();
    for i in order {
        for (s, e) in occurrences(prompt, &hits[i].value) {
            if taken.iter().all(|&(ts, te, _)| e <= ts || te <= s) {
                taken.push((s, e, i));
            }
        }
    }
    taken.sort_by_key(|&(s, e, _)| (s, e));
    taken
}

/// `prompt` with every occurrence of each value in `remove` replaced by "[removed]".
pub fn remove_values(prompt: &str, remove: &[CarryHit]) -> String {
    let mut out = prompt.to_owned();
    for (s, e, _) in ranges_for(prompt, remove).into_iter().rev() {
        out.replace_range(s..e, REMOVED_PLACEHOLDER);
    }
    out
}

/// The spans raised by source for `ask`, with offsets into `prompt`.
pub fn source_spans(prompt: &str, ask: &[CarryHit]) -> Vec<SourceSpan> {
    ranges_for(prompt, ask)
        .into_iter()
        .map(|(s, e, i)| SourceSpan {
            entity: PfEntityDecoded {
                start_byte: s,
                end_byte: e,
                score: 1.0,
                label: CARRIED_FIELD_CATEGORY.to_owned(),
                span_text: prompt[s..e].to_owned(),
            },
            suggestion: ask[i].suggestion.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::personal_fact_bridge::BRIDGE_SOURCE_ID;
    use crate::conductor::privacy::types::{PersonalField, Sensitivity};

    fn field(name: &str, value: &str, tier2: AbstractionPolicy, source: &str) -> PersonalField {
        PersonalField {
            field_name: name.to_owned(),
            field_value: value.to_owned(),
            sensitivity: Sensitivity::Personal,
            sensitivity_severity: 2,
            source_id: source.to_owned(),
            abstraction_tier2: tier2,
            abstraction_tier3: AbstractionPolicy::Omit,
        }
    }

    fn track(fields: Vec<PersonalField>) -> GateTrack {
        let mut t = GateTrack::new();
        for f in fields {
            t.add_field(f).unwrap();
        }
        t
    }

    fn names(h: &[CarryHit]) -> Vec<&str> {
        h.iter().map(|x| x.field_name.as_str()).collect()
    }

    #[test]
    fn the_minimum_matches_gate_2() {
        let t = track(vec![
            field("industry", "Ops", AbstractionPolicy::Omit, BRIDGE_SOURCE_ID),
            field(
                "employer",
                "Acme",
                AbstractionPolicy::Omit,
                BRIDGE_SOURCE_ID,
            ),
        ]);
        let m = scan_matches("ops at acme", &t, None);
        assert_eq!(
            m.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
            ["employer"]
        );
        assert!(occurrences("ops", "Ops").is_empty());
        assert_eq!(occurrences("acme", "Acme"), [(0, 4)]);
    }

    /// The policy at the step's tier decides: omit and summarize are asked (summarize with
    /// its label as the suggestion), not_permitted is removed, a field this step released
    /// raw is skipped, a non-bridged field and a short value are not scanned, and a
    /// paraphrase is not caught.
    #[test]
    fn classify_by_policy_source_and_what_was_shared() {
        let t = track(vec![
            field(
                "location_city",
                "Leeds",
                AbstractionPolicy::Omit,
                BRIDGE_SOURCE_ID,
            ),
            field(
                "job_title",
                "Staff Engineer",
                AbstractionPolicy::Summarize,
                BRIDGE_SOURCE_ID,
            ),
            field(
                "employer",
                "Acme Corp",
                AbstractionPolicy::NotPermitted,
                BRIDGE_SOURCE_ID,
            ),
            field(
                "industry",
                "Banking",
                AbstractionPolicy::Pass,
                BRIDGE_SOURCE_ID,
            ),
            field(
                "nickname",
                "Jojo-the-Great",
                AbstractionPolicy::Omit,
                "some_other_source",
            ),
            field("country", "UK", AbstractionPolicy::Omit, BRIDGE_SOURCE_ID),
        ]);
        let carried = [
            "A staff engineer in LEEDS at Acme Corp, banking, Jojo-the-Great, UK",
            "also West Yorkshire",
        ];
        let shared = vec!["industry".to_owned()];
        let h = classify(&carried, &t, &shared, 2);
        assert_eq!(names(&h.ask), ["job_title", "location_city"]);
        assert_eq!(names(&h.remove), ["employer"]);
        let job = &h.ask[0];
        assert_eq!(job.value, "Staff Engineer");
        let label = job.suggestion.as_deref().expect("a summarize label");
        assert_ne!(label, "Staff Engineer");
        assert!(h.ask[1].suggestion.is_none(), "omit offers no suggestion");
        assert!(h.remove[0].suggestion.is_none());
        // Without `shared`, the pass field released raw elsewhere is asked too.
        let h = classify(&carried, &t, &[], 2);
        assert!(names(&h.ask).contains(&"industry"));
        // A tier-1 abstraction (a hosted step at the floor) reads the tier-2 policy.
        assert_eq!(
            classify(&carried, &t, &shared, 1),
            classify(&carried, &t, &shared, 2)
        );
        // Nothing carried, nothing found.
        assert!(classify(&[], &t, &[], 2).is_empty());
    }

    /// Every occurrence is found, the user's typed text included, case-insensitively,
    /// with byte offsets that survive non-ASCII text before the match.
    #[test]
    fn occurrences_are_every_match_with_byte_offsets() {
        let p = "User: I live in leeds.\nRequirements: ünïcode — Leeds bike, LEEDS";
        let o = occurrences(p, "Leeds");
        assert_eq!(o.len(), 3);
        for (s, e) in o {
            assert!(p[s..e].eq_ignore_ascii_case("leeds"));
        }
        assert_eq!(occurrences("Leedsleeds", "Leeds"), [(0, 5), (5, 10)]);
        assert!(occurrences("Leed", "Leeds").is_empty());
    }

    /// Overlaps are resolved longest value first, so the spans always apply.
    #[test]
    fn overlapping_values_keep_the_longest_and_removal_replaces_every_occurrence() {
        let hit = |n: &str, v: &str| CarryHit {
            field_name: n.to_owned(),
            value: v.to_owned(),
            suggestion: None,
        };
        let ask = [hit("location_city", "Leeds"), hit("employer", "Leeds Bank")];
        let p = "At Leeds Bank in Leeds.";
        let spans = source_spans(p, &ask);
        let got: Vec<&str> = spans.iter().map(|s| s.entity.span_text.as_str()).collect();
        assert_eq!(got, ["Leeds Bank", "Leeds"]);
        assert!(spans
            .iter()
            .all(|s| s.entity.label == CARRIED_FIELD_CATEGORY));
        assert_eq!(remove_values(p, &ask), "At [removed] in [removed].");
        assert_eq!(remove_values(p, &[]), p);
    }

    /// Debug never prints a value.
    #[test]
    fn debug_never_prints_the_value() {
        let h = CarryHit {
            field_name: "location_city".to_owned(),
            value: "SECRET-CITY".to_owned(),
            suggestion: Some("SECRET-LABEL".to_owned()),
        };
        let d = format!("{h:?}");
        assert!(!d.contains("SECRET"), "{d}");
        let s = source_spans("in SECRET-CITY", &[h]);
        let d = format!("{s:?}");
        assert!(!d.contains("SECRET"), "{d}");
    }
}
