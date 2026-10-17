// src-tauri/src/commands/provider_credentials.rs
//
// Group 28 -- hosted-provider credential acquisition (items.id=714,
// decisions.id=875 point 7, spec Part 8g). Part (a), five commands that back
// the "connect" dialog:
//   begin_provider_connect, open_provider_page, check_clipboard_for_key,
//   save_provider_key, end_provider_connect.
// Part (b), items.id=734, four commands for the OpenRouter OAuth PKCE connect:
//   start_oauth_connect, submit_oauth_code, get_oauth_connect_status,
//   cancel_oauth_connect.
// The key an OAuth connect obtains is held in the session and saved by the
// same save_provider_key (source SessionCandidate); no command returns it.
//
// CONTRACT NOTES (the frontend's contract; the handoff repeats them):
//   - Every statement is a stable CODE or a short string; no prose here.
//   - The frontend never sends a URL: it names a session and a `ProviderPage`.
//   - No credential, clipboard character or variable value is ever returned.
//     The clipboard offer carries a state and a length only.
//   - Nothing here runs by itself. open_provider_page launches a browser,
//     check_clipboard_for_key reads the clipboard (and only that command
//     does), save_provider_key sends the user's key to its own provider once;
//     each is a user click inside an open session.
//   - The key typed or offered is write-only: it goes in, never comes out.
//
// The `*_impl` functions hold the behavior and take their collaborators as
// parameters so the tests use a fake browser, a fake clipboard and a
// loopback provider; the #[tauri::command] wrappers only resolve state.

use std::collections::BTreeMap;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use sqlx::SqliteConnection;
use tauri::State;
use zeroize::Zeroizing;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::commands::provider_spend::internal;
use crate::conductor::executor::hosted_client;
use crate::persistence::integration_keys_store::open_integration_keys_db;
use crate::persistence::provider_store::{self, Provider, ProviderModel};
use crate::provider_credentials::clipboard::{ClipboardSource, PluginClipboard};
use crate::provider_credentials::flow::{save_impl, BuildFn, CheckState, SaveParams};
use crate::provider_credentials::key_pattern::{match_clipboard, pattern_is_usable};
use crate::provider_credentials::oauth::callback::ServeConfig;
use crate::provider_credentials::oauth::flow::{self as oauth_flow, exchange_client, OauthEnv};
use crate::provider_credentials::oauth::{OauthMode, OauthStatus};
use crate::provider_credentials::opener::{SystemOpener, UrlOpener};
use crate::provider_credentials::session::{ConnectSessions, SessionError};
use crate::provider_credentials::url_policy::{self, Page, UrlRefusal};
use crate::provider_credentials::{acquisition, declared_variables, is_hosted};
use crate::providers::hosted_call::CallContext;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// A provider page the dialog can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum ProviderPage {
    /// Where the provider issues API keys.
    Keys,
    /// The provider's own spend-limit page (a local cap cannot stop a leaked key).
    Spend,
}

impl From<ProviderPage> for Page {
    fn from(p: ProviderPage) -> Self {
        match p {
            ProviderPage::Keys => Page::Keys,
            ProviderPage::Spend => Page::Spend,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct ProviderConnectInfo {
    pub session_id: String,
    /// `credential_acquisition.method` from the row (`guided_paste`,
    /// `oauth_pkce`, ...). Pasting a key works whatever it says.
    pub method: Option<String>,
    /// Names of the per-user variables the row needs (for example
    /// `account_id`); the dialog must collect each.
    pub user_variables: Vec<String>,
    pub keys_page_available: bool,
    pub spend_limit_available: bool,
    /// The row carries a usable key pattern, so the clipboard can be offered.
    pub clipboard_assist_available: bool,
    /// The row has a model list or a key-check path, so a key can be checked.
    pub key_check_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum ClipboardState {
    /// The clipboard holds one line matching the key pattern. The text is held
    /// in memory for `save_provider_key`; it is not returned.
    Matches,
    NoMatch,
    /// The clipboard is empty, not text, or could not be read.
    Unavailable,
    /// The row has no usable key pattern; the clipboard was NOT read.
    NoPattern,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct ClipboardOffer {
    pub state: ClipboardState,
    /// Characters in the matched text; 0 unless `Matches`.
    pub length: u32,
}

/// Where the key to save comes from. Never derives Debug: it can hold a key.
#[derive(Deserialize, Type)]
pub enum KeySource {
    /// The user typed or pasted it into the dialog.
    Typed { api_key: String },
    /// The text `check_clipboard_for_key` matched and the user then confirmed.
    ClipboardCandidate,
    /// The key the session already holds: an OAuth connect's result
    /// (`get_oauth_connect_status` says `KeyReady`), or a confirmed clipboard
    /// key. Reads the same slot as `ClipboardCandidate`.
    SessionCandidate,
}

#[derive(Deserialize, Type)]
pub struct SaveKeyRequest {
    pub session_id: String,
    pub source: KeySource,
    /// Values for the row's `user_variables`, by name.
    pub variables: BTreeMap<String, String>,
    /// Save even if the provider could not be reached to check the key. Never
    /// overrides a key the provider rejected.
    pub save_unverified: bool,
}

impl std::fmt::Debug for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Typed { .. } => f.write_str("KeySource::Typed([redacted])"),
            Self::ClipboardCandidate => f.write_str("KeySource::ClipboardCandidate"),
            Self::SessionCandidate => f.write_str("KeySource::SessionCandidate"),
        }
    }
}

impl std::fmt::Debug for SaveKeyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SaveKeyRequest")
            .field("session_id", &"[not printed]")
            .field("source", &self.source)
            .field("variables", &"[not printed]")
            .field("save_unverified", &self.save_unverified)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct SaveOutcome {
    pub verified: bool,
    pub check: CheckState,
}

// ---------------------------------------------------------------------------
// Shared lookups
// ---------------------------------------------------------------------------

fn session_code(e: SessionError) -> String {
    match e {
        SessionError::NoSession => "no_session",
        SessionError::Expired => "session_expired",
    }
    .to_owned()
}

async fn signed_in_user(key_registry: &KeyRegistry) -> Result<String, String> {
    key_registry
        .with_key(|k| k.user_id.clone())
        .await
        .ok_or_else(|| "not logged in".to_owned())
}

async fn load_provider(pool: &sqlx::SqlitePool, id: &str) -> Result<Provider, String> {
    provider_store::get_provider(pool, id)
        .await
        .map_err(|e| internal("provider lookup", e))?
        .filter(is_hosted)
        .ok_or_else(|| "unknown_provider".to_owned())
}

// ---------------------------------------------------------------------------
// begin_provider_connect
// ---------------------------------------------------------------------------

pub(crate) fn begin_impl(
    sessions: &ConnectSessions,
    provider: &Provider,
    user_id: &str,
    now: Instant,
) -> Result<ProviderConnectInfo, String> {
    if !is_hosted(provider) {
        return Err("unknown_provider".to_owned());
    }
    let user_variables = declared_variables(provider).ok_or("misconfigured")?;
    let acq = acquisition(provider);
    let acq_key_check = acq.key_check_path.as_deref();
    let session_id = sessions
        .begin(user_id, &provider.id, now)
        .map_err(|e| internal("session id", e))?;
    Ok(ProviderConnectInfo {
        session_id,
        method: acq.method,
        user_variables,
        keys_page_available: url_policy::resolve(provider, Page::Keys).is_ok(),
        spend_limit_available: url_policy::resolve(provider, Page::Spend).is_ok(),
        clipboard_assist_available: acq.key_pattern.as_deref().is_some_and(pattern_is_usable),
        key_check_available: provider.models_path.is_some() || acq_key_check.is_some(),
    })
}

/// Open the connect dialog's session for a hosted provider. Writes nothing and
/// contacts nothing.
#[tauri::command]
#[specta::specta]
pub async fn begin_provider_connect(
    provider_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    sessions: State<'_, ConnectSessions>,
) -> Result<ProviderConnectInfo, String> {
    let user_id = signed_in_user(&key_registry).await?;
    let provider = load_provider(&pool, &provider_id).await?;
    begin_impl(&sessions, &provider, &user_id, Instant::now())
}

// ---------------------------------------------------------------------------
// open_provider_page
// ---------------------------------------------------------------------------

/// Resolve the URL for `page` inside a live session; nothing is opened here.
pub(crate) fn prepare_open_impl(
    sessions: &ConnectSessions,
    provider: &Provider,
    session_id: &str,
    user_id: &str,
    page: ProviderPage,
    now: Instant,
) -> Result<String, String> {
    sessions
        .with_session(session_id, user_id, now, |s| s.provider_id.clone())
        .map_err(session_code)
        .and_then(|pid| {
            (pid == provider.id)
                .then_some(())
                .ok_or_else(|| "no_session".to_owned())
        })?;
    url_policy::resolve(provider, page.into()).map_err(|e| match e {
        UrlRefusal::NoLink => "no_link".to_owned(),
        UrlRefusal::Refused => "link_refused".to_owned(),
    })
}

pub(crate) fn open_with(opener: &dyn UrlOpener, url: &str) -> Result<(), String> {
    opener.open(url).map_err(|e| {
        log::warn!("provider_credentials: {e}");
        "open_failed".to_owned()
    })
}

/// Open the provider's keys page or spend-limit page in the default browser.
/// The URL comes from the provider row and must pass the URL policy.
#[tauri::command]
#[specta::specta]
pub async fn open_provider_page(
    session_id: String,
    page: ProviderPage,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    sessions: State<'_, ConnectSessions>,
) -> Result<(), String> {
    let user_id = signed_in_user(&key_registry).await?;
    let provider_id = sessions
        .with_session(&session_id, &user_id, Instant::now(), |s| {
            s.provider_id.clone()
        })
        .map_err(session_code)?;
    let provider = load_provider(&pool, &provider_id).await?;
    let url = prepare_open_impl(
        &sessions,
        &provider,
        &session_id,
        &user_id,
        page,
        Instant::now(),
    )?;
    tokio::task::spawn_blocking(move || open_with(&SystemOpener, &url))
        .await
        .map_err(|e| internal("open task", e))?
}

// ---------------------------------------------------------------------------
// check_clipboard_for_key
// ---------------------------------------------------------------------------

pub(crate) fn check_clipboard_impl(
    sessions: &ConnectSessions,
    clipboard: &dyn ClipboardSource,
    provider: &Provider,
    session_id: &str,
    user_id: &str,
    now: Instant,
) -> Result<ClipboardOffer, String> {
    // The session check comes first: with no open dialog the clipboard is
    // never touched.
    sessions
        .with_session(session_id, user_id, now, |s| s.provider_id == provider.id)
        .map_err(session_code)?
        .then_some(())
        .ok_or_else(|| "no_session".to_owned())?;

    let offer = |state, length| ClipboardOffer { state, length };
    let pattern = acquisition(provider)
        .key_pattern
        .filter(|p| pattern_is_usable(p));
    let Some(pattern) = pattern else {
        // No usable pattern: nothing may be offered, so nothing is read.
        return Ok(offer(ClipboardState::NoPattern, 0));
    };
    let _ = sessions.with_session(session_id, user_id, now, |s| s.clear_candidate());

    let Some(text) = clipboard.read_text() else {
        return Ok(offer(ClipboardState::Unavailable, 0));
    };
    match match_clipboard(&pattern, &text) {
        Ok(Some(candidate)) => {
            let length = u32::try_from(candidate.chars().count()).unwrap_or(u32::MAX);
            sessions
                .with_session(session_id, user_id, now, |s| s.set_candidate(candidate))
                .map_err(session_code)?;
            Ok(offer(ClipboardState::Matches, length))
        }
        Ok(None) => Ok(offer(ClipboardState::NoMatch, 0)),
        Err(_) => Ok(offer(ClipboardState::NoPattern, 0)),
    }
}

/// Read the clipboard once, on the user's click, and say whether it holds a
/// key the row's pattern recognises. Returns a state and a length only.
#[tauri::command]
#[specta::specta]
pub async fn check_clipboard_for_key(
    session_id: String,
    app: tauri::AppHandle,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    sessions: State<'_, ConnectSessions>,
) -> Result<ClipboardOffer, String> {
    let user_id = signed_in_user(&key_registry).await?;
    let provider_id = sessions
        .with_session(&session_id, &user_id, Instant::now(), |s| {
            s.provider_id.clone()
        })
        .map_err(session_code)?;
    let provider = load_provider(&pool, &provider_id).await?;
    check_clipboard_impl(
        &sessions,
        &PluginClipboard(app),
        &provider,
        &session_id,
        &user_id,
        Instant::now(),
    )
}

// ---------------------------------------------------------------------------
// save_provider_key
// ---------------------------------------------------------------------------

/// Take the key out of the request and the session. The typed key moves into a
/// zeroizing buffer; a clipboard candidate is copied out of the session (it is
/// cleared only after a successful save).
pub(crate) fn take_key(
    sessions: &ConnectSessions,
    session_id: &str,
    user_id: &str,
    source: KeySource,
    now: Instant,
) -> Result<(Zeroizing<String>, String), String> {
    match source {
        KeySource::Typed { api_key } => {
            let key = Zeroizing::new(api_key);
            let pid = sessions
                .with_session(session_id, user_id, now, |s| s.provider_id.clone())
                .map_err(session_code)?;
            Ok((key, pid))
        }
        KeySource::ClipboardCandidate | KeySource::SessionCandidate => sessions
            .with_session(session_id, user_id, now, |s| {
                s.candidate()
                    .map(|c| (Zeroizing::new(c.to_owned()), s.provider_id.clone()))
            })
            .map_err(session_code)?
            .ok_or_else(|| "no_candidate".to_owned()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn save_key_impl(
    sessions: &ConnectSessions,
    client: &crate::providers::hosted_client::HostedClient,
    conn: &mut SqliteConnection,
    provider: &Provider,
    models: &[ProviderModel],
    req: SaveKeyRequest,
    user_id: &str,
    build: BuildFn,
    now: Instant,
    now_utc: DateTime<Utc>,
) -> Result<SaveOutcome, String> {
    let SaveKeyRequest {
        session_id,
        source,
        variables,
        save_unverified,
    } = req;
    let (key, session_provider) = take_key(sessions, &session_id, user_id, source, now)?;
    if session_provider != provider.id {
        return Err("no_session".to_owned());
    }
    let state = save_impl(
        client,
        conn,
        &SaveParams {
            provider,
            models,
            key: &key,
            variables: &variables,
            save_unverified,
        },
        build,
        now_utc,
    )
    .await?;
    // The key is stored; a clipboard candidate has done its job.
    let _ = sessions.with_session(&session_id, user_id, now, |s| s.clear_candidate());
    Ok(SaveOutcome {
        verified: state == CheckState::Verified,
        check: state,
    })
}

/// Check the key with its provider, then store it for the signed-in user.
/// The key goes in and never comes out.
#[tauri::command]
#[specta::specta]
pub async fn save_provider_key(
    request: SaveKeyRequest,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    sessions: State<'_, ConnectSessions>,
) -> Result<SaveOutcome, String> {
    let (user_id, key_hex_str) = key_registry
        .with_key(|k| (k.user_id.clone(), key_hex(&k.data_key)))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;
    let provider_id = sessions
        .with_session(&request.session_id, &user_id, Instant::now(), |s| {
            s.provider_id.clone()
        })
        .map_err(session_code)?;
    let provider = load_provider(&pool, &provider_id).await?;
    let models = provider_store::list_models_for_provider(&pool, &provider_id)
        .await
        .map_err(|e| internal("model lookup", e))?;
    let mut conn = open_integration_keys_db(&user_id, &key_hex_str)
        .await
        .map_err(|e| internal("open key store", e))?;
    save_key_impl(
        &sessions,
        hosted_client(),
        &mut conn,
        &provider,
        &models,
        request,
        &user_id,
        CallContext::build,
        Instant::now(),
        Utc::now(),
    )
    .await
}

// ---------------------------------------------------------------------------
// end_provider_connect
// ---------------------------------------------------------------------------

/// Close the dialog's session and drop any clipboard text it held. Ending an
/// unknown session is a no-op.
#[tauri::command]
#[specta::specta]
pub async fn end_provider_connect(
    session_id: String,
    key_registry: State<'_, KeyRegistry>,
    sessions: State<'_, ConnectSessions>,
) -> Result<(), String> {
    let user_id = signed_in_user(&key_registry).await?;
    sessions.end(&session_id, &user_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// OAuth connect (items.id=734)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub struct OauthStarted {
    /// The mode actually used. `Paste` when `Loopback` was asked for and no
    /// loopback listener could be bound.
    pub mode: OauthMode,
}

fn oauth_env(sessions: &ConnectSessions) -> OauthEnv {
    OauthEnv {
        sessions: sessions.clone(),
        opener: std::sync::Arc::new(SystemOpener),
        exchange: exchange_client(),
        cfg: ServeConfig::default(),
    }
}

async fn session_provider(
    key_registry: &KeyRegistry,
    pool: &sqlx::SqlitePool,
    sessions: &ConnectSessions,
    session_id: &str,
) -> Result<(String, Provider), String> {
    let user_id = signed_in_user(key_registry).await?;
    let provider_id = sessions
        .with_session(session_id, &user_id, Instant::now(), |s| {
            s.provider_id.clone()
        })
        .map_err(session_code)?;
    let provider = load_provider(pool, &provider_id).await?;
    Ok((user_id, provider))
}

/// Open the provider's approval page for an OAuth connect and start listening
/// for the answer (or, in paste mode, wait for the code). A user click inside
/// an open session; the page address comes from the provider row and the URL
/// policy, never from the frontend.
#[tauri::command]
#[specta::specta]
pub async fn start_oauth_connect(
    session_id: String,
    mode: OauthMode,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    sessions: State<'_, ConnectSessions>,
) -> Result<OauthStarted, String> {
    let (user_id, provider) =
        session_provider(&key_registry, &pool, &sessions, &session_id).await?;
    let used = oauth_flow::start(
        &oauth_env(&sessions),
        &provider,
        &session_id,
        &user_id,
        mode,
    )
    .await?;
    Ok(OauthStarted { mode: used })
}

/// Paste mode: exchange the one-time code the provider showed. The code is
/// write-only; the answer is a phase and, on success, the key's length.
#[tauri::command]
#[specta::specta]
pub async fn submit_oauth_code(
    session_id: String,
    code: String,
    key_registry: State<'_, KeyRegistry>,
    sessions: State<'_, ConnectSessions>,
) -> Result<OauthStatus, String> {
    let code = Zeroizing::new(code);
    let user_id = signed_in_user(&key_registry).await?;
    oauth_flow::submit_code(&oauth_env(&sessions), &session_id, &user_id, code).await
}

/// Where the OAuth connect has got to. A phase, a length and a failure code;
/// never the key.
#[tauri::command]
#[specta::specta]
pub async fn get_oauth_connect_status(
    session_id: String,
    key_registry: State<'_, KeyRegistry>,
    sessions: State<'_, ConnectSessions>,
) -> Result<OauthStatus, String> {
    let user_id = signed_in_user(&key_registry).await?;
    oauth_flow::status(&oauth_env(&sessions), &session_id, &user_id)
}

/// Stop the OAuth connect: the listener closes and a pending exchange is
/// abandoned. A key that already arrived stays available to save.
#[tauri::command]
#[specta::specta]
pub async fn cancel_oauth_connect(
    session_id: String,
    key_registry: State<'_, KeyRegistry>,
    sessions: State<'_, ConnectSessions>,
) -> Result<(), String> {
    let user_id = signed_in_user(&key_registry).await?;
    oauth_flow::cancel(&oauth_env(&sessions), &session_id, &user_id)
}
