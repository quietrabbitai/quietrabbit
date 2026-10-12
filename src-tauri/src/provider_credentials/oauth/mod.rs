// src-tauri/src/provider_credentials/oauth/mod.rs
//
// The OpenRouter OAuth PKCE connect, part (b) of items.id=714 (items.id=734;
// decisions.id=875 point 7, decisions.id=881, spec Part 8g, AUTH section 16
// point 3).
//
//   pkce      verifier, S256 challenge, state, constant-time compare
//   callback  the loopback listener and its request parser (the only listener)
//   exchange  the https POST that turns the approval code into a key
//   flow      ties them to the connect session
//
// HOW IT FITS PART (a). The flow ends with the key held as the connect
// session's zeroized candidate, exactly where a matched clipboard key sits.
// The user's next click is the existing `save_provider_key` (source
// `SessionCandidate`), which checks the key and stores it with `auth_type`
// `api_key`. The frontend never sees the code, the verifier or the key: it
// sees a phase and, when a key is ready, its length.
//
// NOTHING RUNS BY ITSELF. A flow starts only from `start_oauth_connect`, a
// click inside an open session. It ends at the first valid callback, the
// deadline, a cancel, or the end of its session (end, logout, idle lock,
// expiry, a newer session).

pub mod callback;
pub mod exchange;
pub mod flow;
pub mod pkce;

use serde::{Deserialize, Serialize};
use specta::Type;

use super::session::OauthPhase;

/// How the approval gets back to QR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum OauthMode {
    /// The provider sends the browser to a loopback address QR listens on.
    Loopback,
    /// The provider shows a one-time code and the user pastes it.
    Paste,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum OauthState {
    /// No flow in this session.
    Idle,
    WaitingForBrowser,
    WaitingForCode,
    Exchanging,
    /// The key is held in the session; `save_provider_key` with
    /// `SessionCandidate` stores it.
    KeyReady,
    Failed,
    Cancelled,
}

/// What the dialog is told. A code, never any text from the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct OauthStatus {
    pub state: OauthState,
    /// Characters in the key; 0 unless `KeyReady`.
    pub key_length: u32,
    /// A stable code when `Failed`.
    pub failure: Option<String>,
}

impl From<Option<OauthPhase>> for OauthStatus {
    fn from(phase: Option<OauthPhase>) -> Self {
        let (state, key_length, failure) = match phase {
            None => (OauthState::Idle, 0, None),
            Some(OauthPhase::WaitingForBrowser) => (OauthState::WaitingForBrowser, 0, None),
            Some(OauthPhase::WaitingForCode) => (OauthState::WaitingForCode, 0, None),
            Some(OauthPhase::Exchanging) => (OauthState::Exchanging, 0, None),
            Some(OauthPhase::KeyReady(n)) => (OauthState::KeyReady, n, None),
            Some(OauthPhase::Failed(code)) => (OauthState::Failed, 0, Some(code.to_owned())),
            Some(OauthPhase::Cancelled) => (OauthState::Cancelled, 0, None),
        };
        Self {
            state,
            key_length,
            failure,
        }
    }
}
