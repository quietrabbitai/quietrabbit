// src-tauri/src/provider_credentials/tests.rs
//
// Tests for credential acquisition, part (a) (items.id=714). Neutral fixtures;
// a fake browser, a fake clipboard, a real encrypted key store and a loopback
// "provider". No real provider, browser or clipboard is ever used.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use chrono::{TimeZone, Utc};
use serde_json::{json, Value};
use zeroize::Zeroizing;

use crate::commands::provider_credentials::{
    begin_impl, check_clipboard_impl, open_with, prepare_open_impl, save_key_impl, ClipboardState,
    KeySource, ProviderPage, SaveKeyRequest,
};
use crate::persistence::integration_keys_store::{
    get_active_key, open_integration_keys_db, QR_HOSTED_KEY_TYPE,
};
use crate::persistence::provider_store::{Provider, ProviderModel};
use crate::provider_catalog::BASELINE_CATALOG_JSON;
use crate::provider_credentials::clipboard::ClipboardSource;
use crate::provider_credentials::flow::CheckState;
use crate::provider_credentials::opener::UrlOpener;
use crate::provider_credentials::session::{ConnectSessions, SESSION_TTL};
use crate::providers::hosted_call::CallContext;
use crate::providers::hosted_client::HostedClient;
use crate::providers::loopback_test_server::{Canned, LoopbackServer};
use crate::spend::fixtures::{model, provider};

const KEY_HEX: &str = "0606060606060606060606060606060606060606060606060606060606060606";
const USER: &str = "cred-user";
const SECRET: &str = "fx_secretkey0123456789abcdef";
const PATTERN: &str = r"fx_[A-Za-z0-9]{8,64}";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A provider row whose keys and spend pages are the baseline catalog's own,
/// so the URL policy has a shipped host to anchor to. Derived from the
/// baseline file, not named here (decisions.id=872).
fn baselined_provider() -> Provider {
    let doc: Value = serde_json::from_str(BASELINE_CATALOG_JSON).unwrap();
    let row = doc["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["keys_page_url"].is_string() && p["spend_limit_url"].is_string())
        .expect("the baseline has a provider with both pages");
    let mut p = provider();
    p.id = row["id"].as_str().unwrap().to_owned();
    p.spend_limit_url = row["spend_limit_url"].as_str().map(str::to_owned);
    p.credential_acquisition = Some(json!({
        "method": "guided_paste",
        "auth_url": null,
        "key_pattern": PATTERN,
        "keys_page_url": row["keys_page_url"],
    }));
    p.models_path = Some("/models".to_owned());
    p
}

fn loopback_provider(server: Option<&LoopbackServer>, variables: &[&str]) -> Provider {
    let mut p = provider();
    p.models_path = server.map(|_| "/models".to_owned());
    let base = server.map_or("http://127.0.0.1:1".to_owned(), LoopbackServer::base);
    let template = if variables.is_empty() {
        format!("{base}/v1")
    } else {
        format!("{base}/acct/{{account_id}}/v1")
    };
    p.base_url_template = Some(template);
    p.auth_scheme = Some("bearer".to_owned());
    p.extra_headers = Some(json!({}));
    p.user_variables = Some(json!(variables));
    p.credential_acquisition = Some(json!({"method": "guided_paste", "key_pattern": PATTERN}));
    p
}

fn models() -> Vec<ProviderModel> {
    vec![model("fixture-model-a")]
}

fn client() -> HostedClient {
    HostedClient::for_loopback_tests(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(2),
        1024 * 1024,
    )
}

fn now_utc() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

struct FakeOpener {
    opened: Mutex<Vec<String>>,
    fail: bool,
}

impl FakeOpener {
    fn new(fail: bool) -> Self {
        Self {
            opened: Mutex::new(Vec::new()),
            fail,
        }
    }
}

impl UrlOpener for FakeOpener {
    fn open(&self, url: &str) -> Result<(), String> {
        self.opened.lock().unwrap().push(url.to_owned());
        if self.fail {
            Err("could not open the browser: NotFound".to_owned())
        } else {
            Ok(())
        }
    }
}

struct FakeClipboard {
    text: Option<String>,
    reads: AtomicUsize,
}

impl FakeClipboard {
    fn new(text: Option<&str>) -> Self {
        Self {
            text: text.map(str::to_owned),
            reads: AtomicUsize::new(0),
        }
    }
    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl ClipboardSource for FakeClipboard {
    fn read_text(&self) -> Option<Zeroizing<String>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.text.clone().map(Zeroizing::new)
    }
}

async fn with_store<F, Fut>(f: F)
where
    F: FnOnce(sqlx::SqliteConnection) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _lock = crate::test_support::ENV_MUTEX.lock().await;
    let saved_root = std::env::var("QR_DATA_ROOT").ok();
    let tempdir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("QR_DATA_ROOT", tempdir.path());
    let conn = open_integration_keys_db(USER, KEY_HEX).await.expect("open");
    f(conn).await;
    match saved_root {
        Some(v) => std::env::set_var("QR_DATA_ROOT", v),
        None => std::env::remove_var("QR_DATA_ROOT"),
    }
}

async fn stored(
    provider_id: &str,
) -> Option<crate::persistence::integration_keys_store::IntegrationKey> {
    get_active_key(USER, KEY_HEX, provider_id, QR_HOSTED_KEY_TYPE, None)
        .await
        .unwrap()
}

fn typed(
    session_id: &str,
    key: &str,
    vars: &[(&str, &str)],
    save_unverified: bool,
) -> SaveKeyRequest {
    SaveKeyRequest {
        session_id: session_id.to_owned(),
        source: KeySource::Typed {
            api_key: key.to_owned(),
        },
        variables: vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<BTreeMap<_, _>>(),
        save_unverified,
    }
}

async fn save(
    sessions: &ConnectSessions,
    conn: &mut sqlx::SqliteConnection,
    p: &Provider,
    req: SaveKeyRequest,
) -> Result<crate::commands::provider_credentials::SaveOutcome, String> {
    save_key_impl(
        sessions,
        &client(),
        conn,
        p,
        &models(),
        req,
        USER,
        CallContext::build_for_loopback,
        Instant::now(),
        now_utc(),
    )
    .await
}

const OK_LIST: &str = r#"{"data":[{"id":"fixture-model-a"}]}"#;

// ---------------------------------------------------------------------------
// begin_provider_connect
// ---------------------------------------------------------------------------

#[test]
fn begin_reports_what_the_dialog_can_offer() {
    let sessions = ConnectSessions::default();
    let p = baselined_provider();
    let info = begin_impl(&sessions, &p, USER, Instant::now()).unwrap();
    assert_eq!(info.session_id.len(), 32);
    assert_eq!(info.method.as_deref(), Some("guided_paste"));
    assert!(info.keys_page_available && info.spend_limit_available);
    assert!(info.clipboard_assist_available && info.key_check_available);
    assert!(info.user_variables.is_empty());
}

#[test]
fn begin_hides_what_the_row_cannot_back() {
    let sessions = ConnectSessions::default();
    // No spend page, no pattern, no model list, and a declared variable.
    let mut p = baselined_provider();
    p.spend_limit_url = None;
    p.models_path = None;
    p.user_variables = Some(json!(["account_id"]));
    p.credential_acquisition = Some(json!({
        "method": "guided_paste",
        "key_pattern": null,
        "keys_page_url": p.credential_acquisition.as_ref().unwrap()["keys_page_url"],
    }));
    let info = begin_impl(&sessions, &p, USER, Instant::now()).unwrap();
    assert!(info.keys_page_available);
    assert!(!info.spend_limit_available);
    assert!(!info.clipboard_assist_available);
    assert!(!info.key_check_available);
    assert_eq!(info.user_variables, vec!["account_id".to_owned()]);

    // An unusable pattern also disables the offer.
    p.credential_acquisition = Some(json!({"method": "guided_paste", "key_pattern": "("}));
    let info = begin_impl(&sessions, &p, USER, Instant::now()).unwrap();
    assert!(!info.clipboard_assist_available);
    assert!(!info.keys_page_available, "no keys page in the row");
}

#[test]
fn begin_refuses_a_row_that_is_not_hosted_or_is_malformed() {
    let sessions = ConnectSessions::default();
    let mut local = provider();
    local.provider_type = "local_model".to_owned();
    assert_eq!(
        begin_impl(&sessions, &local, USER, Instant::now()).unwrap_err(),
        "unknown_provider"
    );
    let mut bad = provider();
    bad.user_variables = Some(json!("account_id"));
    assert_eq!(
        begin_impl(&sessions, &bad, USER, Instant::now()).unwrap_err(),
        "misconfigured"
    );
}

// ---------------------------------------------------------------------------
// open_provider_page
// ---------------------------------------------------------------------------

#[test]
fn a_page_opens_only_inside_a_live_session_for_the_same_provider() {
    let sessions = ConnectSessions::default();
    let p = baselined_provider();
    let t0 = Instant::now();
    let id = begin_impl(&sessions, &p, USER, t0).unwrap().session_id;

    let url = prepare_open_impl(&sessions, &p, &id, USER, ProviderPage::Keys, t0).unwrap();
    assert!(url.starts_with("https://"));
    assert!(prepare_open_impl(&sessions, &p, &id, USER, ProviderPage::Spend, t0).is_ok());

    let go = |sid: &str, user: &str, p: &Provider, now: Instant| {
        prepare_open_impl(&sessions, p, sid, user, ProviderPage::Keys, now)
    };
    assert_eq!(go("nope", USER, &p, t0).unwrap_err(), "no_session");
    assert_eq!(go(&id, "someone-else", &p, t0).unwrap_err(), "no_session");
    let mut other = p.clone();
    other.id = "another-provider".to_owned();
    assert_eq!(go(&id, USER, &other, t0).unwrap_err(), "no_session");
    assert_eq!(
        go(&id, USER, &p, t0 + SESSION_TTL).unwrap_err(),
        "session_expired"
    );
    assert_eq!(
        go(&id, USER, &p, t0).unwrap_err(),
        "no_session",
        "expiry dropped it"
    );
}

#[test]
fn a_missing_or_unsafe_row_url_is_never_opened() {
    let sessions = ConnectSessions::default();
    let t0 = Instant::now();
    let mut p = baselined_provider();
    let id = begin_impl(&sessions, &p, USER, t0).unwrap().session_id;
    let try_page = |p: &Provider, page| prepare_open_impl(&sessions, p, &id, USER, page, t0);

    // NULL spend page.
    let good_spend = p.spend_limit_url.clone();
    p.spend_limit_url = None;
    assert_eq!(try_page(&p, ProviderPage::Spend).unwrap_err(), "no_link");

    // Every unsafe or unanchored variant is refused, not opened.
    for bad in [
        "http://console.example.test/keys",
        "https://user:pw@console.example.test/keys",
        "https://127.0.0.1/keys",
        "https://localhost/keys",
        "https://console.example.test:8443/keys",
        "https://not-the-shipped-host.example.test/keys",
        "javascript:alert(1)",
    ] {
        p.spend_limit_url = Some(bad.to_owned());
        assert_eq!(
            try_page(&p, ProviderPage::Spend).unwrap_err(),
            "link_refused",
            "{bad}"
        );
    }
    // A provider the baseline does not know has no shipped host to anchor to.
    p.spend_limit_url = good_spend;
    p.id = "fixture-not-in-baseline".to_owned();
    let id2 = begin_impl(&sessions, &p, USER, t0).unwrap().session_id;
    assert_eq!(
        prepare_open_impl(&sessions, &p, &id2, USER, ProviderPage::Spend, t0).unwrap_err(),
        "link_refused"
    );
}

#[test]
fn the_opener_gets_exactly_the_checked_url_and_failure_is_a_code() {
    let fake = FakeOpener::new(false);
    open_with(&fake, "https://console.example.test/keys").unwrap();
    assert_eq!(
        fake.opened.lock().unwrap().as_slice(),
        ["https://console.example.test/keys"]
    );

    let failing = FakeOpener::new(true);
    assert_eq!(
        open_with(&failing, "https://console.example.test/keys").unwrap_err(),
        "open_failed"
    );
}

// ---------------------------------------------------------------------------
// check_clipboard_for_key
// ---------------------------------------------------------------------------

fn session_for(sessions: &ConnectSessions, p: &Provider) -> String {
    begin_impl(sessions, p, USER, Instant::now())
        .unwrap()
        .session_id
}

#[test]
fn the_clipboard_is_not_read_without_a_live_session() {
    let sessions = ConnectSessions::default();
    let p = baselined_provider();
    let t0 = Instant::now();
    let id = begin_impl(&sessions, &p, USER, t0).unwrap().session_id;
    let clip = FakeClipboard::new(Some(SECRET));

    for (sid, user, now) in [
        ("nope", USER, t0),
        (id.as_str(), "someone-else", t0),
        (id.as_str(), USER, t0 + SESSION_TTL),
    ] {
        assert!(check_clipboard_impl(&sessions, &clip, &p, sid, user, now).is_err());
    }
    assert_eq!(clip.reads(), 0, "no session, no read");
}

#[test]
fn the_clipboard_is_not_read_when_the_row_has_no_usable_pattern() {
    let sessions = ConnectSessions::default();
    for pattern in [json!(null), json!("("), json!("")] {
        let mut p = baselined_provider();
        p.credential_acquisition = Some(json!({"method": "guided_paste", "key_pattern": pattern}));
        let id = session_for(&sessions, &p);
        let clip = FakeClipboard::new(Some(SECRET));
        let offer = check_clipboard_impl(&sessions, &clip, &p, &id, USER, Instant::now()).unwrap();
        assert_eq!(offer.state, ClipboardState::NoPattern);
        assert_eq!(offer.length, 0);
        assert_eq!(
            clip.reads(),
            0,
            "no pattern, nothing may be offered, so nothing is read"
        );
    }
}

#[test]
fn a_matching_clipboard_is_held_but_never_returned() {
    let sessions = ConnectSessions::default();
    let p = baselined_provider();
    let id = session_for(&sessions, &p);
    let clip = FakeClipboard::new(Some(&format!("{SECRET}\n")));
    let offer = check_clipboard_impl(&sessions, &clip, &p, &id, USER, Instant::now()).unwrap();
    assert_eq!(offer.state, ClipboardState::Matches);
    assert_eq!(offer.length as usize, SECRET.len());
    assert_eq!(clip.reads(), 1);

    let wire = serde_json::to_string(&offer).unwrap();
    assert!(!wire.contains(SECRET) && !wire.contains("fx_"), "{wire}");
    assert!(!format!("{offer:?}").contains(SECRET));

    // Held for the explicit second click.
    let held = sessions
        .with_session(&id, USER, Instant::now(), |s| {
            s.candidate().map(str::to_owned)
        })
        .unwrap();
    assert_eq!(held.as_deref(), Some(SECRET));
}

#[test]
fn a_non_matching_or_missing_clipboard_offers_nothing_and_drops_an_earlier_candidate() {
    let sessions = ConnectSessions::default();
    let p = baselined_provider();
    let id = session_for(&sessions, &p);
    let now = Instant::now();
    let hit = FakeClipboard::new(Some(SECRET));
    assert_eq!(
        check_clipboard_impl(&sessions, &hit, &p, &id, USER, now)
            .unwrap()
            .state,
        ClipboardState::Matches
    );
    for (text, expect) in [
        (Some("not a key"), ClipboardState::NoMatch),
        (Some("fx_abc\nfx_abcdefgh"), ClipboardState::NoMatch),
        (Some("fx_abcdefgh extra"), ClipboardState::NoMatch),
        (None, ClipboardState::Unavailable),
    ] {
        // Put a candidate back, then check what the new read does to it.
        let _ = check_clipboard_impl(&sessions, &hit, &p, &id, USER, now);
        let clip = FakeClipboard::new(text);
        let offer = check_clipboard_impl(&sessions, &clip, &p, &id, USER, now).unwrap();
        assert_eq!(offer.state, expect, "{text:?}");
        assert_eq!(offer.length, 0);
        let has = sessions
            .with_session(&id, USER, now, |s| s.has_candidate())
            .unwrap();
        assert!(
            !has,
            "a failed check must not leave the old candidate behind"
        );
    }
}

// ---------------------------------------------------------------------------
// save_provider_key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_accepted_key_is_stored_with_its_variable_and_verification() {
    with_store(|mut conn| async move {
        let server = LoopbackServer::start(vec![Canned::json(200, OK_LIST)]);
        let p = loopback_provider(Some(&server), &["account_id"]);
        let sessions = ConnectSessions::default();
        let id = session_for(&sessions, &p);

        let out = save(
            &sessions,
            &mut conn,
            &p,
            typed(
                &id,
                &format!("  {SECRET}\n"),
                &[("account_id", "abc-123")],
                false,
            ),
        )
        .await
        .unwrap();
        assert!(out.verified);
        assert_eq!(out.check, CheckState::Verified);

        // The variable reached the URL; the key reached the header; nothing else did.
        let sent = &server.captured()[0];
        assert_eq!(sent.method, "GET");
        assert_eq!(sent.path, "/acct/abc-123/v1/models");
        assert_eq!(
            sent.header("authorization"),
            Some(format!("Bearer {SECRET}").as_str())
        );
        assert!(sent.body.is_empty());

        let key = stored(&p.id).await.expect("stored");
        assert_eq!(key.credential, SECRET, "stored trimmed");
        assert_eq!(key.auth_type.as_deref(), Some("api_key"));
        assert_eq!(key.extra_metadata, json!({"account_id": "abc-123"}));
        assert_eq!(
            key.last_verified_at.as_deref(),
            Some("2026-10-14T12:00:00+00:00")
        );

        // The stored variable round-trips into a call context for a real call.
        let route = crate::providers::hosted_call::HostedRoute::from_rows(&p, &models()[0]);
        assert!(
            CallContext::build_for_loopback(&route, &key.credential, &key.extra_metadata).is_ok()
        );

        let wire = serde_json::to_string(&out).unwrap();
        assert!(!wire.contains(SECRET));
    })
    .await;
}

#[tokio::test]
async fn a_rejected_key_is_never_stored_even_when_the_user_chose_to_save_anyway() {
    with_store(|mut conn| async move {
        let server = LoopbackServer::start(vec![Canned::json(401, r#"{"error":"nope"}"#)]);
        let p = loopback_provider(Some(&server), &[]);
        let sessions = ConnectSessions::default();
        let id = session_for(&sessions, &p);
        for save_unverified in [false, true] {
            let err = save(
                &sessions,
                &mut conn,
                &p,
                typed(&id, SECRET, &[], save_unverified),
            )
            .await
            .unwrap_err();
            assert_eq!(err, "invalid_api_key");
            assert!(!err.contains(SECRET));
        }
        assert!(stored(&p.id).await.is_none());
    })
    .await;
}

#[tokio::test]
async fn an_unreachable_check_stores_nothing_unless_the_user_confirms() {
    with_store(|mut conn| async move {
        let server = LoopbackServer::start(vec![Canned::json(503, "{}")]);
        let p = loopback_provider(Some(&server), &[]);
        let sessions = ConnectSessions::default();
        let id = session_for(&sessions, &p);

        let err = save(&sessions, &mut conn, &p, typed(&id, SECRET, &[], false))
            .await
            .unwrap_err();
        assert_eq!(err, "could_not_verify");
        assert!(
            stored(&p.id).await.is_none(),
            "nothing stored without the second confirm"
        );

        let out = save(&sessions, &mut conn, &p, typed(&id, SECRET, &[], true))
            .await
            .unwrap();
        assert!(!out.verified);
        assert_eq!(out.check, CheckState::Inconclusive);
        let key = stored(&p.id).await.expect("stored after confirm");
        assert_eq!(key.credential, SECRET);
        assert_eq!(key.last_verified_at, None);
    })
    .await;
}

#[tokio::test]
async fn a_row_with_no_model_list_is_stored_unverified_with_no_network_call() {
    with_store(|mut conn| async move {
        let server = LoopbackServer::start(vec![Canned::json(200, OK_LIST)]);
        let mut p = loopback_provider(Some(&server), &["account_id"]);
        p.models_path = None;
        let sessions = ConnectSessions::default();
        let id = session_for(&sessions, &p);
        let out = save(
            &sessions,
            &mut conn,
            &p,
            typed(&id, SECRET, &[("account_id", "acct_9")], false),
        )
        .await
        .unwrap();
        assert_eq!(out.check, CheckState::Unavailable);
        assert!(!out.verified);
        assert_eq!(server.connections(), 0);
        let key = stored(&p.id).await.unwrap();
        assert_eq!(key.extra_metadata, json!({"account_id": "acct_9"}));
        assert_eq!(key.last_verified_at, None);
    })
    .await;
}

#[tokio::test]
async fn bad_input_is_refused_before_anything_is_sent_or_stored() {
    with_store(|mut conn| async move {
        let server = LoopbackServer::start(vec![Canned::json(200, OK_LIST)]);
        let p = loopback_provider(Some(&server), &["account_id"]);
        let sessions = ConnectSessions::default();
        let id = session_for(&sessions, &p);
        let good = [("account_id", "abc-123")];

        let cases: Vec<(SaveKeyRequest, &str)> = vec![
            (typed(&id, "", &good, false), "invalid_key_syntax"),
            (typed(&id, "two words", &good, false), "invalid_key_syntax"),
            (
                typed(&id, "line1\nline2", &good, false),
                "invalid_key_syntax",
            ),
            (
                typed(&id, &"k".repeat(600), &good, false),
                "invalid_key_syntax",
            ),
            (typed(&id, SECRET, &[], false), "variable_missing"),
            (
                typed(&id, SECRET, &[("account_id", "a/b")], false),
                "variable_invalid",
            ),
            (
                typed(&id, SECRET, &[("account_id", "..")], false),
                "variable_invalid",
            ),
            (
                typed(&id, SECRET, &[("account_id", "x.y")], false),
                "variable_invalid",
            ),
            (
                typed(&id, SECRET, &[("account_id", "")], false),
                "variable_invalid",
            ),
            (
                typed(&id, SECRET, &[("account_id", "a"), ("extra", "b")], false),
                "variable_undeclared",
            ),
            (typed("no-such-session", SECRET, &good, false), "no_session"),
        ];
        for (req, code) in cases {
            let shown = format!("{req:?}");
            assert_eq!(
                save(&sessions, &mut conn, &p, req).await.unwrap_err(),
                code,
                "{shown}"
            );
        }
        assert_eq!(server.connections(), 0, "bad input reaches no provider");
        assert!(stored(&p.id).await.is_none());
    })
    .await;
}

#[tokio::test]
async fn a_row_that_declares_no_variables_refuses_one() {
    with_store(|mut conn| async move {
        let p = loopback_provider(None, &[]);
        let sessions = ConnectSessions::default();
        let id = session_for(&sessions, &p);
        let err = save(
            &sessions,
            &mut conn,
            &p,
            typed(&id, SECRET, &[("account_id", "a")], false),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "variable_undeclared");
    })
    .await;
}

#[tokio::test]
async fn a_clipboard_candidate_is_saved_once_and_then_cleared() {
    with_store(|mut conn| async move {
        let server = LoopbackServer::start(vec![Canned::json(200, OK_LIST)]);
        let p = loopback_provider(Some(&server), &[]);
        let sessions = ConnectSessions::default();
        let id = session_for(&sessions, &p);
        let from_clipboard = |id: &str| SaveKeyRequest {
            session_id: id.to_owned(),
            source: KeySource::ClipboardCandidate,
            variables: BTreeMap::new(),
            save_unverified: false,
        };

        // No candidate yet.
        assert_eq!(
            save(&sessions, &mut conn, &p, from_clipboard(&id))
                .await
                .unwrap_err(),
            "no_candidate"
        );

        let clip = FakeClipboard::new(Some(SECRET));
        let offer = check_clipboard_impl(&sessions, &clip, &p, &id, USER, Instant::now()).unwrap();
        assert_eq!(offer.state, ClipboardState::Matches);

        let out = save(&sessions, &mut conn, &p, from_clipboard(&id))
            .await
            .unwrap();
        assert!(out.verified);
        assert_eq!(stored(&p.id).await.unwrap().credential, SECRET);

        // Used up: a second save has nothing to save.
        assert_eq!(
            save(&sessions, &mut conn, &p, from_clipboard(&id))
                .await
                .unwrap_err(),
            "no_candidate"
        );
    })
    .await;
}

#[tokio::test]
async fn a_failed_save_keeps_the_candidate_for_a_retry() {
    with_store(|mut conn| async move {
        let server =
            LoopbackServer::start(vec![Canned::json(503, "{}"), Canned::json(200, OK_LIST)]);
        let p = loopback_provider(Some(&server), &[]);
        let sessions = ConnectSessions::default();
        let id = session_for(&sessions, &p);
        let clip = FakeClipboard::new(Some(SECRET));
        check_clipboard_impl(&sessions, &clip, &p, &id, USER, Instant::now()).unwrap();
        let from_clipboard = SaveKeyRequest {
            session_id: id.clone(),
            source: KeySource::ClipboardCandidate,
            variables: BTreeMap::new(),
            save_unverified: false,
        };
        assert_eq!(
            save(&sessions, &mut conn, &p, from_clipboard)
                .await
                .unwrap_err(),
            "could_not_verify"
        );
        let retry = SaveKeyRequest {
            session_id: id,
            source: KeySource::ClipboardCandidate,
            variables: BTreeMap::new(),
            save_unverified: false,
        };
        assert!(
            save(&sessions, &mut conn, &p, retry)
                .await
                .unwrap()
                .verified
        );
    })
    .await;
}

#[test]
fn request_types_never_print_a_key() {
    let req = typed(
        "session-id-value",
        SECRET,
        &[("account_id", "acct-private")],
        false,
    );
    for shown in [
        format!("{req:?}"),
        format!("{req:#?}"),
        format!("{:?}", Some(&req)),
    ] {
        assert!(!shown.contains(SECRET), "{shown}");
        assert!(!shown.contains("acct-private"), "{shown}");
        assert!(!shown.contains("session-id-value"), "{shown}");
    }
    let src = KeySource::Typed {
        api_key: SECRET.to_owned(),
    };
    assert!(!format!("{src:?}").contains(SECRET));
}

// ---------------------------------------------------------------------------
// The shipped rows (shared.db after every migration)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_shipped_hosted_row_can_connect_by_paste_and_its_links_pass_the_policy() {
    use crate::provider_credentials::url_policy::{self, Page};
    let _lock = crate::test_support::ENV_MUTEX.lock().await;
    let saved_root = std::env::var("QR_DATA_ROOT").ok();
    let tempdir = tempfile::tempdir().unwrap();
    std::env::set_var("QR_DATA_ROOT", tempdir.path());
    crate::persistence::migrations::migrate_shared_db()
        .await
        .unwrap();
    let pool =
        sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
            &crate::providers::utils::db_path_shared(),
        ))
        .await
        .unwrap();
    let rows =
        crate::persistence::provider_store::list_providers_by_type(&pool, "cloud_inference_api")
            .await
            .unwrap();
    assert!(rows.len() >= 10, "the curated hosted list is present");

    let sessions = ConnectSessions::default();
    for p in rows.iter().filter(|p| p.api_dialect.is_some()) {
        let acq = crate::provider_credentials::acquisition(p);
        // No row names a method with no code path behind it in this build.
        assert_ne!(acq.method.as_deref(), Some("local_signin"), "{}", p.id);
        // Pasting works for every row.
        let info = begin_impl(&sessions, p, USER, Instant::now())
            .unwrap_or_else(|e| panic!("{}: {e}", p.id));
        // The keys page is shipped for every curated row, and passes the policy.
        assert!(info.keys_page_available, "{}", p.id);
        // A spend page, where one is shipped, passes the policy too.
        if p.spend_limit_url.is_some() {
            assert!(url_policy::resolve(p, Page::Spend).is_ok(), "{}", p.id);
        }
        // A shipped key pattern must be usable and must refuse sloppy text.
        if let Some(pattern) = acq.key_pattern.as_deref() {
            assert!(
                crate::provider_credentials::key_pattern::pattern_is_usable(pattern),
                "{}",
                p.id
            );
            for sloppy in ["", " ", "a b", "a\nb", "https://example.test/key"] {
                let hit =
                    crate::provider_credentials::key_pattern::match_clipboard(pattern, sloppy);
                assert!(matches!(hit, Ok(None)), "{} accepted {sloppy:?}", p.id);
            }
        }
    }
    pool.close().await;
    match saved_root {
        Some(v) => std::env::set_var("QR_DATA_ROOT", v),
        None => std::env::remove_var("QR_DATA_ROOT"),
    }
}

// ---------------------------------------------------------------------------
// Source scans: what may call what
// ---------------------------------------------------------------------------

fn production_files() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
    let mut out = Vec::new();
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        if name == "tests.rs" || name.ends_with("_tests.rs") || name == "loopback_test_server.rs" {
            continue;
        }
        let src = std::fs::read_to_string(&f).unwrap();
        // Cut at the file's test MODULE (a lone #[cfg(test)] can gate one fn).
        let cut = src
            .match_indices("#[cfg(test)]")
            .find(|(i, tag)| {
                let rest = src[i + tag.len()..].trim_start();
                rest.starts_with("mod ") || rest.starts_with("#[path")
            })
            .map_or(src.len(), |(i, _)| i);
        let rel = f
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        out.push((rel, src[..cut].to_owned()));
    }
    out
}

fn code_lines_containing(needle: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for (file, src) in production_files() {
        for line in src.lines() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains(needle) {
                hits.push(format!("{file}: {}", code.trim()));
            }
        }
    }
    hits
}

#[test]
fn only_the_opener_module_asks_the_os_to_open_a_url() {
    let hits = code_lines_containing("open::that");
    assert!(!hits.is_empty(), "the opener exists");
    assert!(
        hits.iter()
            .all(|h| h.starts_with("provider_credentials/opener.rs:")),
        "{hits:#?}"
    );
}

#[test]
fn credential_capture_reads_the_clipboard_in_exactly_one_place() {
    // (The Cloud Chat pane host has its own, older clipboard use for pasting
    // into a provider pane; it is outside credential capture and not in scope.)
    let hits: Vec<String> = code_lines_containing(".read_text(")
        .into_iter()
        .filter(|h| {
            h.starts_with("provider_credentials/")
                || h.starts_with("commands/provider_credentials.rs")
        })
        .collect();
    assert_eq!(hits.len(), 2, "{hits:#?}");
    assert!(
        hits.iter()
            .any(|h| h.starts_with("provider_credentials/clipboard.rs:")),
        "{hits:#?}"
    );
    // The one call in the commands file sits inside check_clipboard_impl, which
    // only runs from check_clipboard_for_key (the user's click) after the
    // session check.
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/commands/provider_credentials.rs"),
    )
    .unwrap();
    let start = src.find("fn check_clipboard_impl(").unwrap();
    let end = src.find("pub async fn check_clipboard_for_key(").unwrap();
    let at = src.find("clipboard.read_text()").unwrap();
    assert!(
        start < at && at < end,
        "read_text left check_clipboard_impl"
    );
    let session_check = src[start..at].contains("with_session");
    assert!(session_check, "the session check must precede the read");
}

#[test]
fn the_webview_is_granted_no_opener_and_no_clipboard_permission() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
    let mut seen = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        for banned in ["opener", "clipboard", "shell"] {
            assert!(!text.contains(banned), "{} grants {banned}", path.display());
        }
        seen += 1;
    }
    assert!(seen > 0);
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .unwrap();
    for banned in ["tauri-plugin-opener", "tauri-plugin-shell"] {
        assert!(
            !manifest.lines().any(|l| l.trim_start().starts_with(banned)),
            "{banned} must not be a dependency"
        );
    }
}

#[test]
fn credential_commands_take_no_url_from_the_frontend() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/commands/provider_credentials.rs"),
    )
    .unwrap();
    let start = src.find("pub async fn open_provider_page(").unwrap();
    let signature = &src[start..src[start..].find(") -> Result").unwrap() + start];
    assert!(
        !signature.to_ascii_lowercase().contains("url"),
        "{signature}"
    );
    assert!(signature.contains("page: ProviderPage"));
}
