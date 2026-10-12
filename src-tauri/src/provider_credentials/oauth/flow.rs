// src-tauri/src/provider_credentials/oauth/flow.rs
//
// Start, finish, cancel and report an OAuth connect inside a connect session
// (items.id=734). The collaborators (browser opener, exchange client, session
// store, timings) arrive in `OauthEnv`, so the tests use a fake browser, a
// loopback exchange fixture and short timeouts, and the command wrappers only
// resolve state.
//
// ORDER OF START. Check the session and the row; resolve both OAuth addresses
// through the URL policy (an unsafe row fails here, before any socket or
// browser); make the PKCE pair and the state; bind the loopback listeners (if
// that fails, fall back to paste mode); register the flow in the session; only
// then open the browser. A browser that cannot be opened ends the flow.
//
// THE KEY'S PATH. Exchange reply -> zeroizing string -> the session's
// candidate (`finish_oauth_with_key`) -> the user's click on
// `save_provider_key` (source `SessionCandidate`) -> `save_impl`. If the
// session or the flow is gone by the time the reply arrives, the key is
// dropped, zeroized, without being stored.

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::callback::{
    bind_loopback, serve, CallbackOutcome, Listeners, ServeConfig, CALLBACK_HOST, CALLBACK_PATH,
    MAX_CODE_BYTES,
};
use super::exchange::ExchangeClient;
use super::pkce::{new_flow_id, new_state, Pkce, CHALLENGE_METHOD};
use super::{OauthMode, OauthStatus};
use crate::persistence::provider_store::Provider;
use crate::provider_credentials::opener::UrlOpener;
use crate::provider_credentials::session::{
    ConnectSessions, OauthFlow, OauthPhase, PasteFlow, SessionError,
};
use crate::provider_credentials::url_policy::{resolve_oauth, OauthUrl, UrlRefusal};
use crate::provider_credentials::{acquisition, is_hosted};

/// The label OpenRouter prefills on the key it creates.
pub const KEY_LABEL: &str = "Quiet Rabbit";

/// `credential_acquisition.method` this flow serves.
pub const METHOD: &str = "oauth_pkce";

pub struct OauthEnv {
    pub sessions: ConnectSessions,
    pub opener: Arc<dyn UrlOpener>,
    pub exchange: Arc<ExchangeClient>,
    pub cfg: ServeConfig,
}

/// The one production exchange client (a connection pool; holds no secret).
pub fn exchange_client() -> Arc<ExchangeClient> {
    static C: OnceLock<Arc<ExchangeClient>> = OnceLock::new();
    C.get_or_init(|| Arc::new(ExchangeClient::new())).clone()
}

fn session_err(e: SessionError) -> String {
    e.code().to_owned()
}

fn refusal(e: UrlRefusal, missing: &str) -> String {
    match e {
        UrlRefusal::NoLink => missing.to_owned(),
        UrlRefusal::Refused => "link_refused".to_owned(),
    }
}

/// The authorization address the browser is sent to.
fn authorization_url(
    base: &str,
    pkce: &Pkce,
    callback: Option<&str>,
    state: Option<&str>,
) -> Result<String, String> {
    let mut u = reqwest::Url::parse(base).map_err(|_| "link_refused".to_owned())?;
    {
        let mut q = u.query_pairs_mut();
        if let Some(cb) = callback {
            q.append_pair("callback_url", cb);
        }
        q.append_pair("code_challenge", pkce.challenge());
        q.append_pair("code_challenge_method", CHALLENGE_METHOD);
        if let Some(s) = state {
            q.append_pair("state", s);
        }
        q.append_pair("key_label", KEY_LABEL);
    }
    Ok(u.into())
}

fn set_phase(
    sessions: &ConnectSessions,
    session_id: &str,
    user_id: &str,
    flow_id: u64,
    phase: OauthPhase,
) -> bool {
    sessions
        .with_session(session_id, user_id, Instant::now(), |s| {
            s.set_oauth_phase(flow_id, phase)
        })
        .unwrap_or(false)
}

/// Record an exchange result in the session. A key goes in as the candidate;
/// a session or flow that is gone drops it.
fn finish(
    sessions: &ConnectSessions,
    session_id: &str,
    user_id: &str,
    flow_id: u64,
    result: Result<Zeroizing<String>, super::exchange::ExchangeError>,
) {
    match result {
        Ok(key) => {
            let _ = sessions.with_session(session_id, user_id, Instant::now(), |s| {
                s.finish_oauth_with_key(flow_id, key)
            });
        }
        Err(e) => {
            set_phase(
                sessions,
                session_id,
                user_id,
                flow_id,
                OauthPhase::Failed(e.code()),
            );
        }
    }
}

struct LoopbackJob {
    sessions: ConnectSessions,
    exchange: Arc<ExchangeClient>,
    cfg: ServeConfig,
    user_id: String,
    session_id: String,
    flow_id: u64,
    listeners: Listeners,
    state: Zeroizing<String>,
    pkce: Pkce,
    exchange_url: String,
    cancel: CancellationToken,
}

async fn run_loopback(job: LoopbackJob) {
    let LoopbackJob {
        sessions,
        exchange,
        cfg,
        user_id,
        session_id,
        flow_id,
        listeners,
        state,
        pkce,
        exchange_url,
        cancel,
    } = job;
    let outcome = serve(listeners, &state, &cfg, &cancel).await;
    drop(state);
    let fail = |code: &'static str| {
        set_phase(
            &sessions,
            &session_id,
            &user_id,
            flow_id,
            OauthPhase::Failed(code),
        );
    };
    match outcome {
        CallbackOutcome::Code(code) => {
            if !set_phase(
                &sessions,
                &session_id,
                &user_id,
                flow_id,
                OauthPhase::Exchanging,
            ) {
                return;
            }
            let result = tokio::select! {
                _ = cancel.cancelled() => return,
                r = exchange.exchange(&exchange_url, &code, pkce.verifier()) => r,
            };
            finish(&sessions, &session_id, &user_id, flow_id, result);
        }
        CallbackOutcome::Timeout => fail("callback_timeout"),
        CallbackOutcome::TooManyInvalid => fail("too_many_requests"),
        CallbackOutcome::Cancelled => {}
    }
}

/// Begin an OAuth connect for the session's provider and open the browser.
/// Returns the mode actually used: `Loopback` falls back to `Paste` when no
/// loopback listener can be bound.
pub async fn start(
    env: &OauthEnv,
    provider: &Provider,
    session_id: &str,
    user_id: &str,
    requested: OauthMode,
) -> Result<OauthMode, String> {
    let now = Instant::now();
    let in_session = env
        .sessions
        .with_session(session_id, user_id, now, |s| s.provider_id == provider.id)
        .map_err(session_err)?;
    if !in_session || !is_hosted(provider) {
        return Err("no_session".to_owned());
    }
    if acquisition(provider).method.as_deref() != Some(METHOD) {
        return Err("not_oauth".to_owned());
    }
    let auth_url = resolve_oauth(provider, OauthUrl::Auth).map_err(|e| refusal(e, "no_link"))?;
    let exchange_url =
        resolve_oauth(provider, OauthUrl::Exchange).map_err(|e| refusal(e, "no_exchange_url"))?;

    let pkce = Pkce::generate().map_err(|e| internal("pkce", &e))?;
    let state = new_state().map_err(|e| internal("state", &e))?;
    let flow_id = new_flow_id().map_err(|e| internal("flow id", &e))?;

    let listeners = match requested {
        OauthMode::Loopback => bind_loopback().await.ok(),
        OauthMode::Paste => None,
    };
    let mode = if listeners.is_some() {
        OauthMode::Loopback
    } else {
        OauthMode::Paste
    };

    let callback = listeners
        .as_ref()
        .map(|l| format!("http://{CALLBACK_HOST}:{}{CALLBACK_PATH}", l.port()));
    // The provider echoes `state` only on a callback; paste mode has none.
    let url = authorization_url(
        &auth_url,
        &pkce,
        callback.as_deref(),
        listeners.is_some().then_some(state.as_str()),
    )?;

    let cancel = CancellationToken::new();
    let paste = listeners.is_none().then(|| PasteFlow {
        verifier: pkce.verifier_owned(),
        exchange_url: exchange_url.clone(),
        deadline: now + env.cfg.flow_ttl,
    });
    let phase = if paste.is_some() {
        OauthPhase::WaitingForCode
    } else {
        OauthPhase::WaitingForBrowser
    };
    env.sessions
        .with_session(session_id, user_id, now, |s| {
            s.begin_oauth(OauthFlow::new(flow_id, phase, cancel.clone(), paste))
        })
        .map_err(session_err)?;

    if let Some(listeners) = listeners {
        tokio::spawn(run_loopback(LoopbackJob {
            sessions: env.sessions.clone(),
            exchange: env.exchange.clone(),
            cfg: env.cfg.clone(),
            user_id: user_id.to_owned(),
            session_id: session_id.to_owned(),
            flow_id,
            listeners,
            state,
            pkce,
            exchange_url,
            cancel,
        }));
    }

    let opener = env.opener.clone();
    let opened = tokio::task::spawn_blocking(move || opener.open(&url))
        .await
        .map_err(|e| internal("open task", &e.to_string()))?;
    if let Err(e) = opened {
        // The error text carries no URL (opener.rs).
        log::warn!("provider_credentials: oauth: {e}");
        let _ = env
            .sessions
            .with_session(session_id, user_id, Instant::now(), |s| s.cancel_oauth());
        return Err("open_failed".to_owned());
    }
    Ok(mode)
}

fn internal(what: &str, e: &str) -> String {
    log::warn!("provider_credentials: oauth: {what} failed: {e}");
    "internal".to_owned()
}

/// Paste mode: the user pasted the code the provider showed. Exchanges it and
/// returns the resulting status. The code goes into a zeroizing buffer on
/// arrival and is checked for shape before anything is sent.
pub async fn submit_code(
    env: &OauthEnv,
    session_id: &str,
    user_id: &str,
    code: Zeroizing<String>,
) -> Result<OauthStatus, String> {
    let now = Instant::now();
    type Taken = (u64, Zeroizing<String>, String, Instant, CancellationToken);
    let taken: Taken = env
        .sessions
        .with_session(
            session_id,
            user_id,
            now,
            |s| -> Result<Taken, &'static str> {
                let id = s.oauth_flow_id().ok_or("no_oauth_flow")?;
                let (verifier, url, deadline) = s.paste_parts(id).ok_or("not_paste_mode")?;
                if s.oauth_phase() != Some(OauthPhase::WaitingForCode) {
                    return Err("oauth_not_waiting");
                }
                let cancel = s.oauth_cancel_token(id).ok_or("no_oauth_flow")?;
                // Claim the single submission before leaving the lock.
                s.set_oauth_phase(id, OauthPhase::Exchanging);
                Ok((id, verifier, url, deadline, cancel))
            },
        )
        .map_err(session_err)?
        .map_err(str::to_owned)?;
    let (flow_id, verifier, url, deadline, cancel) = taken;

    let trimmed = code.trim();
    let shaped = !trimmed.is_empty()
        && trimmed.len() <= MAX_CODE_BYTES
        && trimmed.chars().all(|c| c.is_ascii_graphic());
    if !shaped || now >= deadline {
        let reason = if shaped {
            "code_expired"
        } else {
            "invalid_code"
        };
        // A malformed paste is the user's typo: let them try again. An expired
        // code ends the flow.
        let phase = if shaped {
            OauthPhase::Failed(reason)
        } else {
            OauthPhase::WaitingForCode
        };
        set_phase(&env.sessions, session_id, user_id, flow_id, phase);
        return Err(reason.to_owned());
    }

    let result = tokio::select! {
        _ = cancel.cancelled() => return Ok(OauthStatus::from(Some(OauthPhase::Cancelled))),
        r = env.exchange.exchange(&url, trimmed, &verifier) => r,
    };
    finish(&env.sessions, session_id, user_id, flow_id, result);
    status(env, session_id, user_id)
}

/// The flow's phase as the dialog sees it. Paste mode's code expires here too.
pub fn status(env: &OauthEnv, session_id: &str, user_id: &str) -> Result<OauthStatus, String> {
    let now = Instant::now();
    env.sessions
        .with_session(session_id, user_id, now, |s| {
            if s.oauth_phase() == Some(OauthPhase::WaitingForCode) {
                if let Some(id) = s.oauth_flow_id() {
                    if s.paste_parts(id).is_some_and(|(_, _, d)| now >= d) {
                        s.set_oauth_phase(id, OauthPhase::Failed("code_expired"));
                    }
                }
            }
            OauthStatus::from(s.oauth_phase())
        })
        .map_err(session_err)
}

/// Stop the flow: the listener closes and a pending exchange is abandoned.
/// A key already received stays available to save.
pub fn cancel(env: &OauthEnv, session_id: &str, user_id: &str) -> Result<(), String> {
    env.sessions
        .with_session(session_id, user_id, Instant::now(), |s| s.cancel_oauth())
        .map_err(session_err)
}
