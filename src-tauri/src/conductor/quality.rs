// src-tauri/src/conductor/quality.rs
//
// cb-06 -- Quality/completeness assessment.
// Foundation block, items.id=128 / decisions.id=621 (catalog_status
// 'confirmed'). BUILD STATUS 2026-07-25 (Chat-DEV full-catalog survey,
// handoff id=103): confirmed a true gap -- ConductorError::QualityBelowFloor
// (F2) existed as a defined failure-taxonomy variant with zero producers
// anywhere in the codebase. This module is the producer.
//
// Evaluates a draft, candidate, or record set against stated criteria and
// produces a pass/fail-with-gaps signal, optionally raising
// ConductorError::QualityBelowFloor to route through FailureHandler::handle()
// (conductor/failure.rs) via its existing F2 branch (retry / offer_tier2 /
// await_user, tier- and retry-count-dependent -- unchanged by this module).
//
// SCOPE: this block is the assessor only. It does not retry, does not call
// a provider, and does not decide what happens after a fail-with-gaps
// verdict -- that remains FailureHandler's job, exactly as it already
// handles every other ConductorError variant. QualityAssessor::assess()
// is a pure function of (content, criteria) -> QualityVerdict; callers
// (StepExecutor or a Focus's own step logic) decide whether and when to
// invoke it and whether to convert a fail verdict into a raised error.
//
// SOURCE-AWARE WEIGHTING: cb-06's description marks this "a toggle, not a
// separate block." Implemented as QualityCriterion.weight (f32, default
// 1.0 via Default) -- a caller who does not care about per-criterion
// weighting can omit it entirely and every criterion counts equally.
//
// CRITERION KINDS: three built-in evaluators cover the two named use cases
// (draft prose, record-set completeness) without inventing a plugin system
// this block's description does not ask for:
//   NonEmpty        -- field/content must be present and non-blank
//   MinLength        -- content must reach a minimum character count
//   RequiredFields   -- a record (as a set of named fields) must contain
//                       every field in a caller-supplied list
// A fourth kind, Custom, accepts a caller-supplied predicate for anything
// the built-ins don't cover, so this module does not need to grow a new
// variant for every future adopter's specific rule.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// QualityCriterion
// ---------------------------------------------------------------------------

/// What is being checked, and how strictly it counts toward the verdict.
///
/// `Custom` holds a boxed predicate rather than a serializable payload --
/// this struct is constructed in Rust by the calling step, not deserialized
/// from IPC/JSON, so `#[derive(Serialize, Deserialize)]` is deliberately not
/// on this type (see QualityVerdict below, which IS the IPC-facing type).
#[derive(Clone)]
pub struct QualityCriterion {
    /// Human-readable name surfaced in QualityGap.criterion_name.
    pub name: String,
    pub kind: CriterionKind,
    /// Relative importance, 0.0-1.0+. Default 1.0 -- equal weighting.
    pub weight: f32,
}

#[derive(Clone)]
pub enum CriterionKind {
    /// content (or the named field, for RequiredFields-adjacent single-field
    /// checks) must be present and non-blank after trim.
    NonEmpty,
    /// content must reach at least this many characters after trim.
    MinLength(usize),
    /// every name in this list must be a key in the record map passed to
    /// assess_record(), with a non-blank value.
    RequiredFields(Vec<String>),
    /// caller-supplied predicate over the full content string. Returns
    /// true = criterion satisfied. For anything the built-ins don't cover.
    Custom(std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>),
}

impl QualityCriterion {
    pub fn new(name: impl Into<String>, kind: CriterionKind) -> Self {
        Self { name: name.into(), kind, weight: 1.0 }
    }

    pub fn with_weight(mut self, weight: f32) -> Self {
        self.weight = weight;
        self
    }
}

// ---------------------------------------------------------------------------
// QualityGap / QualityVerdict -- the IPC-facing result types
// ---------------------------------------------------------------------------

/// One failed criterion, surfaced to the caller / UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityGap {
    pub criterion_name: String,
    pub detail: String,
}

/// The pass/fail-with-gaps signal this block produces.
///
/// `passed` is true only if every criterion with weight > 0.0 was satisfied.
/// A criterion with weight == 0.0 is evaluated (its gap, if any, still
/// appears in `gaps` for visibility) but never fails the verdict -- this is
/// how a caller can downgrade a criterion to advisory-only without removing
/// it, matching the "toggle, not a separate block" framing for weighting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityVerdict {
    pub passed: bool,
    pub gaps: Vec<QualityGap>,
    /// Sum of satisfied criteria's weights / sum of all criteria's weights.
    /// 1.0 when there are no criteria (vacuously complete) or all pass.
    pub score: f32,
}

impl QualityVerdict {
    /// Render this verdict as the plain_language string for
    /// ConductorError::QualityBelowFloor, if the caller chooses to raise one.
    /// Does not raise anything itself -- see module doc comment on scope.
    pub fn to_plain_language(&self) -> String {
        if self.gaps.is_empty() {
            return "Quality check found no issues.".to_owned();
        }
        let list = self
            .gaps
            .iter()
            .map(|g| format!("{}: {}", g.criterion_name, g.detail))
            .collect::<Vec<_>>()
            .join("; ");
        format!("Quality check found gaps -- {}", list)
    }
}

// ---------------------------------------------------------------------------
// QualityAssessor
// ---------------------------------------------------------------------------

/// Stateless evaluator. Mirrors FailureHandler's stateless design
/// (conductor/failure.rs) -- no retry_count or run state lives here;
/// callers own that.
pub struct QualityAssessor {
    criteria: Vec<QualityCriterion>,
}

impl QualityAssessor {
    pub fn new(criteria: Vec<QualityCriterion>) -> Self {
        Self { criteria }
    }

    /// Evaluate free-form content (a draft) against this assessor's criteria.
    /// RequiredFields criteria are skipped here -- they need a record map,
    /// not a content string. Use assess_record() when checking a field set.
    pub fn assess_content(&self, content: &str) -> QualityVerdict {
        let trimmed = content.trim();
        let mut gaps = Vec::new();
        let mut weight_total = 0.0f32;
        let mut weight_satisfied = 0.0f32;

        for c in &self.criteria {
            let (satisfied, detail) = match &c.kind {
                CriterionKind::NonEmpty => {
                    if trimmed.is_empty() {
                        (false, "content is empty".to_owned())
                    } else {
                        (true, String::new())
                    }
                }
                CriterionKind::MinLength(min) => {
                    if trimmed.len() < *min {
                        (
                            false,
                            format!("content is {} characters, minimum is {}", trimmed.len(), min),
                        )
                    } else {
                        (true, String::new())
                    }
                }
                CriterionKind::RequiredFields(_) => {
                    // Not applicable to a bare content string -- vacuously
                    // satisfied here so a mixed criteria list doesn't force
                    // callers to split assess_content/assess_record calls.
                    (true, String::new())
                }
                CriterionKind::Custom(f) => {
                    if f(trimmed) {
                        (true, String::new())
                    } else {
                        (false, "custom criterion not satisfied".to_owned())
                    }
                }
            };

            weight_total += c.weight;
            if satisfied {
                weight_satisfied += c.weight;
            } else {
                gaps.push(QualityGap { criterion_name: c.name.clone(), detail });
            }
        }

        self.finish(gaps, weight_total, weight_satisfied)
    }

    /// Evaluate a record (named field -> value) against this assessor's
    /// criteria. NonEmpty/MinLength/Custom apply to the concatenation of all
    /// field values (order-independent join, single space separator) --
    /// RequiredFields applies directly to the field-name set.
    pub fn assess_record(&self, record: &std::collections::HashMap<String, String>) -> QualityVerdict {
        let joined: String = {
            let mut vals: Vec<&str> = record.values().map(|s| s.as_str()).collect();
            vals.sort_unstable(); // deterministic join order
            vals.join(" ")
        };
        let trimmed = joined.trim();

        let mut gaps = Vec::new();
        let mut weight_total = 0.0f32;
        let mut weight_satisfied = 0.0f32;

        for c in &self.criteria {
            let (satisfied, detail) = match &c.kind {
                CriterionKind::NonEmpty => {
                    if trimmed.is_empty() {
                        (false, "record has no non-blank field values".to_owned())
                    } else {
                        (true, String::new())
                    }
                }
                CriterionKind::MinLength(min) => {
                    if trimmed.len() < *min {
                        (
                            false,
                            format!("combined field values are {} characters, minimum is {}", trimmed.len(), min),
                        )
                    } else {
                        (true, String::new())
                    }
                }
                CriterionKind::RequiredFields(names) => {
                    let missing: Vec<&str> = names
                        .iter()
                        .filter(|n| {
                            record
                                .get(n.as_str())
                                .map(|v| v.trim().is_empty())
                                .unwrap_or(true)
                        })
                        .map(|s| s.as_str())
                        .collect();
                    if missing.is_empty() {
                        (true, String::new())
                    } else {
                        (false, format!("missing or blank field(s): {}", missing.join(", ")))
                    }
                }
                CriterionKind::Custom(f) => {
                    if f(trimmed) {
                        (true, String::new())
                    } else {
                        (false, "custom criterion not satisfied".to_owned())
                    }
                }
            };

            weight_total += c.weight;
            if satisfied {
                weight_satisfied += c.weight;
            } else {
                gaps.push(QualityGap { criterion_name: c.name.clone(), detail });
            }
        }

        self.finish(gaps, weight_total, weight_satisfied)
    }

    fn finish(&self, gaps: Vec<QualityGap>, weight_total: f32, weight_satisfied: f32) -> QualityVerdict {
        let score = if weight_total <= 0.0 { 1.0 } else { weight_satisfied / weight_total };
        // Fails only if a nonzero-weight criterion was not satisfied --
        // i.e. score < 1.0 on the nonzero-weight subset. Since weight-0.0
        // criteria never subtract from weight_satisfied relative to their
        // own (zero) contribution to weight_total, score == 1.0 iff every
        // nonzero-weight criterion passed.
        let passed = score >= 1.0;
        QualityVerdict { passed, gaps, score }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn empty_criteria_vacuously_passes() {
        let a = QualityAssessor::new(vec![]);
        let v = a.assess_content("anything");
        assert!(v.passed);
        assert_eq!(v.score, 1.0);
        assert!(v.gaps.is_empty());
    }

    #[test]
    fn non_empty_passes_on_content() {
        let a = QualityAssessor::new(vec![QualityCriterion::new("has content", CriterionKind::NonEmpty)]);
        let v = a.assess_content("hello");
        assert!(v.passed);
    }

    #[test]
    fn non_empty_fails_on_blank() {
        let a = QualityAssessor::new(vec![QualityCriterion::new("has content", CriterionKind::NonEmpty)]);
        let v = a.assess_content("   ");
        assert!(!v.passed);
        assert_eq!(v.gaps.len(), 1);
        assert_eq!(v.gaps[0].criterion_name, "has content");
    }

    #[test]
    fn min_length_fails_under_threshold() {
        let a = QualityAssessor::new(vec![QualityCriterion::new("length", CriterionKind::MinLength(20))]);
        let v = a.assess_content("too short");
        assert!(!v.passed);
        assert!(v.gaps[0].detail.contains("minimum is 20"));
    }

    #[test]
    fn min_length_passes_at_threshold() {
        let a = QualityAssessor::new(vec![QualityCriterion::new("length", CriterionKind::MinLength(5))]);
        let v = a.assess_content("12345");
        assert!(v.passed);
    }

    #[test]
    fn required_fields_all_present_passes() {
        let a = QualityAssessor::new(vec![QualityCriterion::new(
            "required",
            CriterionKind::RequiredFields(vec!["title".to_owned(), "body".to_owned()]),
        )]);
        let mut record = HashMap::new();
        record.insert("title".to_owned(), "Hello".to_owned());
        record.insert("body".to_owned(), "World".to_owned());
        let v = a.assess_record(&record);
        assert!(v.passed);
    }

    #[test]
    fn required_fields_missing_fails_with_names() {
        let a = QualityAssessor::new(vec![QualityCriterion::new(
            "required",
            CriterionKind::RequiredFields(vec!["title".to_owned(), "body".to_owned()]),
        )]);
        let mut record = HashMap::new();
        record.insert("title".to_owned(), "Hello".to_owned());
        let v = a.assess_record(&record);
        assert!(!v.passed);
        assert!(v.gaps[0].detail.contains("body"));
    }

    #[test]
    fn required_fields_blank_value_counts_as_missing() {
        let a = QualityAssessor::new(vec![QualityCriterion::new(
            "required",
            CriterionKind::RequiredFields(vec!["title".to_owned()]),
        )]);
        let mut record = HashMap::new();
        record.insert("title".to_owned(), "   ".to_owned());
        let v = a.assess_record(&record);
        assert!(!v.passed);
    }

    #[test]
    fn required_fields_skipped_in_assess_content() {
        // RequiredFields is vacuously satisfied against a bare content
        // string -- it needs a record map. A mixed criteria list should
        // not force callers to special-case this.
        let a = QualityAssessor::new(vec![QualityCriterion::new(
            "required",
            CriterionKind::RequiredFields(vec!["anything".to_owned()]),
        )]);
        let v = a.assess_content("some draft text");
        assert!(v.passed);
    }

    #[test]
    fn custom_predicate_controls_verdict() {
        let a = QualityAssessor::new(vec![QualityCriterion::new(
            "no placeholder text",
            CriterionKind::Custom(std::sync::Arc::new(|s: &str| !s.contains("TODO"))),
        )]);
        assert!(a.assess_content("finished work").passed);
        assert!(!a.assess_content("TODO: finish this").passed);
    }

    #[test]
    fn zero_weight_criterion_is_advisory_only() {
        let a = QualityAssessor::new(vec![
            QualityCriterion::new("required", CriterionKind::NonEmpty),
            QualityCriterion::new("nice to have", CriterionKind::MinLength(1000)).with_weight(0.0),
        ]);
        let v = a.assess_content("short but present");
        // The weight-0.0 MinLength criterion fails (content is well under
        // 1000 chars) but must not flip the overall verdict.
        assert!(v.passed, "zero-weight criterion must not fail the verdict");
        // Still visible as a gap for UI/debugging purposes.
        assert!(v.gaps.iter().any(|g| g.criterion_name == "nice to have"));
    }

    #[test]
    fn weighted_criteria_produce_partial_score() {
        let a = QualityAssessor::new(vec![
            QualityCriterion::new("a", CriterionKind::NonEmpty).with_weight(1.0),
            QualityCriterion::new("b", CriterionKind::MinLength(1000)).with_weight(1.0),
        ]);
        let v = a.assess_content("present but short");
        assert!(!v.passed);
        assert_eq!(v.score, 0.5);
    }

    #[test]
    fn multiple_gaps_all_reported() {
        let a = QualityAssessor::new(vec![
            QualityCriterion::new("non-empty", CriterionKind::NonEmpty),
            QualityCriterion::new("length", CriterionKind::MinLength(50)),
        ]);
        let v = a.assess_content("");
        assert!(!v.passed);
        assert_eq!(v.gaps.len(), 2);
    }

    #[test]
    fn to_plain_language_no_gaps() {
        let v = QualityVerdict { passed: true, gaps: vec![], score: 1.0 };
        assert_eq!(v.to_plain_language(), "Quality check found no issues.");
    }

    #[test]
    fn to_plain_language_lists_gaps() {
        let v = QualityVerdict {
            passed: false,
            gaps: vec![
                QualityGap { criterion_name: "title".to_owned(), detail: "missing".to_owned() },
                QualityGap { criterion_name: "length".to_owned(), detail: "too short".to_owned() },
            ],
            score: 0.0,
        };
        let s = v.to_plain_language();
        assert!(s.contains("title: missing"));
        assert!(s.contains("length: too short"));
    }

    #[test]
    fn assess_record_joins_deterministically_regardless_of_insertion_order() {
        // HashMap iteration order is not guaranteed -- confirm the sorted
        // join keeps MinLength deterministic across two maps built with
        // insertions in a different order but identical content.
        let a = QualityAssessor::new(vec![QualityCriterion::new("length", CriterionKind::MinLength(5))]);
        let mut r1 = HashMap::new();
        r1.insert("a".to_owned(), "ab".to_owned());
        r1.insert("b".to_owned(), "cd".to_owned());
        let mut r2 = HashMap::new();
        r2.insert("b".to_owned(), "cd".to_owned());
        r2.insert("a".to_owned(), "ab".to_owned());
        assert_eq!(a.assess_record(&r1).score, a.assess_record(&r2).score);
    }
}
