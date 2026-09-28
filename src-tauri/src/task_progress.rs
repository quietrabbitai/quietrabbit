// src-tauri/src/task_progress.rs
//
// items.id=436: generic Tauri progress-event primitive. No repeated-
// progress-event precedent existed anywhere in this codebase before this
// (confirmed by search) -- every existing app.emit()/handle.emit() call
// (commands/execution.rs, commands/ingest.rs, commands/consent.rs,
// commands/messages.rs, conductor/privacy/gate3.rs, conductor/lifecycle.rs,
// cloud_chat_gpu_pane/pane_host.rs) fires once per state transition, not
// many times per second. Built here as a standalone module -- sibling to
// task_supervision.rs, same cross-cutting-infra placement -- so the first
// real caller (providers::ollama_install's streaming model pull) isn't the
// only thing that can ever use it.
//
// EMIT IDIOM: matches every existing emit() call site in this codebase --
// errors are logged via log::warn! and swallowed, never propagated. A
// dropped progress event is a UX inconvenience, not a correctness failure;
// the final "complete"/"failed"/"cancelled" event is what callers should
// treat as authoritative, not any single intermediate one.

use std::collections::HashMap;
use std::time::Instant;

use serde::Serialize;
use specta::Type;
use tauri::Emitter;

/// The one Tauri event name every progress-reporting task emits.
pub const TASK_PROGRESS_EVENT: &str = "task-progress";

/// Generic progress payload. `kind` identifies the operation type (e.g.
/// `"ollama_pull"`) so a single frontend listener can distinguish streams
/// for different features without this module needing to know about any
/// of them. `current`/`total` are omitted (`None`) when the underlying
/// operation hasn't reported byte-level progress yet (e.g. the "pulling
/// manifest" phase of an Ollama pull, before any digest has a known size).
#[derive(Debug, Clone, Serialize, Type)]
pub struct TaskProgressPayload {
    pub task_id: String,
    pub kind: String,
    /// Free-text phase label (e.g. "downloading", "verifying", "complete",
    /// "failed", "cancelled"). Open vocabulary, not an enum -- same
    /// precedent as `providers.provider_type`: a mechanical/display label,
    /// not something branched on by this module.
    pub phase: String,
    pub current: Option<u64>,
    pub total: Option<u64>,
    pub message: Option<String>,
}

/// Emit a `task-progress` event. Errors are logged and swallowed -- same
/// idiom as every other `.emit()` call site in this codebase.
pub fn emit_progress(handle: &tauri::AppHandle, payload: &TaskProgressPayload) {
    if let Err(e) = handle.emit(TASK_PROGRESS_EVENT, payload) {
        log::warn!(
            "task_progress: emit failed for task '{}': {e}",
            payload.task_id
        );
    }
}

const MIN_EMIT_INTERVAL_MS: u128 = 500;
const MIN_EMIT_PERCENT_DELTA: f64 = 2.0;

/// Throttles how often a caller emits progress for a given `task_id`,
/// so a fast byte-count callback (Ollama's `/api/pull` NDJSON stream can
/// emit many lines per second) doesn't turn into an equally fast flood of
/// Tauri events. Emits on whichever comes first: >=500ms since the last
/// emit for this task, or >=2% completion delta -- plus always allows the
/// very first call for a task_id (so a listener sees an immediate 0%/
/// starting event) since there is no prior state to compare against.
///
/// One instance is expected to live for the duration of a single task
/// (e.g. constructed fresh inside `ollama_install::run_install`), not
/// shared across tasks -- `task_id` keying exists so a single throttle can
/// still be reused if a future caller wants one shared instance, not
/// because this module assumes that usage today.
#[derive(Default)]
pub struct ProgressThrottle {
    last_emit: HashMap<String, (Instant, f64)>,
}

impl ProgressThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true if the caller should emit now for `task_id`, given
    /// `current`/`total` bytes (or any other progress unit — this module
    /// doesn't care which). `total == 0` always returns true (nothing to
    /// compute a percentage against — let the caller decide what to send).
    pub fn should_emit(&mut self, task_id: &str, current: u64, total: u64) -> bool {
        if total == 0 {
            return true;
        }
        let percent = (current as f64 / total as f64) * 100.0;
        let now = Instant::now();

        match self.last_emit.get(task_id) {
            None => {
                self.last_emit.insert(task_id.to_owned(), (now, percent));
                true
            }
            Some((last_time, last_percent)) => {
                let elapsed_ms = now.duration_since(*last_time).as_millis();
                let percent_delta = (percent - last_percent).abs();
                if elapsed_ms >= MIN_EMIT_INTERVAL_MS || percent_delta >= MIN_EMIT_PERCENT_DELTA {
                    self.last_emit.insert(task_id.to_owned(), (now, percent));
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Drops this task_id's throttle state. Call after emitting a final
    /// "complete"/"failed"/"cancelled" event so a task_id can't leak state
    /// for the lifetime of a long-running process (not a concern for the
    /// one-shot-per-install usage today, but cheap to do correctly).
    pub fn clear(&mut self, task_id: &str) {
        self.last_emit.remove(task_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_always_emits() {
        let mut t = ProgressThrottle::new();
        assert!(t.should_emit("task-1", 0, 100));
    }

    #[test]
    fn zero_total_always_emits() {
        let mut t = ProgressThrottle::new();
        assert!(t.should_emit("task-1", 0, 0));
        assert!(t.should_emit("task-1", 0, 0));
    }

    #[test]
    fn suppresses_small_delta_within_interval() {
        let mut t = ProgressThrottle::new();
        assert!(t.should_emit("task-1", 0, 100));
        // 1% delta, no time elapsed -- should be suppressed.
        assert!(!t.should_emit("task-1", 1, 100));
    }

    #[test]
    fn emits_on_large_percent_delta() {
        let mut t = ProgressThrottle::new();
        assert!(t.should_emit("task-1", 0, 100));
        // 5% delta clears the 2% threshold even with no time elapsed.
        assert!(t.should_emit("task-1", 5, 100));
    }

    #[test]
    fn independent_tasks_have_independent_state() {
        let mut t = ProgressThrottle::new();
        assert!(t.should_emit("task-1", 0, 100));
        assert!(!t.should_emit("task-1", 1, 100));
        // A different task_id has never been seen -- must emit regardless
        // of task-1's just-suppressed state.
        assert!(t.should_emit("task-2", 0, 100));
    }

    #[test]
    fn clear_resets_state_for_task() {
        let mut t = ProgressThrottle::new();
        assert!(t.should_emit("task-1", 0, 100));
        assert!(!t.should_emit("task-1", 1, 100));
        t.clear("task-1");
        assert!(t.should_emit("task-1", 1, 100));
    }
}
