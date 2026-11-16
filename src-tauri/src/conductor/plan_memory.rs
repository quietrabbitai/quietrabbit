// src-tauri/src/conductor/plan_memory.rs
//
// items.id=901 stage 3 (ruling R-A and R-D of the approved plan): the MEMORY-ONLY HOME of
// the run plan. It holds three kinds of short-lived state that must never be persisted:
//
//   1. APPROVALS of a latched chat (a one-time session), of a chat whose latch cannot be
//      read, and of a chat whose database could not be opened. The persisted home
//      (persistence::plan_approval_store) refuses all of these, so the user's approval
//      lives here instead and dies with the process. A restart means the user is asked
//      again. It never means a step is allowed.
//   2. OFFERS of a run with NO chat (submit_focus_run). get_run_plan registers the offer
//      under an opaque 128-bit random token, approve_run_plan approves THAT token, and the
//      run's plan gate takes the approval and consumes it with the run. A token is single
//      use, bound to its user, Persona and Focus, expires, and is never reused by a later
//      run. (The run is the unit here: there is no chat to hang an approval on.)
//   3. The duplicate-turn MARKER: (chat, saved message id, SHA-256 of the content) left when
//      the in-run plan check finds PlanRequired AFTER send_message saved the user's turn,
//      so the call-again can skip the save. One per chat, a newer one replaces it. The
//      text itself is never held, only its hash.
//
// BOUNDS (stated in the plan and pinned by tests):
//   * approvals:  at most MAX_APPROVALS (64) entries, each lives APPROVAL_TTL (2 hours) from
//                 the moment it was approved (absolute, not sliding);
//   * offers:     at most MAX_OFFERS (16), each lives OFFER_TTL (10 minutes);
//   * markers:    at most MAX_MARKERS (64), one per chat, each lives MARKER_TTL (120 seconds).
//   On insert the expired entries are dropped first, then the oldest are evicted until there
//   is room. Every read drops what has expired.
//
// CLEARING. clear_user (a key leaves the registry: logout, idle or ceiling lock, a key
// rotation finishing, a changed account; wired in main.rs through KeyRegistry::on_key_leave),
// clear_chat (a chat is archived today; items.id=888's chat deletion must call it too) and
// clear_all. Nothing here outlives a logout or a clear.
//
// WHAT THIS MODULE NEVER DOES. It is never serialized (no serde derive in this file), never
// logged (no log call), never put in a checkpoint or a crash report, and never shown in
// Debug: the Debug impls print counts, not ids, digests, pins, tokens or ceilings. It does
// not reach the persisted store or any database. The scan tests pin all of that.
//
// NOTHING HERE WIDENS A GATE. A value read from here is only ever COMPARED against the plan
// the run recomputed (conductor::plan_approval::verify). A missing value means "ask again".

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::conductor::plan_approval::Approval;

pub const MAX_APPROVALS: usize = 64;
pub const APPROVAL_TTL: Duration = Duration::from_secs(2 * 60 * 60);
pub const MAX_OFFERS: usize = 16;
pub const OFFER_TTL: Duration = Duration::from_secs(10 * 60);
pub const MAX_MARKERS: usize = 64;
pub const MARKER_TTL: Duration = Duration::from_secs(120);

/// Whose approval: the user, the Persona, the chat and the Focus.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ChatScope {
    pub user_id: String,
    pub persona_id: String,
    pub context_key: String,
    pub focus_id: String,
}

impl fmt::Debug for ChatScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChatScope(<redacted>)")
    }
}

/// Who owns an offer of a run with no chat.
#[derive(Clone, PartialEq, Eq)]
pub struct OfferOwner {
    pub user_id: String,
    pub persona_id: String,
    pub focus_id: String,
}

impl fmt::Debug for OfferOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OfferOwner(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferError {
    /// No such offer for this owner, or it expired.
    NotFound,
    /// The digest is not the one the offer was registered with: the plan changed.
    PlanChanged,
    /// The operating system's random source failed, so no token was made.
    NoRandomness,
}

struct HeldApproval {
    approval: Approval,
    approved_at: Instant,
}

struct HeldOffer {
    owner: OfferOwner,
    /// The pin-free digest the user was shown.
    shown_digest: String,
    approval: Option<Approval>,
    created: Instant,
}

/// What the duplicate-turn marker holds: a message id and a hash of the content.
pub struct Marker {
    pub message_id: String,
    content_sha: [u8; 32],
    created: Instant,
}

impl Marker {
    /// Whether `content` is the text the marker was made from.
    pub fn matches(&self, content: &str) -> bool {
        self.content_sha == sha(content)
    }
}

impl fmt::Debug for Marker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Marker(<redacted>)")
    }
}

fn sha(content: &str) -> [u8; 32] {
    Sha256::digest(content.as_bytes()).into()
}

#[derive(Default)]
struct State {
    approvals: HashMap<ChatScope, HeldApproval>,
    offers: HashMap<String, HeldOffer>,
    /// (user, Persona, chat) -> marker.
    markers: HashMap<(String, String, String), Marker>,
}

/// The memory-only home. Managed as Tauri state (main.rs).
#[derive(Clone, Default)]
pub struct PlanMemory {
    inner: Arc<Mutex<State>>,
}

impl fmt::Debug for PlanMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.lock();
        write!(
            f,
            "PlanMemory {{ approvals: {}, offers: {}, markers: {} }}",
            s.approvals.len(),
            s.offers.len(),
            s.markers.len()
        )
    }
}

/// Drop what has expired, then evict the oldest until one more fits.
fn make_room<K: Clone + Eq + std::hash::Hash, V>(
    map: &mut HashMap<K, V>,
    max: usize,
    ttl: Duration,
    born: impl Fn(&V) -> Instant,
) {
    map.retain(|_, v| born(v).elapsed() < ttl);
    while map.len() >= max {
        let oldest = map
            .iter()
            .min_by_key(|(_, v)| born(v))
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => {
                map.remove(&k);
            }
            None => break,
        }
    }
}

impl PlanMemory {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A poisoned lock still holds consistent data (every critical section is a few
        // map operations).
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    // -- approvals of a chat ---------------------------------------------------------

    /// Hold the approval of a chat (replacing an earlier one).
    pub fn put_chat_approval(&self, scope: &ChatScope, approval: Approval) {
        let mut s = self.lock();
        s.approvals.remove(scope);
        make_room(&mut s.approvals, MAX_APPROVALS, APPROVAL_TTL, |h| {
            h.approved_at
        });
        s.approvals.insert(
            scope.clone(),
            HeldApproval {
                approval,
                approved_at: Instant::now(),
            },
        );
    }

    /// The approval held for a chat, unless it has expired (an expired one is dropped).
    pub fn get_chat_approval(&self, scope: &ChatScope) -> Option<Approval> {
        let mut s = self.lock();
        s.approvals
            .retain(|_, h| h.approved_at.elapsed() < APPROVAL_TTL);
        s.approvals.get(scope).map(|h| h.approval.clone())
    }

    /// Withdraw the approval held for a chat. Returns whether there was one.
    pub fn remove_chat_approval(&self, scope: &ChatScope) -> bool {
        self.lock().approvals.remove(scope).is_some()
    }

    // -- offers of a run with no chat ----------------------------------------------

    /// Register the offer the user was shown (its pin-free digest) and return the opaque
    /// token that names it.
    pub fn register_offer(
        &self,
        owner: &OfferOwner,
        shown_digest: &str,
    ) -> Result<String, OfferError> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| OfferError::NoRandomness)?;
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let mut s = self.lock();
        make_room(&mut s.offers, MAX_OFFERS, OFFER_TTL, |o| o.created);
        s.offers.insert(
            token.clone(),
            HeldOffer {
                owner: owner.clone(),
                shown_digest: shown_digest.to_owned(),
                approval: None,
                created: Instant::now(),
            },
        );
        Ok(token)
    }

    /// Attach the user's approval to the offer. The shown digest must be the one the
    /// offer was registered with, else the plan changed and nothing is approved.
    pub fn approve_offer(
        &self,
        token: &str,
        owner: &OfferOwner,
        shown_digest: &str,
        approval: Approval,
    ) -> Result<(), OfferError> {
        let mut s = self.lock();
        s.offers.retain(|_, o| o.created.elapsed() < OFFER_TTL);
        let offer = s
            .offers
            .get_mut(token)
            .filter(|o| o.owner == *owner)
            .ok_or(OfferError::NotFound)?;
        if offer.shown_digest != shown_digest {
            return Err(OfferError::PlanChanged);
        }
        offer.approval = Some(approval);
        Ok(())
    }

    /// Take the approval of an offer and CONSUME the offer with it: single use, so a
    /// later run can never reuse it. Another owner's offer, an expired one and one that
    /// is not yet approved give None and stay where they are (an expired one is dropped).
    pub fn take_offer_approval(&self, token: &str, owner: &OfferOwner) -> Option<Approval> {
        let mut s = self.lock();
        s.offers.retain(|_, o| o.created.elapsed() < OFFER_TTL);
        let ready = s
            .offers
            .get(token)
            .is_some_and(|o| o.owner == *owner && o.approval.is_some());
        if !ready {
            return None;
        }
        s.offers.remove(token).and_then(|o| o.approval)
    }

    /// Withdraw an offer and its approval (a revocation of a chatless approval).
    pub fn revoke_offer(&self, token: &str, owner: &OfferOwner) -> bool {
        let mut s = self.lock();
        if s.offers.get(token).is_some_and(|o| o.owner == *owner) {
            s.offers.remove(token);
            true
        } else {
            false
        }
    }

    // -- the duplicate-turn marker ---------------------------------------------------

    /// Leave the marker for a chat: the saved message and a hash of its content. One per
    /// chat: a newer one replaces it.
    pub fn set_marker(
        &self,
        user_id: &str,
        persona_id: &str,
        context_key: &str,
        message_id: &str,
        content: &str,
    ) {
        let mut s = self.lock();
        let key = (
            user_id.to_owned(),
            persona_id.to_owned(),
            context_key.to_owned(),
        );
        s.markers.remove(&key);
        make_room(&mut s.markers, MAX_MARKERS, MARKER_TTL, |m| m.created);
        s.markers.insert(
            key,
            Marker {
                message_id: message_id.to_owned(),
                content_sha: sha(content),
                created: Instant::now(),
            },
        );
    }

    /// Take the chat's marker, if it has not expired. ALWAYS removes it: a match consumes
    /// it and a mismatch discards it, so the caller decides with `matches` and the chat's
    /// own state and never leaves it behind.
    pub fn take_marker(
        &self,
        user_id: &str,
        persona_id: &str,
        context_key: &str,
    ) -> Option<Marker> {
        let mut s = self.lock();
        let key = (
            user_id.to_owned(),
            persona_id.to_owned(),
            context_key.to_owned(),
        );
        let marker = s.markers.remove(&key)?;
        (marker.created.elapsed() < MARKER_TTL).then_some(marker)
    }

    // -- clearing --------------------------------------------------------------------

    /// Everything of one user (the key left the registry).
    pub fn clear_user(&self, user_id: &str) {
        let mut s = self.lock();
        s.approvals.retain(|k, _| k.user_id != user_id);
        s.offers.retain(|_, o| o.owner.user_id != user_id);
        s.markers.retain(|k, _| k.0 != user_id);
    }

    /// Everything of one chat: its approvals (every Focus) and its marker. A chat that is
    /// archived or deleted must call this (items.id=888's chat deletion too).
    pub fn clear_chat(&self, user_id: &str, persona_id: &str, context_key: &str) {
        let mut s = self.lock();
        s.approvals.retain(|k, _| {
            !(k.user_id == user_id && k.persona_id == persona_id && k.context_key == context_key)
        });
        s.markers.remove(&(
            user_id.to_owned(),
            persona_id.to_owned(),
            context_key.to_owned(),
        ));
    }

    pub fn clear_all(&self) {
        let mut s = self.lock();
        s.approvals.clear();
        s.offers.clear();
        s.markers.clear();
    }

    // -- counts, for tests and diagnostics -------------------------------------------

    pub fn approval_count(&self) -> usize {
        self.lock().approvals.len()
    }
    pub fn offer_count(&self) -> usize {
        self.lock().offers.len()
    }
    pub fn marker_count(&self) -> usize {
        self.lock().markers.len()
    }

    #[cfg(test)]
    pub(crate) fn age_approval(&self, scope: &ChatScope, by: Duration) {
        if let Some(h) = self.lock().approvals.get_mut(scope) {
            h.approved_at = h.approved_at.checked_sub(by).unwrap_or(h.approved_at);
        }
    }
    #[cfg(test)]
    pub(crate) fn age_offer(&self, token: &str, by: Duration) {
        if let Some(o) = self.lock().offers.get_mut(token) {
            o.created = o.created.checked_sub(by).unwrap_or(o.created);
        }
    }
    #[cfg(test)]
    pub(crate) fn age_marker(&self, user: &str, persona: &str, chat: &str, by: Duration) {
        let key = (user.to_owned(), persona.to_owned(), chat.to_owned());
        if let Some(m) = self.lock().markers.get_mut(&key) {
            m.created = m.created.checked_sub(by).unwrap_or(m.created);
        }
    }
}

#[cfg(test)]
#[path = "plan_memory_tests.rs"]
mod tests;
