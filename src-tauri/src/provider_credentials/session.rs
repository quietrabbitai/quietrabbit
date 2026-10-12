// src-tauri/src/provider_credentials/session.rs
//
// The "connect dialog is open" state (items.id=714). In memory only; nothing
// here is persisted. The clipboard may be read, and a page opened, only
// while a session exists for the signed-in user (spec Part 8g, AUTH
// section 16 point 3).
//
// One session per app: beginning a new one replaces and zeroizes the old. A
// session expires after `SESSION_TTL`, and every lookup also requires the
// session to belong to the user who is signed in now, so a session cannot
// outlive a logout-and-login as someone else.
//
// An OAuth connect (items.id=734) lives inside its session as an `OauthFlow`:
// its phase, a cancel token, and in paste mode the PKCE verifier. Dropping the
// session (end, clear at logout or idle lock, expiry, a new session) drops the
// flow, and dropping the flow cancels it, which closes the loopback listener
// its task owns. No secret is in the phase: a key that arrived is the
// session's zeroized `candidate`, exactly as for a clipboard key.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// How long a connect dialog's session lasts without being ended.
pub const SESSION_TTL: Duration = Duration::from_secs(15 * 60);

/// Why a session cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// No such session, or it belongs to another user.
    NoSession,
    /// It ran past `SESSION_TTL`; it has been dropped.
    Expired,
}

/// Where an OAuth connect has got to. Carries no secret: `KeyReady` holds only
/// the key's length (the key itself is the session's candidate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OauthPhase {
    /// The browser was opened; the loopback listener is waiting for the callback.
    WaitingForBrowser,
    /// Paste mode: waiting for the user to paste the code the provider shows.
    WaitingForCode,
    Exchanging,
    KeyReady(u32),
    /// Ended without a key; the value is a stable code.
    Failed(&'static str),
    Cancelled,
}

/// What paste mode keeps between the click that opens the browser and the
/// click that submits the code.
pub struct PasteFlow {
    pub verifier: Zeroizing<String>,
    pub exchange_url: String,
    pub deadline: Instant,
}

impl std::fmt::Debug for PasteFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasteFlow([redacted])")
    }
}

/// One OAuth connect inside a session. Dropping it cancels it.
pub struct OauthFlow {
    id: u64,
    phase: OauthPhase,
    cancel: CancellationToken,
    paste: Option<PasteFlow>,
}

impl OauthFlow {
    pub fn new(
        id: u64,
        phase: OauthPhase,
        cancel: CancellationToken,
        paste: Option<PasteFlow>,
    ) -> Self {
        Self {
            id,
            phase,
            cancel,
            paste,
        }
    }
}

impl Drop for OauthFlow {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl SessionError {
    /// The stable code the IPC layer returns.
    pub fn code(self) -> &'static str {
        match self {
            Self::NoSession => "no_session",
            Self::Expired => "session_expired",
        }
    }
}

pub struct ConnectSession {
    pub provider_id: String,
    user_id: String,
    expires_at: Instant,
    /// A key waiting for the user's confirm: clipboard text the user was
    /// offered, or the key an OAuth exchange returned.
    candidate: Option<Zeroizing<String>>,
    oauth: Option<OauthFlow>,
}

impl ConnectSession {
    /// Start an OAuth flow, replacing (and cancelling) any earlier one and
    /// dropping any candidate: a new connect starts from nothing.
    pub fn begin_oauth(&mut self, flow: OauthFlow) {
        self.candidate = None;
        self.oauth = Some(flow);
    }

    pub fn oauth_phase(&self) -> Option<OauthPhase> {
        self.oauth.as_ref().map(|f| f.phase)
    }

    /// Whether `flow_id` is the flow this session has now and it has not been
    /// cancelled. A finished or replaced flow's task must not write anything.
    fn live_flow(&mut self, flow_id: u64) -> Option<&mut OauthFlow> {
        self.oauth
            .as_mut()
            .filter(|f| f.id == flow_id && f.phase != OauthPhase::Cancelled)
    }

    /// Move flow `flow_id` to `phase`. False when it is no longer the live flow.
    pub fn set_oauth_phase(&mut self, flow_id: u64, phase: OauthPhase) -> bool {
        match self.live_flow(flow_id) {
            Some(f) => {
                f.phase = phase;
                true
            }
            None => false,
        }
    }

    /// The exchange returned `key`: hold it as the candidate and mark the flow
    /// `KeyReady`. False (and the key is dropped, zeroized) when the flow is
    /// no longer live.
    pub fn finish_oauth_with_key(&mut self, flow_id: u64, key: Zeroizing<String>) -> bool {
        let length = u32::try_from(key.chars().count()).unwrap_or(u32::MAX);
        let live = self
            .live_flow(flow_id)
            .map(|f| {
                f.phase = OauthPhase::KeyReady(length);
                f.paste = None;
            })
            .is_some();
        if live {
            self.candidate = Some(key);
        }
        live
    }

    /// End the flow at the user's request: its task stops, its listener
    /// closes, paste state is dropped. A no-op without a flow.
    pub fn cancel_oauth(&mut self) {
        if let Some(f) = self.oauth.as_mut() {
            f.cancel.cancel();
            f.paste = None;
            if !matches!(f.phase, OauthPhase::KeyReady(_)) {
                f.phase = OauthPhase::Cancelled;
            }
        }
    }

    /// Paste mode: the verifier (a zeroizing copy), the exchange URL and the
    /// deadline of the live flow, while it is waiting for a code.
    pub fn paste_parts(&self, flow_id: u64) -> Option<(Zeroizing<String>, String, Instant)> {
        let f = self.oauth.as_ref().filter(|f| f.id == flow_id)?;
        let p = f.paste.as_ref()?;
        Some((p.verifier.clone(), p.exchange_url.clone(), p.deadline))
    }

    pub fn oauth_flow_id(&self) -> Option<u64> {
        self.oauth.as_ref().map(|f| f.id)
    }

    /// The flow's cancel token, so a collaborator can race its work against it.
    pub fn oauth_cancel_token(&self, flow_id: u64) -> Option<CancellationToken> {
        self.oauth
            .as_ref()
            .filter(|f| f.id == flow_id)
            .map(|f| f.cancel.clone())
    }

    pub fn set_candidate(&mut self, key: Zeroizing<String>) {
        self.candidate = Some(key);
    }

    pub fn clear_candidate(&mut self) {
        self.candidate = None;
    }

    pub fn candidate(&self) -> Option<&str> {
        self.candidate.as_deref().map(String::as_str)
    }

    pub fn has_candidate(&self) -> bool {
        self.candidate.is_some()
    }
}

/// Cloneable handle: the OAuth listener task holds its own to write its result
/// back into the session it belongs to.
#[derive(Default, Clone)]
pub struct ConnectSessions {
    slot: Arc<Mutex<Option<(String, ConnectSession)>>>,
}

fn new_session_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

impl ConnectSessions {
    /// Start a session for `user_id` on `provider_id`, dropping any other.
    pub fn begin(&self, user_id: &str, provider_id: &str, now: Instant) -> Result<String, String> {
        let id = new_session_id()?;
        let session = ConnectSession {
            provider_id: provider_id.to_owned(),
            user_id: user_id.to_owned(),
            expires_at: now + SESSION_TTL,
            candidate: None,
            oauth: None,
        };
        *self.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some((id.clone(), session));
        Ok(id)
    }

    /// Run `f` on the session `id` if it is live and belongs to `user_id`.
    pub fn with_session<R>(
        &self,
        id: &str,
        user_id: &str,
        now: Instant,
        f: impl FnOnce(&mut ConnectSession) -> R,
    ) -> Result<R, SessionError> {
        let mut guard = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_mut() {
            Some((sid, s)) if sid == id && s.user_id == user_id => {
                if now >= s.expires_at {
                    *guard = None;
                    return Err(SessionError::Expired);
                }
                Ok(f(s))
            }
            _ => Err(SessionError::NoSession),
        }
    }

    /// End the session `id` if it exists (any owner may end its own; an
    /// unknown id is a no-op). The candidate is zeroized on drop.
    pub fn end(&self, id: &str, user_id: &str) {
        let mut guard = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(guard.as_ref(), Some((sid, s)) if sid == id && s.user_id == user_id) {
            *guard = None;
        }
    }

    /// Drop the session, if any, and its candidate (logout).
    pub fn clear(&self) {
        *self.slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.slot.lock().unwrap().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_usable_by_its_owner_until_it_expires() {
        let s = ConnectSessions::default();
        let t0 = Instant::now();
        let id = s.begin("u1", "p1", t0).unwrap();
        assert_eq!(id.len(), 32);
        assert_eq!(
            s.with_session(&id, "u1", t0, |x| x.provider_id.clone()),
            Ok("p1".into())
        );
        assert_eq!(
            s.with_session(&id, "u1", t0 + SESSION_TTL, |_| ()),
            Err(SessionError::Expired)
        );
        // Expiry drops it.
        assert!(s.is_empty());
        assert_eq!(
            s.with_session(&id, "u1", t0, |_| ()),
            Err(SessionError::NoSession)
        );
    }

    #[test]
    fn another_user_or_another_id_gets_no_session() {
        let s = ConnectSessions::default();
        let t0 = Instant::now();
        let id = s.begin("u1", "p1", t0).unwrap();
        assert_eq!(
            s.with_session(&id, "u2", t0, |_| ()),
            Err(SessionError::NoSession)
        );
        assert_eq!(
            s.with_session("nope", "u1", t0, |_| ()),
            Err(SessionError::NoSession)
        );
        // A wrong guess did not end the real session.
        assert!(s.with_session(&id, "u1", t0, |_| ()).is_ok());
    }

    #[test]
    fn a_new_session_replaces_the_old_and_ending_clears() {
        let s = ConnectSessions::default();
        let t0 = Instant::now();
        let first = s.begin("u1", "p1", t0).unwrap();
        s.with_session(&first, "u1", t0, |x| {
            x.set_candidate(Zeroizing::new("k".into()))
        })
        .unwrap();
        let second = s.begin("u1", "p2", t0).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            s.with_session(&first, "u1", t0, |_| ()),
            Err(SessionError::NoSession)
        );
        assert_eq!(
            s.with_session(&second, "u1", t0, |x| x.has_candidate()),
            Ok(false)
        );
        s.end(&second, "u2");
        assert!(!s.is_empty(), "another user cannot end it");
        s.end(&second, "u1");
        assert!(s.is_empty());
        s.end(&second, "u1");
    }

    #[test]
    fn the_candidate_can_be_set_read_and_cleared() {
        let s = ConnectSessions::default();
        let t0 = Instant::now();
        let id = s.begin("u1", "p1", t0).unwrap();
        s.with_session(&id, "u1", t0, |x| {
            x.set_candidate(Zeroizing::new("sk-1".into()))
        })
        .unwrap();
        assert_eq!(
            s.with_session(&id, "u1", t0, |x| x.candidate().map(str::to_owned)),
            Ok(Some("sk-1".to_owned()))
        );
        s.with_session(&id, "u1", t0, |x| x.clear_candidate())
            .unwrap();
        assert_eq!(
            s.with_session(&id, "u1", t0, |x| x.has_candidate()),
            Ok(false)
        );
    }
}
