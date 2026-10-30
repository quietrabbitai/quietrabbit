// src-tauri/src/retry_wait.rs
//
// items.id=817 (decisions.id=923): the `retry-wait` push event. Before a hosted
// call is retried the executor waits (items.id=727: the larger of the
// provider's Retry-After and a jittered backoff). This event tells the run's
// own surface that it is waiting, why, for how long and which retry it is, and
// that the wait is over, so the surface can show a "waiting to retry" row with
// a Cancel.
//
// WHY A DEDICATED EVENT, NOT FIELDS ON `task-progress`: TaskProgressPayload is
// a byte-progress shape (task_id, kind, current, total) with a free-text
// `message`. Seconds, reason, provider, model and attempt do not fit it, and
// its `message` is a standing path for text to reach the frontend. This payload
// is a closed struct, so a test can pin its exact key set.
//
// EVENT CONTRACT (hand-declared on the front end, frontend/src/chat/
// retryWait.ts, like `message-delta` and `run-status-update`; not exported to
// bindings.ts):
//   "retry-wait"  { focus_run_id, focus_name, step_display_name, provider_id,
//                   model_id, reason, seconds, attempt, max_attempts, state }
//     reason = "busy" | "slow" | "unavailable"
//     state  = "waiting" | "clear"
//   `waiting` is emitted BEFORE the wait sleeps and carries the TOTAL wait
//   (whole seconds, rounded up). `clear` is emitted when the wait ends for any
//   reason: the retry starts, the run is stopped or cancelled, or the wait
//   future is dropped. A dropped emit is swallowed by design, so the front end
//   also clears a row shortly after its own deadline.
//
// NO USER CONTENT: ids, a Focus name and a step name (authored definitions),
// numbers and two closed vocabularies. Never the prompt, a reply, an error
// message or a key. No provider or model is named in code (decisions.id=872,
// 875): the ids are data from the failure metadata and the front end resolves
// the display names from the provider row.

use std::time::Duration;

use serde::Serialize;
use specta::Type;
use tauri::Emitter;

use crate::conductor::failure::{ConductorError, FailureResult, MAX_RETRIES};

pub const RETRY_WAIT_EVENT: &str = "retry-wait";

/// `FailureResult.metadata` keys that carry the wait's context from the place
/// the provider error is classified (`execute_once`) to the retry loop, which
/// has neither the error nor the selected model. Ids and a closed vocabulary
/// only.
pub const RETRY_REASON_KEY: &str = "retry_reason";
pub const RETRY_PROVIDER_KEY: &str = "retry_provider_id";
pub const RETRY_MODEL_KEY: &str = "retry_model_id";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RetryWaitReason {
    /// The provider rate-limited the call.
    Busy,
    /// The call timed out.
    Slow,
    /// The provider could not be reached or answered with a server error.
    Unavailable,
}

impl RetryWaitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Slow => "slow",
            Self::Unavailable => "unavailable",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "busy" => Some(Self::Busy),
            "slow" => Some(Self::Slow),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }

    /// The reason a provider error is waited out for, or None for an error
    /// that is not a hosted-provider retry.
    pub fn of_error(e: &ConductorError) -> Option<Self> {
        match e {
            ConductorError::ProviderRateLimit { .. } => Some(Self::Busy),
            ConductorError::ProviderTimeout { .. } => Some(Self::Slow),
            ConductorError::ProviderUnavailable { .. } => Some(Self::Unavailable),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RetryWaitState {
    Waiting,
    Clear,
}

/// The whole payload. Adding a field here is a privacy decision: the exact-keys
/// test in this file fails until it is made on purpose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct RetryWaitPayload {
    pub focus_run_id: String,
    pub focus_name: String,
    pub step_display_name: String,
    pub provider_id: String,
    pub model_id: String,
    pub reason: RetryWaitReason,
    /// The total wait, whole seconds rounded up.
    pub seconds: u64,
    /// 1-based number of the retry this wait comes before.
    pub attempt: u32,
    pub max_attempts: u32,
    pub state: RetryWaitState,
}

/// Where the payloads go. The production implementation emits a Tauri event; a
/// test records them.
pub trait RetryWaitNotice: Send + Sync {
    fn notify(&self, payload: &RetryWaitPayload);
}

/// Emits `retry-wait`. Errors are logged and swallowed, like every other
/// `.emit()` call site (task_progress.rs idiom); a dropped event is a UX
/// inconvenience, never a correctness failure.
pub struct TauriRetryWaitNotice {
    handle: tauri::AppHandle<tauri::Wry>,
}

impl TauriRetryWaitNotice {
    pub fn new(handle: tauri::AppHandle<tauri::Wry>) -> Self {
        Self { handle }
    }
}

impl RetryWaitNotice for TauriRetryWaitNotice {
    fn notify(&self, payload: &RetryWaitPayload) {
        if let Err(e) = self.handle.emit(RETRY_WAIT_EVENT, payload) {
            // The error text is the emitter's, not the payload's.
            log::warn!("retry_wait: emit retry-wait failed: {e}");
        }
    }
}

/// Add the wait's context to a hosted Retry result (see the key constants).
pub fn attach_context(
    result: &mut FailureResult,
    reason: RetryWaitReason,
    provider_id: &str,
    model_id: &str,
) {
    let meta = result.metadata.get_or_insert_with(Default::default);
    meta.insert(RETRY_REASON_KEY.to_owned(), reason.as_str().into());
    meta.insert(RETRY_PROVIDER_KEY.to_owned(), provider_id.into());
    meta.insert(RETRY_MODEL_KEY.to_owned(), model_id.into());
}

/// Whole seconds, rounded up, so a 1.2 s wait reads "2" and never "1".
fn whole_seconds_up(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

/// What one run's waits are announced as: the sink plus the run's own names.
pub(crate) struct RetryWaitIdentity<'a> {
    pub sink: &'a dyn RetryWaitNotice,
    pub focus_run_id: &'a str,
    pub focus_name: &'a str,
    pub step_display_name: &'a str,
}

impl<'a> RetryWaitIdentity<'a> {
    /// Announce a wait of `total` before retry number `retry_count + 1` and
    /// return the guard whose drop announces its end. None (and nothing is
    /// sent) for a zero-length wait or when the result lacks the context
    /// `attach_context` adds: no row is better than a wrong one.
    pub fn begin(
        &self,
        result: &FailureResult,
        total: Duration,
        retry_count: u32,
    ) -> Option<ClearOnDrop<'a>> {
        if total.is_zero() {
            return None;
        }
        let meta = result.metadata.as_ref()?;
        let text = |key: &str| meta.get(key).and_then(|v| v.as_str());
        let payload = RetryWaitPayload {
            focus_run_id: self.focus_run_id.to_owned(),
            focus_name: self.focus_name.to_owned(),
            step_display_name: self.step_display_name.to_owned(),
            provider_id: text(RETRY_PROVIDER_KEY)?.to_owned(),
            model_id: text(RETRY_MODEL_KEY)?.to_owned(),
            reason: RetryWaitReason::parse(text(RETRY_REASON_KEY)?)?,
            seconds: whole_seconds_up(total),
            attempt: retry_count.saturating_add(1),
            max_attempts: MAX_RETRIES,
            state: RetryWaitState::Waiting,
        };
        self.sink.notify(&payload);
        Some(ClearOnDrop {
            sink: self.sink,
            payload,
        })
    }
}

/// Announces the end of a wait when dropped: the wait ran out (the retry is
/// about to start), the run was stopped or cancelled, or the future waiting was
/// dropped. One mechanism for every ending, so none can be forgotten.
pub(crate) struct ClearOnDrop<'a> {
    sink: &'a dyn RetryWaitNotice,
    payload: RetryWaitPayload,
}

impl Drop for ClearOnDrop<'_> {
    fn drop(&mut self) {
        self.payload.state = RetryWaitState::Clear;
        self.sink.notify(&self.payload);
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// Records every payload with the (paused-clock) instant it arrived.
    #[derive(Default)]
    pub struct RecordingNotice {
        pub events: Mutex<Vec<(tokio::time::Instant, RetryWaitPayload)>>,
    }

    impl RecordingNotice {
        pub fn payloads(&self) -> Vec<RetryWaitPayload> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|(_, p)| p.clone())
                .collect()
        }
    }

    impl RetryWaitNotice for RecordingNotice {
        fn notify(&self, payload: &RetryWaitPayload) {
            self.events
                .lock()
                .unwrap()
                .push((tokio::time::Instant::now(), payload.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::RecordingNotice;
    use super::*;
    use crate::conductor::failure::{FailureAction, FailureSeverity};
    use std::collections::{BTreeSet, HashMap};

    fn result_with_context() -> FailureResult {
        let mut r = FailureResult {
            action: FailureAction::Retry,
            failure_mode: Some("F10".to_owned()),
            plain_language: "SECRET-USER-TEXT-IN-AN-ERROR".to_owned(),
            is_recoverable: true,
            severity: FailureSeverity::Suggest,
            step_id: Some("s1".to_owned()),
            focus_id: Some("focus-1".to_owned()),
            metadata: Some(HashMap::new()),
        };
        attach_context(&mut r, RetryWaitReason::Busy, "prov-1", "model-1");
        r
    }

    fn identity(sink: &RecordingNotice) -> RetryWaitIdentity<'_> {
        RetryWaitIdentity {
            sink,
            focus_run_id: "run-1",
            focus_name: "Focus Name",
            step_display_name: "Step Name",
        }
    }

    /// The payload is exactly these fields. A new field - above all one that
    /// could carry user content - fails this test until it is added here on
    /// purpose, with the privacy decision made.
    #[test]
    fn the_payload_has_exactly_the_allowed_fields() {
        let sink = RecordingNotice::default();
        let guard = identity(&sink)
            .begin(&result_with_context(), Duration::from_millis(1200), 1)
            .expect("an event");
        drop(guard);
        for p in sink.payloads() {
            let v = serde_json::to_value(&p).unwrap();
            let keys: BTreeSet<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
            let allowed: BTreeSet<&str> = [
                "focus_run_id",
                "focus_name",
                "step_display_name",
                "provider_id",
                "model_id",
                "reason",
                "seconds",
                "attempt",
                "max_attempts",
                "state",
            ]
            .into_iter()
            .collect();
            assert_eq!(keys, allowed);
        }
    }

    #[test]
    fn the_payload_carries_no_error_or_user_text() {
        let sink = RecordingNotice::default();
        drop(identity(&sink).begin(&result_with_context(), Duration::from_secs(3), 0));
        for p in sink.payloads() {
            let json = serde_json::to_string(&p).unwrap();
            assert!(!json.contains("SECRET-USER-TEXT"), "{json}");
        }
    }

    #[test]
    fn values_serialize_as_the_documented_strings_and_numbers() {
        let sink = RecordingNotice::default();
        let guard = identity(&sink)
            .begin(&result_with_context(), Duration::from_millis(1200), 1)
            .unwrap();
        let v = serde_json::to_value(&sink.payloads()[0]).unwrap();
        assert_eq!(v["reason"], "busy");
        assert_eq!(v["state"], "waiting");
        assert_eq!(v["seconds"], 2, "1.2 s rounds up");
        assert_eq!(v["attempt"], 2, "retry_count 1 is retry number 2");
        assert_eq!(v["max_attempts"], MAX_RETRIES);
        assert_eq!(v["focus_run_id"], "run-1");
        assert_eq!(v["provider_id"], "prov-1");
        assert_eq!(v["model_id"], "model-1");
        drop(guard);
        let v = serde_json::to_value(&sink.payloads()[1]).unwrap();
        assert_eq!(v["state"], "clear");
        assert_eq!(v["attempt"], 2, "the clear names the same wait");
    }

    #[test]
    fn no_event_for_a_zero_wait_or_missing_context() {
        let sink = RecordingNotice::default();
        assert!(identity(&sink)
            .begin(&result_with_context(), Duration::ZERO, 0)
            .is_none());
        let mut bare = result_with_context();
        bare.metadata = None;
        assert!(identity(&sink)
            .begin(&bare, Duration::from_secs(3), 0)
            .is_none());
        for key in [RETRY_REASON_KEY, RETRY_PROVIDER_KEY, RETRY_MODEL_KEY] {
            let mut partial = result_with_context();
            partial.metadata.as_mut().unwrap().remove(key);
            assert!(identity(&sink)
                .begin(&partial, Duration::from_secs(3), 0)
                .is_none());
        }
        let mut junk = result_with_context();
        junk.metadata
            .as_mut()
            .unwrap()
            .insert(RETRY_REASON_KEY.to_owned(), "mystery".into());
        assert!(identity(&sink)
            .begin(&junk, Duration::from_secs(3), 0)
            .is_none());
        assert!(sink.payloads().is_empty());
    }

    #[test]
    fn each_provider_error_class_has_its_own_reason() {
        let msg = || "x".to_owned();
        assert_eq!(
            RetryWaitReason::of_error(&ConductorError::ProviderRateLimit {
                plain_language: msg(),
                retry_after_secs: Some(2),
            }),
            Some(RetryWaitReason::Busy)
        );
        assert_eq!(
            RetryWaitReason::of_error(&ConductorError::ProviderTimeout {
                plain_language: msg(),
            }),
            Some(RetryWaitReason::Slow)
        );
        assert_eq!(
            RetryWaitReason::of_error(&ConductorError::ProviderUnavailable {
                plain_language: msg(),
            }),
            Some(RetryWaitReason::Unavailable)
        );
        assert_eq!(
            RetryWaitReason::of_error(&ConductorError::MissingApiKey {
                plain_language: msg(),
            }),
            None
        );
    }

    #[test]
    fn attach_context_keeps_an_existing_retry_after() {
        let mut r = result_with_context();
        r.metadata.as_mut().unwrap().insert(
            crate::conductor::failure::RETRY_AFTER_METADATA_KEY.to_owned(),
            7.into(),
        );
        attach_context(&mut r, RetryWaitReason::Slow, "p", "m");
        let m = r.metadata.unwrap();
        assert_eq!(m[crate::conductor::failure::RETRY_AFTER_METADATA_KEY], 7);
        assert_eq!(m[RETRY_REASON_KEY], "slow");
    }
}
