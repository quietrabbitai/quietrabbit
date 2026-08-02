// src-tauri/src/conductor/propose_route.rs
//
// cb-10 -- Propose -> evaluate feedback -> route.
// Foundation block, items.id=128 / decisions.id=621 (catalog_status
// 'confirmed'). BUILD STATUS 2026-07-25 (Chat-DEV full-catalog survey,
// handoff id=103): confirmed a true gap -- the only related hit anywhere
// in the codebase was domain_context_store.rs's proposed_content /
// proposed_preset fields, a data-staging concept for domain-knowledge
// summaries (unrelated feature), not a generic propose/evaluate-against-
// expected-outcome/branch control-flow pattern. No such branching logic
// existed anywhere. This module is that pattern.
//
// SCOPE (cb-10 description): "Domain-specific logic lives in customization
// layer." This block is deliberately thin -- it holds the three-stage
// control-flow shape (propose -> collect feedback -> evaluate -> branch)
// and nothing about what a proposal or feedback signal actually MEANS for
// any given Focus. A Focus's own step logic supplies:
//   - what "proposed" looks like (a draft, a suggested next action, a
//     candidate answer -- this module treats it as an opaque String)
//   - how to interpret a feedback signal against the expected outcome
//     (via a caller-supplied evaluator closure, same pattern as cb-06's
//     QualityCriterion::Custom -- see quality.rs)
//   - what happens on each branch (this module only returns the branch
//     decision; it does not itself retry, escalate, or call a provider)
//
// RELATIONSHIP TO OTHER BLOCKS: this is a general-purpose control-flow
// primitive, not a replacement for FailureHandler (failure.rs, the
// provider/infra error taxonomy) or QualityAssessor (quality.rs, criteria-
// based pass/fail scoring). A Focus step can use ProposeEvaluateRoute for
// its own domain decision ("did the user like this vacation itinerary
// draft, or should I revise it, or should I ask a human for help") while
// still using FailureHandler for provider errors and QualityAssessor for
// completeness checks on the same content -- the three compose, they do
// not overlap.
//
// NOT WIRED INTO executor.rs: StepExecutor's step sequence
// (Architecture Section 6.3, see executor.rs module doc) is a fixed
// 15-step pipeline; propose/evaluate/route is a general reusable
// primitive a Focus's own customization-layer logic can call, not a
// steps 1-15 concern. Matches cb-06 and cb-09's own "not wired to a live
// call site yet" precedent -- no existing Focus step in the codebase
// currently needs this pattern; it is ready for a future adopter.

// ---------------------------------------------------------------------------
// FeedbackSignal
// ---------------------------------------------------------------------------

/// The result/feedback collected after a proposal was made. Deliberately a
/// plain content string plus a caller-interpreted success flag rather than
/// a typed domain object -- this block does not know what a Focus's
/// feedback looks like (a user's typed reply, a tool's return value, a
/// re-run's diff). The evaluator closure passed to `evaluate()` is where
/// domain interpretation happens.
#[derive(Debug, Clone)]
pub struct FeedbackSignal {
    pub content: String,
    /// Caller-supplied convenience flag -- e.g. "the user clicked accept"
    /// or "the retried step returned without error". Not required to be
    /// meaningful; evaluate() also receives the full FeedbackSignal so a
    /// caller who needs to inspect `content` directly still can.
    pub succeeded: bool,
}

// ---------------------------------------------------------------------------
// RouteDecision
// ---------------------------------------------------------------------------

/// The three branches named in cb-10's own description
/// (continue/retry differently/escalate), plus Stop for a caller that
/// decides no further action is appropriate. Matches the shape of
/// FailureAction (failure.rs) deliberately -- both are "what happens
/// next" enums -- without importing FailureAction itself, since this
/// block's branches are evaluated against a caller's own expected
/// outcome, not against ConductorError's fixed taxonomy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    /// Feedback matched the expected outcome -- proceed as planned.
    Continue,
    /// Feedback did not match, but a different approach may succeed.
    /// Carries an optional caller-facing note on what to try differently.
    RetryDifferently { note: Option<String> },
    /// Feedback indicates this needs a human decision the caller cannot
    /// make on its own (mirrors FailureAction::AwaitUser's shape without
    /// depending on it).
    Escalate { reason: String },
    /// Caller-determined: no further action should be taken.
    Stop,
}

// ---------------------------------------------------------------------------
// RouteOutcome
// ---------------------------------------------------------------------------

/// The full result of one propose -> evaluate -> route cycle.
#[derive(Debug, Clone)]
pub struct RouteOutcome {
    pub decision: RouteDecision,
    /// The proposal content this outcome was evaluated against, carried
    /// through for callers that log or display the full cycle.
    pub proposal: String,
    pub feedback: FeedbackSignal,
}

// ---------------------------------------------------------------------------
// ProposeEvaluateRoute
// ---------------------------------------------------------------------------

/// Stateless orchestrator for one propose/evaluate/route cycle. Mirrors
/// FailureHandler and QualityAssessor's stateless design -- no run state
/// lives here; callers own proposal history, retry counts, and any
/// escalation-count ceiling.
///
/// The evaluator is a caller-supplied closure: (&FeedbackSignal) -> RouteDecision.
/// This block does not ship a default evaluator, matching cb-10's
/// "domain-specific logic lives in customization layer" framing --
/// there is no generic notion of "matches expected outcome" that would
/// be correct across Focuses.
pub struct ProposeEvaluateRoute<F>
where
    F: Fn(&FeedbackSignal) -> RouteDecision,
{
    evaluator: F,
}

impl<F> ProposeEvaluateRoute<F>
where
    F: Fn(&FeedbackSignal) -> RouteDecision,
{
    pub fn new(evaluator: F) -> Self {
        Self { evaluator }
    }

    /// Run one cycle: given a proposal that was already made (this block
    /// does not generate proposals itself -- the caller's own step logic
    /// does that, e.g. via a provider call) and the feedback collected in
    /// response, evaluate and return the branch decision.
    pub fn evaluate(&self, proposal: impl Into<String>, feedback: FeedbackSignal) -> RouteOutcome {
        let decision = (self.evaluator)(&feedback);
        RouteOutcome {
            decision,
            proposal: proposal.into(),
            feedback,
        }
    }
}

// ---------------------------------------------------------------------------
// Common evaluator helpers
// ---------------------------------------------------------------------------

/// The simplest possible evaluator: succeeded -> Continue, else -> Stop.
/// A starting point for callers who do not need RetryDifferently/Escalate
/// distinctions -- not a required entry point, just a convenience so the
/// most common case (binary accept/reject) does not require writing a
/// closure by hand.
pub fn simple_accept_or_stop(feedback: &FeedbackSignal) -> RouteDecision {
    if feedback.succeeded {
        RouteDecision::Continue
    } else {
        RouteDecision::Stop
    }
}

/// A three-branch evaluator with a retry ceiling: succeeded -> Continue;
/// failed and under the ceiling -> RetryDifferently; failed at or past
/// the ceiling -> Escalate. `attempt_count` is 1-indexed (the caller's
/// Nth attempt) and owned by the caller, matching FailureHandler's own
/// retry_count-is-caller-owned convention (failure.rs).
pub fn retry_then_escalate(
    feedback: &FeedbackSignal,
    attempt_count: u32,
    max_attempts: u32,
) -> RouteDecision {
    if feedback.succeeded {
        return RouteDecision::Continue;
    }
    if attempt_count < max_attempts {
        RouteDecision::RetryDifferently { note: None }
    } else {
        RouteDecision::Escalate {
            reason: format!(
                "No acceptable result after {attempt_count} attempt(s) (max {max_attempts})."
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_feedback() -> FeedbackSignal {
        FeedbackSignal {
            content: "looks good".to_owned(),
            succeeded: true,
        }
    }

    fn fail_feedback() -> FeedbackSignal {
        FeedbackSignal {
            content: "not quite right".to_owned(),
            succeeded: false,
        }
    }

    #[test]
    fn evaluate_carries_proposal_and_feedback_through() {
        let per = ProposeEvaluateRoute::new(simple_accept_or_stop);
        let outcome = per.evaluate("draft v1", ok_feedback());
        assert_eq!(outcome.proposal, "draft v1");
        assert_eq!(outcome.feedback.content, "looks good");
    }

    #[test]
    fn simple_accept_or_stop_continue_on_success() {
        let per = ProposeEvaluateRoute::new(simple_accept_or_stop);
        let outcome = per.evaluate("draft", ok_feedback());
        assert_eq!(outcome.decision, RouteDecision::Continue);
    }

    #[test]
    fn simple_accept_or_stop_stops_on_failure() {
        let per = ProposeEvaluateRoute::new(simple_accept_or_stop);
        let outcome = per.evaluate("draft", fail_feedback());
        assert_eq!(outcome.decision, RouteDecision::Stop);
    }

    #[test]
    fn retry_then_escalate_continues_on_success_regardless_of_attempt() {
        let per = ProposeEvaluateRoute::new(|fb: &FeedbackSignal| retry_then_escalate(fb, 3, 3));
        let outcome = per.evaluate("draft", ok_feedback());
        assert_eq!(outcome.decision, RouteDecision::Continue);
    }

    #[test]
    fn retry_then_escalate_retries_under_ceiling() {
        let per = ProposeEvaluateRoute::new(|fb: &FeedbackSignal| retry_then_escalate(fb, 1, 3));
        let outcome = per.evaluate("draft", fail_feedback());
        assert_eq!(
            outcome.decision,
            RouteDecision::RetryDifferently { note: None }
        );
    }

    #[test]
    fn retry_then_escalate_escalates_at_ceiling() {
        let per = ProposeEvaluateRoute::new(|fb: &FeedbackSignal| retry_then_escalate(fb, 3, 3));
        let outcome = per.evaluate("draft", fail_feedback());
        match outcome.decision {
            RouteDecision::Escalate { reason } => {
                assert!(reason.contains("3 attempt"));
                assert!(reason.contains("max 3"));
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    #[test]
    fn retry_then_escalate_escalates_past_ceiling() {
        // attempt_count > max_attempts (caller kept going past the ceiling)
        // must still escalate, not retry -- < comparison, not !=.
        let per = ProposeEvaluateRoute::new(|fb: &FeedbackSignal| retry_then_escalate(fb, 5, 3));
        let outcome = per.evaluate("draft", fail_feedback());
        assert!(matches!(outcome.decision, RouteDecision::Escalate { .. }));
    }

    #[test]
    fn custom_evaluator_can_produce_retry_differently_with_note() {
        let per =
            ProposeEvaluateRoute::new(|_fb: &FeedbackSignal| RouteDecision::RetryDifferently {
                note: Some("try a shorter draft".to_owned()),
            });
        let outcome = per.evaluate("draft", fail_feedback());
        match outcome.decision {
            RouteDecision::RetryDifferently { note } => {
                assert_eq!(note.as_deref(), Some("try a shorter draft"));
            }
            other => panic!("expected RetryDifferently, got {other:?}"),
        }
    }

    #[test]
    fn custom_evaluator_can_inspect_feedback_content() {
        // Confirms the evaluator receives the full FeedbackSignal, not just
        // the succeeded flag -- a caller needing to inspect `content`
        // directly (e.g. keyword matching on a user's typed reply) can.
        let per = ProposeEvaluateRoute::new(|fb: &FeedbackSignal| {
            if fb.content.contains("not quite") {
                RouteDecision::Escalate {
                    reason: "user expressed dissatisfaction".to_owned(),
                }
            } else {
                RouteDecision::Continue
            }
        });
        let outcome = per.evaluate("draft", fail_feedback());
        assert!(matches!(outcome.decision, RouteDecision::Escalate { .. }));
    }

    #[test]
    fn custom_evaluator_can_produce_stop() {
        let per = ProposeEvaluateRoute::new(|_fb: &FeedbackSignal| RouteDecision::Stop);
        let outcome = per.evaluate("draft", ok_feedback());
        assert_eq!(outcome.decision, RouteDecision::Stop);
    }
}
