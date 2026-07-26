// src-tauri/src/conductor/reentry.rs
//
// cb-07 -- Named re-entry with delta scope.
// Foundation block, items.id=128 / decisions.id=621 (catalog_status
// 'confirmed'). BUILD STATUS 2026-07-25 (Chat-DEV full-catalog survey,
// handoff id=103): confirmed a true gap -- FocusRun.current_step: usize
// (lifecycle.rs) only ever moves forward; the only two write sites are
// both inside execute()'s own forward-iterating loop (lifecycle.rs
// execute(), `for offset in 0..step_count.saturating_sub(start)`).
// Checkpoint/resume machinery restores an INTERRUPTED run to continue
// forward from where it stopped -- structurally different from a user
// intentionally jumping back to an earlier stage with new information and
// re-running forward from there. No such backward-jump/delta-scope
// mechanism existed anywhere. This module is that mechanism.
//
// DESIGN CHOICE -- NON-INVASIVE COMPUTATION, NOT A LIFECYCLE MUTATION
// (P1: no architectural improvisation): this module does NOT touch
// lifecycle.rs, does NOT reach into FocusRun.current_step, and does NOT
// itself call execute() again. It computes a ReentryPlan -- which prior
// steps' outputs must be discarded and which step index execution should
// resume from -- given a target step to re-enter and the run's existing
// step outputs. Applying that plan (setting current_step, rebuilding
// TaskTrack, re-invoking execute()) is left to the caller, exactly as
// cb-07's own description frames it as "re-running forward from that
// point" using the run's own forward machinery, not a new execution path.
// This keeps execute()'s forward-only invariant (D6-347's current_step
// contract) completely undisturbed -- a genuine architectural change to
// FocusRun's execution model is outside this block's scope and would need
// its own decision, not an inference made while building a foundation
// block.
//
// "DELTA SCOPE": the plan distinguishes steps that must be discarded
// (the target step onward -- their outputs are stale once new information
// is introduced at the re-entry point) from steps that remain valid
// (everything strictly before the target step). output_vars produced by
// discarded steps are named explicitly in the plan so a caller can also
// invalidate anything downstream that referenced them (e.g. a
// SharedStateTrack promotion keyed on a now-stale output_var) -- this
// module does not attempt that invalidation itself, since SharedStateTrack
// promotion tracking is out of this block's stated scope.

use std::collections::HashSet;

use crate::conductor::types::{TaskStep, TaskTrack};

// ---------------------------------------------------------------------------
// ReentryError
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReentryError {
    #[error("step '{0}' was not found among this run's completed steps")]
    StepNotFound(String),
    #[error("step '{0}' is the run's current/most-recent step -- nothing precedes it to re-enter from with new information; re-run the step directly instead")]
    NothingToDiscard(String),
}

// ---------------------------------------------------------------------------
// ReentryPlan
// ---------------------------------------------------------------------------

/// The computed effect of jumping back to `target_step_id` with new
/// information. Read-only -- applying it (rebuilding TaskTrack, resetting
/// FocusRun.current_step, re-invoking execute()) is the caller's job.
#[derive(Debug, Clone)]
pub struct ReentryPlan {
    /// The step index (into the focus definition's step list) execution
    /// should resume from. Equal to the index of target_step_id.
    pub resume_from_index: usize,
    pub target_step_id: String,
    /// step_ids whose recorded output is now stale and must be discarded
    /// -- target_step_id itself plus every step that ran after it.
    /// In original execution order.
    pub discarded_step_ids: Vec<String>,
    /// output_var names produced by any discarded step, deduplicated.
    /// A caller can use this to also invalidate downstream references
    /// (e.g. SharedStateTrack promotions) that read one of these vars,
    /// though this module does not perform that invalidation itself
    /// (see module doc "DELTA SCOPE").
    pub stale_output_vars: Vec<String>,
    /// Steps strictly before target_step_id -- their outputs remain valid
    /// and are preserved as-is in the rebuilt TaskTrack.
    pub preserved_step_ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// plan_reentry
// ---------------------------------------------------------------------------

/// Compute a ReentryPlan for jumping back to `target_step_id` within an
/// already-executed TaskTrack. `focus_step_order` is the full ordered list
/// of step_ids from the focus definition -- needed because TaskTrack alone
/// (types.rs) does not expose a step_id -> index lookup, and a caller must
/// know which index to resume execute() from, not just which steps to drop.
///
/// Errors if target_step_id was never executed (StepNotFound), or if it is
/// already the run's most recently completed step -- there is nothing
/// after it to discard, so re-entering "from" it is a no-op the caller
/// should instead handle as a plain re-run (NothingToDiscard).
pub fn plan_reentry(
    task_track: &TaskTrack,
    focus_step_order: &[String],
    target_step_id: &str,
) -> Result<ReentryPlan, ReentryError> {
    let executed: Vec<&TaskStep> = task_track.steps().iter().collect();

    let target_pos = executed
        .iter()
        .position(|s| s.step_id == target_step_id)
        .ok_or_else(|| ReentryError::StepNotFound(target_step_id.to_owned()))?;

    if target_pos == executed.len() - 1 {
        return Err(ReentryError::NothingToDiscard(target_step_id.to_owned()));
    }

    let preserved_step_ids: Vec<String> =
        executed[..target_pos].iter().map(|s| s.step_id.clone()).collect();

    let discarded: &[&TaskStep] = &executed[target_pos..];
    let discarded_step_ids: Vec<String> = discarded.iter().map(|s| s.step_id.clone()).collect();

    let mut seen = HashSet::new();
    let mut stale_output_vars = Vec::new();
    for s in discarded {
        if let Some(var) = &s.output_var {
            if !var.is_empty() && seen.insert(var.clone()) {
                stale_output_vars.push(var.clone());
            }
        }
    }

    let resume_from_index = focus_step_order
        .iter()
        .position(|id| id == target_step_id)
        .ok_or_else(|| ReentryError::StepNotFound(target_step_id.to_owned()))?;

    Ok(ReentryPlan {
        resume_from_index,
        target_step_id: target_step_id.to_owned(),
        discarded_step_ids,
        stale_output_vars,
        preserved_step_ids,
    })
}

/// Rebuild a TaskTrack containing only the preserved (pre-target) steps
/// from an existing track and a plan. A convenience for the common case --
/// a caller with more specific rebuild needs (e.g. also replaying some
/// preserved steps through a fresh pipeline) can instead read
/// plan.preserved_step_ids directly and build their own track.
///
/// sensitivity_ceiling on the rebuilt track is recomputed from the
/// preserved steps only (TaskTrack::add_step's own monotonic-max logic) --
/// it does NOT inherit the ceiling contributed by discarded steps, since
/// those steps' content no longer exists in this track.
pub fn rebuild_preserved_track(task_track: &TaskTrack, plan: &ReentryPlan) -> TaskTrack {
    let mut rebuilt = TaskTrack::new();
    let preserved: HashSet<&str> =
        plan.preserved_step_ids.iter().map(|s| s.as_str()).collect();
    for step in task_track.steps() {
        if preserved.contains(step.step_id.as_str()) {
            rebuilt.add_step(step.clone());
        }
    }
    rebuilt
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, output_var: Option<&str>, severity: i32) -> TaskStep {
        TaskStep {
            step_id: id.to_owned(),
            output_var: output_var.map(|s| s.to_owned()),
            content: format!("content for {id}"),
            sensitivity_severity: severity,
            routing_tier_used: 1,
        }
    }

    fn track_with(steps: &[TaskStep]) -> TaskTrack {
        let mut t = TaskTrack::new();
        for s in steps {
            t.add_step(s.clone());
        }
        t
    }

    #[test]
    fn plan_reentry_target_not_found_errors() {
        let track = track_with(&[step("s1", None, 1)]);
        let order = vec!["s1".to_owned(), "s2".to_owned()];
        let err = plan_reentry(&track, &order, "does-not-exist").unwrap_err();
        assert_eq!(err, ReentryError::StepNotFound("does-not-exist".to_owned()));
    }

    #[test]
    fn plan_reentry_most_recent_step_errors_nothing_to_discard() {
        let track = track_with(&[step("s1", None, 1), step("s2", None, 1)]);
        let order = vec!["s1".to_owned(), "s2".to_owned()];
        let err = plan_reentry(&track, &order, "s2").unwrap_err();
        assert_eq!(err, ReentryError::NothingToDiscard("s2".to_owned()));
    }

    #[test]
    fn plan_reentry_middle_step_splits_preserved_and_discarded() {
        let track = track_with(&[
            step("s1", None, 1),
            step("s2", None, 1),
            step("s3", None, 1),
        ]);
        let order = vec!["s1".to_owned(), "s2".to_owned(), "s3".to_owned()];
        let plan = plan_reentry(&track, &order, "s2").unwrap();
        assert_eq!(plan.preserved_step_ids, vec!["s1".to_owned()]);
        assert_eq!(plan.discarded_step_ids, vec!["s2".to_owned(), "s3".to_owned()]);
        assert_eq!(plan.resume_from_index, 1);
        assert_eq!(plan.target_step_id, "s2");
    }

    #[test]
    fn plan_reentry_first_step_discards_everything() {
        let track = track_with(&[step("s1", None, 1), step("s2", None, 1)]);
        let order = vec!["s1".to_owned(), "s2".to_owned()];
        let plan = plan_reentry(&track, &order, "s1").unwrap();
        assert!(plan.preserved_step_ids.is_empty());
        assert_eq!(plan.discarded_step_ids, vec!["s1".to_owned(), "s2".to_owned()]);
        assert_eq!(plan.resume_from_index, 0);
    }

    #[test]
    fn plan_reentry_collects_stale_output_vars_deduplicated() {
        let track = track_with(&[
            step("s1", Some("draft"), 1),
            step("s2", Some("review"), 1),
            step("s3", Some("review"), 1), // same var name reused -- dedup
            step("s4", None, 1),
        ]);
        let order = vec!["s1".to_owned(), "s2".to_owned(), "s3".to_owned(), "s4".to_owned()];
        let plan = plan_reentry(&track, &order, "s2").unwrap();
        assert_eq!(plan.stale_output_vars, vec!["review".to_owned()]);
        // s1's "draft" is preserved, not discarded -- must not appear.
        assert!(!plan.stale_output_vars.contains(&"draft".to_owned()));
    }

    #[test]
    fn plan_reentry_empty_output_var_not_collected() {
        let track = track_with(&[
            step("s1", None, 1),
            step("s2", Some(""), 1),
        ]);
        let order = vec!["s1".to_owned(), "s2".to_owned()];
        let plan = plan_reentry(&track, &order, "s1").unwrap();
        assert!(plan.stale_output_vars.is_empty());
    }

    #[test]
    fn rebuild_preserved_track_keeps_only_preserved_steps() {
        let track = track_with(&[
            step("s1", Some("a"), 2),
            step("s2", Some("b"), 3),
            step("s3", Some("c"), 1),
        ]);
        let order = vec!["s1".to_owned(), "s2".to_owned(), "s3".to_owned()];
        let plan = plan_reentry(&track, &order, "s2").unwrap();
        let rebuilt = rebuild_preserved_track(&track, &plan);
        assert_eq!(rebuilt.steps().len(), 1);
        assert_eq!(rebuilt.steps()[0].step_id, "s1");
        assert_eq!(rebuilt.get_output("a"), Some("content for s1"));
        assert_eq!(rebuilt.get_output("b"), None);
    }

    #[test]
    fn rebuild_preserved_track_ceiling_reflects_only_preserved_steps() {
        // Discarded step s2 has the highest severity (3) -- the rebuilt
        // track's ceiling must not inherit it once s2 is gone.
        let track = track_with(&[
            step("s1", None, 1),
            step("s2", None, 3),
            step("s3", None, 1),
        ]);
        let order = vec!["s1".to_owned(), "s2".to_owned(), "s3".to_owned()];
        let plan = plan_reentry(&track, &order, "s2").unwrap();
        let rebuilt = rebuild_preserved_track(&track, &plan);
        assert_eq!(rebuilt.sensitivity_ceiling(), 1);
    }

    #[test]
    fn rebuild_preserved_track_empty_when_reentering_first_step() {
        let track = track_with(&[step("s1", None, 1), step("s2", None, 1)]);
        let order = vec!["s1".to_owned(), "s2".to_owned()];
        let plan = plan_reentry(&track, &order, "s1").unwrap();
        let rebuilt = rebuild_preserved_track(&track, &plan);
        assert!(rebuilt.steps().is_empty());
    }

    #[test]
    fn plan_reentry_resume_index_matches_focus_step_order_not_track_position() {
        // focus_step_order can differ in length/content from what actually
        // executed (e.g. a run stopped early) -- resume_from_index must
        // come from focus_step_order's own position, not the track's.
        let track = track_with(&[step("s2", None, 1), step("s3", None, 1)]);
        let order = vec!["s1".to_owned(), "s2".to_owned(), "s3".to_owned()];
        let plan = plan_reentry(&track, &order, "s2").unwrap();
        assert_eq!(plan.resume_from_index, 1); // s2's index in focus_step_order
    }
}
