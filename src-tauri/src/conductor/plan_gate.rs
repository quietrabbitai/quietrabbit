// src-tauri/src/conductor/plan_gate.rs
//
// items.id=901 stage 4 (decisions.id=942 Q2 and Q3): the PLAN GATE of a run start. A child
// module of conductor::lifecycle (declared there with #[path]) so it can read the run's
// private ceiling.
//
// WHERE IT RUNS. commands::execution::load_and_authorize_run calls it AFTER load() and
// authorize_checks() and BEFORE open_record(): the focus_runs row is not written yet, so a
// run that needs an approval it does not have leaves nothing behind (an `initializing` row
// would be demoted to a resumable paused run after five minutes). It returns PlanGate::
// Required and the caller returns the typed LoadOutcome::PlanRequired; no run is parked for
// the plan, and the caller calls again once the user has approved (approve-then-call-again,
// the shape of API Chat's ReviewRequired).
//
// WHAT IT DOES.
//   1. A Focus with no model-block step skips everything: no plan, no read, no write.
//      A bound API Chat run keeps its bound destination (it never resolves), so it skips too.
//   2. It BUILDS the plan from the run's own Focus definition, launch resolution and
//      ceiling (conductor::run_plan::build). A plan that cannot be built is an error and the
//      run does not start (LifecycleError::PlanUnavailable): fail closed.
//   3. It READS the approval, if any: a chat's from the memory-only home then the persisted
//      row (conductor::plan_approval::read_chat_approval), or, for a run with no chat, the
//      one the opaque token names (take_offer_approval, which CONSUMES the token even if the
//      verification below then fails, so a token is spent by the run it was presented to). A
//      run with no plan memory (a rehydrated or swept run) reads nothing. A read that fails is
//      "no approval", never permission.
//   4. It VERIFIES the approval against the plan it just built (plan_approval::verify: the
//      digest is recomputed from the stored pins, no client digest is read).
//   5. Outcome: a verified approval is kept on the run (`plan`) for the executor's check;
//      with none, a plan that needs an approval (some reachable step may go hosted) returns
//      Required, and a plan that needs none lets the run proceed with no approval (so only
//      resolver picks of local models may run).
//
// NOTHING HERE WIDENS A GATE. The gate can only refuse a start or hand the executor an
// approval to compare picks against. It reads no launch value as an approval: launching, a
// launch mode or an answer never approves a hosted or metered step.

use std::sync::Arc;

use crate::conductor::lifecycle::{DisclosureLoggerForRun, FocusRun, LifecycleError};
use crate::conductor::plan_approval::{self, Approval};
use crate::conductor::plan_memory::{ChatScope, OfferOwner};
use crate::conductor::run_plan::{self, BuildInput, RunPlan};

/// Whether a Focus's composition cites a model block: ONE indexed read of shared.db
/// (focus_block_compositions by focus id and block id). A .focus file cannot carry a model
/// block (they are database-only), so a Focus with no such row has none. The plan preflight
/// of a chat send (commands::execution::plan_preflight) uses it to cost a Focus with no
/// model block this read and nothing else.
pub(crate) async fn focus_has_model_block(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
) -> Result<bool, sqlx::Error> {
    let marks = crate::conductor::model_block::MODEL_STABLE_IDS
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT 1 FROM focus_block_compositions WHERE focus_id = ? AND block_stable_id IN ({marks}) LIMIT 1"
    );
    let mut q = sqlx::query_scalar::<_, i64>(&sql).bind(focus_id);
    for id in crate::conductor::model_block::MODEL_STABLE_IDS {
        q = q.bind(*id);
    }
    Ok(q.fetch_optional(pool).await?.is_some())
}

/// Why a plan could not be offered (items.id=901 stage 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanOfferError {
    /// More than the guard path cap of paths: no plan is offered, no hosted step runs.
    TooManyPaths,
    /// Any other reason; no detail travels.
    Unavailable,
}

/// What the plan gate decided for a run start.
#[derive(Debug)]
pub(crate) enum PlanGate {
    /// The run may start. Whether it acts under an approval is on the run (`plan`).
    Proceed,
    /// The plan needs an approval the run does not have. Nothing was written.
    Required(Box<RunPlan>),
}

impl<L: DisclosureLoggerForRun> FocusRun<L> {
    /// Compute the run plan and settle its approval (see the module comment). Requires
    /// load() and authorize_checks() to have succeeded.
    pub(crate) async fn plan_gate(&mut self) -> Result<PlanGate, LifecycleError> {
        self.plan = None;
        let def = self
            .focus_def
            .as_ref()
            .expect("plan_gate() requires load() to have succeeded first");

        // 1. No model block, or a bound API Chat run: nothing is read or written.
        if !run_plan::has_planned_step(&def.steps) || self.chat_binding.is_some() {
            return Ok(PlanGate::Proceed);
        }

        // 2. The plan, from this run's own definition, launch resolution and ceiling.
        let plan = self
            .build_plan()
            .await
            .map_err(|_| LifecycleError::PlanUnavailable)?;
        let Some(plan) = plan else {
            return Ok(PlanGate::Proceed);
        };

        // 3 and 4. The approval, read and verified against the plan just built.
        let verified = self
            .read_plan_approval()
            .await
            .filter(|a| plan_approval::verify(&plan, a));

        // 5. The outcome.
        match (verified, plan.needs_approval) {
            (Some(approval), _) => {
                self.plan = Some(Arc::new(approval));
                Ok(PlanGate::Proceed)
            }
            (None, true) => Ok(PlanGate::Required(Box::new(plan))),
            (None, false) => Ok(PlanGate::Proceed),
        }
    }

    /// The plan of this run, from its own definition, launch resolution and ceiling: the ONE
    /// call to run_plan::build (a scan pins it), shared by the gate and by plan_offer so the
    /// digest a screen is shown is the digest the run recomputes.
    async fn build_plan(&self) -> Result<Option<RunPlan>, run_plan::PlanError> {
        let def = self
            .focus_def
            .as_ref()
            .expect("build_plan() requires load() to have succeeded first");
        run_plan::build(&BuildInput {
            pool: &self.pool,
            user_id: &self.user_id,
            key_hex: self.key_hex.as_deref().unwrap_or(""),
            def,
            launch: &self.launch,
            focus_ceiling: self._focus_external_access,
        })
        .await
    }

    /// The plan a screen shows (items.id=901 stage 8, commands::run_plan::get_run_plan), and
    /// whether the run would already proceed under a stored approval of this chat. Requires
    /// load() and authorize_checks(), exactly as the gate does, on a run built the same way
    /// (execution::plan_probe_run). Reads only: it writes nothing and consumes nothing (the
    /// probe run holds no plan token, so an offer is never taken here). None: the Focus has
    /// no planned step.
    pub(crate) async fn plan_offer(&self) -> Result<Option<(RunPlan, bool)>, PlanOfferError> {
        let plan = self.build_plan().await.map_err(|e| match e {
            run_plan::PlanError::TooManyPaths => PlanOfferError::TooManyPaths,
            _ => PlanOfferError::Unavailable,
        })?;
        let Some(plan) = plan else {
            return Ok(None);
        };
        let approved = self
            .read_plan_approval()
            .await
            .is_some_and(|a| plan_approval::verify(&plan, &a));
        Ok(Some((plan, approved)))
    }

    /// The approval this run may act under, if one can be read. None for any failure.
    async fn read_plan_approval(&self) -> Option<Approval> {
        let memory = self.plan_memory.as_ref()?;
        match (&self.context_key, &self.plan_token) {
            (Some(chat), _) => {
                let scope = ChatScope {
                    user_id: self.user_id.clone(),
                    persona_id: self.persona_id.clone(),
                    context_key: chat.clone(),
                    focus_id: self.focus_id.clone(),
                };
                // A database that cannot be opened is a latch that cannot be read: only the
                // memory home is then consulted.
                let mut conn = crate::persistence::output_store::open_outputs_db_retrying(
                    &self.user_id,
                    &self.persona_id,
                    self.key_hex.as_deref().unwrap_or(""),
                )
                .await
                .ok();
                plan_approval::read_chat_approval(memory, conn.as_mut(), &scope)
                    .await
                    .ok()
                    .flatten()
            }
            // A run with no chat: the offer the token names, consumed with this run.
            (None, Some(token)) => memory.take_offer_approval(
                token,
                &OfferOwner {
                    user_id: self.user_id.clone(),
                    persona_id: self.persona_id.clone(),
                    focus_id: self.focus_id.clone(),
                },
            ),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
#[path = "plan_gate_tests.rs"]
mod tests;
