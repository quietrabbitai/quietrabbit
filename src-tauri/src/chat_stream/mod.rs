// src-tauri/src/chat_stream/mod.rs
//
// items.id=715 (decisions.id=874, 875, 883): the run-side handle for a
// streamed reply. Three things live here:
//   - `ChatStreamRegistry` / `ChatStream`: a CancellationToken per streaming
//     run, owner-checked, removed on EVERY ending (follows the model-install
//     pull registry, commands/model_install.rs);
//   - `TauriDeltaSink`: the `message-delta` push event;
//   - the event payload types.
//
// The seam: a run streams only when its owner sets `FocusRun.chat_stream`
// (items.id=716 does that for API Chat; nothing in 715 does). A run without
// it sends `stream: false` exactly as before, and a local (Ollama) step
// ignores it.
//
// EVENT CONTRACT (hand-declared on the front end like the other push events;
// not exported to bindings.ts):
//   "message-delta"  { focus_run_id, step_id, seq, event }
//     event = { type: "delta", text }          display-only text
//           | { type: "end", outcome, personal_flagged, audit_unavailable }
//   `message-content-ready` REMAINS the completion signal: it fires after the
//   step, its gates, the output and the transcript backfill, and the
//   refetched message is the authoritative text. A delta is never saved,
//   never logged, and never retracted (decisions.id=883).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use specta::Type;
use tokio_util::sync::CancellationToken;

use crate::providers::hosted_stream::{StreamEnd, StreamOutcome, StreamSink};

pub const MESSAGE_DELTA_EVENT: &str = "message-delta";

/// How many ended runs the registry remembers so that cancelling a run that
/// just finished is a no-op rather than an error.
const TOMBSTONES: usize = 64;

// ---------------------------------------------------------------------------
// Event payloads
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum StreamOutcomeWire {
    Complete,
    Failed,
    Cancelled,
}

impl std::fmt::Debug for StreamOutcomeWire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        })
    }
}

impl From<StreamOutcome> for StreamOutcomeWire {
    fn from(o: StreamOutcome) -> Self {
        match o {
            StreamOutcome::Complete => Self::Complete,
            StreamOutcome::Failed => Self::Failed,
            StreamOutcome::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Clone, Serialize, Type)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeltaEvent {
    Delta {
        text: String,
    },
    End {
        outcome: StreamOutcomeWire,
        personal_flagged: bool,
        audit_unavailable: bool,
    },
}

#[derive(Clone, Serialize, Type)]
pub struct MessageDeltaPayload {
    pub focus_run_id: String,
    pub step_id: String,
    pub seq: u32,
    pub event: DeltaEvent,
}

// The text is a model's reply: never printed (decisions.id=883).
impl std::fmt::Debug for MessageDeltaPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("MessageDeltaPayload");
        d.field("focus_run_id", &self.focus_run_id)
            .field("step_id", &self.step_id)
            .field("seq", &self.seq);
        match &self.event {
            DeltaEvent::Delta { text } => d.field("delta_len", &text.len()),
            DeltaEvent::End {
                outcome,
                personal_flagged,
                audit_unavailable,
            } => d
                .field("end", outcome)
                .field("personal_flagged", personal_flagged)
                .field("audit_unavailable", audit_unavailable),
        };
        d.finish()
    }
}

// ---------------------------------------------------------------------------
// Sink
// ---------------------------------------------------------------------------

/// Emits `message-delta` events for one step of one run.
pub struct TauriDeltaSink {
    handle: tauri::AppHandle<tauri::Wry>,
    focus_run_id: String,
    step_id: String,
    next_seq: AtomicU32,
}

impl TauriDeltaSink {
    pub fn new(
        handle: tauri::AppHandle<tauri::Wry>,
        focus_run_id: String,
        step_id: String,
    ) -> Self {
        Self {
            handle,
            focus_run_id,
            step_id,
            next_seq: AtomicU32::new(0),
        }
    }

    fn emit(&self, seq: u32, event: DeltaEvent) {
        use tauri::Emitter;
        let payload = MessageDeltaPayload {
            focus_run_id: self.focus_run_id.clone(),
            step_id: self.step_id.clone(),
            seq,
            event,
        };
        if let Err(e) = self.handle.emit(MESSAGE_DELTA_EVENT, &payload) {
            // The error text is the emitter's, not the reply's.
            log::warn!("chat_stream: emit message-delta failed: {e}");
        }
    }
}

impl StreamSink for TauriDeltaSink {
    fn delta(&self, seq: u32, text: &str) {
        self.next_seq.store(seq.saturating_add(1), Ordering::SeqCst);
        self.emit(
            seq,
            DeltaEvent::Delta {
                text: text.to_owned(),
            },
        );
    }

    fn end(&self, end: &StreamEnd) {
        let seq = self.next_seq.load(Ordering::SeqCst);
        self.emit(
            seq,
            DeltaEvent::End {
                outcome: end.outcome.into(),
                personal_flagged: end.personal_flagged,
                audit_unavailable: end.audit_unavailable,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

struct Entry {
    user_id: String,
    generation: u64,
    token: CancellationToken,
}

#[derive(Default)]
struct Inner {
    live: HashMap<String, Entry>,
    /// (user_id, run_id) of recently ended streams, oldest first.
    ended: VecDeque<(String, String)>,
    next_generation: u64,
}

/// Run id -> cancellation token for the streams in flight. Cloneable; managed
/// as Tauri state (main.rs).
#[derive(Clone, Default)]
pub struct ChatStreamRegistry {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelError {
    /// No such stream for this user: unknown, or another user's. The two are
    /// deliberately indistinguishable.
    NotFound,
}

/// Held by the run for as long as it may stream. The registry entry goes away
/// when the last clone drops - on completion, cancellation, failure, or when
/// the task is dropped.
#[derive(Clone)]
pub struct ChatStream(Arc<StreamHandle>);

struct StreamHandle {
    token: CancellationToken,
    registry: ChatStreamRegistry,
    user_id: String,
    run_id: String,
    generation: u64,
}

impl ChatStream {
    pub fn token(&self) -> &CancellationToken {
        &self.0.token
    }
}

impl std::fmt::Debug for ChatStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatStream")
            .field("run_id", &self.0.run_id)
            .field("cancelled", &self.0.token.is_cancelled())
            .finish()
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        let mut inner = self.registry.lock();
        // Remove only our own entry: a newer registration of the same run id
        // must survive an older handle's drop.
        let ours = inner
            .live
            .get(&self.run_id)
            .is_some_and(|e| e.generation == self.generation);
        if ours {
            inner.live.remove(&self.run_id);
        }
        inner
            .ended
            .push_back((self.user_id.clone(), self.run_id.clone()));
        while inner.ended.len() > TOMBSTONES {
            inner.ended.pop_front();
        }
    }
}

impl ChatStreamRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock still holds consistent data (every critical
        // section is a few map operations).
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Register a streaming run for `user_id` and return the handle the run
    /// carries.
    pub fn register(&self, user_id: &str, run_id: &str) -> ChatStream {
        let token = CancellationToken::new();
        let mut inner = self.lock();
        inner.next_generation += 1;
        let generation = inner.next_generation;
        inner.live.insert(
            run_id.to_owned(),
            Entry {
                user_id: user_id.to_owned(),
                generation,
                token: token.clone(),
            },
        );
        drop(inner);
        ChatStream(Arc::new(StreamHandle {
            token,
            registry: self.clone(),
            user_id: user_id.to_owned(),
            run_id: run_id.to_owned(),
            generation,
        }))
    }

    /// Cancel `run_id` on behalf of `user_id`. Idempotent; a no-op for a
    /// stream that has just ended; an error for an unknown run and for
    /// another user's run (which trips nothing).
    pub fn cancel(&self, user_id: &str, run_id: &str) -> Result<(), CancelError> {
        let inner = self.lock();
        if let Some(entry) = inner.live.get(run_id) {
            if entry.user_id == user_id {
                entry.token.cancel();
                return Ok(());
            }
            return Err(CancelError::NotFound);
        }
        if inner.ended.iter().any(|(u, r)| u == user_id && r == run_id) {
            return Ok(());
        }
        Err(CancelError::NotFound)
    }

    /// Test and diagnostics: how many streams are registered.
    pub fn live_count(&self) -> usize {
        self.lock().live.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_trips_the_owners_token_and_is_idempotent() {
        let reg = ChatStreamRegistry::default();
        let s = reg.register("u1", "run-1");
        assert!(!s.token().is_cancelled());
        assert_eq!(reg.cancel("u1", "run-1"), Ok(()));
        assert!(s.token().is_cancelled());
        assert_eq!(reg.cancel("u1", "run-1"), Ok(()));
    }

    #[test]
    fn another_users_run_is_an_error_and_trips_nothing() {
        let reg = ChatStreamRegistry::default();
        let s = reg.register("u1", "run-1");
        assert_eq!(reg.cancel("u2", "run-1"), Err(CancelError::NotFound));
        assert!(!s.token().is_cancelled());
        // Indistinguishable from an unknown run.
        assert_eq!(reg.cancel("u2", "nope"), Err(CancelError::NotFound));
        assert_eq!(reg.cancel("u1", "nope"), Err(CancelError::NotFound));
    }

    #[test]
    fn the_entry_is_removed_on_every_ending() {
        let reg = ChatStreamRegistry::default();
        // Completion: the run finishes and drops its handle.
        let s = reg.register("u1", "done");
        assert_eq!(reg.live_count(), 1);
        drop(s);
        assert_eq!(reg.live_count(), 0);
        // Cancellation then drop.
        let s = reg.register("u1", "cancelled");
        reg.cancel("u1", "cancelled").unwrap();
        drop(s);
        assert_eq!(reg.live_count(), 0);
        // A task that is dropped mid-flight (clones in a spawned task).
        let s = reg.register("u1", "aborted");
        let s2 = s.clone();
        let task = std::thread::spawn(move || {
            let _held = s2;
        });
        task.join().unwrap();
        assert_eq!(reg.live_count(), 1, "one clone is still held");
        drop(s);
        assert_eq!(reg.live_count(), 0);
    }

    #[test]
    fn cancel_after_the_stream_ended_is_a_no_op_for_its_owner_only() {
        let reg = ChatStreamRegistry::default();
        drop(reg.register("u1", "run-1"));
        assert_eq!(reg.cancel("u1", "run-1"), Ok(()));
        assert_eq!(reg.cancel("u2", "run-1"), Err(CancelError::NotFound));
    }

    #[test]
    fn tombstones_are_bounded() {
        let reg = ChatStreamRegistry::default();
        for i in 0..(TOMBSTONES + 10) {
            drop(reg.register("u1", &format!("run-{i}")));
        }
        assert_eq!(
            reg.cancel("u1", "run-0"),
            Err(CancelError::NotFound),
            "the oldest ended run has been forgotten"
        );
        assert_eq!(reg.cancel("u1", &format!("run-{}", TOMBSTONES + 9)), Ok(()));
        assert!(reg.lock().ended.len() <= TOMBSTONES);
    }

    #[test]
    fn an_older_handle_dropping_does_not_remove_a_newer_registration() {
        let reg = ChatStreamRegistry::default();
        let old = reg.register("u1", "run-1");
        let new = reg.register("u1", "run-1");
        drop(old);
        assert_eq!(reg.live_count(), 1);
        reg.cancel("u1", "run-1").unwrap();
        assert!(new.token().is_cancelled());
    }

    #[test]
    fn payload_debug_never_prints_text() {
        let p = MessageDeltaPayload {
            focus_run_id: "r".to_owned(),
            step_id: "s".to_owned(),
            seq: 3,
            event: DeltaEvent::Delta {
                text: "secret reply".to_owned(),
            },
        };
        let shown = format!("{p:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("delta_len"));
    }

    #[test]
    fn payload_wire_shape() {
        let p = MessageDeltaPayload {
            focus_run_id: "r".to_owned(),
            step_id: "s".to_owned(),
            seq: 0,
            event: DeltaEvent::Delta {
                text: "hi".to_owned(),
            },
        };
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            serde_json::json!({"focus_run_id":"r","step_id":"s","seq":0,
                "event":{"type":"delta","text":"hi"}})
        );
        let e = MessageDeltaPayload {
            event: DeltaEvent::End {
                outcome: StreamOutcomeWire::Cancelled,
                personal_flagged: true,
                audit_unavailable: false,
            },
            ..p
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap()["event"],
            serde_json::json!({"type":"end","outcome":"cancelled",
                "personal_flagged":true,"audit_unavailable":false})
        );
    }
}
