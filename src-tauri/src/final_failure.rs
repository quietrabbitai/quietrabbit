// src-tauri/src/final_failure.rs
//
// items.id=831 (decisions.id=925, FINAL_FAILURE_DESIGN_ITEM818_20261027.md):
// the message a user gets after the LAST retry of a hosted call also fails,
// and the `final-failure` push event that tells the chat surface which reply
// that message is, so it can offer a real Try again.
//
// WORDING lives here, once. It uses the words of the "waiting to retry" row
// (retry_wait.rs, decisions.id=923): the same three reasons, the same
// `Provider · Model` naming. The provider is named from its provider row, as
// data, and the model by its id, like the wait row (decisions.id=872, 875: no
// provider or model is named in code). The retry count is MAX_RETRIES.
//
// WHY AN EVENT, NOT A COLUMN: a reply row has nowhere to keep a reason or an
// action (is_error, reply_status, provider_id and model_id only), and a column
// is a schema change. `finalize_chat_reply` already holds both the reply row
// and the `FailureResult`, so it announces the failure on a closed event, like
// `retry-wait`. The sentence itself is the stored reply, so it survives a
// restart; the buttons do not (the front end keeps the event in memory).
//
// EVENT CONTRACT (hand-declared on the front end, frontend/src/chat/
// finalFailure.ts, like `retry-wait`; not exported to bindings.ts):
//   "final-failure"  { focus_run_id, message_id, reason, provider_id, model_id }
//     reason = "busy" | "slow" | "unavailable"
// NO USER CONTENT: ids and one closed vocabulary. Never the prompt, a reply,
// an error message or a key.

use serde::Serialize;

use crate::conductor::failure::{FailureAction, FailureResult, MAX_RETRIES};
use crate::retry_wait::{
    self, RetryWaitReason, RETRY_MODEL_KEY, RETRY_PROVIDER_KEY, RETRY_REASON_KEY,
};

pub const FINAL_FAILURE_EVENT: &str = "final-failure";

/// `FailureResult.metadata` key marking a result as the final failure of a
/// hosted call (value `true`). The reason, provider and model travel under the
/// `retry_wait` keys.
pub const FINAL_FAILURE_KEY: &str = "final_failure";

/// The failure mode of every hosted-provider call (executor.rs
/// HOSTED_FAILURE_MODE).
const HOSTED_FAILURE_MODE: &str = "F10";

/// Where the message is shown, which decides its first sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// API Chat: the reason sentence alone.
    Bare,
    /// A Focus run, QR Chat included: "Step {Step} in {Focus} is paused."
    /// comes first, as in the wait row.
    Focus { focus: &'a str, step: &'a str },
}

/// How the failed provider is named in the sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who<'a> {
    /// The provider row's display name and the model id.
    Named { provider: &'a str, model: &'a str },
    /// The provider row could not be read; the sentence still says what
    /// happened without naming anything.
    Unknown,
}

fn times(n: u32) -> String {
    if n == 1 {
        "once".to_owned()
    } else {
        format!("{n} times")
    }
}

/// The message for a final failure. The retry count is MAX_RETRIES: the first
/// attempt plus that many retries (design doc, "We retried 3 times is exact").
pub fn compose(reason: RetryWaitReason, who: Who<'_>, scope: Scope<'_>) -> String {
    let retried = format!("We retried {}.", times(MAX_RETRIES));
    let sentence = match who {
        Who::Named { provider, model } => {
            let name = format!("{provider} · {model}");
            match reason {
                RetryWaitReason::Busy => {
                    format!("{name} is too busy to answer right now. {retried}")
                }
                RetryWaitReason::Slow => {
                    format!("{name} is taking too long to answer. {retried}")
                }
                RetryWaitReason::Unavailable => {
                    format!("We can't reach {name} right now. {retried}")
                }
            }
        }
        Who::Unknown => match reason {
            RetryWaitReason::Busy => {
                format!("The provider is too busy to answer right now. {retried}")
            }
            RetryWaitReason::Slow => {
                format!("The provider is taking too long to answer. {retried}")
            }
            RetryWaitReason::Unavailable => {
                format!("We can't reach the provider right now. {retried}")
            }
        },
    };
    match scope {
        Scope::Bare => sentence,
        Scope::Focus { focus, step } if !step.is_empty() && !focus.is_empty() => {
            format!("Step {step} in {focus} is paused. {sentence}")
        }
        Scope::Focus { step, .. } if !step.is_empty() => {
            format!("Step {step} is paused. {sentence}")
        }
        Scope::Focus { .. } => sentence,
    }
}

/// Whether `result` is the exhausted result of a hosted-provider call: the
/// escalation `FailureHandler::handle` returns once `retry_count` reaches
/// MAX_RETRIES for a hosted error class.
pub fn is_hosted_exhaustion(result: &FailureResult, retry_count: u32) -> bool {
    retry_count >= MAX_RETRIES
        && result.action == FailureAction::AwaitUser
        && result.failure_mode.as_deref() == Some(HOSTED_FAILURE_MODE)
}

/// Turn an exhausted hosted result into the final-failure message: the marker
/// and ids go in the metadata, the sentence replaces `plain_language`.
pub fn apply(
    result: &mut FailureResult,
    reason: RetryWaitReason,
    provider_id: &str,
    model_id: &str,
    who: Who<'_>,
    scope: Scope<'_>,
) {
    retry_wait::attach_context(result, reason, provider_id, model_id);
    if let Some(meta) = result.metadata.as_mut() {
        meta.insert(FINAL_FAILURE_KEY.to_owned(), true.into());
    }
    result.plain_language = compose(reason, who, scope);
}

/// The whole event payload. Adding a field here is a privacy decision: the
/// exact-keys test in this file fails until it is made on purpose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FinalFailurePayload {
    pub focus_run_id: String,
    pub message_id: String,
    pub reason: RetryWaitReason,
    pub provider_id: String,
    pub model_id: String,
}

/// The event for a run's failure, or None when it is not a final failure.
pub fn payload_for(
    failure: &FailureResult,
    focus_run_id: &str,
    message_id: &str,
) -> Option<FinalFailurePayload> {
    let meta = failure.metadata.as_ref()?;
    if meta.get(FINAL_FAILURE_KEY)?.as_bool() != Some(true) {
        return None;
    }
    let text = |key: &str| meta.get(key).and_then(|v| v.as_str());
    Some(FinalFailurePayload {
        focus_run_id: focus_run_id.to_owned(),
        message_id: message_id.to_owned(),
        reason: RetryWaitReason::parse(text(RETRY_REASON_KEY)?)?,
        provider_id: text(RETRY_PROVIDER_KEY)?.to_owned(),
        model_id: text(RETRY_MODEL_KEY)?.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::failure::FailureSeverity;
    use std::collections::BTreeSet;

    fn named() -> Who<'static> {
        Who::Named {
            provider: "Provider A",
            model: "model-1",
        }
    }

    fn exhausted() -> FailureResult {
        FailureResult {
            action: FailureAction::AwaitUser,
            failure_mode: Some("F10".to_owned()),
            plain_language: "fallback".to_owned(),
            is_recoverable: true,
            severity: FailureSeverity::Stop,
            step_id: Some("s1".to_owned()),
            focus_id: Some("f1".to_owned()),
            metadata: None,
        }
    }

    #[test]
    fn the_three_reasons_have_the_designed_sentences() {
        let retried = format!("We retried {} times.", MAX_RETRIES);
        assert_eq!(
            compose(RetryWaitReason::Busy, named(), Scope::Bare),
            format!("Provider A · model-1 is too busy to answer right now. {retried}")
        );
        assert_eq!(
            compose(RetryWaitReason::Slow, named(), Scope::Bare),
            format!("Provider A · model-1 is taking too long to answer. {retried}")
        );
        assert_eq!(
            compose(RetryWaitReason::Unavailable, named(), Scope::Bare),
            format!("We can't reach Provider A · model-1 right now. {retried}")
        );
    }

    #[test]
    fn the_count_is_max_retries_not_a_literal() {
        assert_eq!(times(MAX_RETRIES), format!("{MAX_RETRIES} times"));
        assert_eq!(times(1), "once");
        assert_eq!(times(5), "5 times");
        let s = compose(RetryWaitReason::Busy, named(), Scope::Bare);
        assert!(s.ends_with(&format!("We retried {} times.", MAX_RETRIES)));
    }

    #[test]
    fn a_focus_run_puts_the_what_first() {
        let s = compose(
            RetryWaitReason::Slow,
            named(),
            Scope::Focus {
                focus: "Writing Assistant",
                step: "Draft outline",
            },
        );
        assert!(
            s.starts_with(
                "Step Draft outline in Writing Assistant is paused. Provider A · model-1 is taking"
            ),
            "{s}"
        );
        let no_focus = compose(
            RetryWaitReason::Slow,
            named(),
            Scope::Focus {
                focus: "",
                step: "Draft outline",
            },
        );
        assert!(
            no_focus.starts_with("Step Draft outline is paused. "),
            "{no_focus}"
        );
        let nothing = compose(
            RetryWaitReason::Slow,
            named(),
            Scope::Focus {
                focus: "",
                step: "",
            },
        );
        assert_eq!(
            nothing,
            compose(RetryWaitReason::Slow, named(), Scope::Bare)
        );
    }

    #[test]
    fn an_unreadable_provider_row_is_not_replaced_by_a_guess() {
        for reason in [
            RetryWaitReason::Busy,
            RetryWaitReason::Slow,
            RetryWaitReason::Unavailable,
        ] {
            let s = compose(reason, Who::Unknown, Scope::Bare);
            assert!(
                s.contains("the provider") || s.starts_with("The provider"),
                "{s}"
            );
            assert!(!s.contains('·'), "{s}");
        }
    }

    #[test]
    fn no_bracket_text_is_left_in_the_message() {
        for reason in [
            RetryWaitReason::Busy,
            RetryWaitReason::Slow,
            RetryWaitReason::Unavailable,
        ] {
            let s = compose(
                reason,
                named(),
                Scope::Focus {
                    focus: "F",
                    step: "S",
                },
            );
            assert!(!s.contains('[') && !s.contains(']'), "{s}");
        }
    }

    #[test]
    fn only_a_hosted_exhaustion_qualifies() {
        assert!(is_hosted_exhaustion(&exhausted(), MAX_RETRIES));
        assert!(!is_hosted_exhaustion(&exhausted(), MAX_RETRIES - 1));
        let mut local = exhausted();
        local.failure_mode = Some("F1".to_owned());
        assert!(!is_hosted_exhaustion(&local, MAX_RETRIES));
        let mut retry = exhausted();
        retry.action = FailureAction::Retry;
        assert!(!is_hosted_exhaustion(&retry, MAX_RETRIES));
        let mut offer = exhausted();
        offer.action = FailureAction::OfferTier2;
        assert!(!is_hosted_exhaustion(&offer, MAX_RETRIES));
    }

    #[test]
    fn apply_marks_the_result_and_replaces_the_text() {
        let mut r = exhausted();
        apply(
            &mut r,
            RetryWaitReason::Busy,
            "prov-1",
            "model-1",
            named(),
            Scope::Bare,
        );
        assert!(r.plain_language.contains("too busy"));
        let m = r.metadata.as_ref().unwrap();
        assert_eq!(m[FINAL_FAILURE_KEY], true);
        assert_eq!(m[RETRY_REASON_KEY], "busy");
        assert_eq!(m[RETRY_PROVIDER_KEY], "prov-1");
        assert_eq!(m[RETRY_MODEL_KEY], "model-1");
        let p = payload_for(&r, "run-1", "msg-1").expect("a payload");
        assert_eq!(p.focus_run_id, "run-1");
        assert_eq!(p.message_id, "msg-1");
        assert_eq!(p.reason, RetryWaitReason::Busy);
        assert_eq!(p.provider_id, "prov-1");
        assert_eq!(p.model_id, "model-1");
    }

    #[test]
    fn no_payload_without_the_marker_or_the_context() {
        assert!(payload_for(&exhausted(), "r", "m").is_none());
        // The wait row's context alone (a Retry result) is not a final failure.
        let mut wait_only = exhausted();
        retry_wait::attach_context(&mut wait_only, RetryWaitReason::Slow, "p", "m");
        assert!(payload_for(&wait_only, "r", "m").is_none());
        let mut r = exhausted();
        apply(
            &mut r,
            RetryWaitReason::Slow,
            "p",
            "m",
            Who::Unknown,
            Scope::Bare,
        );
        for key in [RETRY_REASON_KEY, RETRY_PROVIDER_KEY, RETRY_MODEL_KEY] {
            let mut partial = r.clone();
            partial.metadata.as_mut().unwrap().remove(key);
            assert!(payload_for(&partial, "r", "m").is_none(), "{key}");
        }
        let mut junk = r.clone();
        junk.metadata
            .as_mut()
            .unwrap()
            .insert(RETRY_REASON_KEY.to_owned(), "mystery".into());
        assert!(payload_for(&junk, "r", "m").is_none());
        let mut off = r.clone();
        off.metadata
            .as_mut()
            .unwrap()
            .insert(FINAL_FAILURE_KEY.to_owned(), false.into());
        assert!(payload_for(&off, "r", "m").is_none());
    }

    /// The payload is exactly these fields. A new field - above all one that
    /// could carry user content - fails this test until it is added here on
    /// purpose, with the privacy decision made.
    #[test]
    fn the_payload_has_exactly_the_allowed_fields_and_no_user_text() {
        let mut r = exhausted();
        r.plain_language = "SECRET-USER-TEXT-IN-AN-ERROR".to_owned();
        apply(
            &mut r,
            RetryWaitReason::Unavailable,
            "prov-1",
            "model-1",
            named(),
            Scope::Bare,
        );
        let p = payload_for(&r, "run-1", "msg-1").unwrap();
        let v = serde_json::to_value(&p).unwrap();
        let keys: BTreeSet<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        let allowed: BTreeSet<&str> = [
            "focus_run_id",
            "message_id",
            "reason",
            "provider_id",
            "model_id",
        ]
        .into_iter()
        .collect();
        assert_eq!(keys, allowed);
        assert_eq!(v["reason"], "unavailable");
        assert!(!serde_json::to_string(&p)
            .unwrap()
            .contains("SECRET-USER-TEXT"));
    }
}
