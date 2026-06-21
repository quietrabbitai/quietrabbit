// src-tauri/src/conductor/concurrency.rs
// ConductorScheduler — resource arbitration for concurrent focus runs.
//
// Enforces two independent ceilings (Architecture Section 6.10):
//   MAX_CONCURRENT_INFERENCE=1  — only one Ollama call at a time (GPU serialization)
//   MAX_CONCURRENT_RUNS         — total simultaneous focus runs (default 3, env-tunable)
//
// Interactive focuses request cooperative yield from background focuses at step boundaries.
// Preemption is cooperative — background runs check should_yield() BEFORE acquiring
// the inference slot for their next step. Active inference calls cannot be interrupted.
// This is intentional Layer 3 architecture (Section 6.10), not a limitation — Layer 4
// is the planned home for a strict priority queue.
//
// Layer 3: scheduling infrastructure only. Lifecycle wires should_yield() checks
// at step boundaries in the same pass as executor wiring.
// Layer 4: replace raw semaphore priority logic with a proper priority queue.
//
// Port of conductor/concurrency.py — Python threading primitives replaced:
//   threading.Semaphore → tokio::sync::Semaphore (async acquire)
//   threading.Lock      → std::sync::Mutex        (synchronous lock)
//
// Mutex choice — std::sync::Mutex (not tokio::sync::Mutex):
//   The invariant that makes this correct: NO Mutex guard is ever held across
//   an .await point. All critical sections are CPU-local (map lookups, counter
//   increments) — no I/O, no blocking operations, no async work inside any lock.
//   std::sync::Mutex is the direct analogue of Python's threading.Lock() and is
//   correct for non-IO coordination metadata.
//   Intentionally non-scalable: bounded by design assumption of low contention
//   (MAX_CONCURRENT_RUNS ≤ ~3–8). Revisit if that ceiling grows significantly.
//
// Non-RAII resource management (permit.forget + add_permits):
//   Preserved from Python oracle's explicit acquire/release method contract.
//   Python lifecycle calls semaphore.release() in finally blocks — Rust lifecycle
//   calls release_*() at cleanup. This is oracle-faithful, not an oversight.
//   Every acquire path has exactly one corresponding release path.
//   NOT replaced with SlotGuard RAII: that would redesign the lifecycle contract.
//
// interactive_wait_count:
//   Informational only (Layer 4 signal). Intentionally non-deterministic —
//   can drift under Tokio task cancellation, same as Python oracle.
//   Excluded from golden-vector parity validation. Not used for runtime decisions.
//
// preempt_requested_for:
//   Eventual-consistency coordination flag — intentionally allowed to be stale.
//   Cleared opportunistically on release, not transactionally. Designed behavior;
//   do not "fix" perceived race conditions here without Layer 4 redesign.
//
// Env config: all timeouts and capacity read once at new() — not reloadable
//   at runtime. Environment changes after startup are not observed.

use std::collections::HashMap;
use std::env;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Hard limit — GPU cannot safely parallelize inference calls.
pub const MAX_CONCURRENT_INFERENCE: usize = 1;

fn read_max_concurrent_runs() -> usize {
    env::var("QR_MAX_CONCURRENT_PATHS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
}

fn read_run_slot_timeout() -> Duration {
    let secs = env::var("QR_PATH_SLOT_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(30.0);
    Duration::from_secs_f64(secs)
}

fn read_inference_slot_timeout() -> Duration {
    let secs = env::var("QR_INFERENCE_SLOT_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(60.0);
    Duration::from_secs_f64(secs)
}

// ---------------------------------------------------------------------------
// PathPriority
// ---------------------------------------------------------------------------

/// Priority of a focus run — controls cooperative yield behaviour.
/// Names kept identical to Python oracle: INTERACTIVE / BACKGROUND.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPriority {
    /// User-initiated, foreground run.
    Interactive,
    /// Scheduled or deferred run.
    Background,
}

// ---------------------------------------------------------------------------
// SchedulerState
// ---------------------------------------------------------------------------

struct SchedulerState {
    active_runs: HashMap<String, PathPriority>,
    inference_holder: Option<String>,
    /// Eventual-consistency coordination flag. Intentionally allowed to be stale.
    /// Cleared opportunistically on release, not transactionally. Designed behavior.
    preempt_requested_for: Option<String>,
    /// Layer 4 priority queue signal — informational only. Non-deterministic under
    /// task cancellation (same as Python oracle). Excluded from parity validation.
    interactive_wait_count: u32,
}

// ---------------------------------------------------------------------------
// ConductorScheduler
// ---------------------------------------------------------------------------

/// Manages two resource ceilings:
///   - inference_slot: Semaphore(1) — serializes GPU calls
///   - run_slots: Semaphore(MAX_CONCURRENT_RUNS) — caps total active runs
///
/// Interactive runs request cooperative yield from background runs.
/// Preemption is cooperative — background runs check should_yield() at step
/// boundaries. This is intentional Layer 3 architecture; strict priority
/// enforcement is deferred to Layer 4.
///
/// False returns from acquire_*() indicate resource contention.
/// Callers are responsible for mapping false to plain-language recovery options
/// (e.g. "Quiet Rabbit is busy — your run will start shortly").
///
/// INVARIANT: release_*() must only be called after the matching acquire_*()
/// returned true. Callers that acquire conditionally must track success themselves.
/// Layer 4 should move to explicit ownership tokens to enforce this structurally.
///
/// interactive_wait_count: tracks queued interactive requests for future
/// priority-queue implementation (Layer 4). Currently informational only.
pub struct ConductorScheduler {
    inference_slot: Semaphore,
    run_slots: Semaphore,
    /// Cached at new() — env config is not reloadable at runtime.
    run_slot_timeout: Duration,
    /// Cached at new() — env config is not reloadable at runtime.
    inference_slot_timeout: Duration,
    /// Guards active_runs, inference_holder, preempt_requested_for,
    /// interactive_wait_count. std::sync::Mutex: no guard is ever held across
    /// an .await point — all critical sections are CPU-local.
    state: Mutex<SchedulerState>,
}

impl ConductorScheduler {
    pub fn new() -> Self {
        Self {
            inference_slot: Semaphore::new(MAX_CONCURRENT_INFERENCE),
            run_slots: Semaphore::new(read_max_concurrent_runs()),
            run_slot_timeout: read_run_slot_timeout(),
            inference_slot_timeout: read_inference_slot_timeout(),
            state: Mutex::new(SchedulerState {
                active_runs: HashMap::new(),
                inference_holder: None,
                preempt_requested_for: None,
                interactive_wait_count: 0,
            }),
        }
    }

    // -- Run slot management -------------------------------------------------

    /// Acquire a run execution slot before starting a focus run.
    /// Returns true on success, false on timeout (resource contention).
    /// Caller maps false to plain-language recovery message.
    pub async fn acquire_run_slot(&self, run_id: &str, priority: PathPriority) -> bool {
        match timeout(self.run_slot_timeout, self.run_slots.acquire()).await {
            Ok(Ok(permit)) => {
                permit.forget();
                self.state
                    .lock()
                    .unwrap()
                    .active_runs
                    .insert(run_id.to_string(), priority);
                true
            }
            _ => false,
        }
    }

    /// Release a run execution slot at cleanup.
    /// Safe to call on unregistered run_id (logs a warning — debug observability
    /// only, not part of logical contract).
    /// Must only be called after acquire_run_slot() returned true.
    pub fn release_run_slot(&self, run_id: &str) {
        let removed = self
            .state
            .lock()
            .unwrap()
            .active_runs
            .remove(run_id)
            .is_some();
        if removed {
            self.run_slots.add_permits(1);
        } else {
            log::warn!(
                "conductor_scheduler: release_run_slot called for unregistered \
                 run_id={run_id} — possible double-release or missing acquire"
            );
        }
    }

    // -- Inference slot management -------------------------------------------

    /// Acquire the inference slot before calling Ollama.
    /// Interactive runs target the current background holder for cooperative yield.
    /// Returns true on success, false on timeout (resource contention).
    /// Caller maps false to plain-language recovery message.
    pub async fn acquire_inference_slot(&self, run_id: &str, priority: PathPriority) -> bool {
        // Arrival block. Lock dropped before .await — invariant preserved.
        {
            let mut state = self.state.lock().unwrap();
            if priority == PathPriority::Interactive {
                state.interactive_wait_count =
                    state.interactive_wait_count.saturating_add(1);
                if let Some(holder) = state.inference_holder.clone() {
                    if state.active_runs.get(&holder) == Some(&PathPriority::Background) {
                        state.preempt_requested_for = Some(holder);
                    }
                }
            }
        }

        let acquired =
            match timeout(self.inference_slot_timeout, self.inference_slot.acquire()).await {
                Ok(Ok(permit)) => {
                    permit.forget();
                    true
                }
                _ => false,
            };

        // Post-acquire update. Lock dropped before returning — invariant preserved.
        {
            let mut state = self.state.lock().unwrap();
            if acquired {
                state.inference_holder = Some(run_id.to_string());
                // Only clear preemption signal when an interactive run acquires.
                // Background runs acquiring the slot leave the signal intact so
                // that a waiting interactive run can still trigger yield on the
                // next background reacquisition.
                if priority == PathPriority::Interactive {
                    state.preempt_requested_for = None;
                    state.interactive_wait_count =
                        state.interactive_wait_count.saturating_sub(1);
                }
            } else {
                // Timeout — undo arrival-block increment to keep count accurate.
                if priority == PathPriority::Interactive {
                    state.interactive_wait_count =
                        state.interactive_wait_count.saturating_sub(1);
                }
            }
        }

        acquired
    }

    /// Release the inference slot after an Ollama call completes.
    /// Must be called in a finally/drop guard to guarantee release.
    /// Non-holders are silently ignored — semaphore is not released.
    /// Clears stale preemption target if this holder was being targeted,
    /// preventing spurious yield signals on future background acquisitions.
    pub fn release_inference_slot(&self, run_id: &str) {
        let should_release = {
            let mut state = self.state.lock().unwrap();
            if state.inference_holder.as_deref() == Some(run_id) {
                state.inference_holder = None;
                if state.preempt_requested_for.as_deref() == Some(run_id) {
                    state.preempt_requested_for = None;
                }
                true
            } else {
                false
            }
        };
        if should_release {
            self.inference_slot.add_permits(1);
        }
    }

    // -- Cooperative yield check (called at step boundaries) -----------------

    /// Returns true if this background run should yield before reacquiring
    /// the inference slot for its next step.
    /// Called by background runs at each step boundary, BEFORE acquire_inference_slot().
    /// Interactive runs should never call this.
    ///
    /// Synchronous. std::sync::Mutex lock — no .await. Lock held only for
    /// two map lookups; released before returning.
    pub fn should_yield(&self, run_id: &str) -> bool {
        let state = self.state.lock().unwrap();
        if state.active_runs.get(run_id) != Some(&PathPriority::Background) {
            return false;
        }
        state.preempt_requested_for.as_deref() == Some(run_id)
    }

    // -- Diagnostics ---------------------------------------------------------

    /// Current number of active focus runs.
    pub fn active_run_count(&self) -> usize {
        self.state.lock().unwrap().active_runs.len()
    }

    /// run_id currently holding the inference slot, or None.
    pub fn inference_slot_holder(&self) -> Option<String> {
        self.state.lock().unwrap().inference_holder.clone()
    }
}

impl Default for ConductorScheduler {
    fn default() -> Self {
        Self::new()
    }
}
