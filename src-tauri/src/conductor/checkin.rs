// src-tauri/src/conductor/checkin.rs
//
// cb-08 -- Two-axis proactive check-in.
// Foundation block, items.id=128 / decisions.id=621 (catalog_status
// 'confirmed'). BUILD STATUS 2026-07-25 (Chat-DEV full-catalog survey,
// handoff id=103): confirmed a true gap -- no session-start surfacing
// mechanism, no "unprompted items" concept, no two-signal gating (learning
// value + engagement) found anywhere. The only search hit was an unrelated
// word collision ("checking ownership" in migration lock code). This
// module is that mechanism.
//
// GATING RULE -- SOURCED, NOT INFERRED (P1): the two-axis model itself is
// not this block's invention. decisions.id=500 (legacy D6-458, "Two-axis
// conversational follow-up model") is an active, R1-scope, all-Focuses
// standing decision that defines the exact rule:
//   1. Learning value  -- is there a durable fact/preference/insight worth
//      capturing?
//   2. Engagement signal -- is the user elaborating and inviting
//      continuation, or mentioning in passing?
//   Rule: BOTH signals must be present to justify following a thread
//   further. Either alone is insufficient. See decisions.id=500's four
//   named outcomes (surface-for-extract-confirm / acknowledge-only /
//   follow-thread / move-on) -- reproduced as CheckinDecision below.
// This module implements that rule as a reusable evaluator. It does not
// redefine or approximate the two-axis logic.
//
// SCOPE DIFFERENCE FROM decisions.id=500: decisions.id=500 governs
// conversational elicitation depth (how far to follow a thread mid-
// conversation). cb-08's own description frames the same two-axis
// mechanism applied to a different moment -- SESSION START, surfacing
// previously-noted candidate items ("forgotten favorites, unresolved
// feedback") rather than deciding whether to keep probing live. Both are
// the same underlying rule (both signals required) applied at different
// trigger points; decisions.id=500 does not restrict itself to mid-
// conversation only, and nothing in its text excludes a session-open
// application. This module's CandidateItem/evaluate() is the general
// two-axis evaluator; SessionOpenCheckin narrows it to the session-start
// use case cb-08 names, by filtering a caller-supplied backlog of
// candidate items down to ones where both signals are present -- it does
// NOT define a new gating rule.
//
// NOT WIRED TO A LIVE CALL SITE: matches cb-06/cb-07/cb-09/cb-10's own
// posture. No existing session-start flow in the codebase currently
// builds or consumes a "backlog of unprompted items" -- that data source
// (what items exist to be surfaced, e.g. from extract_confirm_candidates
// or a dedicated backlog table) is out of this block's scope. This module
// evaluates candidates it is handed; sourcing them is a future adopter's
// job, same division of responsibility QualityAssessor has with its
// caller (quality.rs) and ProposeEvaluateRoute has with its caller
// (propose_route.rs).

// ---------------------------------------------------------------------------
// Signal strength
// ---------------------------------------------------------------------------

/// Strength of a signal, not just presence/absence -- decisions.id=500
/// says "depth is proportional to signal strength" when both are present,
/// so a bare bool would lose information a caller needs for that.
/// Ord derived: None < Low < Medium < High, for straightforward threshold
/// comparisons (`signal >= SignalStrength::Medium`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SignalStrength {
    None,
    Low,
    Medium,
    High,
}

impl SignalStrength {
    /// A signal counts as "present" for the two-axis rule at anything
    /// above None. decisions.id=500's rule is binary presence/absence at
    /// the gating level ("both signals must be present") even though
    /// strength varies continuously once gated in.
    pub fn is_present(self) -> bool {
        self > SignalStrength::None
    }
}

// ---------------------------------------------------------------------------
// CandidateItem
// ---------------------------------------------------------------------------

/// One thing that might be worth surfacing -- a forgotten favorite, an
/// unresolved feedback item, a thread that could be followed further.
/// The two signal fields are caller-supplied; this module does not itself
/// infer learning value or engagement from raw text (that inference is
/// Focus/context-specific and out of a foundation block's scope, same as
/// QualityAssessor not inventing its own criteria).
#[derive(Debug, Clone)]
pub struct CandidateItem {
    pub id: String,
    pub summary: String,
    pub learning_value: SignalStrength,
    pub engagement: SignalStrength,
}

// ---------------------------------------------------------------------------
// CheckinDecision -- decisions.id=500's four named outcomes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckinDecision {
    /// High learning value + low engagement: surface for extract-and-confirm,
    /// do not probe conversationally.
    SurfaceForConfirm,
    /// High engagement + low learning value: acknowledge warmly, do not
    /// follow the thread for capture purposes.
    AcknowledgeOnly,
    /// Both present: follow the thread. Depth is proportional to signal
    /// strength -- this module reports the decision, not a depth value;
    /// a caller wanting depth reads learning_value/engagement directly.
    FollowThread,
    /// Neither present: move on.
    MoveOn,
}

/// Apply decisions.id=500's exact rule to one candidate's two signals.
/// Pure function -- no state, mirrors QualityAssessor / ProposeEvaluateRoute.
pub fn evaluate(learning_value: SignalStrength, engagement: SignalStrength) -> CheckinDecision {
    match (learning_value.is_present(), engagement.is_present()) {
        (true, true) => CheckinDecision::FollowThread,
        (true, false) => CheckinDecision::SurfaceForConfirm,
        (false, true) => CheckinDecision::AcknowledgeOnly,
        (false, false) => CheckinDecision::MoveOn,
    }
}

// ---------------------------------------------------------------------------
// SessionOpenCheckin -- cb-08's session-start framing
// ---------------------------------------------------------------------------

/// Filters a caller-supplied backlog of candidate items down to the ones
/// worth surfacing at session open, per decisions.id=500's rule. A
/// candidate is surfaced when evaluate() returns SurfaceForConfirm or
/// FollowThread -- i.e. learning_value is present. AcknowledgeOnly and
/// MoveOn candidates are never proactively surfaced at session start:
/// AcknowledgeOnly exists for live conversational threads where the user
/// is already present to acknowledge; there is no live thread to
/// acknowledge into at session open, so a pure-engagement-signal item
/// with no learning value has nothing to surface.
pub struct SessionOpenCheckin;

impl SessionOpenCheckin {
    /// Returns surfaced candidates in the SAME order they were given --
    /// no re-ranking. If a caller wants strength-based ordering (per
    /// decisions.id=500's "depth is proportional to signal strength"),
    /// sort candidates before calling, or sort the returned items on
    /// learning_value afterward.
    pub fn surfaced_candidates(candidates: &[CandidateItem]) -> Vec<&CandidateItem> {
        candidates
            .iter()
            .filter(|c| {
                matches!(
                    evaluate(c.learning_value, c.engagement),
                    CheckinDecision::SurfaceForConfirm | CheckinDecision::FollowThread
                )
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, lv: SignalStrength, eng: SignalStrength) -> CandidateItem {
        CandidateItem { id: id.to_owned(), summary: format!("candidate {id}"), learning_value: lv, engagement: eng }
    }

    // -- SignalStrength --------------------------------------------------

    #[test]
    fn signal_strength_ordering() {
        assert!(SignalStrength::None < SignalStrength::Low);
        assert!(SignalStrength::Low < SignalStrength::Medium);
        assert!(SignalStrength::Medium < SignalStrength::High);
    }

    #[test]
    fn signal_strength_is_present() {
        assert!(!SignalStrength::None.is_present());
        assert!(SignalStrength::Low.is_present());
        assert!(SignalStrength::High.is_present());
    }

    // -- evaluate() : decisions.id=500's four outcomes --------------------

    #[test]
    fn evaluate_both_present_follows_thread() {
        assert_eq!(
            evaluate(SignalStrength::High, SignalStrength::Medium),
            CheckinDecision::FollowThread
        );
    }

    #[test]
    fn evaluate_high_learning_low_engagement_surfaces_for_confirm() {
        assert_eq!(
            evaluate(SignalStrength::High, SignalStrength::None),
            CheckinDecision::SurfaceForConfirm
        );
    }

    #[test]
    fn evaluate_high_engagement_low_learning_acknowledges_only() {
        assert_eq!(
            evaluate(SignalStrength::None, SignalStrength::High),
            CheckinDecision::AcknowledgeOnly
        );
    }

    #[test]
    fn evaluate_neither_present_moves_on() {
        assert_eq!(
            evaluate(SignalStrength::None, SignalStrength::None),
            CheckinDecision::MoveOn
        );
    }

    #[test]
    fn evaluate_low_strength_still_counts_as_present() {
        // decisions.id=500's rule is presence/absence at the gating level --
        // even the weakest nonzero signal must gate the same as a strong one.
        assert_eq!(
            evaluate(SignalStrength::Low, SignalStrength::Low),
            CheckinDecision::FollowThread
        );
    }

    // -- SessionOpenCheckin ------------------------------------------------

    #[test]
    fn surfaced_candidates_includes_surface_for_confirm() {
        let items = vec![candidate("a", SignalStrength::High, SignalStrength::None)];
        let surfaced = SessionOpenCheckin::surfaced_candidates(&items);
        assert_eq!(surfaced.len(), 1);
        assert_eq!(surfaced[0].id, "a");
    }

    #[test]
    fn surfaced_candidates_includes_follow_thread() {
        let items = vec![candidate("a", SignalStrength::Medium, SignalStrength::Medium)];
        let surfaced = SessionOpenCheckin::surfaced_candidates(&items);
        assert_eq!(surfaced.len(), 1);
    }

    #[test]
    fn surfaced_candidates_excludes_acknowledge_only() {
        // No learning value -- nothing worth surfacing at session open,
        // even with high engagement recorded from a past thread.
        let items = vec![candidate("a", SignalStrength::None, SignalStrength::High)];
        let surfaced = SessionOpenCheckin::surfaced_candidates(&items);
        assert!(surfaced.is_empty());
    }

    #[test]
    fn surfaced_candidates_excludes_move_on() {
        let items = vec![candidate("a", SignalStrength::None, SignalStrength::None)];
        let surfaced = SessionOpenCheckin::surfaced_candidates(&items);
        assert!(surfaced.is_empty());
    }

    #[test]
    fn surfaced_candidates_preserves_input_order() {
        let items = vec![
            candidate("a", SignalStrength::High, SignalStrength::None),
            candidate("b", SignalStrength::None, SignalStrength::High), // filtered out
            candidate("c", SignalStrength::Medium, SignalStrength::Low),
        ];
        let surfaced = SessionOpenCheckin::surfaced_candidates(&items);
        let ids: Vec<&str> = surfaced.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c"]);
    }

    #[test]
    fn surfaced_candidates_empty_backlog_returns_empty() {
        let items: Vec<CandidateItem> = vec![];
        assert!(SessionOpenCheckin::surfaced_candidates(&items).is_empty());
    }

    #[test]
    fn surfaced_candidates_mixed_backlog_all_four_outcomes() {
        let items = vec![
            candidate("surface", SignalStrength::High, SignalStrength::None),
            candidate("ack_only", SignalStrength::None, SignalStrength::High),
            candidate("follow", SignalStrength::Low, SignalStrength::Low),
            candidate("move_on", SignalStrength::None, SignalStrength::None),
        ];
        let surfaced = SessionOpenCheckin::surfaced_candidates(&items);
        let ids: Vec<&str> = surfaced.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["surface", "follow"]);
    }
}
