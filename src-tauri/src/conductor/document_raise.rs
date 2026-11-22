// src-tauri/src/conductor/document_raise.rs
//
// items.id=903 unit D (decisions.id=942 Q14 Path B, decisions.id=944, decisions.id=963
// ruling 7): the ONE-DOCUMENT CEILING RAISE. The user may lift the tier ceiling for one
// document of one chat, above their focus_settings ceiling and never above the Focus's
// own, after a deliberate two-step confirm. Path C, staying local, is the default and
// needs nothing. This module holds the rules and the challenge tokens; the commands are
// commands/document_raise.rs, the stored raise is document_sessions.raised_ceiling
// (persistence::document_session_store), and the run reads it through
// document_session_store::effective_ceiling.
//
// THE RULES (check(), pure), each refusal with its own code, in this order:
//   protected_persona    the persona's type is protected OR the Focus's focus_profile is
//                        protected (decisions.id=963 ruling 5);
//   one_time_chat        the chat is latched: it stores nothing, a raise included;
//   no_document          no document session for the chat and Focus, or one of another
//                        document type than the run's;
//   above_focus_ceiling  the target is above the Focus's own ceiling (the loaded Focus:
//                        its row, its bundled file and the tier manifest, strictest);
//   not_a_raise          the target is at or below what the document already allows.
//
// THE FRICTION (ruling 7): request returns a challenge, a single-use 128-bit token held
// in memory only (RaiseChallenges, never stored, never logged), bound to the user,
// Persona, chat, Focus, document type and target, expiring after CHALLENGE_TTL, with a
// fixed disclosure summary and an earliest-confirm time MIN_WAIT after the request.
// Confirm refuses a token that is unknown, another user's, expired, or not acknowledged;
// one confirmed TOO EARLY is refused and stays valid (the wait is the friction, not a
// trap). A token is consumed by a confirm that passes those checks, whatever happens
// after. Wording is placeholder for Chat-BRAND (en.json document.raise.*).
//
// NOTHING HERE APPROVES A HOSTED OR METERED STEP (decisions.id=942 Q3): a raise only
// widens what a step MAY reach; the run plan's approval, the spend gate, the resolver,
// step_treatment and the outbound review are unchanged and still decide (a scan pins
// that none of them names the raise). While EXTERNAL_MODEL_BLOCKS_ALLOWED is false no
// Writing Assistant step can reach a hosted model, so a raise is inert until
// items.id=913.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::conductor::tokens::ExternalAccess;
use crate::persistence::document_session_store::{effective_ceiling, DocumentSession};

/// How long a challenge lives.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(10 * 60);
/// The least time between a request and its confirm.
pub const MIN_WAIT: Duration = Duration::from_secs(5);
/// Most challenges held at once; the oldest is dropped on overflow.
pub const MAX_CHALLENGES: usize = 16;
/// The version of the disclosure the screen shows (consent_audit_records.text_version).
pub const RAISE_TEXT_VERSION: i64 = 1;
/// The audit row's fixed summary: never user or model content.
pub const RAISE_AUDIT_SUMMARY: &str =
    "One-document ceiling raise: the user confirmed a higher tier for one document of one chat.";

/// Why a raise is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaiseRefusal {
    ProtectedPersona,
    OneTimeChat,
    NoDocument,
    AboveFocusCeiling,
    NotARaise,
}

impl RaiseRefusal {
    pub fn code(self) -> &'static str {
        match self {
            RaiseRefusal::ProtectedPersona => "protected_persona",
            RaiseRefusal::OneTimeChat => "one_time_chat",
            RaiseRefusal::NoDocument => "no_document",
            RaiseRefusal::AboveFocusCeiling => "above_focus_ceiling",
            RaiseRefusal::NotARaise => "not_a_raise",
        }
    }
}

/// What check() reads. Every value is read fresh by the caller.
pub struct RaiseCheck<'a> {
    pub persona_protected: bool,
    pub focus_profile_protected: bool,
    pub latched: bool,
    pub session: Option<&'a DocumentSession>,
    /// The run's selected document type (the chat's answer or the default).
    pub run_document_type: &'a str,
    /// The loaded Focus's own ceiling.
    pub focus_ceiling: ExternalAccess,
    /// The user's focus_settings.max_permitted_tier.
    pub user_max: ExternalAccess,
    pub target: ExternalAccess,
}

/// The rules, in the module header's order.
pub fn check(c: &RaiseCheck) -> Result<(), RaiseRefusal> {
    if c.persona_protected || c.focus_profile_protected {
        return Err(RaiseRefusal::ProtectedPersona);
    }
    if c.latched {
        return Err(RaiseRefusal::OneTimeChat);
    }
    let session = c
        .session
        .filter(|s| s.document_type == c.run_document_type)
        .ok_or(RaiseRefusal::NoDocument)?;
    if c.target > c.focus_ceiling {
        return Err(RaiseRefusal::AboveFocusCeiling);
    }
    let now =
        effective_ceiling(Some(session), c.run_document_type, c.user_max).min(c.focus_ceiling);
    if c.target <= now {
        return Err(RaiseRefusal::NotARaise);
    }
    Ok(())
}

/// What a challenge is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeScope {
    pub user_id: String,
    pub persona_id: String,
    pub context_key: String,
    pub focus_id: String,
    pub document_type: String,
    pub target: ExternalAccess,
}

#[derive(Clone)]
struct Challenge {
    scope: ChallengeScope,
    issued: Instant,
}

/// Why a confirm is refused before anything is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmRefusal {
    /// Unknown, already used, or another user's.
    InvalidToken,
    TooEarly,
    Expired,
}

impl ConfirmRefusal {
    pub fn code(self) -> &'static str {
        match self {
            ConfirmRefusal::InvalidToken => "invalid_token",
            ConfirmRefusal::TooEarly => "too_early",
            ConfirmRefusal::Expired => "expired",
        }
    }
}

/// The challenges in memory (Tauri managed state). Never serialized, logged or shown
/// in Debug; cleared for a user when the account's key leaves (main.rs).
#[derive(Clone, Default)]
pub struct RaiseChallenges {
    inner: Arc<Mutex<HashMap<String, Challenge>>>,
}

impl std::fmt::Debug for RaiseChallenges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RaiseChallenges(<redacted>)")
    }
}

impl RaiseChallenges {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Challenge>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new single-use token for `scope`, issued at `now`. None when the system has
    /// no randomness (the request then fails; no guessable token is ever issued).
    pub fn issue(&self, scope: ChallengeScope, now: Instant) -> Option<String> {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).ok()?;
        let token: String = b.iter().map(|x| format!("{x:02x}")).collect();
        let mut m = self.lock();
        m.retain(|_, c| now.saturating_duration_since(c.issued) < CHALLENGE_TTL);
        while m.len() >= MAX_CHALLENGES {
            let oldest = m
                .iter()
                .min_by_key(|(_, c)| c.issued)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    m.remove(&k);
                }
                None => break,
            }
        }
        m.insert(token.clone(), Challenge { scope, issued: now });
        Some(token)
    }

    /// Take the token for `user_id` at `now`. Too early: refused and kept. Expired: refused
    /// and removed. Unknown or another user's: refused (another user's is kept for its
    /// owner). Otherwise it is removed and its scope returned.
    pub fn take(
        &self,
        token: &str,
        user_id: &str,
        now: Instant,
    ) -> Result<ChallengeScope, ConfirmRefusal> {
        let mut m = self.lock();
        let Some(c) = m.get(token) else {
            return Err(ConfirmRefusal::InvalidToken);
        };
        if c.scope.user_id != user_id {
            return Err(ConfirmRefusal::InvalidToken);
        }
        let age = now.saturating_duration_since(c.issued);
        if age >= CHALLENGE_TTL {
            m.remove(token);
            return Err(ConfirmRefusal::Expired);
        }
        if age < MIN_WAIT {
            return Err(ConfirmRefusal::TooEarly);
        }
        Ok(m.remove(token).expect("checked above").scope)
    }

    /// Everything of one user (logout, lock).
    pub fn clear_user(&self, user_id: &str) {
        self.lock().retain(|_, c| c.scope.user_id != user_id);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::document_session_store::DocumentPrivacyFlag;

    fn session(doc: &str) -> DocumentSession {
        DocumentSession {
            context_key: "c".to_owned(),
            focus_id: "writing-assistant".to_owned(),
            document_type: doc.to_owned(),
            document_privacy_flag: None,
            raised_ceiling: None,
            consent_audit_id: None,
            version: 1,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn base<'a>(s: Option<&'a DocumentSession>) -> RaiseCheck<'a> {
        RaiseCheck {
            persona_protected: false,
            focus_profile_protected: false,
            latched: false,
            session: s,
            run_document_type: "cover_letter",
            focus_ceiling: ExternalAccess::AnonymousRequired,
            user_max: ExternalAccess::LocalOnly,
            target: ExternalAccess::AnonymousRequired,
        }
    }

    #[test]
    fn each_refusal_has_its_own_code_in_order() {
        let s = session("cover_letter");
        assert_eq!(check(&base(Some(&s))), Ok(()));
        let mut c = base(Some(&s));
        c.persona_protected = true;
        c.latched = true;
        assert_eq!(check(&c), Err(RaiseRefusal::ProtectedPersona));
        let mut c = base(Some(&s));
        c.focus_profile_protected = true;
        assert_eq!(check(&c), Err(RaiseRefusal::ProtectedPersona));
        let mut c = base(Some(&s));
        c.latched = true;
        assert_eq!(check(&c), Err(RaiseRefusal::OneTimeChat));
        assert_eq!(check(&base(None)), Err(RaiseRefusal::NoDocument));
        let other = session("rma_letter");
        assert_eq!(check(&base(Some(&other))), Err(RaiseRefusal::NoDocument));
        let mut c = base(Some(&s));
        c.target = ExternalAccess::AnonymousPreferred;
        assert_eq!(check(&c), Err(RaiseRefusal::AboveFocusCeiling));
        let mut c = base(Some(&s));
        c.user_max = ExternalAccess::AnonymousRequired;
        assert_eq!(check(&c), Err(RaiseRefusal::NotARaise));
        let mut c = base(Some(&s));
        c.target = ExternalAccess::LocalOnly;
        assert_eq!(check(&c), Err(RaiseRefusal::NotARaise));
        let codes: Vec<&str> = [
            RaiseRefusal::ProtectedPersona,
            RaiseRefusal::OneTimeChat,
            RaiseRefusal::NoDocument,
            RaiseRefusal::AboveFocusCeiling,
            RaiseRefusal::NotARaise,
        ]
        .iter()
        .map(|r| r.code())
        .collect();
        assert_eq!(
            codes,
            [
                "protected_persona",
                "one_time_chat",
                "no_document",
                "above_focus_ceiling",
                "not_a_raise"
            ]
        );
    }

    #[test]
    fn a_flagged_document_can_be_raised_and_an_existing_raise_counts() {
        let mut s = session("cover_letter");
        s.document_privacy_flag = Some(DocumentPrivacyFlag::Legal);
        let mut c = base(Some(&s));
        c.user_max = ExternalAccess::AnonymousRequired;
        // The flag holds the document at local_only, so the target is a raise.
        assert_eq!(check(&c), Ok(()));
        // Once raised, the same target is not a raise any more.
        s.raised_ceiling = Some(ExternalAccess::AnonymousRequired);
        let c = RaiseCheck {
            session: Some(&s),
            ..base(Some(&s))
        };
        assert_eq!(check(&c), Err(RaiseRefusal::NotARaise));
    }

    #[test]
    fn the_effective_ceiling_never_exceeds_the_focus_and_a_raise_is_for_one_type() {
        let mut s = session("cover_letter");
        s.raised_ceiling = Some(ExternalAccess::AnonymousRequired);
        assert_eq!(
            effective_ceiling(Some(&s), "cover_letter", ExternalAccess::LocalOnly),
            ExternalAccess::AnonymousRequired
        );
        // Another document type: no raise.
        assert_eq!(
            effective_ceiling(Some(&s), "rma_letter", ExternalAccess::LocalOnly),
            ExternalAccess::LocalOnly
        );
        // The raise overrides the flag for this document.
        s.document_privacy_flag = Some(DocumentPrivacyFlag::Medical);
        assert_eq!(
            effective_ceiling(Some(&s), "cover_letter", ExternalAccess::AnonymousPreferred),
            ExternalAccess::AnonymousRequired
        );
        // The caller clamps with the Focus ceiling: never above it.
        let focus = ExternalAccess::LocalOnly;
        assert_eq!(
            effective_ceiling(Some(&s), "cover_letter", ExternalAccess::LocalOnly).min(focus),
            ExternalAccess::LocalOnly
        );
    }

    /// decisions.id=947 with the flag overridden in spirit: once a raise lifts the run's
    /// ceiling, a HOSTED pick of a model-block step is external at the lower of the
    /// ceiling and the step's own highest tier; with no raise (a local-only ceiling) a
    /// hosted pick is refused before any gate, and a local pick is local either way.
    #[test]
    fn a_raised_ceiling_lets_a_hosted_pick_be_external_at_the_lower_tier() {
        use crate::conductor::step_treatment::{derive, Basis, CeilingValues, Chosen};
        let mut s = session("cover_letter");
        s.raised_ceiling = Some(ExternalAccess::AnonymousPreferred);
        let focus = ExternalAccess::Unrestricted;
        let raised =
            effective_ceiling(Some(&s), "cover_letter", ExternalAccess::LocalOnly).min(focus);
        assert_eq!(raised, ExternalAccess::AnonymousPreferred);
        let ceiling = |access| CeilingValues {
            access,
            execution_tier: if access == ExternalAccess::LocalOnly {
                1
            } else {
                2
            },
            abstraction_tier: if access == ExternalAccess::LocalOnly {
                1
            } else {
                2
            },
            raw_abstraction: if access == ExternalAccess::LocalOnly {
                1
            } else {
                2
            },
        };
        let hosted = Basis::Resolved {
            highest_tier: ExternalAccess::AnonymousRequired,
            chosen: Chosen::Hosted,
        };
        let t = derive(ceiling(raised), hosted).unwrap();
        assert!(t.hosted);
        assert_eq!(t.access, ExternalAccess::AnonymousRequired);
        // No raise: the user's local-only ceiling refuses a hosted pick.
        let unraised = effective_ceiling(None, "cover_letter", ExternalAccess::LocalOnly);
        assert!(derive(ceiling(unraised), hosted).is_err());
        // A local pick is local under either ceiling.
        let local = Basis::Resolved {
            highest_tier: ExternalAccess::AnonymousRequired,
            chosen: Chosen::Local,
        };
        assert!(!derive(ceiling(raised), local).unwrap().hosted);
    }

    fn scope(user: &str) -> ChallengeScope {
        ChallengeScope {
            user_id: user.to_owned(),
            persona_id: "p".to_owned(),
            context_key: "c".to_owned(),
            focus_id: "writing-assistant".to_owned(),
            document_type: "cover_letter".to_owned(),
            target: ExternalAccess::AnonymousRequired,
        }
    }

    #[test]
    fn a_token_is_single_use_waits_and_expires() {
        let m = RaiseChallenges::default();
        let t0 = Instant::now();
        let token = m.issue(scope("u"), t0).unwrap();
        assert_eq!(token.len(), 32);
        // Too early: refused and kept.
        assert_eq!(
            m.take(&token, "u", t0 + Duration::from_secs(4)),
            Err(ConfirmRefusal::TooEarly)
        );
        // Another user cannot use or burn it.
        assert_eq!(
            m.take(&token, "v", t0 + MIN_WAIT),
            Err(ConfirmRefusal::InvalidToken)
        );
        // Its owner, after the wait: once.
        assert_eq!(m.take(&token, "u", t0 + MIN_WAIT), Ok(scope("u")));
        assert_eq!(
            m.take(&token, "u", t0 + MIN_WAIT),
            Err(ConfirmRefusal::InvalidToken)
        );
        // Expired.
        let late = m.issue(scope("u"), t0).unwrap();
        assert_eq!(
            m.take(&late, "u", t0 + CHALLENGE_TTL),
            Err(ConfirmRefusal::Expired)
        );
        assert_eq!(
            m.take(&late, "u", t0 + MIN_WAIT),
            Err(ConfirmRefusal::InvalidToken)
        );
    }

    #[test]
    fn the_store_is_bounded_and_cleared_per_user() {
        let m = RaiseChallenges::default();
        let t0 = Instant::now();
        for i in 0..(MAX_CHALLENGES + 4) {
            m.issue(scope("u"), t0 + Duration::from_millis(i as u64))
                .unwrap();
        }
        assert_eq!(m.len(), MAX_CHALLENGES);
        m.issue(scope("v"), t0).unwrap();
        m.clear_user("u");
        assert_eq!(m.len(), 1);
        assert_eq!(format!("{m:?}"), "RaiseChallenges(<redacted>)");
    }
}
