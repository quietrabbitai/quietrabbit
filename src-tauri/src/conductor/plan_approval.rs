// src-tauri/src/conductor/plan_approval.rs
//
// items.id=901 stage 3: the APPROVAL MODEL of the run plan. What an approval is, how one is
// made from the plan the user was shown, how a stored one is checked against the plan the
// run recomputes, and where it lives (ruling R-A: two homes).
//
// WHAT AN APPROVAL HOLDS. A digest, a per-run cost ceiling in micro-USD, and the user's pins
// (an explicit choice of an unmeasured local model or a paid API model for a step). The
// stored digest is `plan.digest_with(&pins)`: the pin-free plan digest that get_run_plan
// showed, with the pins folded in (conductor::run_plan). The pins are also kept apart, as the
// choices of the persisted row (choices_json), so a later run can recompute the digest.
//
// HOW AN APPROVAL IS MADE (approve). The command takes the PIN-FREE digest the user was shown.
// The server recomputes the plan and compares: a different digest is PlanChanged and nothing
// is approved. Then each pin must be one of the plan's own SELECTABLE offers for its step, of
// an approvable strategy (only an unmeasured local model or a paid API model can be pinned:
// a model the resolver would pick anyway needs no pin), at most one per step. The ceiling must
// cover the plan's worst-case estimate under the pins and may not exceed
// PLAN_CEILING_MAX_MICROS.
//
// HOW A STORED APPROVAL IS USED (verify). The run recomputes the plan from the Focus, the
// launch answers and the current ceiling, and recomputes `digest_with(stored pins)`. It is
// compared with the stored digest. A digest the CLIENT sends is never used by a run: the only
// client digest is the one approve() compares to the freshly recomputed plan.
//
// WHERE IT LIVES (ruling R-A).
//   * an UNLATCHED chat: persisted in the persona's outputs.db by plan_approval_store::
//     put_approval (one BEGIN IMMEDIATE, latch checked first), on a connection opened with
//     open_outputs_db_retrying;
//   * a LATCHED chat, a chat whose latch cannot be read, a chat whose database cannot be
//     opened: NOTHING is written. The approval goes to the memory-only home
//     (conductor::plan_memory), never to the persisted one;
//   * a run with NO chat: only the run-scoped offer in the memory-only home, by opaque token,
//     consumed with the run (plan_memory).
//   Reading prefers memory, since a memory approval can only be NEWER than a persisted one
//   (it was made after the latch). Revocation removes both. Approvals persisted before a latch
//   was set stay and still read; no retention sweep covers them until items.id=888.
//
// NOTHING HERE WIDENS A GATE. An approval narrows nothing and widens nothing by itself: the
// executor still stops any step beyond it (stage 4) and the spend gate and the outbound
// review still run. This module names no launch value (a scan pins it): launching, a mode or
// an answer never approves anything.

use serde::Deserialize;
use sqlx::SqliteConnection;

use crate::conductor::plan_memory::{ChatScope, PlanMemory};
use crate::conductor::run_plan::{OfferStrategy, Pin, RunPlan};
use crate::persistence::plan_approval_store::{
    delete_approval, get_approval, put_approval, ApprovalStoreError, ApprovalWrite,
};
use crate::persistence::session_store::LatchError;

/// The most a user may set as a run's cost ceiling: 25 USD in micro-USD. A bound on the
/// approval, not a price: the spend gate's own limits still apply under it.
pub const PLAN_CEILING_MAX_MICROS: u64 = 25_000_000;

/// What the user approved. Debug shows nothing of it.
#[derive(Clone, PartialEq, Eq)]
pub struct Approval {
    /// `plan.digest_with(&pins)`.
    pub digest: String,
    pub ceiling_micros: u64,
    pub pins: Vec<Pin>,
}

impl std::fmt::Debug for Approval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Approval(<redacted>)")
    }
}

impl Approval {
    /// Whether the approval holds a PAID API pin for this step: the only way a model-block
    /// step may go hosted (the executor stops every other hosted pick). The Gate 3
    /// look-ahead (lifecycle::next_execution_tier_under) reads exactly this fact, and a test
    /// pins it against the executor's own rule.
    pub fn has_paid_pin(&self, action_id: &str) -> bool {
        self.pins
            .iter()
            .any(|p| p.action_id == action_id && p.strategy == OfferStrategy::PaidApi)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    #[error("the plan changed since it was shown")]
    PlanChanged,
    #[error("a choice names a step the plan does not have")]
    UnknownStep,
    #[error("a step has more than one choice")]
    DuplicatePin,
    #[error("that kind of choice needs no approval")]
    PinNotAllowed,
    #[error("a choice is not one of the offers of its step")]
    PinNotOffered,
    #[error("a choice names an offer that cannot be selected")]
    NotSelectable,
    #[error("the ceiling is below the plan's worst-case estimate")]
    CeilingBelowEstimate,
    #[error("the ceiling is above the most a run may be approved for")]
    CeilingTooHigh,
    #[error("the stored choices could not be read")]
    BadChoices,
    #[error("the stored approval could not be read")]
    Unreadable,
    #[error("the approval could not be stored")]
    Unavailable,
}

// ---------------------------------------------------------------------------
// Making and checking an approval (pure)
// ---------------------------------------------------------------------------

/// Make an approval from the plan the server just recomputed and what the user sent: the
/// PIN-FREE digest they were shown, a ceiling and their pins.
pub fn approve(
    plan: &RunPlan,
    shown_digest: &str,
    ceiling_micros: u64,
    pins: Vec<Pin>,
) -> Result<Approval, ApprovalError> {
    if plan.digest != shown_digest {
        return Err(ApprovalError::PlanChanged);
    }
    if ceiling_micros > PLAN_CEILING_MAX_MICROS {
        return Err(ApprovalError::CeilingTooHigh);
    }
    for (i, pin) in pins.iter().enumerate() {
        let step = plan
            .steps
            .iter()
            .find(|s| s.action_id == pin.action_id)
            .ok_or(ApprovalError::UnknownStep)?;
        if pins[..i].iter().any(|p| p.action_id == pin.action_id) {
            return Err(ApprovalError::DuplicatePin);
        }
        if pin.strategy == OfferStrategy::Local {
            return Err(ApprovalError::PinNotAllowed);
        }
        let offer = step
            .offers
            .iter()
            .find(|o| {
                o.strategy == pin.strategy
                    && o.provider_id == pin.provider_id
                    && o.model_id == pin.model_id
            })
            .ok_or(ApprovalError::PinNotOffered)?;
        if !offer.selectable {
            return Err(ApprovalError::NotSelectable);
        }
    }
    if ceiling_micros < plan.estimate_micros(&pins) {
        return Err(ApprovalError::CeilingBelowEstimate);
    }
    Ok(Approval {
        digest: plan.digest_with(&pins),
        ceiling_micros,
        pins,
    })
}

/// Whether a stored approval still describes the plan the run just recomputed: the digest of
/// the plan under the stored pins equals the stored digest. The run's own plan is the only
/// input; no client digest is read.
pub fn verify(plan: &RunPlan, stored: &Approval) -> bool {
    plan.digest_with(&stored.pins) == stored.digest
}

/// The pins as the persisted row's choices_json.
pub fn pins_to_json(pins: &[Pin]) -> Result<String, ApprovalError> {
    serde_json::to_string(pins).map_err(|_| ApprovalError::Unavailable)
}

/// The pins of a persisted row. A choice this build cannot read is an error, never skipped.
pub fn pins_from_json(json: &str) -> Result<Vec<Pin>, ApprovalError> {
    #[derive(Deserialize)]
    #[serde(transparent)]
    struct Pins(Vec<Pin>);
    serde_json::from_str::<Pins>(json)
        .map(|p| p.0)
        .map_err(|_| ApprovalError::BadChoices)
}

// ---------------------------------------------------------------------------
// The two homes of an approval of a chat
// ---------------------------------------------------------------------------

/// Where a saved approval went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Home {
    Persisted { version: i64 },
    Memory,
}

/// What a revocation removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Revoked {
    pub memory: bool,
    pub persisted: bool,
}

fn to_row(approval: &Approval) -> Result<(i64, String), ApprovalError> {
    let ceiling = i64::try_from(approval.ceiling_micros).map_err(|_| ApprovalError::Unavailable)?;
    Ok((ceiling, pins_to_json(&approval.pins)?))
}

/// Save the approval of a chat. `conn` is the persona's outputs.db, or None when it could
/// not be opened (the latch then cannot be read). Persisted ONLY when the write succeeds
/// under the latch check; a latched chat, an unreadable latch or any store failure sends the
/// approval to the memory-only home and writes nothing. THE ONLY CALLER of put_approval.
pub async fn save_chat_approval(
    memory: &PlanMemory,
    conn: Option<&mut SqliteConnection>,
    scope: &ChatScope,
    approval: &Approval,
) -> Result<Home, ApprovalError> {
    let (ceiling, choices) = to_row(approval)?;
    let Some(conn) = conn else {
        memory.put_chat_approval(scope, approval.clone());
        return Ok(Home::Memory);
    };
    let write = ApprovalWrite {
        context_key: &scope.context_key,
        focus_id: &scope.focus_id,
        digest: &approval.digest,
        ceiling_micros: ceiling,
        choices_json: &choices,
    };
    match put_approval(conn, &write).await {
        Ok(outcome) => {
            // One source of truth: an older memory approval of this chat is dropped.
            memory.remove_chat_approval(scope);
            Ok(Home::Persisted {
                version: outcome.version(),
            })
        }
        // Latched, latch unreadable, or the write failed: nothing was written.
        Err(ApprovalStoreError::Latch(LatchError::Suppressed))
        | Err(ApprovalStoreError::Latch(LatchError::Unreadable(_)))
        | Err(ApprovalStoreError::Database(_)) => {
            memory.put_chat_approval(scope, approval.clone());
            Ok(Home::Memory)
        }
        // A malformed write is a bug, not a reason to hold it somewhere else.
        Err(ApprovalStoreError::Validation(_)) => Err(ApprovalError::Unavailable),
    }
}

/// Read the approval of a chat: memory first (it can only be newer), then the persisted
/// row. A persisted row that cannot be read, or a missing connection, is an Err and the
/// caller treats it as NO approval: never as permission.
pub async fn read_chat_approval(
    memory: &PlanMemory,
    conn: Option<&mut SqliteConnection>,
    scope: &ChatScope,
) -> Result<Option<Approval>, ApprovalError> {
    if let Some(held) = memory.get_chat_approval(scope) {
        return Ok(Some(held));
    }
    let Some(conn) = conn else {
        return Err(ApprovalError::Unreadable);
    };
    let row = get_approval(conn, &scope.context_key, &scope.focus_id)
        .await
        .map_err(|_| ApprovalError::Unreadable)?;
    row.map(|r| {
        Ok(Approval {
            digest: r.digest,
            ceiling_micros: u64::try_from(r.ceiling_micros)
                .map_err(|_| ApprovalError::Unreadable)?,
            pins: pins_from_json(&r.choices_json)?,
        })
    })
    .transpose()
}

/// Withdraw the approval of a chat from BOTH homes. Not latch-gated: it only removes a
/// permission. With no connection only the memory home is reached and the persisted one is
/// reported as an error, so the caller can retry.
pub async fn revoke_chat_approval(
    memory: &PlanMemory,
    conn: Option<&mut SqliteConnection>,
    scope: &ChatScope,
) -> Result<Revoked, ApprovalError> {
    let memory_removed = memory.remove_chat_approval(scope);
    let Some(conn) = conn else {
        return Err(ApprovalError::Unavailable);
    };
    let persisted = delete_approval(conn, &scope.context_key, &scope.focus_id)
        .await
        .map_err(|_| ApprovalError::Unavailable)?;
    Ok(Revoked {
        memory: memory_removed,
        persisted,
    })
}

#[cfg(test)]
#[path = "plan_approval_tests.rs"]
mod tests;
