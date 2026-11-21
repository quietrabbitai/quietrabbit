// src-tauri/src/conductor/privacy/allowed_answers.rs
//
// items.id=924 unit 1 (Chat-PM plan approval 2026-11-20, D1 and D3): the ANSWERS a
// review span allows, and the one rule that decides whether an answer, live or
// remembered, may be applied to it. Pure (no I/O). gate3's partition and the
// outbound review's application (step_review::outbound_for and derive_outcome) both
// call it, so a remembered decision and a live answer meet the same rule.
//
// WHAT A SPAN ALLOWS. Keep private is always allowed. Generalize is always allowed,
// but when release is NOT allowed a replacement that contains the original text
// (case-insensitively) is not a generalization and counts as keep private. Release
// original is allowed only when `release` is true, and a REMEMBERED (standing,
// Persona-wide) release only when `standing_release` is also true.
//
// WHO SETS IT. Only a span QuietRabbit raises by SOURCE (gate3::SourceSpan) can
// carry less than ALL: a span the Privacy Filter found allows every answer, as
// before, and the copy review and API Chat pass no source spans. Nothing the
// frontend sends sets or widens it: the allowed set travels server side (the
// consent payload is display only, and the step_reviews record keeps it for the
// resume).
//
// WHAT HAPPENS TO AN ANSWER THE SPAN DOES NOT ALLOW.
//   remembered (gate3 partition): on a span that allows no release at all it is
//     applied as keep private (there is nothing a question could release); on a
//     span that allows a live release but not a remembered one it is ignored and the
//     span is asked again.
//   live (the review's application): the span is removed, never sent, exactly as an
//     unanswered span is.
// An unknown decision word is already keep private (redact::Decision::parse).

use serde::{Deserialize, Serialize};

use crate::api_chat::redact::{Decision, SpanDecision};

/// The answers a review span allows (see the header). Serialized into the
/// step_reviews record and the consent payload; a record written before this item
/// has none and reads as ALL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct AllowedAnswers {
    /// Release original may be applied (as a live answer, or remembered from this run
    /// by the user).
    pub release: bool,
    /// A remembered (standing, Persona-wide) release may be applied.
    pub standing_release: bool,
}

impl AllowedAnswers {
    /// Every answer: a span the Privacy Filter found, and a bridged personal fact.
    pub const ALL: Self = Self {
        release: true,
        standing_release: true,
    };
    /// Release only as an answer given in this run (a field with the
    /// access-credential flag, decisions.id=475, D3).
    pub const NO_STANDING_RELEASE: Self = Self {
        release: true,
        standing_release: false,
    };
    /// No release at all (a `never` field, D1).
    pub const NO_RELEASE: Self = Self {
        release: false,
        standing_release: false,
    };

    pub fn is_all(&self) -> bool {
        *self == Self::ALL
    }
}

impl Default for AllowedAnswers {
    fn default() -> Self {
        Self::ALL
    }
}

/// Where a remembered decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remembered {
    /// The Persona's standing preference, or a row of this run that the user did
    /// not write (an auto-reapplied one).
    Standing,
    /// An answer the user wrote in this run (save_preference set, original_text
    /// NULL: output_store's own distinction).
    ThisRun,
}

/// What gate3's partition does with a remembered decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Apply it as it is.
    Apply,
    /// Apply keep private instead (the span allows no release).
    KeepPrivate,
    /// Not applied: the span is asked again.
    AskAgain,
}

fn contains_ignoring_case(hay: &str, needle: &str) -> bool {
    let needle = needle.trim();
    !needle.is_empty() && hay.to_lowercase().contains(&needle.to_lowercase())
}

/// True when the answer may be applied to a span with `original` text under
/// `allowed`, with the remembered origin when it is one (None: a live answer).
fn permitted(
    allowed: AllowedAnswers,
    remembered: Option<Remembered>,
    decision: Decision,
    suggestion: Option<&str>,
    user_modified: Option<&str>,
    original: &str,
) -> bool {
    match decision {
        Decision::KeepPrivate => true,
        Decision::ReleaseOriginal => {
            allowed.release
                && (remembered != Some(Remembered::Standing) || allowed.standing_release)
        }
        Decision::Generalize => {
            if allowed.release {
                return true;
            }
            let replacement = user_modified
                .filter(|m| !m.trim().is_empty())
                .or(suggestion)
                .unwrap_or("");
            !contains_ignoring_case(replacement, original)
        }
    }
}

/// The partition's rule for a remembered decision (see the header).
pub fn admit_remembered(
    allowed: AllowedAnswers,
    remembered: Remembered,
    decision: &str,
    suggestion: Option<&str>,
    user_modified: Option<&str>,
    original: &str,
) -> Admission {
    if permitted(
        allowed,
        Some(remembered),
        Decision::parse(decision),
        suggestion,
        user_modified,
        original,
    ) {
        Admission::Apply
    } else if allowed.release {
        Admission::AskAgain
    } else {
        Admission::KeepPrivate
    }
}

/// The review's rule for an answer it is about to apply: the answer itself, or the
/// span removed (unresolved) when `allowed` does not permit it. `original` is the
/// span's text in the prompt.
pub fn enforce(allowed: AllowedAnswers, original: &str, answer: SpanDecision) -> SpanDecision {
    if permitted(
        allowed,
        None,
        answer.decision,
        answer.suggestion_text.as_deref(),
        answer.user_modified_text.as_deref(),
        original,
    ) {
        answer
    } else {
        SpanDecision::unresolved(answer.start_byte, answer.end_byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = "C02XK1ZZJGH5";

    fn answer(d: Decision, suggestion: Option<&str>, modified: Option<&str>) -> SpanDecision {
        SpanDecision {
            start_byte: 0,
            end_byte: ORIGINAL.len(),
            decision: d,
            suggestion_text: suggestion.map(str::to_owned),
            user_modified_text: modified.map(str::to_owned),
        }
    }

    const DECISIONS: [&str; 4] = ["release_original", "generalize", "keep_private", "unknown"];
    const REPLACEMENTS: [Option<&str>; 5] = [
        None,
        Some(""),
        Some("[serial number]"),
        Some("C02XK1ZZJGH5"),
        Some("serial c02xk1zzjgh5 please"),
    ];

    #[test]
    fn a_span_that_allows_no_release_is_never_released_by_any_answer_live_or_remembered() {
        for decision in DECISIONS {
            for suggestion in REPLACEMENTS {
                for modified in REPLACEMENTS {
                    let live = enforce(
                        AllowedAnswers::NO_RELEASE,
                        ORIGINAL,
                        answer(Decision::parse(decision), suggestion, modified),
                    );
                    let text = crate::api_chat::redact::apply(ORIGINAL, vec![live]).unwrap();
                    assert!(
                        !text.to_lowercase().contains(&ORIGINAL.to_lowercase()),
                        "live {decision} {suggestion:?} {modified:?} sent {text}"
                    );
                    for origin in [Remembered::Standing, Remembered::ThisRun] {
                        let a = admit_remembered(
                            AllowedAnswers::NO_RELEASE,
                            origin,
                            decision,
                            suggestion,
                            modified,
                            ORIGINAL,
                        );
                        if a == Admission::Apply {
                            let applied = SpanDecision {
                                decision: Decision::parse(decision),
                                ..answer(Decision::KeepPrivate, suggestion, modified)
                            };
                            let text =
                                crate::api_chat::redact::apply(ORIGINAL, vec![applied]).unwrap();
                            assert!(
                                !text.to_lowercase().contains(&ORIGINAL.to_lowercase()),
                                "remembered {origin:?} {decision} {suggestion:?} {modified:?} \
                                 sent {text}"
                            );
                        } else {
                            assert_eq!(a, Admission::KeepPrivate, "{decision}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_remembered_release_is_ignored_where_only_a_live_one_is_allowed() {
        let a = AllowedAnswers::NO_STANDING_RELEASE;
        assert_eq!(
            admit_remembered(
                a,
                Remembered::Standing,
                "release_original",
                None,
                None,
                ORIGINAL
            ),
            Admission::AskAgain
        );
        assert_eq!(
            admit_remembered(
                a,
                Remembered::ThisRun,
                "release_original",
                None,
                None,
                ORIGINAL
            ),
            Admission::Apply
        );
        assert_eq!(
            admit_remembered(a, Remembered::Standing, "generalize", None, None, ORIGINAL),
            Admission::Apply
        );
        assert_eq!(
            admit_remembered(
                a,
                Remembered::Standing,
                "keep_private",
                None,
                None,
                ORIGINAL
            ),
            Admission::Apply
        );
        let live = enforce(a, ORIGINAL, answer(Decision::ReleaseOriginal, None, None));
        assert_eq!(live.decision, Decision::ReleaseOriginal);
    }

    #[test]
    fn every_answer_applies_unchanged_where_everything_is_allowed() {
        for decision in DECISIONS {
            for origin in [Remembered::Standing, Remembered::ThisRun] {
                assert_eq!(
                    admit_remembered(
                        AllowedAnswers::ALL,
                        origin,
                        decision,
                        Some(ORIGINAL),
                        Some(ORIGINAL),
                        ORIGINAL
                    ),
                    Admission::Apply
                );
            }
            let a = answer(Decision::parse(decision), Some(ORIGINAL), Some(ORIGINAL));
            assert_eq!(enforce(AllowedAnswers::ALL, ORIGINAL, a.clone()), a);
        }
    }

    #[test]
    fn a_generalization_that_does_not_carry_the_original_stands_where_release_is_not_allowed() {
        let a = answer(Decision::Generalize, Some("[serial number]"), None);
        assert_eq!(enforce(AllowedAnswers::NO_RELEASE, ORIGINAL, a.clone()), a);
        let edited = answer(
            Decision::Generalize,
            Some("[serial number]"),
            Some("a laptop"),
        );
        assert_eq!(
            enforce(AllowedAnswers::NO_RELEASE, ORIGINAL, edited.clone()),
            edited
        );
        // A blank edit is judged by the suggestion (stricter than redact::apply,
        // which removes a blank edit).
        let blank = answer(Decision::Generalize, Some(ORIGINAL), Some("  "));
        assert_eq!(
            enforce(AllowedAnswers::NO_RELEASE, ORIGINAL, blank).decision,
            Decision::KeepPrivate
        );
    }

    /// Only the carry scan's device source (items.id=924 unit 2) names a restricted set
    /// in production; every other span allows every answer.
    #[test]
    fn only_the_carry_scan_restricts_a_span() {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push((
                        p.display().to_string(),
                        std::fs::read_to_string(&p).unwrap(),
                    ));
                }
            }
        }
        let mut files = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut restricting = Vec::new();
        for (path, src) in files {
            if path.ends_with("allowed_answers.rs") {
                continue;
            }
            // Production code is everything before the file's test module (a
            // #[cfg(test)] item above it, such as gate3's test-only wrappers, is kept:
            // over-scanning only makes the pin stricter).
            let end = ["\n#[cfg(test)]\nmod tests", "\nmod tests {"]
                .iter()
                .filter_map(|c| src.find(c))
                .min()
                .unwrap_or(src.len());
            let prod = &src[..end];
            for name in [
                "NO_RELEASE",
                "NO_STANDING_RELEASE",
                "release: false",
                "standing_release: false",
            ] {
                if prod.contains(name) {
                    restricting.push((path.clone(), name));
                }
            }
        }
        // items.id=924: the device values of carry_scan (a never value's removed hit
        // and an access-credential field's spans) are the only restriction.
        assert!(!restricting.is_empty());
        for (path, name) in &restricting {
            assert!(
                path.ends_with("conductor/carry_scan.rs")
                    && ["NO_RELEASE", "NO_STANDING_RELEASE"].contains(name),
                "{path} names {name}"
            );
        }
    }

    #[test]
    fn a_record_without_the_field_reads_as_every_answer() {
        #[derive(Deserialize)]
        struct Old {
            #[serde(default)]
            allowed: AllowedAnswers,
        }
        let old: Old = serde_json::from_str("{}").unwrap();
        assert!(old.allowed.is_all());
        assert_eq!(AllowedAnswers::default(), AllowedAnswers::ALL);
    }
}
