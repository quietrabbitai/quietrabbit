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

use std::sync::Mutex;
use std::time::{Duration, Instant};

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

pub struct ConnectSession {
    pub provider_id: String,
    user_id: String,
    expires_at: Instant,
    /// The clipboard text the user was offered and has not yet confirmed.
    candidate: Option<Zeroizing<String>>,
}

impl ConnectSession {
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

#[derive(Default)]
pub struct ConnectSessions {
    slot: Mutex<Option<(String, ConnectSession)>>,
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
