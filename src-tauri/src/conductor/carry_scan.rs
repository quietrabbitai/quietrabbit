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
// - Group facts and persona_context are out of scope.
//
// DEVICE VALUES (items.id=924, the same mechanism widened, not a second one). A pass
// that seeds a device name keeps a DeviceSeed (device_seeding.rs). classify_devices
// raises every value of it whose disclosure is not pass:
// - TRIGGER: ANY occurrence in the final prompt (the derived name itself, a carried
//   variable, {previous_output}, the user's own text), case-insensitive, at least 4
//   characters, of the stored value or its brace-cleaned form. Device text has no Gate 1
//   projection and no raw-release record, so nothing is skipped.
// - ACTION by its disclosure: never -> removed before the review, logged by field name,
//   never shown; ask -> a span of category qr_device_field ("Device detail", placeholder
//   wording) with the field's suggestion, its fact key device_fact:<hash of entity, field,
//   value> (so a remembered answer applies to the same value only), and the answers it
//   allows: every answer, except a field with the access-credential flag, whose
//   remembered release is not applied (NO_STANDING_RELEASE: a release in this run still
//   counts). No shipped field has the flag since decisions.id=956 (the serial is an
//   ordinary ask field: a remembered release applies); the branch stays for any
//   flagged field.
// - LIMITS, beside the ones above: a value seeded in an EARLIER pass that a session
//   variable or artifact carries into this pass is not scanned unless this pass seeds
//   the device again; a value reformatted (a serial with spaces or dashes added, a
//   zero-padded address) is missed; a device name made of common words over-asks. A
//   serial the user types in a format the Privacy Filter misses (a Lenovo "S/N
//   PF2ABC3D") is caught only when that device is seeded in the pass.

use crate::api_chat::redact::REMOVED_PLACEHOLDER;
use crate::conductor::device_fields::Disclosure;
use crate::conductor::device_seeding::{device_fact_key, DeviceSeed};
use crate::conductor::personal_fact_bridge::is_bridged_source;
use crate::conductor::privacy::abstraction::{apply_abstraction, AbstractionLevel};
use crate::conductor::privacy::allowed_answers::AllowedAnswers;
use crate::conductor::privacy::gate2::scan_matches;
use crate::conductor::privacy::gate3::{SourceSpan, CARRIED_FIELD_CATEGORY, DEVICE_FIELD_CATEGORY};
use crate::conductor::privacy::privacy_filter::PfEntityDecoded;
use crate::conductor::privacy::types::{AbstractionPolicy, PersonalTrack as GateTrack};

/// Gate 2's minimum match length (gate2::MIN_MATCH_LENGTH, private there; a test pins
/// that scan_matches ignores a 3-character value and matches a 4-character one).
pub const MIN_VALUE_CHARS: usize = 4;

/// One value a source flags in this prompt: a bridged field an earlier step's output
/// carried, or a device value the pass seeded (items.id=924). Debug never prints the
/// value.
#[derive(Clone, PartialEq, Eq)]
pub struct CarryHit {
    pub field_name: String,
    pub value: String,
    pub suggestion: Option<String>,
    /// The span's category (CARRIED_FIELD_CATEGORY or DEVICE_FIELD_CATEGORY).
    pub category: &'static str,
    /// The answers the span allows (items.id=924).
    pub allowed: AllowedAnswers,
    /// The span's fact key when the source gives one (a device value); else gate3
    /// resolves it as for any span.
    pub fact_key: Option<String>,
}

impl std::fmt::Debug for CarryHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CarryHit")
            .field("field_name", &self.field_name)
            .field("category", &self.category)
            .field("has_suggestion", &self.suggestion.is_some())
            .field("allowed", &self.allowed)
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

    /// Both sources' hits together (items.id=924).
    pub fn extend(&mut self, other: CarryHits) {
        self.ask.extend(other.ask);
        self.remove.extend(other.remove);
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
            category: CARRIED_FIELD_CATEGORY,
            // A bridged field that is asked allows every answer (its not_permitted
            // values never reach the review: they are removed).
            allowed: AllowedAnswers::ALL,
            fact_key: None,
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

/// items.id=924 (D7): the bridged fields whose policy is ask, released raw to this step
/// (executor::to_gate_track_asks): every occurrence of each value in `prompt` is raised,
/// with every answer, wherever it came from (the projected field, a carried output, the
/// user's text). A value under MIN_VALUE_CHARS is never released as ask, so never here.
pub fn classify_asked(prompt: &str, gate_track: &GateTrack, asked: &[String]) -> CarryHits {
    let mut hits = CarryHits::default();
    for name in asked {
        let Some(field) = gate_track.fields().get(name) else {
            continue;
        };
        if !is_bridged_source(&field.source_id)
            || occurrences(prompt, &field.field_value).is_empty()
        {
            continue;
        }
        hits.ask.push(CarryHit {
            field_name: name.clone(),
            value: field.field_value.clone(),
            suggestion: None,
            category: CARRIED_FIELD_CATEGORY,
            allowed: AllowedAnswers::ALL,
            fact_key: None,
        });
    }
    hits
}

/// items.id=924: the device values `seed` holds that occur anywhere in `prompt` and are
/// not pass (see the header). The stored value and its brace-cleaned form (what a
/// rendering shows) are both looked for.
pub fn classify_devices(prompt: &str, seed: &DeviceSeed) -> CarryHits {
    let mut hits = CarryHits::default();
    for v in seed.values() {
        if v.disclosure == Disclosure::Pass {
            continue;
        }
        let cleaned = crate::conductor::seeding::clean(&v.value);
        let mut forms = vec![v.value.clone()];
        if cleaned != v.value {
            forms.push(cleaned);
        }
        for form in forms {
            if occurrences(prompt, &form).is_empty() {
                continue;
            }
            let hit = CarryHit {
                field_name: v.field.clone(),
                value: form,
                suggestion: v.suggestion.clone(),
                category: DEVICE_FIELD_CATEGORY,
                allowed: if v.access_credential {
                    AllowedAnswers::NO_STANDING_RELEASE
                } else {
                    AllowedAnswers::ALL
                },
                fact_key: Some(device_fact_key(&v.entity_id, &v.field, &v.value)),
            };
            match v.disclosure {
                Disclosure::Never => hits.remove.push(CarryHit {
                    suggestion: None,
                    allowed: AllowedAnswers::NO_RELEASE,
                    ..hit
                }),
                _ => hits.ask.push(hit),
            }
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
                label: ask[i].category.to_owned(),
                span_text: prompt[s..e].to_owned(),
            },
            suggestion: ask[i].suggestion.clone(),
            allowed: ask[i].allowed,
            fact_key: ask[i].fact_key.clone(),
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
            category: CARRIED_FIELD_CATEGORY,
            allowed: AllowedAnswers::ALL,
            fact_key: None,
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
            category: CARRIED_FIELD_CATEGORY,
            allowed: AllowedAnswers::ALL,
            fact_key: None,
        };
        let d = format!("{h:?}");
        assert!(!d.contains("SECRET"), "{d}");
        let s = source_spans("in SECRET-CITY", &[h]);
        let d = format!("{s:?}");
        assert!(!d.contains("SECRET"), "{d}");
    }

    // -- items.id=924: device values ---------------------------------------------

    use crate::conductor::device_seeding::{SeededValue, DEVICE_NAME_FIELD};

    fn dv(field: &str, value: &str, d: Disclosure, cred: bool, sugg: Option<&str>) -> SeededValue {
        SeededValue {
            entity_id: "ent-1".to_owned(),
            field: field.to_owned(),
            value: value.to_owned(),
            disclosure: d,
            access_credential: cred,
            suggestion: sugg.map(str::to_owned),
        }
    }

    fn rma_seed() -> DeviceSeed {
        DeviceSeed::for_test(
            vec![
                dv(
                    DEVICE_NAME_FIELD,
                    "Study laptop",
                    Disclosure::Ask,
                    false,
                    Some("[device]"),
                ),
                dv(
                    "serial_number",
                    "PF2ABC3D",
                    Disclosure::Ask,
                    false,
                    Some("[serial number]"),
                ),
                dv("make", "Lenovo", Disclosure::Pass, false, None),
                dv(
                    "warranty_contact",
                    "help@warranty.example",
                    Disclosure::Never,
                    false,
                    None,
                ),
                dv(
                    "retailer",
                    "Box",
                    Disclosure::Ask,
                    false,
                    Some("[retailer]"),
                ),
            ],
            None,
        )
    }

    /// An ask value is raised wherever it is (a derived text, a carried output, the
    /// user's own words), with its suggestion, its device key and its allowed answers:
    /// every answer for the name and the serial (decisions.id=956). A never
    /// value is removed and never raised; a pass value and a value under 4 characters
    /// are not looked at.
    #[test]
    fn device_values_are_raised_by_disclosure_wherever_they_occur() {
        let prompt = "RMA for my study LAPTOP? No: Study laptop, S/N PF2ABC3D (pf2abc3d typed), \
                      Lenovo, bought at Box, contact help@warranty.example";
        let hits = classify_devices(prompt, &rma_seed());
        let ask: Vec<(&str, &str)> = hits
            .ask
            .iter()
            .map(|h| (h.field_name.as_str(), h.value.as_str()))
            .collect();
        assert_eq!(
            ask,
            [
                (DEVICE_NAME_FIELD, "Study laptop"),
                ("serial_number", "PF2ABC3D")
            ]
        );
        assert_eq!(hits.remove.len(), 1);
        assert_eq!(hits.remove[0].field_name, "warranty_contact");
        assert_eq!(hits.remove[0].allowed, AllowedAnswers::NO_RELEASE);

        let cleaned = remove_values(prompt, &hits.remove);
        assert!(!cleaned.contains("help@warranty.example"));
        let spans = source_spans(&cleaned, &hits.ask);
        let got: Vec<(&str, Option<&str>, AllowedAnswers)> = spans
            .iter()
            .map(|s| {
                (
                    s.entity.span_text.as_str(),
                    s.suggestion.as_deref(),
                    s.allowed,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("study LAPTOP", Some("[device]"), AllowedAnswers::ALL),
                ("Study laptop", Some("[device]"), AllowedAnswers::ALL),
                ("PF2ABC3D", Some("[serial number]"), AllowedAnswers::ALL),
                ("pf2abc3d", Some("[serial number]"), AllowedAnswers::ALL),
            ]
        );
        for s in &spans {
            assert_eq!(s.entity.label, DEVICE_FIELD_CATEGORY);
            assert_eq!(
                &cleaned[s.entity.start_byte..s.entity.end_byte],
                s.entity.span_text
            );
        }
        assert_eq!(
            spans[2].fact_key,
            Some(device_fact_key("ent-1", "serial_number", "PF2ABC3D"))
        );
        assert_eq!(
            spans[2].fact_key, spans[3].fact_key,
            "one fact, both occurrences"
        );
        assert_eq!(
            spans[0].fact_key, spans[1].fact_key,
            "the name in either case"
        );
        // Nothing seeded, nothing raised.
        assert!(classify_devices(prompt, &DeviceSeed::default()).is_empty());
    }

    /// decisions.id=956: the serial is an ordinary ask field, so its span allows every
    /// answer, a remembered release included. A field the seed marks as an access
    /// credential (a fixture: no shipped field has the flag) is still a live release
    /// only, and its removal rule for a never value is unchanged.
    #[test]
    fn only_a_flagged_field_is_restricted_to_a_live_release() {
        let seed = DeviceSeed::for_test(
            vec![
                dv(
                    "serial_number",
                    "PF2ABC3D",
                    Disclosure::Ask,
                    false,
                    Some("[serial number]"),
                ),
                dv(
                    "fixture_credential",
                    "ZX-4471-Q",
                    Disclosure::Ask,
                    true,
                    Some("[fixture]"),
                ),
            ],
            None,
        );
        let hits = classify_devices("S/N PF2ABC3D and ref ZX-4471-Q", &seed);
        let got: Vec<(&str, AllowedAnswers)> = hits
            .ask
            .iter()
            .map(|h| (h.field_name.as_str(), h.allowed))
            .collect();
        assert_eq!(
            got,
            [
                ("serial_number", AllowedAnswers::ALL),
                ("fixture_credential", AllowedAnswers::NO_STANDING_RELEASE),
            ]
        );
    }

    /// The forms a rendering shows: a stored value with braces renders cleaned
    /// (parentheses), and both forms are looked for. A reformatted value is missed (a
    /// stated limit).
    #[test]
    fn a_device_value_is_found_stored_or_cleaned_and_missed_when_reformatted() {
        let seed = DeviceSeed::for_test(
            vec![dv(
                "serial_number",
                "AB{12}CD",
                Disclosure::Ask,
                false,
                Some("[serial number]"),
            )],
            None,
        );
        let a = classify_devices("Serial number: AB(12)CD", &seed);
        assert_eq!(a.ask.len(), 1);
        assert_eq!(a.ask[0].value, "AB(12)CD");
        let b = classify_devices("model said AB{12}CD", &seed);
        assert_eq!(b.ask[0].value, "AB{12}CD");
        assert_eq!(
            a.ask[0].fact_key, b.ask[0].fact_key,
            "keyed by the stored value"
        );
        assert!(
            classify_devices("AB 12 CD", &seed).is_empty(),
            "reformatted: missed"
        );
    }
}
