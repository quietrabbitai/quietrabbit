// src-tauri/src/api_chat/review.rs
//
// The outbound Privacy Guardian review of one API Chat message (items.id=716).
//
// WHAT IS REVIEWED. Only the NEW user turn, once, when it is sent. Every
// earlier user turn was reviewed when it was sent, and what was actually sent
// is kept (messages.outbound_content) and replayed as history, so a span the
// user already decided on is never asked about again. This is deliberate:
// gate3 reuses a decision only for a span whose fact identity it can resolve
// (partition_by_prior_decision), and a name or similar span often has none,
// so a review of the whole history prefix on every turn would re-ask the same
// question and could not be guaranteed otherwise.
//
// WHO APPLIES THE ANSWERS. The server, because the server is what sends. The
// copy review applies them in the front end; for a send that would let the
// text that leaves differ from what the user decided. Gate3 reports the spans
// it asked about (`Gate3Result::review_spans`) and the ones it settled from a
// standing preference (`auto_resolved`). The first are held here, keyed by the
// review key, together with a hash of the exact text reviewed. When the user
// has answered (the decisions are recorded under the key) and the send comes
// back with the key, the held spans and the recorded answers are applied to
// the text, server side. The front end never supplies the text that is sent.
//
// A span with no recorded answer is removed. A held review is single use, is
// bound to its user, Persona and conversation, expires, and does not match if
// the text changed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::api_chat::redact::{apply, Decision, SpanDecision};
use crate::conductor::privacy::types::{AutoResolvedSpan, ConsentSpanItem, Gate3Result};
use crate::persistence::output_store::RecordedSpanDecision;

/// How long a held review lives. A user who walks away from the modal starts
/// a new review when they come back.
pub const HELD_REVIEW_TTL: Duration = Duration::from_secs(30 * 60);

/// Most reviews held at once; the oldest is dropped past this.
const MAX_HELD: usize = 64;

/// What gate3 said, reduced to the three things a send can do with it.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to ask. `auto` are spans settled from a standing preference.
    Clean { auto: Vec<SpanDecision> },
    /// The user must answer. `spans` are the items the consent request asked
    /// about, `auto` the ones already settled.
    Pending {
        spans: Vec<SpanRef>,
        auto: Vec<SpanDecision>,
    },
    /// Blocked, timed out, or an outcome that is none of the above. Fail closed.
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanRef {
    pub span_id: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

fn auto_to_decisions(auto: &[AutoResolvedSpan]) -> Vec<SpanDecision> {
    auto.iter()
        .map(|a| SpanDecision {
            start_byte: a.start_byte as usize,
            end_byte: a.end_byte as usize,
            decision: Decision::parse(&a.decision),
            suggestion_text: a.suggestion_text.clone(),
            user_modified_text: a.user_modified_text.clone(),
        })
        .collect()
}

fn span_refs(spans: &[ConsentSpanItem]) -> Vec<SpanRef> {
    spans
        .iter()
        .map(|s| SpanRef {
            span_id: s.span_id.clone(),
            start_byte: s.start_byte,
            end_byte: s.end_byte,
        })
        .collect()
}

pub fn verdict_of(result: &Gate3Result) -> Verdict {
    if result.blocked || result.timeout {
        return Verdict::Blocked;
    }
    if result.pending_consent {
        return Verdict::Pending {
            spans: span_refs(&result.review_spans),
            auto: auto_to_decisions(&result.auto_resolved),
        };
    }
    if result.approved {
        return Verdict::Clean {
            auto: auto_to_decisions(&result.auto_resolved),
        };
    }
    Verdict::Blocked
}

/// The text that leaves for a clean review: the original with any
/// standing-preference decisions applied. None when they cannot be applied.
pub fn outbound_for_clean(content: &str, auto: Vec<SpanDecision>) -> Option<String> {
    apply(content, auto)
}

/// A review waiting for the user's answers.
#[derive(Debug, Clone)]
pub struct HeldReview {
    pub user_id: String,
    pub persona_id: String,
    pub context_key: String,
    content_sha: [u8; 32],
    pub spans: Vec<SpanRef>,
    pub auto: Vec<SpanDecision>,
    held_at: Instant,
}

fn sha(content: &str) -> [u8; 32] {
    Sha256::digest(content.as_bytes()).into()
}

impl HeldReview {
    pub fn new(
        user_id: &str,
        persona_id: &str,
        context_key: &str,
        content: &str,
        spans: Vec<SpanRef>,
        auto: Vec<SpanDecision>,
    ) -> Self {
        Self {
            user_id: user_id.to_owned(),
            persona_id: persona_id.to_owned(),
            context_key: context_key.to_owned(),
            content_sha: sha(content),
            spans,
            auto,
            held_at: Instant::now(),
        }
    }

    /// The text to send once the user has answered. Every held span takes its
    /// recorded answer; one with no answer is removed. None when the spans
    /// cannot be applied safely, and the caller must not send.
    pub fn outbound(&self, content: &str, answers: &[RecordedSpanDecision]) -> Option<String> {
        let mut all: Vec<SpanDecision> = self.auto.clone();
        for span in &self.spans {
            let d = match answers.iter().find(|a| a.span_id == span.span_id) {
                Some(a) => SpanDecision {
                    start_byte: span.start_byte,
                    end_byte: span.end_byte,
                    decision: Decision::parse(&a.decision),
                    suggestion_text: a.suggestion_text.clone(),
                    user_modified_text: a.user_modified_text.clone(),
                },
                None => SpanDecision::unresolved(span.start_byte, span.end_byte),
            };
            all.push(d);
        }
        apply(content, all)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TakeError {
    /// No such review for this user, Persona and conversation, or it expired.
    NotFound,
    /// The text is not the text that was reviewed. The review is spent.
    TextChanged,
}

/// Held reviews, by review key. Managed as Tauri state (main.rs). Holds
/// offsets and a hash, never message text.
#[derive(Clone, Default)]
pub struct ReviewRegistry {
    inner: Arc<Mutex<HashMap<String, HeldReview>>>,
}

impl ReviewRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, HeldReview>> {
        // A poisoned lock still holds consistent data (every critical section
        // is a few map operations).
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn hold(&self, review_key: &str, held: HeldReview) {
        let mut map = self.lock();
        map.retain(|_, h| h.held_at.elapsed() < HELD_REVIEW_TTL);
        while map.len() >= MAX_HELD {
            let oldest = map
                .iter()
                .min_by_key(|(_, h)| h.held_at)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    map.remove(&k);
                }
                None => break,
            }
        }
        map.insert(review_key.to_owned(), held);
    }

    /// Take (and spend) a held review. Owner, Persona, conversation and age
    /// are checked first; another user's review is `NotFound` and stays put.
    pub fn take(
        &self,
        review_key: &str,
        user_id: &str,
        persona_id: &str,
        context_key: &str,
        content: &str,
    ) -> Result<HeldReview, TakeError> {
        let mut map = self.lock();
        let ours = map.get(review_key).is_some_and(|h| {
            h.user_id == user_id
                && h.persona_id == persona_id
                && h.context_key == context_key
                && h.held_at.elapsed() < HELD_REVIEW_TTL
        });
        if !ours {
            return Err(TakeError::NotFound);
        }
        let held = map.remove(review_key).ok_or(TakeError::NotFound)?;
        if held.content_sha != sha(content) {
            return Err(TakeError::TextChanged);
        }
        Ok(held)
    }

    /// Test and diagnostics: how many reviews are held.
    pub fn held_count(&self) -> usize {
        self.lock().len()
    }

    #[cfg(test)]
    fn age(&self, review_key: &str, by: Duration) {
        if let Some(h) = self.lock().get_mut(review_key) {
            h.held_at -= by;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(span_id: &str, decision: &str) -> RecordedSpanDecision {
        RecordedSpanDecision {
            span_id: span_id.to_owned(),
            decision: decision.to_owned(),
            suggestion_text: None,
            user_modified_text: None,
        }
    }

    fn held(content: &str) -> HeldReview {
        HeldReview::new(
            "u1",
            "p1",
            "chat-1",
            content,
            vec![
                SpanRef {
                    span_id: "s1".into(),
                    start_byte: 5,
                    end_byte: 8,
                },
                SpanRef {
                    span_id: "s2".into(),
                    start_byte: 12,
                    end_byte: 20,
                },
            ],
            vec![],
        )
    }

    const TEXT: &str = "call Ann at 555-0100 now";

    #[test]
    fn the_recorded_answers_are_what_leaves() {
        let h = held(TEXT);
        let out = h.outbound(
            TEXT,
            &[
                answer("s1", "keep_private"),
                answer("s2", "release_original"),
            ],
        );
        assert_eq!(out.as_deref(), Some("call [removed] at 555-0100 now"));
    }

    #[test]
    fn a_span_the_user_did_not_answer_is_removed_never_sent() {
        let h = held(TEXT);
        let out = h.outbound(TEXT, &[answer("s1", "release_original")]);
        assert_eq!(out.as_deref(), Some("call Ann at [removed] now"));
        let none = h.outbound(TEXT, &[]);
        assert_eq!(none.as_deref(), Some("call [removed] at [removed] now"));
    }

    #[test]
    fn standing_preference_spans_are_applied_with_the_interactive_ones() {
        let mut h = held(TEXT);
        h.spans.truncate(1); // only Ann is asked about
        h.auto = vec![SpanDecision {
            start_byte: 12,
            end_byte: 20,
            decision: Decision::KeepPrivate,
            suggestion_text: None,
            user_modified_text: None,
        }];
        let out = h.outbound(TEXT, &[answer("s1", "release_original")]);
        assert_eq!(out.as_deref(), Some("call Ann at [removed] now"));
    }

    #[test]
    fn spans_that_cannot_be_applied_mean_nothing_is_sent() {
        let mut h = held(TEXT);
        h.spans[1].end_byte = 999;
        assert_eq!(h.outbound(TEXT, &[]), None);
    }

    #[test]
    fn a_held_review_is_single_use_and_checks_everything() {
        let reg = ReviewRegistry::default();
        reg.hold("k1", held(TEXT));
        assert_eq!(reg.held_count(), 1);

        // Another user, Persona or conversation cannot take it, and it stays.
        assert_eq!(
            reg.take("k1", "u2", "p1", "chat-1", TEXT).unwrap_err(),
            TakeError::NotFound
        );
        assert_eq!(
            reg.take("k1", "u1", "p2", "chat-1", TEXT).unwrap_err(),
            TakeError::NotFound
        );
        assert_eq!(
            reg.take("k1", "u1", "p1", "chat-2", TEXT).unwrap_err(),
            TakeError::NotFound
        );
        assert_eq!(reg.held_count(), 1);

        assert!(reg.take("k1", "u1", "p1", "chat-1", TEXT).is_ok());
        assert_eq!(
            reg.take("k1", "u1", "p1", "chat-1", TEXT).unwrap_err(),
            TakeError::NotFound,
            "single use"
        );
        assert_eq!(reg.held_count(), 0);
    }

    #[test]
    fn edited_text_does_not_match_and_spends_the_review() {
        let reg = ReviewRegistry::default();
        reg.hold("k1", held(TEXT));
        assert_eq!(
            reg.take("k1", "u1", "p1", "chat-1", "call Bob at 555-0100 now")
                .unwrap_err(),
            TakeError::TextChanged
        );
        assert_eq!(reg.held_count(), 0, "a changed text needs a fresh review");
    }

    #[test]
    fn an_expired_review_is_gone() {
        let reg = ReviewRegistry::default();
        reg.hold("k1", held(TEXT));
        reg.age("k1", HELD_REVIEW_TTL + Duration::from_secs(1));
        assert_eq!(
            reg.take("k1", "u1", "p1", "chat-1", TEXT).unwrap_err(),
            TakeError::NotFound
        );
    }

    #[test]
    fn the_registry_is_bounded() {
        let reg = ReviewRegistry::default();
        for i in 0..(MAX_HELD + 10) {
            reg.hold(&format!("k{i}"), held(TEXT));
        }
        assert!(reg.held_count() <= MAX_HELD);
    }

    #[test]
    fn verdicts_fail_closed() {
        let blocked = Gate3Result {
            blocked: true,
            ..Default::default()
        };
        assert_eq!(verdict_of(&blocked), Verdict::Blocked);
        let timeout = Gate3Result {
            timeout: true,
            ..Default::default()
        };
        assert_eq!(verdict_of(&timeout), Verdict::Blocked);
        // Neither approved, pending nor blocked: not a green light.
        assert_eq!(verdict_of(&Gate3Result::default()), Verdict::Blocked);
        let clean = Gate3Result {
            approved: true,
            ..Default::default()
        };
        assert_eq!(verdict_of(&clean), Verdict::Clean { auto: vec![] });
        let pending = Gate3Result {
            pending_consent: true,
            auto_resolved: vec![AutoResolvedSpan {
                start_byte: 1,
                end_byte: 4,
                decision: "weird".into(),
                suggestion_text: None,
                user_modified_text: None,
            }],
            ..Default::default()
        };
        match verdict_of(&pending) {
            Verdict::Pending { spans, auto } => {
                assert!(spans.is_empty());
                assert_eq!(auto.len(), 1);
                assert_eq!(auto[0].decision, Decision::KeepPrivate);
            }
            other => panic!("expected pending, got {other:?}"),
        }
    }

    #[test]
    fn a_clean_review_applies_standing_preferences() {
        let out = outbound_for_clean(
            "ask Ann",
            vec![SpanDecision {
                start_byte: 4,
                end_byte: 7,
                decision: Decision::KeepPrivate,
                suggestion_text: None,
                user_modified_text: None,
            }],
        );
        assert_eq!(out.as_deref(), Some("ask [removed]"));
        assert_eq!(
            outbound_for_clean("plain", vec![]).as_deref(),
            Some("plain")
        );
    }
}
