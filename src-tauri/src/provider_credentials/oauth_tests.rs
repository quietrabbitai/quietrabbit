// src-tauri/src/provider_credentials/oauth_tests.rs
//
// Tests for the OAuth PKCE connect (items.id=734) and the key-check path.
// Neutral fixtures; a fake browser, a loopback "provider" for the exchange and
// the key check, a raw TCP client standing in for the user's browser. No real
// provider, browser or OAuth call is ever made.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use zeroize::Zeroizing;

use super::oauth::callback::ServeConfig;
use super::oauth::exchange::{ExchangeClient, ExchangeError};
use super::oauth::flow::{self, OauthEnv};
use super::oauth::{OauthMode, OauthState, OauthStatus};
use super::session::{ConnectSessions, SESSION_TTL};
use super::tests::{
    baselined_provider, client, models, now_utc, session_for, stored, typed, with_store,
    FakeOpener, KEY_HEX, OK_LIST, USER,
};
use super::url_policy::{resolve_oauth, OauthUrl, UrlRefusal};
use crate::commands::provider_credentials::{begin_impl, save_key_impl, KeySource, SaveKeyRequest};
use crate::persistence::provider_store::Provider;
use crate::provider_credentials::flow::CheckState;
use crate::providers::hosted_call::CallContext;
use crate::providers::loopback_test_server::{Canned, LoopbackServer};

const ISSUED_KEY: &str = "fx_issuedkey0123456789abcdef";
const CODE: &str = "fx_approval_code_1";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// scheme://host of the baseline-anchored fixture row's keys page.
fn origin(p: &Provider) -> String {
    let keys = p.credential_acquisition.as_ref().unwrap()["keys_page_url"]
        .as_str()
        .unwrap()
        .to_owned();
    let u = reqwest::Url::parse(&keys).unwrap();
    format!("{}://{}", u.scheme(), u.host_str().unwrap())
}

/// An oauth_pkce row anchored to the baseline's host, whose inference base and
/// key check point at `check` (a loopback server) when given.
fn oauth_provider(check: Option<&LoopbackServer>) -> Provider {
    let mut p = baselined_provider();
    let o = origin(&p);
    let keys = p.credential_acquisition.as_ref().unwrap()["keys_page_url"].clone();
    p.credential_acquisition = Some(json!({
        "method": "oauth_pkce",
        "auth_url": format!("{o}/auth"),
        "exchange_url": format!("{o}/api/v1/auth/keys"),
        "key_pattern": null,
        "keys_page_url": keys,
        "key_check_path": "/key",
    }));
    p.base_url_template = Some(match check {
        Some(s) => format!("{}/v1", s.base()),
        None => "http://127.0.0.1:1/v1".to_owned(),
    });
    p.auth_scheme = Some("bearer".to_owned());
    p.extra_headers = Some(json!({}));
    p.user_variables = Some(json!([]));
    p
}

fn short_cfg() -> ServeConfig {
    ServeConfig {
        flow_ttl: Duration::from_secs(10),
        read_timeout: Duration::from_millis(300),
        max_invalid: 20,
    }
}

struct Rig {
    env: OauthEnv,
    opener: Arc<FakeOpener>,
    exchange: Arc<ExchangeClient>,
}

fn rig(sessions: &ConnectSessions, exchange_server: Option<&LoopbackServer>) -> Rig {
    rig_with(sessions, exchange_server, short_cfg(), false)
}

fn rig_with(
    sessions: &ConnectSessions,
    exchange_server: Option<&LoopbackServer>,
    cfg: ServeConfig,
    opener_fails: bool,
) -> Rig {
    let mut ex = ExchangeClient::for_loopback_tests();
    if let Some(s) = exchange_server {
        ex = ex.sending_to(&s.base());
    }
    let exchange = Arc::new(ex);
    let opener = Arc::new(FakeOpener::new(opener_fails));
    Rig {
        env: OauthEnv {
            sessions: sessions.clone(),
            opener: opener.clone(),
            exchange: exchange.clone(),
            cfg,
        },
        opener,
        exchange,
    }
}

fn key_reply() -> Canned {
    Canned::json(200, &json!({ "key": ISSUED_KEY }).to_string())
}

struct Opened {
    url: reqwest::Url,
}

impl Opened {
    fn of(rig: &Rig) -> Self {
        let opened = rig.opener.opened.lock().unwrap();
        assert_eq!(opened.len(), 1, "exactly one page opened");
        Self {
            url: reqwest::Url::parse(&opened[0]).unwrap(),
        }
    }
    fn param(&self, name: &str) -> Option<String> {
        self.url
            .query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    }
    fn callback_port(&self) -> u16 {
        reqwest::Url::parse(&self.param("callback_url").expect("callback_url"))
            .unwrap()
            .port()
            .unwrap()
    }
}

async fn browser_get(port: u16, path_and_query: &str) -> String {
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let req =
        format!("GET {path_and_query} HTTP/1.1\r\nHost: localhost:{port}\r\nUser-Agent: t\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

async fn wait_for(rig: &Rig, id: &str, want: OauthState) -> OauthStatus {
    let mut last = None;
    for _ in 0..120 {
        let st = flow::status(&rig.env, id, USER).unwrap();
        if st.state == want {
            return st;
        }
        last = Some(st);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never reached {want:?}; last {last:?}");
}

async fn port_closes(port: u16) -> bool {
    for _ in 0..60 {
        if TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .is_err()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn start_loopback(rig: &Rig, p: &Provider, id: &str) -> (Opened, u16, String) {
    let mode = flow::start(&rig.env, p, id, USER, OauthMode::Loopback)
        .await
        .unwrap();
    assert_eq!(mode, OauthMode::Loopback);
    let o = Opened::of(rig);
    let port = o.callback_port();
    let state = o.param("state").expect("state");
    (o, port, state)
}

// ---------------------------------------------------------------------------
// The URL policy for the two OAuth addresses
// ---------------------------------------------------------------------------

#[test]
fn both_oauth_addresses_resolve_when_they_sit_on_the_providers_own_host() {
    let p = oauth_provider(None);
    let o = origin(&p);
    assert_eq!(resolve_oauth(&p, OauthUrl::Auth), Ok(format!("{o}/auth")));
    assert_eq!(
        resolve_oauth(&p, OauthUrl::Exchange),
        Ok(format!("{o}/api/v1/auth/keys"))
    );
}

#[test]
fn an_unsafe_or_unanchored_oauth_address_is_refused() {
    let base = oauth_provider(None);
    let o = origin(&base);
    let host = o.trim_start_matches("https://").to_owned();
    let bad = [
        format!("http://{host}/api/v1/auth/keys"),
        format!("https://other.example.test/api/v1/auth/keys"),
        format!("https://{host}.evil.test/api/v1/auth/keys"),
        format!("https://user@{host}/api/v1/auth/keys"),
        format!("https://{host}:8443/api/v1/auth/keys"),
        format!("https://{host}/api/v1/auth/keys?code_challenge=x"),
        format!("https://{host}/api/v1/auth/keys#frag"),
        "https://127.0.0.1/api/v1/auth/keys".to_owned(),
        "https://localhost/api/v1/auth/keys".to_owned(),
        "javascript:alert(1)".to_owned(),
        String::new(),
    ];
    for raw in bad {
        let mut p = base.clone();
        p.credential_acquisition.as_mut().unwrap()["exchange_url"] = json!(raw);
        assert_eq!(
            resolve_oauth(&p, OauthUrl::Exchange),
            Err(UrlRefusal::Refused),
            "{raw}"
        );
        let mut p = base.clone();
        p.credential_acquisition.as_mut().unwrap()["auth_url"] = json!(raw);
        assert_eq!(
            resolve_oauth(&p, OauthUrl::Auth),
            Err(UrlRefusal::Refused),
            "{raw}"
        );
    }
    // Missing field is NoLink, not a refusal.
    let mut p = base.clone();
    p.credential_acquisition
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("exchange_url");
    assert_eq!(
        resolve_oauth(&p, OauthUrl::Exchange),
        Err(UrlRefusal::NoLink)
    );
    // A provider the baseline does not ship has no anchor: refused.
    let mut p = base;
    p.id = "not-in-the-baseline".to_owned();
    assert_eq!(resolve_oauth(&p, OauthUrl::Auth), Err(UrlRefusal::Refused));
}

// ---------------------------------------------------------------------------
// The code exchange
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_exchange_posts_the_code_and_verifier_and_returns_the_key() {
    let server = LoopbackServer::start(vec![key_reply()]);
    let c = ExchangeClient::for_loopback_tests();
    let key = c
        .exchange(
            &format!("{}/exchange", server.base()),
            CODE,
            "the_verifier_1",
        )
        .await
        .unwrap();
    assert_eq!(&*key, ISSUED_KEY);
    let sent = &server.captured()[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.path, "/exchange");
    assert_eq!(sent.header("content-type"), Some("application/json"));
    assert_eq!(sent.header("authorization"), None);
    assert_eq!(
        sent.json(),
        json!({"code": CODE, "code_verifier": "the_verifier_1", "code_challenge_method": "S256"})
    );
}

#[tokio::test]
async fn the_exchange_maps_every_failure_to_a_code_that_carries_no_secret() {
    let cases: Vec<(Canned, ExchangeError)> = vec![
        (Canned::json(400, "{}"), ExchangeError::CodeRejected),
        (Canned::json(403, "{}"), ExchangeError::CodeRejected),
        (Canned::json(404, "{}"), ExchangeError::CodeRejected),
        (Canned::json(429, "{}"), ExchangeError::RateLimited),
        (Canned::json(500, "{}"), ExchangeError::Failed),
        (Canned::json(200, "not json"), ExchangeError::Malformed),
        (Canned::json(200, "{}"), ExchangeError::Malformed),
        (Canned::json(200, r#"{"key":""}"#), ExchangeError::Malformed),
        (
            Canned::json(200, r#"{"key":"has a space"}"#),
            ExchangeError::Malformed,
        ),
        (
            Canned::json(200, r#"{"key":" padded "}"#),
            ExchangeError::Malformed,
        ),
        (Canned::json(200, r#"{"key":7}"#), ExchangeError::Malformed),
        (
            Canned::json(200, &format!(r#"{{"key":"{}"}}"#, "a".repeat(20 * 1024))),
            ExchangeError::Malformed,
        ),
    ];
    for (reply, want) in cases {
        let server = LoopbackServer::start(vec![reply]);
        let got = ExchangeClient::for_loopback_tests()
            .exchange(&format!("{}/x", server.base()), CODE, "verifier_x")
            .await;
        assert_eq!(got.err(), Some(want));
        let code = want.code();
        for secret in [CODE, "verifier_x", ISSUED_KEY] {
            assert!(!code.contains(secret));
        }
    }
    // Nothing listening.
    let got = ExchangeClient::for_loopback_tests()
        .exchange("http://127.0.0.1:1/x", CODE, "v")
        .await;
    assert_eq!(got.err(), Some(ExchangeError::Unreachable));
}

#[tokio::test]
async fn a_redirect_from_the_exchange_is_not_followed() {
    let target = LoopbackServer::start(vec![key_reply()]);
    let server = LoopbackServer::start(vec![
        Canned::json(302, "{}").with_header("Location", &format!("{}/steal", target.base()))
    ]);
    let got = ExchangeClient::for_loopback_tests()
        .exchange(&format!("{}/x", server.base()), CODE, "verifier_x")
        .await;
    assert_eq!(got.err(), Some(ExchangeError::Failed));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(target.connections(), 0, "the redirect target saw nothing");
}

#[tokio::test]
async fn the_production_exchange_client_will_not_speak_plain_http() {
    let server = LoopbackServer::start(vec![key_reply()]);
    let got = ExchangeClient::new()
        .exchange(&format!("{}/x", server.base()), CODE, "verifier_x")
        .await;
    assert_eq!(got.err(), Some(ExchangeError::Unreachable));
    assert_eq!(server.connections(), 0, "no request left the process");
}

// ---------------------------------------------------------------------------
// The whole loopback flow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_loopback_connect_ends_with_the_key_held_and_saved_by_the_existing_path() {
    with_store(|mut conn| async move {
        let exchange = LoopbackServer::start(vec![key_reply()]);
        let check = LoopbackServer::start(vec![Canned::json(200, r#"{"data":{"label":"x"}}"#)]);
        let p = oauth_provider(Some(&check));
        let sessions = ConnectSessions::default();
        let r = rig(&sessions, Some(&exchange));
        let id = session_for(&sessions, &p);

        let (opened, port, state) = start_loopback(&r, &p, &id).await;

        // What the browser was sent to.
        let o = origin(&p);
        assert_eq!(
            opened.url.as_str().split('?').next(),
            Some(format!("{o}/auth").as_str())
        );
        assert_eq!(
            opened.param("callback_url"),
            Some(format!("http://localhost:{port}/callback"))
        );
        assert_eq!(
            opened.param("code_challenge_method").as_deref(),
            Some("S256")
        );
        let challenge = opened.param("code_challenge").unwrap();
        assert_eq!(challenge.len(), 43);
        assert!(URL_SAFE_NO_PAD.decode(&state).unwrap().len() * 8 >= 128);
        assert_eq!(opened.param("key_label").as_deref(), Some("Quiet Rabbit"));
        assert_eq!(
            flow::status(&r.env, &id, USER).unwrap().state,
            OauthState::WaitingForBrowser
        );

        // The user approves; the provider sends the browser back.
        let page = browser_get(port, &format!("/callback?code={CODE}&state={state}")).await;
        assert!(page.starts_with("HTTP/1.1 200 "));
        assert!(!page.contains(CODE));
        let st = wait_for(&r, &id, OauthState::KeyReady).await;
        assert_eq!(st.key_length as usize, ISSUED_KEY.len());
        assert_eq!(st.failure, None);
        let wire = serde_json::to_string(&st).unwrap();
        assert!(!wire.contains(ISSUED_KEY) && !wire.contains(CODE));
        assert!(
            port_closes(port).await,
            "the listener closed after the callback"
        );

        // What the exchange was sent: the code, the verifier whose S256 is the
        // challenge the browser got, and no credential. It was asked for the
        // policy-checked https address on the provider's host.
        assert_eq!(r.exchange.asked(), vec![format!("{o}/api/v1/auth/keys")]);
        let sent = &exchange.captured()[0];
        assert_eq!(sent.path, "/api/v1/auth/keys");
        assert_eq!(sent.header("authorization"), None);
        let body = sent.json();
        assert_eq!(body["code"], CODE);
        assert_eq!(body["code_challenge_method"], "S256");
        let verifier = body["code_verifier"].as_str().unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            challenge
        );

        // Nothing is stored until the user's click; then the existing path
        // checks the key at its key-check path and stores it as an api_key.
        assert!(stored(&p.id).await.is_none());
        let out = save_key_impl(
            &sessions,
            &client(),
            &mut conn,
            &p,
            &models(),
            SaveKeyRequest {
                session_id: id.clone(),
                source: KeySource::SessionCandidate,
                variables: Default::default(),
                save_unverified: false,
            },
            USER,
            CallContext::build_for_loopback,
            Instant::now(),
            now_utc(),
        )
        .await
        .unwrap();
        assert!(out.verified);
        assert_eq!(out.check, CheckState::Verified);
        let sent = &check.captured()[0];
        assert_eq!(
            sent.path, "/v1/key",
            "the key-check path, not the model list"
        );
        assert_eq!(
            sent.header("authorization"),
            Some(format!("Bearer {ISSUED_KEY}").as_str())
        );
        let key = stored(&p.id).await.expect("stored");
        assert_eq!(key.credential, ISSUED_KEY);
        assert_eq!(key.auth_type.as_deref(), Some("api_key"));
        assert!(key.last_verified_at.is_some());
        assert_eq!(KEY_HEX.len(), 64);

        // The candidate did its job: a second save has nothing to take.
        let again = save_key_impl(
            &sessions,
            &client(),
            &mut conn,
            &p,
            &models(),
            SaveKeyRequest {
                session_id: id,
                source: KeySource::SessionCandidate,
                variables: Default::default(),
                save_unverified: false,
            },
            USER,
            CallContext::build_for_loopback,
            Instant::now(),
            now_utc(),
        )
        .await;
        assert_eq!(again.err().as_deref(), Some("no_candidate"));
    })
    .await;
}

#[tokio::test]
async fn requests_that_are_not_the_callback_never_reach_the_exchange() {
    let exchange = LoopbackServer::start(vec![key_reply()]);
    let p = oauth_provider(None);
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, Some(&exchange));
    let id = session_for(&sessions, &p);
    let (_o, port, _state) = start_loopback(&r, &p, &id).await;

    for path in [
        format!("/callback?code={CODE}&state=forged"),
        format!("/callback?code={CODE}"),
        format!("/other?code={CODE}"),
    ] {
        let page = browser_get(port, &path).await;
        assert!(!page.starts_with("HTTP/1.1 200 "), "{path}");
        assert!(!page.contains(CODE));
    }
    assert_eq!(exchange.connections(), 0);
    assert_eq!(
        flow::status(&r.env, &id, USER).unwrap().state,
        OauthState::WaitingForBrowser,
        "still waiting for the real callback"
    );
    flow::cancel(&r.env, &id, USER).unwrap();
    assert!(port_closes(port).await);
}

#[tokio::test]
async fn a_rejected_code_or_a_redirecting_exchange_fails_the_flow_without_a_key() {
    for (reply, want) in [
        (Canned::json(400, "{}"), "code_rejected"),
        (Canned::json(500, "{}"), "exchange_failed"),
        (Canned::json(200, "garbage"), "exchange_malformed"),
    ] {
        let exchange = LoopbackServer::start(vec![reply]);
        let p = oauth_provider(None);
        let sessions = ConnectSessions::default();
        let r = rig(&sessions, Some(&exchange));
        let id = session_for(&sessions, &p);
        let (_o, port, state) = start_loopback(&r, &p, &id).await;
        browser_get(port, &format!("/callback?code={CODE}&state={state}")).await;
        let st = wait_for(&r, &id, OauthState::Failed).await;
        assert_eq!(st.failure.as_deref(), Some(want));
        assert_eq!(st.key_length, 0);
        let held = sessions
            .with_session(&id, USER, Instant::now(), |s| s.has_candidate())
            .unwrap();
        assert!(!held, "no key was kept");
    }
}

#[tokio::test]
async fn a_callback_that_never_comes_ends_the_flow_and_closes_the_port() {
    let p = oauth_provider(None);
    let sessions = ConnectSessions::default();
    let cfg = ServeConfig {
        flow_ttl: Duration::from_millis(300),
        ..short_cfg()
    };
    let r = rig_with(&sessions, None, cfg, false);
    let id = session_for(&sessions, &p);
    let (_o, port, _state) = start_loopback(&r, &p, &id).await;
    let st = wait_for(&r, &id, OauthState::Failed).await;
    assert_eq!(st.failure.as_deref(), Some("callback_timeout"));
    assert!(port_closes(port).await);
}

#[tokio::test]
async fn cancel_stops_the_flow_and_closes_the_port() {
    let p = oauth_provider(None);
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, None);
    let id = session_for(&sessions, &p);
    let (_o, port, state) = start_loopback(&r, &p, &id).await;
    flow::cancel(&r.env, &id, USER).unwrap();
    assert_eq!(
        flow::status(&r.env, &id, USER).unwrap().state,
        OauthState::Cancelled
    );
    assert!(port_closes(port).await);
    // A late callback finds nothing listening.
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .is_err());
    let _ = state;
}

#[tokio::test]
async fn ending_or_clearing_the_session_closes_the_port() {
    let p = oauth_provider(None);

    // end_provider_connect
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, None);
    let id = session_for(&sessions, &p);
    let (_o, port, _s) = start_loopback(&r, &p, &id).await;
    sessions.end(&id, USER);
    assert!(port_closes(port).await, "end");

    // logout and the idle lock both call clear()
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, None);
    let id = session_for(&sessions, &p);
    let (_o, port, _s) = start_loopback(&r, &p, &id).await;
    sessions.clear();
    assert!(port_closes(port).await, "clear");

    // expiry
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, None);
    let t0 = Instant::now();
    let id = begin_impl(&sessions, &p, USER, t0).unwrap().session_id;
    let (_o, port, _s) = start_loopback(&r, &p, &id).await;
    assert!(sessions
        .with_session(&id, USER, t0 + SESSION_TTL, |_| ())
        .is_err());
    assert!(port_closes(port).await, "expiry");

    // a newer session replaces the older one
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, None);
    let id = session_for(&sessions, &p);
    let (_o, port, _s) = start_loopback(&r, &p, &id).await;
    let _second = session_for(&sessions, &p);
    assert!(port_closes(port).await, "replaced");
}

#[tokio::test]
async fn a_second_flow_cancels_the_first() {
    let p = oauth_provider(None);
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, None);
    let id = session_for(&sessions, &p);
    let (_o, first_port, _s) = start_loopback(&r, &p, &id).await;
    r.opener.opened.lock().unwrap().clear();
    let (_o2, second_port, _s2) = start_loopback(&r, &p, &id).await;
    assert_ne!(first_port, second_port);
    assert!(port_closes(first_port).await);
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, second_port))
        .await
        .is_ok());
    flow::cancel(&r.env, &id, USER).unwrap();
}

#[tokio::test]
async fn nobody_else_can_start_read_or_cancel_the_flow() {
    let p = oauth_provider(None);
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, None);
    let id = session_for(&sessions, &p);
    let (_o, port, _s) = start_loopback(&r, &p, &id).await;
    assert_eq!(
        flow::status(&r.env, &id, "someone-else").err().as_deref(),
        Some("no_session")
    );
    assert_eq!(
        flow::cancel(&r.env, &id, "someone-else").err().as_deref(),
        Some("no_session")
    );
    assert_eq!(
        flow::start(&r.env, &p, &id, "someone-else", OauthMode::Loopback)
            .await
            .err()
            .as_deref(),
        Some("no_session")
    );
    assert_eq!(
        flow::submit_code(&r.env, &id, "someone-else", Zeroizing::new("c".into()))
            .await
            .err()
            .as_deref(),
        Some("no_session")
    );
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .is_ok());
    flow::cancel(&r.env, &id, USER).unwrap();
}

#[tokio::test]
async fn a_key_that_arrives_after_the_session_ended_is_dropped() {
    let exchange = LoopbackServer::start(vec![key_reply().with_delay(Duration::from_millis(400))]);
    let p = oauth_provider(None);
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, Some(&exchange));
    let id = session_for(&sessions, &p);
    let (_o, port, state) = start_loopback(&r, &p, &id).await;
    browser_get(port, &format!("/callback?code={CODE}&state={state}")).await;
    wait_for(&r, &id, OauthState::Exchanging).await;
    sessions.end(&id, USER);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(sessions.is_empty());
    assert_eq!(
        flow::status(&r.env, &id, USER).err().as_deref(),
        Some("no_session")
    );
}

#[tokio::test]
async fn nothing_is_opened_or_bound_when_the_row_cannot_back_the_flow() {
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, None);

    // Not an oauth row.
    let mut p = oauth_provider(None);
    p.credential_acquisition.as_mut().unwrap()["method"] = json!("guided_paste");
    let id = session_for(&sessions, &p);
    assert_eq!(
        flow::start(&r.env, &p, &id, USER, OauthMode::Loopback)
            .await
            .err()
            .as_deref(),
        Some("not_oauth")
    );

    // No exchange address; an exchange address on another host; an auth page
    // on another host.
    let cases = [
        ("exchange_url", None, "no_exchange_url"),
        (
            "exchange_url",
            Some("https://other.example.test/x"),
            "link_refused",
        ),
        (
            "auth_url",
            Some("https://other.example.test/auth"),
            "link_refused",
        ),
        (
            "auth_url",
            Some("http://insecure.example.test/auth"),
            "link_refused",
        ),
    ];
    for (field, value, want) in cases {
        let mut p = oauth_provider(None);
        let acq = p
            .credential_acquisition
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap();
        match value {
            Some(v) => {
                acq.insert(field.to_owned(), json!(v));
            }
            None => {
                acq.remove(field);
            }
        }
        let id = session_for(&sessions, &p);
        assert_eq!(
            flow::start(&r.env, &p, &id, USER, OauthMode::Loopback)
                .await
                .err()
                .as_deref(),
            Some(want),
            "{field}"
        );
        assert_eq!(
            flow::status(&r.env, &id, USER).unwrap().state,
            OauthState::Idle,
            "no flow was registered"
        );
    }
    assert!(
        r.opener.opened.lock().unwrap().is_empty(),
        "no browser opened"
    );
}

#[tokio::test]
async fn a_browser_that_cannot_be_opened_ends_the_flow() {
    let p = oauth_provider(None);
    let sessions = ConnectSessions::default();
    let r = rig_with(&sessions, None, short_cfg(), true);
    let id = session_for(&sessions, &p);
    let err = flow::start(&r.env, &p, &id, USER, OauthMode::Loopback).await;
    assert_eq!(err.err().as_deref(), Some("open_failed"));
    let url = r.opener.opened.lock().unwrap()[0].clone();
    let port: u16 = reqwest::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "callback_url")
        .map(|(_, v)| reqwest::Url::parse(&v).unwrap().port().unwrap())
        .unwrap();
    assert!(port_closes(port).await);
    assert_eq!(
        flow::status(&r.env, &id, USER).unwrap().state,
        OauthState::Cancelled
    );
}

// ---------------------------------------------------------------------------
// Paste (headless) mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn paste_mode_has_no_listener_and_exchanges_the_pasted_code() {
    let exchange = LoopbackServer::start(vec![key_reply()]);
    let p = oauth_provider(None);
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, Some(&exchange));
    let id = session_for(&sessions, &p);

    let mode = flow::start(&r.env, &p, &id, USER, OauthMode::Paste)
        .await
        .unwrap();
    assert_eq!(mode, OauthMode::Paste);
    let o = Opened::of(&r);
    assert_eq!(o.param("callback_url"), None, "headless: no callback");
    assert_eq!(o.param("state"), None, "headless: the provider echoes none");
    let challenge = o.param("code_challenge").unwrap();
    assert_eq!(o.param("code_challenge_method").as_deref(), Some("S256"));
    assert_eq!(
        flow::status(&r.env, &id, USER).unwrap().state,
        OauthState::WaitingForCode
    );

    // A mistyped paste is refused locally and may be retried.
    for bad in ["", "two words", "a\nb"] {
        let e = flow::submit_code(&r.env, &id, USER, Zeroizing::new(bad.to_owned())).await;
        assert_eq!(e.err().as_deref(), Some("invalid_code"), "{bad:?}");
    }
    assert_eq!(exchange.connections(), 0);
    assert_eq!(
        flow::status(&r.env, &id, USER).unwrap().state,
        OauthState::WaitingForCode
    );

    let st = flow::submit_code(&r.env, &id, USER, Zeroizing::new(format!("  {CODE}\n")))
        .await
        .unwrap();
    assert_eq!(st.state, OauthState::KeyReady);
    assert_eq!(st.key_length as usize, ISSUED_KEY.len());
    let body = exchange.captured()[0].json();
    assert_eq!(body["code"], CODE);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(body["code_verifier"].as_str().unwrap())),
        challenge
    );

    // The single submission is spent (the verifier went with it).
    let again = flow::submit_code(&r.env, &id, USER, Zeroizing::new(CODE.to_owned())).await;
    assert_eq!(again.err().as_deref(), Some("not_paste_mode"));
    assert_eq!(exchange.connections(), 1);
}

#[tokio::test]
async fn paste_mode_failures_and_expiry_are_codes() {
    let exchange = LoopbackServer::start(vec![Canned::json(400, "{}")]);
    let p = oauth_provider(None);
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, Some(&exchange));
    let id = session_for(&sessions, &p);
    flow::start(&r.env, &p, &id, USER, OauthMode::Paste)
        .await
        .unwrap();
    let st = flow::submit_code(&r.env, &id, USER, Zeroizing::new(CODE.to_owned()))
        .await
        .unwrap();
    assert_eq!(st.state, OauthState::Failed);
    assert_eq!(st.failure.as_deref(), Some("code_rejected"));

    // The code lives ten minutes: an unsent code expires.
    let sessions = ConnectSessions::default();
    let cfg = ServeConfig {
        flow_ttl: Duration::from_millis(100),
        ..short_cfg()
    };
    let r = rig_with(&sessions, Some(&exchange), cfg, false);
    let id = session_for(&sessions, &p);
    flow::start(&r.env, &p, &id, USER, OauthMode::Paste)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let st = flow::status(&r.env, &id, USER).unwrap();
    assert_eq!(st.failure.as_deref(), Some("code_expired"));
}

#[tokio::test]
async fn submit_code_needs_a_paste_flow() {
    let p = oauth_provider(None);
    let sessions = ConnectSessions::default();
    let r = rig(&sessions, None);
    let id = session_for(&sessions, &p);
    let none = flow::submit_code(&r.env, &id, USER, Zeroizing::new(CODE.to_owned())).await;
    assert_eq!(none.err().as_deref(), Some("no_oauth_flow"));
    let (_o, _port, _s) = start_loopback(&r, &p, &id).await;
    let wrong = flow::submit_code(&r.env, &id, USER, Zeroizing::new(CODE.to_owned())).await;
    assert_eq!(wrong.err().as_deref(), Some("not_paste_mode"));
    flow::cancel(&r.env, &id, USER).unwrap();
}

// ---------------------------------------------------------------------------
// The key-check path (Chat-PM ruling at the items.id=734 plan review)
// ---------------------------------------------------------------------------

async fn save_with(
    sessions: &ConnectSessions,
    conn: &mut sqlx::SqliteConnection,
    p: &Provider,
    id: &str,
    key: &str,
) -> Result<crate::commands::provider_credentials::SaveOutcome, String> {
    save_key_impl(
        sessions,
        &client(),
        conn,
        p,
        &models(),
        typed(id, key, &[], false),
        USER,
        CallContext::build_for_loopback,
        Instant::now(),
        now_utc(),
    )
    .await
}

#[tokio::test]
async fn a_row_with_a_key_check_path_is_checked_there_and_a_bad_key_is_rejected() {
    with_store(|mut conn| async move {
        // The model list would accept anything (it is public); the key check
        // path is the one that says no.
        let check = LoopbackServer::start(vec![Canned::json(401, r#"{"error":"no"}"#)]);
        let p = oauth_provider(Some(&check));
        let sessions = ConnectSessions::default();
        let id = session_for(&sessions, &p);
        let e = save_with(&sessions, &mut conn, &p, &id, ISSUED_KEY).await;
        assert_eq!(e.err().as_deref(), Some("invalid_api_key"));
        assert_eq!(check.captured().len(), 1);
        assert_eq!(check.captured()[0].path, "/v1/key");
        assert!(
            stored(&p.id).await.is_none(),
            "a rejected key is never stored"
        );

        // Unsaved-anyway does not override a rejection.
        let again = save_key_impl(
            &sessions,
            &client(),
            &mut conn,
            &p,
            &models(),
            typed(&id, ISSUED_KEY, &[], true),
            USER,
            CallContext::build_for_loopback,
            Instant::now(),
            now_utc(),
        )
        .await;
        assert_eq!(again.err().as_deref(), Some("invalid_api_key"));
    })
    .await;
}

#[tokio::test]
async fn a_good_key_verifies_against_the_key_check_path() {
    with_store(|mut conn| async move {
        let check = LoopbackServer::start(vec![Canned::json(200, r#"{"data":{}}"#)]);
        let p = oauth_provider(Some(&check));
        let sessions = ConnectSessions::default();
        let id = session_for(&sessions, &p);
        let out = save_with(&sessions, &mut conn, &p, &id, ISSUED_KEY)
            .await
            .unwrap();
        assert_eq!(out.check, CheckState::Verified);
        assert_eq!(check.captured()[0].path, "/v1/key");
        assert!(stored(&p.id).await.unwrap().last_verified_at.is_some());
    })
    .await;
}

#[tokio::test]
async fn a_row_with_a_key_check_path_and_no_model_list_is_still_checkable() {
    with_store(|mut conn| async move {
        let check = LoopbackServer::start(vec![Canned::json(200, "{}")]);
        let mut p = oauth_provider(Some(&check));
        p.models_path = None;
        let sessions = ConnectSessions::default();
        let info = begin_impl(&sessions, &p, USER, Instant::now()).unwrap();
        assert!(info.key_check_available);
        let out = save_with(&sessions, &mut conn, &p, &info.session_id, ISSUED_KEY)
            .await
            .unwrap();
        assert_eq!(out.check, CheckState::Verified);
    })
    .await;
}

#[tokio::test]
async fn a_row_without_a_key_check_path_is_checked_against_its_model_list_as_before() {
    with_store(|mut conn| async move {
        let check = LoopbackServer::start(vec![Canned::json(200, OK_LIST)]);
        let mut p = oauth_provider(Some(&check));
        p.credential_acquisition
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("key_check_path");
        p.models_path = Some("/models".to_owned());
        let sessions = ConnectSessions::default();
        let id = session_for(&sessions, &p);
        let out = save_with(&sessions, &mut conn, &p, &id, ISSUED_KEY)
            .await
            .unwrap();
        assert_eq!(out.check, CheckState::Verified);
        assert_eq!(check.captured()[0].path, "/v1/models");

        // And with neither, the key is saved unchecked with no call at all.
        let quiet = LoopbackServer::start(vec![Canned::json(200, "{}")]);
        let mut q = oauth_provider(Some(&quiet));
        q.credential_acquisition
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("key_check_path");
        q.models_path = None;
        let id = session_for(&sessions, &q);
        let out = save_with(&sessions, &mut conn, &q, &id, ISSUED_KEY)
            .await
            .unwrap();
        assert_eq!(out.check, CheckState::Unavailable);
        assert_eq!(quiet.connections(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_key_check_path_that_is_not_a_plain_path_is_refused_before_any_call() {
    with_store(|mut conn| async move {
        for bad in ["key", "/../key", "//key", "/key?x=1", "/k ey"] {
            let check = LoopbackServer::start(vec![Canned::json(200, "{}")]);
            let mut p = oauth_provider(Some(&check));
            p.credential_acquisition.as_mut().unwrap()["key_check_path"] = json!(bad);
            let sessions = ConnectSessions::default();
            let id = session_for(&sessions, &p);
            let e = save_with(&sessions, &mut conn, &p, &id, ISSUED_KEY).await;
            assert_eq!(e.err().as_deref(), Some("misconfigured"), "{bad}");
            assert_eq!(check.connections(), 0, "{bad}");
        }
    })
    .await;
}

// ---------------------------------------------------------------------------
// Source scans
// ---------------------------------------------------------------------------

fn oauth_sources() -> Vec<(String, String)> {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/provider_credentials/oauth");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let src = std::fs::read_to_string(&path).unwrap();
        let cut = src
            .match_indices("#[cfg(test)]")
            .find(|(i, tag)| {
                let rest = src[i + tag.len()..].trim_start();
                rest.starts_with("mod ")
            })
            .map_or(src.len(), |(i, _)| i);
        out.push((
            path.file_name().unwrap().to_string_lossy().into_owned(),
            src[..cut].to_owned(),
        ));
    }
    assert!(out.len() >= 5);
    out
}

#[test]
fn the_oauth_code_never_logs_or_prints_a_secret() {
    let secrets = [
        "code",
        "verifier",
        "state",
        "key",
        "token",
        "challenge",
        "url",
    ];
    let mut checked = 0;
    for (file, src) in oauth_sources() {
        for line in src.lines() {
            let code = line.split("//").next().unwrap_or("");
            let prints = ["log::", "println!", "eprintln!", "dbg!", "tracing::"]
                .iter()
                .any(|m| code.contains(m));
            if prints {
                checked += 1;
                for s in secrets {
                    assert!(
                        !code.to_ascii_lowercase().contains(s),
                        "{file}: a print/log line mentions {s}: {code}"
                    );
                }
            }
        }
    }
    assert!(checked > 0, "the scan saw the log lines it guards");
}

#[test]
fn the_oauth_listener_is_bound_in_exactly_one_place() {
    let mut binds = Vec::new();
    for (file, src) in oauth_sources() {
        for line in src.lines() {
            if line.split("//").next().unwrap_or("").contains("::bind(") {
                binds.push(file.clone());
            }
        }
    }
    assert_eq!(
        binds,
        vec!["callback.rs".to_owned(), "callback.rs".to_owned()]
    );
    // Nowhere else in the credential code binds anything.
    for f in ["flow.rs", "session.rs", "url_policy.rs", "opener.rs"] {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/provider_credentials")
                .join(f),
        )
        .unwrap();
        assert!(!src.contains("::bind("), "{f}");
    }
    // The bind addresses are loopback, never a wildcard.
    let callback = oauth_sources()
        .into_iter()
        .find(|(f, _)| f == "callback.rs")
        .unwrap()
        .1;
    for wild in ["UNSPECIFIED", "0.0.0.0", "\"::\""] {
        assert!(!callback.contains(wild), "{wild}");
    }
}

#[test]
fn oauth_commands_take_no_url_and_return_no_key() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/commands/provider_credentials.rs"),
    )
    .unwrap();
    for name in [
        "start_oauth_connect",
        "submit_oauth_code",
        "get_oauth_connect_status",
        "cancel_oauth_connect",
    ] {
        let start = src
            .find(&format!("pub async fn {name}("))
            .unwrap_or_else(|| panic!("{name}"));
        let end = src[start..].find(") -> Result").unwrap() + start;
        let signature = src[start..end].to_ascii_lowercase();
        assert!(!signature.contains("url"), "{name}: {signature}");
    }
    let status = serde_json::to_value(OauthStatus::from(None)).unwrap();
    assert_eq!(
        status
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        ["failure", "key_length", "state"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    );
    let _: Value = status;
}
