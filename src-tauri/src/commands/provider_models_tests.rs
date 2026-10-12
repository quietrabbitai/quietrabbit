// src-tauri/src/commands/provider_models_tests.rs
//
// Tests for the model-list, selection and price-override IPC logic
// (items.id=711). The `*_impl` functions hold the behavior; the
// #[tauri::command] wrappers only resolve the session. Neutral fixtures, a
// real encrypted store, loopback HTTP only: no provider is ever called.

use std::time::Duration;

use chrono::{NaiveDate, TimeZone, Utc};
use serde_json::json;

use super::*;
use crate::persistence::provider_models_store::{get_live_model_conn, record_refresh_conn};
use crate::providers::hosted_client::LiveModel;
use crate::providers::loopback_test_server::{Canned, LoopbackServer};
use crate::spend::fixtures::{model, provider};

const KEY_HEX: &str = "0505050505050505050505050505050505050505050505050505050505050505";
const SECRET: &str = "sk-refresh-secret-0123456789abcdef";

const PAID: &str =
    r#"{"input_per_mtok":0.50,"output_per_mtok":1.50,"as_of":"2026-10-11","source":"curated"}"#;
const OLD: &str =
    r#"{"input_per_mtok":0.50,"output_per_mtok":1.50,"as_of":"2026-01-01","source":"curated"}"#;

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 14).unwrap()
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn catalog(price: Option<&str>) -> Catalog {
    let mut m = model("fixture-model-a");
    m.is_default = true;
    m.request_options = Some(json!({"max_tokens": 1000}));
    m.context_window_source = Some("curated".to_owned());
    m.context_window_as_of = Some("2026-10-11".to_owned());
    let mut p = provider();
    p.provider_type = "cloud_inference_api".to_owned();
    p.models_path = Some("/models".to_owned());
    p.base_url_template = Some("https://api.example.test/v1".to_owned());
    p.auth_scheme = Some("bearer".to_owned());
    p.extra_headers = Some(json!({}));
    p.user_variables = Some(json!([]));
    Catalog {
        provider: p,
        models: vec![m],
        pricing_texts: vec![("fixture-model-a".to_owned(), price.map(str::to_owned))],
    }
}

async fn with_conn<F, Fut>(f: F)
where
    F: FnOnce(SqliteConnection) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _lock = crate::test_support::ENV_MUTEX.lock().await;
    let saved_root = std::env::var("QR_DATA_ROOT").ok();
    let tempdir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("QR_DATA_ROOT", tempdir.path());
    let conn = store::open("models-user", KEY_HEX).await.expect("open");
    f(conn).await;
    match saved_root {
        Some(v) => std::env::set_var("QR_DATA_ROOT", v),
        None => std::env::remove_var("QR_DATA_ROOT"),
    }
}

fn live(id: &str, ctx: Option<u32>, retires: Option<&str>) -> LiveModel {
    LiveModel {
        id: id.to_owned(),
        context_tokens: ctx,
        retires_at: retires.map(str::to_owned),
    }
}

fn key() -> IntegrationKey {
    IntegrationKey {
        id: "k".to_owned(),
        provider: "fixture-host-a".to_owned(),
        key_type: QR_HOSTED_KEY_TYPE.to_owned(),
        integration_id: "i".to_owned(),
        credential_label: "label".to_owned(),
        credential: SECRET.to_owned(),
        persona_id: None,
        auth_type: Some("api_key".to_owned()),
        expires_at: None,
        is_active: true,
        created_at: "t".to_owned(),
        last_verified_at: None,
        extra_metadata: json!({}),
    }
}

fn loopback_client() -> HostedClient {
    HostedClient::for_loopback_tests(Duration::from_secs(5), Duration::from_secs(2), 1024 * 1024)
}

fn at(server: &LoopbackServer, mut cat: Catalog) -> Catalog {
    cat.provider.base_url_template = Some(format!("{}/v1", server.base()));
    cat
}

async fn refresh(
    server: &LoopbackServer,
    conn: &mut SqliteConnection,
    cat: &Catalog,
    key: Option<IntegrationKey>,
) -> Result<RefreshResultDto, String> {
    let cat = at(
        server,
        Catalog {
            provider: cat.provider.clone(),
            models: cat.models.clone(),
            pricing_texts: cat.pricing_texts.clone(),
        },
    );
    refresh_impl(
        &loopback_client(),
        conn,
        &cat,
        key,
        CallContext::build_for_loopback,
        now(),
    )
    .await
}

fn find<'a>(v: &'a ProviderModelsView, id: &str) -> &'a ProviderModelDto {
    v.models
        .iter()
        .find(|m| m.model_id == id)
        .unwrap_or_else(|| panic!("{id} listed"))
}

// ---------------------------------------------------------------------------
// refresh_available_models
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_refresh_reads_the_list_with_the_users_key_and_records_it() {
    with_conn(|mut conn| async move {
        let server = LoopbackServer::start(vec![Canned::json(
            200,
            r#"{"data":[{"id":"fixture-model-a"},{"id":"live-x","context_window":32000,"retires_at":"2027-01-01"},{"id":"live-y"}]}"#,
        )]);
        let cat = catalog(Some(PAID));
        let r = refresh(&server, &mut conn, &cat, Some(key())).await.unwrap();
        assert_eq!((r.listed, r.new, r.newly_missing, r.returned), (3, 3, 0, 0));

        let sent = &server.captured()[0];
        assert_eq!(sent.method, "GET");
        assert_eq!(sent.path, "/v1/models");
        assert_eq!(sent.header("authorization"), Some(format!("Bearer {SECRET}").as_str()));
        assert!(sent.body.is_empty(), "a list request carries no content");

        let x = get_live_model_conn(&mut conn, "fixture-host-a", "live-x").await.unwrap().unwrap();
        assert_eq!(x.context_tokens, Some(32_000));
        assert_eq!(x.retires_at.as_deref(), Some("2027-01-01"));

        // The key never appears in the result.
        let serialized = serde_json::to_string(&r).unwrap();
        assert!(!serialized.contains(SECRET));
    })
    .await;
}

#[tokio::test]
async fn a_later_refresh_flags_the_models_it_no_longer_names() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        let first = LoopbackServer::start(vec![Canned::json(
            200,
            r#"{"data":[{"id":"fixture-model-a"},{"id":"live-x"},{"id":"live-y"}]}"#,
        )]);
        refresh(&first, &mut conn, &cat, Some(key())).await.unwrap();
        let second =
            LoopbackServer::start(vec![Canned::json(200, r#"{"data":[{"id":"live-x"}]}"#)]);
        let r = refresh(&second, &mut conn, &cat, Some(key()))
            .await
            .unwrap();
        assert_eq!((r.new, r.newly_missing), (0, 2));

        let view = list_impl(&mut conn, &cat, None, today()).await.unwrap();
        // The curated default is absent from the latest list: may be retired,
        // but it is still listed and still the default.
        let a = find(&view, "fixture-model-a");
        assert_eq!(a.listed, Some(false));
        assert!(a.may_be_retired);
        assert!(a
            .notices
            .contains(&Code::WarnModelMayBeRetired.as_str().to_owned()));
        assert!(a.is_default);
        assert!(find(&view, "live-y").may_be_retired);
        assert!(!find(&view, "live-x").may_be_retired);
        assert_eq!(view.listed_count, Some(1));
    })
    .await;
}

#[tokio::test]
async fn an_empty_list_flags_nothing() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        let first = LoopbackServer::start(vec![Canned::json(200, r#"{"data":[{"id":"live-x"}]}"#)]);
        refresh(&first, &mut conn, &cat, Some(key())).await.unwrap();
        let empty = LoopbackServer::start(vec![Canned::json(200, r#"{"data":[]}"#)]);
        assert_eq!(
            refresh(&empty, &mut conn, &cat, Some(key()))
                .await
                .unwrap_err(),
            "empty_model_list"
        );
        let live = get_live_model_conn(&mut conn, "fixture-host-a", "live-x")
            .await
            .unwrap()
            .unwrap();
        assert!(
            live.present,
            "a glitchy empty list must not mass-flag models"
        );
    })
    .await;
}

#[tokio::test]
async fn a_refresh_that_cannot_run_makes_no_request_and_writes_nothing() {
    with_conn(|mut conn| async move {
        let server = LoopbackServer::start(vec![Canned::json(200, r#"{"data":[{"id":"m"}]}"#)]);

        // No list endpoint: Cloudflare-style NULL means no refresh available.
        let mut no_path = catalog(Some(PAID));
        no_path.provider.models_path = None;
        assert_eq!(
            refresh(&server, &mut conn, &no_path, Some(key()))
                .await
                .unwrap_err(),
            "refresh_unavailable"
        );
        // No saved key.
        assert_eq!(
            refresh(&server, &mut conn, &catalog(Some(PAID)), None)
                .await
                .unwrap_err(),
            "missing_api_key"
        );
        // Not a hosted API provider.
        let mut local = catalog(Some(PAID));
        local.provider.provider_type = "local_model".to_owned();
        assert_eq!(
            refresh(&server, &mut conn, &local, Some(key()))
                .await
                .unwrap_err(),
            "unknown_provider"
        );
        assert_eq!(server.connections(), 0);
        assert!(get_refresh_conn(&mut conn, "fixture-host-a")
            .await
            .unwrap()
            .is_none());
    })
    .await;
}

#[tokio::test]
async fn a_provider_failure_leaves_the_previous_list_untouched() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        let ok = LoopbackServer::start(vec![Canned::json(200, r#"{"data":[{"id":"live-x"}]}"#)]);
        refresh(&ok, &mut conn, &cat, Some(key())).await.unwrap();

        for (status, code) in [
            (401u16, "invalid_api_key"),
            (403, "invalid_api_key"),
            (429, "rate_limited"),
        ] {
            let bad = LoopbackServer::start(vec![Canned::json(status, "{}")]);
            assert_eq!(
                refresh(&bad, &mut conn, &cat, Some(key()))
                    .await
                    .unwrap_err(),
                code,
                "{status}"
            );
        }
        let garbage = LoopbackServer::start(vec![Canned::json(200, "[]")]);
        assert_eq!(
            refresh(&garbage, &mut conn, &cat, Some(key()))
                .await
                .unwrap_err(),
            "malformed_response"
        );
        let live = get_live_model_conn(&mut conn, "fixture-host-a", "live-x")
            .await
            .unwrap()
            .unwrap();
        assert!(live.present);
        assert_eq!(
            get_refresh_conn(&mut conn, "fixture-host-a")
                .await
                .unwrap()
                .unwrap()
                .listed_count,
            1
        );
    })
    .await;
}

/// The list request is made on a user click and nowhere else: no timer,
/// startup hook or other command may call it.
#[test]
fn the_list_request_has_exactly_one_production_caller() {
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
    let mut files = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let mut callers = Vec::new();
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        // Test files and the test modules of production files do not count.
        if name.ends_with("_tests.rs") || name == "loopback_test_server.rs" {
            continue;
        }
        let src = std::fs::read_to_string(&f).unwrap();
        // Cut at the file's test MODULE (a lone `#[cfg(test)]` can also gate a
        // single test-only function in the middle of production code).
        let cut = src
            .match_indices("#[cfg(test)]")
            .find(|(i, tag)| {
                let rest = src[i + tag.len()..].trim_start();
                rest.starts_with("mod ") || rest.starts_with("#[path")
            })
            .map_or(src.len(), |(i, _)| i);
        let production = &src[..cut];
        for line in production.lines() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains(".list_models(")
                || code.contains(".check_key(")
                || code.contains(".health_check(")
            {
                callers.push(format!("{name}: {}", code.trim()));
            }
        }
    }
    // Inside the client the three chain into one another (health_check calls
    // check_key, which calls list_models). Outside it, the refresh command is
    // the only caller of anything that reaches the provider's list.
    callers.sort();
    let outside: Vec<&String> = callers
        .iter()
        .filter(|c| !c.starts_with("hosted_client.rs"))
        .collect();
    assert_eq!(
        outside.len(),
        1,
        "unexpected callers of the provider model list: {callers:#?}"
    );
    assert!(outside[0].starts_with("provider_models.rs"), "{callers:#?}");
}

// ---------------------------------------------------------------------------
// list_provider_models
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_list_shows_curated_and_uncurated_models_with_provenance() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        let before = list_impl(&mut conn, &cat, None, today()).await.unwrap();
        assert!(before.refresh_available);
        assert_eq!(before.refreshed_at, None);
        assert_eq!(before.models.len(), 1);
        let a = &before.models[0];
        assert_eq!(a.origin, "curated");
        assert_eq!(
            a.listed, None,
            "nothing is known to be missing before a refresh"
        );
        assert!(!a.may_be_retired);
        assert!(a.selected && a.is_default);
        assert_eq!(a.context.tokens.as_deref(), Some("8192"));
        assert_eq!(a.context.source.as_deref(), Some("curated"));
        assert_eq!(a.context.as_of.as_deref(), Some("2026-10-11"));
        assert_eq!(a.price.state, "price_state.known");
        assert_eq!(a.price.source.as_deref(), Some("curated"));
        assert_eq!(a.price.as_of.as_deref(), Some("2026-10-11"));
        assert_eq!(a.price.input_micros_per_mtok.as_deref(), Some("500000"));

        record_refresh_conn(
            &mut conn,
            "fixture-host-a",
            &[
                live("fixture-model-a", Some(9000), Some("2027-02-02")),
                live("live-x", Some(32_000), None),
                live("live-bare", None, None),
            ],
            now(),
        )
        .await
        .unwrap();
        let view = list_impl(&mut conn, &cat, None, today()).await.unwrap();
        assert_eq!(view.models.len(), 3);

        // The curated row keeps its own window and gains the provider's
        // retirement date, with its source.
        let a = find(&view, "fixture-model-a");
        assert_eq!(a.context.tokens.as_deref(), Some("8192"));
        assert_eq!(
            a.provider_context.as_ref().unwrap().tokens.as_deref(),
            Some("9000")
        );
        assert_eq!(
            a.provider_context.as_ref().unwrap().source.as_deref(),
            Some("provider_api")
        );
        assert_eq!(a.retires_at.as_deref(), Some("2027-02-02"));
        assert_eq!(a.retires_at_source.as_deref(), Some("provider_api"));
        assert!(a
            .notices
            .contains(&Code::WarnModelDeprecated.as_str().to_owned()));

        // An uncurated model: price unknown, privacy unverified, window from
        // the provider with its date.
        let x = find(&view, "live-x");
        assert_eq!(x.origin, "uncurated");
        assert_eq!(x.price.state, "price_state.unknown");
        assert!(x
            .notices
            .contains(&Code::WarnModelUncurated.as_str().to_owned()));
        assert!(x
            .notices
            .contains(&Code::WarnPriceUnknown.as_str().to_owned()));
        assert_eq!(x.context.tokens.as_deref(), Some("32000"));
        assert_eq!(x.context.source.as_deref(), Some("provider_api"));
        assert_eq!(x.context.as_of.as_deref(), Some("2026-10-14"));
        assert_eq!((x.token_factor, x.output_overhead), (1.0, 1.0));
        assert!(!x.selected);
        // A model whose list gave no window: unknown, not a made-up number.
        let bare = find(&view, "live-bare");
        assert_eq!(bare.context.tokens, None);
        assert_eq!(bare.context.source, None);
    })
    .await;
}

#[tokio::test]
async fn a_curated_price_older_than_ninety_days_carries_the_stale_note() {
    with_conn(|mut conn| async move {
        let fresh = list_impl(&mut conn, &catalog(Some(PAID)), None, today())
            .await
            .unwrap();
        assert_eq!(fresh.models[0].price.state, "price_state.known");
        assert!(!fresh.models[0]
            .notices
            .contains(&Code::WarnPriceStale.as_str().to_owned()));

        let old = list_impl(&mut conn, &catalog(Some(OLD)), None, today())
            .await
            .unwrap();
        let m = &old.models[0];
        assert_eq!(m.price.state, "price_state.stale");
        assert_eq!(m.price.age_days, Some(286));
        assert!(m
            .notices
            .contains(&Code::WarnPriceStale.as_str().to_owned()));

        // A user's own price is never "stale": they set it knowingly.
        let cat = catalog(Some(OLD));
        set_price_override_impl(
            &mut conn,
            &cat,
            &SetPriceOverrideRequest {
                provider_id: "fixture-host-a".to_owned(),
                model_id: "fixture-model-a".to_owned(),
                price: Some(PriceInput {
                    input_per_mtok: "1".into(),
                    output_per_mtok: "2".into(),
                    tiers: vec![],
                }),
                confirmed_free: false,
            },
            NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
            now(),
        )
        .await
        .unwrap();
        let later = list_impl(&mut conn, &cat, None, today()).await.unwrap();
        let m = &later.models[0];
        assert_eq!(m.price.state, "price_state.known");
        assert_eq!(m.price.source.as_deref(), Some("user"));
        assert!(m.price.overridden);
    })
    .await;
}

#[tokio::test]
async fn the_selected_flag_follows_the_stored_selection() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        record_refresh_conn(
            &mut conn,
            "fixture-host-a",
            &[live("live-x", None, None)],
            now(),
        )
        .await
        .unwrap();
        let stored = ResolvedModelSelection {
            model_id: "live-x".to_owned(),
            source: SelectionSource::Persona,
        };
        let view = list_impl(&mut conn, &cat, Some(&stored), today())
            .await
            .unwrap();
        assert!(find(&view, "live-x").selected);
        assert!(!find(&view, "fixture-model-a").selected);
        assert!(
            find(&view, "fixture-model-a").is_default,
            "the default is still marked"
        );
        assert_eq!(view.selection.stored_model_id.as_deref(), Some("live-x"));
        assert_eq!(view.selection.stored_from, Some(SelectionSource::Persona));
        assert_eq!(view.selection.effective_model_id.as_deref(), Some("live-x"));
    })
    .await;
}

#[tokio::test]
async fn a_provider_with_no_list_endpoint_says_so() {
    with_conn(|mut conn| async move {
        let mut cat = catalog(Some(PAID));
        cat.provider.models_path = None;
        let view = list_impl(&mut conn, &cat, None, today()).await.unwrap();
        assert!(!view.refresh_available);
    })
    .await;
}

// ---------------------------------------------------------------------------
// set_price_override
// ---------------------------------------------------------------------------

fn price(input: &str, output: &str) -> PriceInput {
    PriceInput {
        input_per_mtok: input.to_owned(),
        output_per_mtok: output.to_owned(),
        tiers: vec![],
    }
}

fn override_req(
    model_id: &str,
    price: Option<PriceInput>,
    confirmed_free: bool,
) -> SetPriceOverrideRequest {
    SetPriceOverrideRequest {
        provider_id: "fixture-host-a".to_owned(),
        model_id: model_id.to_owned(),
        price,
        confirmed_free,
    }
}

#[tokio::test]
async fn a_price_override_replaces_the_curated_price_with_provenance_and_a_date() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        let dto = set_price_override_impl(
            &mut conn,
            &cat,
            &override_req("fixture-model-a", Some(price("0.07", "0.3")), false),
            today(),
            now(),
        )
        .await
        .unwrap();
        assert_eq!(dto.source.as_deref(), Some("user"));
        assert_eq!(dto.as_of.as_deref(), Some("2026-10-14"));
        assert_eq!(dto.input_micros_per_mtok.as_deref(), Some("70000"));
        assert_eq!(dto.output_micros_per_mtok.as_deref(), Some("300000"));
        assert!(dto.overridden);

        // What the spend gate reads is the same price, exactly.
        let raw =
            store::get_price_override_raw_conn(&mut conn, "fixture-host-a", "fixture-model-a")
                .await
                .unwrap()
                .unwrap();
        match resolve_price(Some(&raw), Some(PAID)) {
            PriceQuote::Known(p) => {
                assert_eq!(
                    (p.input_micro_per_mtok, p.output_micro_per_mtok),
                    (70_000, 300_000)
                );
                assert_eq!(p.source, PriceSource::User);
            }
            PriceQuote::Unknown => panic!("the override must read back"),
        }

        // Removing it restores the curated price.
        let dto = set_price_override_impl(
            &mut conn,
            &cat,
            &override_req("fixture-model-a", None, false),
            today(),
            now(),
        )
        .await
        .unwrap();
        assert!(!dto.overridden);
        assert_eq!(dto.source.as_deref(), Some("curated"));
        assert_eq!(dto.input_micros_per_mtok.as_deref(), Some("500000"));
    })
    .await;
}

#[tokio::test]
async fn a_tiered_override_keeps_the_prompt_length_break() {
    with_conn(|mut conn| async move {
        let cat = catalog(None);
        let dto = set_price_override_impl(
            &mut conn,
            &cat,
            &override_req(
                "fixture-model-a",
                Some(PriceInput {
                    input_per_mtok: "0.10".into(),
                    output_per_mtok: "0.50".into(),
                    tiers: vec![
                        PriceTierInput {
                            prompt_tokens_up_to: Some(100_000),
                            prompt_tokens_over: None,
                            input_per_mtok: "0.10".into(),
                            output_per_mtok: "0.50".into(),
                        },
                        PriceTierInput {
                            prompt_tokens_up_to: None,
                            prompt_tokens_over: Some(100_000),
                            input_per_mtok: "0.50".into(),
                            output_per_mtok: "2.50".into(),
                        },
                    ],
                }),
                false,
            ),
            today(),
            now(),
        )
        .await
        .unwrap();
        assert_eq!(dto.tiers.len(), 2);
        assert_eq!(dto.tiers[1].prompt_tokens_over.as_deref(), Some("100000"));
        assert_eq!(dto.tiers[1].input_micros_per_mtok, "500000");

        // The estimate prices a long prompt at the dearer tier.
        let raw =
            store::get_price_override_raw_conn(&mut conn, "fixture-host-a", "fixture-model-a")
                .await
                .unwrap()
                .unwrap();
        let PriceQuote::Known(p) = resolve_price(Some(&raw), None) else {
            panic!("known")
        };
        assert_eq!(p.rates_for(100_000), (100_000, 500_000));
        assert_eq!(p.rates_for(100_001), (500_000, 2_500_000));
    })
    .await;
}

#[tokio::test]
async fn an_override_that_could_be_misread_is_refused_and_nothing_is_written() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        let tier = |up: Option<u32>, over: Option<u32>| PriceTierInput {
            prompt_tokens_up_to: up,
            prompt_tokens_over: over,
            input_per_mtok: "1".into(),
            output_per_mtok: "1".into(),
        };
        let cases: Vec<(PriceInput, &str)> = vec![
            (price("-1", "1"), "invalid_price"),
            (price("1e3", "1"), "invalid_price"),
            (price("0.1234567", "1"), "invalid_price"),
            (price("", "1"), "invalid_price"),
            (price("abc", "1"), "invalid_price"),
            (price(".5", "1"), "invalid_price"),
            (price("1", "1000.000001"), "price_too_high"),
            (price("1", "1000000"), "price_too_high"),
            (
                PriceInput {
                    tiers: vec![tier(Some(5), Some(5))],
                    ..price("1", "1")
                },
                "invalid_tier",
            ),
            (
                PriceInput {
                    tiers: vec![tier(None, None)],
                    ..price("1", "1")
                },
                "invalid_tier",
            ),
            (
                PriceInput {
                    tiers: vec![tier(Some(0), None)],
                    ..price("1", "1")
                },
                "invalid_tier",
            ),
            (
                PriceInput {
                    tiers: (1..=5).map(|n| tier(Some(n), None)).collect(),
                    ..price("1", "1")
                },
                "too_many_tiers",
            ),
            (price("0", "0"), "free_price_needs_confirmation"),
            (price("0.0", "0.00"), "free_price_needs_confirmation"),
        ];
        for (p, want) in cases {
            let err = set_price_override_impl(
                &mut conn,
                &cat,
                &override_req("fixture-model-a", Some(p), false),
                today(),
                now(),
            )
            .await
            .unwrap_err();
            assert_eq!(err, want);
        }
        assert!(
            store::get_price_override_raw_conn(&mut conn, "fixture-host-a", "fixture-model-a")
                .await
                .unwrap()
                .is_none()
        );

        // Zero is allowed once it is confirmed, and is then free.
        let dto = set_price_override_impl(
            &mut conn,
            &cat,
            &override_req("fixture-model-a", Some(price("0", "0")), true),
            today(),
            now(),
        )
        .await
        .unwrap();
        assert!(dto.is_free);
        // A model the provider does not list at all.
        assert_eq!(
            set_price_override_impl(
                &mut conn,
                &cat,
                &override_req("ghost", Some(price("1", "1")), false),
                today(),
                now()
            )
            .await
            .unwrap_err(),
            "unknown_model"
        );
    })
    .await;
}

#[tokio::test]
async fn an_uncurated_model_can_be_priced_by_the_user() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        record_refresh_conn(
            &mut conn,
            "fixture-host-a",
            &[live("live-x", None, None)],
            now(),
        )
        .await
        .unwrap();
        let dto = set_price_override_impl(
            &mut conn,
            &cat,
            &override_req("live-x", Some(price("2", "4")), false),
            today(),
            now(),
        )
        .await
        .unwrap();
        assert_eq!(dto.state, "price_state.known");
        let view = list_impl(&mut conn, &cat, None, today()).await.unwrap();
        let x = find(&view, "live-x");
        assert_eq!(x.price.source.as_deref(), Some("user"));
        assert!(!x
            .notices
            .contains(&Code::WarnPriceUnknown.as_str().to_owned()));
        assert!(
            x.notices
                .contains(&Code::WarnModelUncurated.as_str().to_owned()),
            "still unreviewed"
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// set_model_selection (against a real migrated shared.db)
// ---------------------------------------------------------------------------

mod selection {
    use super::*;
    use crate::persistence::user_provider_preference_store::resolve_model_selection;

    const USER: &str = "sel-cmd-user";

    async fn with_real<F, Fut>(f: F)
    where
        F: FnOnce(sqlx::SqlitePool, Catalog, SqliteConnection) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved = std::env::var("QR_DATA_ROOT").ok();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", dir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("migrate");
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("pool");
        sqlx::query(
            "INSERT INTO users (id, display_name, role, is_primary, auth_enabled, created_at) \
             VALUES (?, 'Fixture', 'user', 1, 1, '2026-01-01T00:00:00Z')",
        )
        .bind(USER)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO personas (id, display_name, persona_type, created_at) \
             VALUES ('p1', 'Fixture', 'personal', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let provider_id: String = sqlx::query_scalar(
            "SELECT m.provider_id FROM provider_models m JOIN providers p ON p.id = m.provider_id \
             WHERE m.is_default = 1 AND p.api_dialect IS NOT NULL ORDER BY m.provider_id LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let cat = load_catalog(&pool, &provider_id).await.unwrap();
        let conn = store::open(USER, KEY_HEX).await.unwrap();
        f(pool, cat, conn).await;
        match saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    fn req(
        cat: &Catalog,
        scope: SelectionScope,
        model: Option<&str>,
        limit: Option<u32>,
    ) -> SetModelSelectionRequest {
        SetModelSelectionRequest {
            provider_id: cat.provider.id.clone(),
            scope,
            persona_id: None,
            model_id: model.map(str::to_owned),
            max_output_tokens: limit,
        }
    }

    #[tokio::test]
    async fn a_curated_model_is_selected_and_cleared_at_each_scope() {
        with_real(|pool, cat, mut conn| async move {
            let id = cat.models[0].model_id.clone();
            for scope in [
                SelectionScope::Account,
                SelectionScope::Persona {
                    persona_id: "p1".into(),
                },
                SelectionScope::Focus {
                    focus_id: "f1".into(),
                },
            ] {
                set_selection_impl(
                    &pool,
                    &mut conn,
                    USER,
                    &cat,
                    &req(&cat, scope.clone(), Some(&id), None),
                )
                .await
                .unwrap();
                set_selection_impl(&pool, &mut conn, USER, &cat, &req(&cat, scope, None, None))
                    .await
                    .unwrap();
            }
            let none =
                resolve_model_selection(&pool, USER, Some("p1"), Some("f1"), &cat.provider.id)
                    .await
                    .unwrap();
            assert!(none.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn a_retired_or_unknown_model_cannot_be_selected() {
        with_real(|pool, cat, mut conn| async move {
            sqlx::query(
                "INSERT INTO provider_models (id, provider_id, model_id, context_window_tokens, is_default, created_at, status) \
                 VALUES ('r', ?, 'm-retired', 4096, 0, 't', 'retired')",
            )
            .bind(&cat.provider.id)
            .execute(&pool)
            .await
            .unwrap();
            let cat = load_catalog(&pool, &cat.provider.id).await.unwrap();
            assert_eq!(
                set_selection_impl(&pool, &mut conn, USER, &cat, &req(&cat, SelectionScope::Account, Some("m-retired"), None)).await.unwrap_err(),
                "spend_block.model_retired"
            );
            assert_eq!(
                set_selection_impl(&pool, &mut conn, USER, &cat, &req(&cat, SelectionScope::Account, Some("ghost"), None)).await.unwrap_err(),
                "unknown_model"
            );
            assert!(resolve_model_selection(&pool, USER, None, None, &cat.provider.id).await.unwrap().is_none());
            assert_eq!(
                set_selection_impl(&pool, &mut conn, USER, &cat, &req(&cat, SelectionScope::Persona { persona_id: String::new() }, None, None)).await.unwrap_err(),
                "invalid_scope"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn an_uncurated_model_needs_an_output_limit_within_its_window() {
        with_real(|pool, cat, mut conn| async move {
            let pid = cat.provider.id.clone();
            record_refresh_conn(
                &mut conn,
                &pid,
                &[
                    live("live-x", Some(8000), None),
                    live("live-nowin", None, None),
                ],
                now(),
            )
            .await
            .unwrap();

            assert_eq!(
                set_selection_impl(
                    &pool,
                    &mut conn,
                    USER,
                    &cat,
                    &req(&cat, SelectionScope::Account, Some("live-x"), None)
                )
                .await
                .unwrap_err(),
                "spend_block.model_output_limit_required"
            );
            for bad in [0u32, 8001] {
                assert_eq!(
                    set_selection_impl(
                        &pool,
                        &mut conn,
                        USER,
                        &cat,
                        &req(&cat, SelectionScope::Account, Some("live-x"), Some(bad))
                    )
                    .await
                    .unwrap_err(),
                    "invalid_output_limit",
                    "{bad}"
                );
            }
            // Nothing was selected by the refusals.
            assert!(resolve_model_selection(&pool, USER, None, None, &pid)
                .await
                .unwrap()
                .is_none());

            set_selection_impl(
                &pool,
                &mut conn,
                USER,
                &cat,
                &req(&cat, SelectionScope::Account, Some("live-x"), Some(800)),
            )
            .await
            .unwrap();
            let sel = resolve_model_selection(&pool, USER, None, None, &pid)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(sel.model_id, "live-x");
            let row = get_live_model_conn(&mut conn, &pid, "live-x")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.user_output_limit, Some(800));

            // Selecting it again at another scope reuses the stored limit.
            set_selection_impl(
                &pool,
                &mut conn,
                USER,
                &cat,
                &req(
                    &cat,
                    SelectionScope::Persona {
                        persona_id: "p1".into(),
                    },
                    Some("live-x"),
                    None,
                ),
            )
            .await
            .unwrap();

            // A model with no reported window takes any limit up to the cap.
            set_selection_impl(
                &pool,
                &mut conn,
                USER,
                &cat,
                &req(
                    &cat,
                    SelectionScope::Focus {
                        focus_id: "f1".into(),
                    },
                    Some("live-nowin"),
                    Some(50_000),
                ),
            )
            .await
            .unwrap();
            assert_eq!(
                set_selection_impl(
                    &pool,
                    &mut conn,
                    USER,
                    &cat,
                    &req(
                        &cat,
                        SelectionScope::Focus {
                            focus_id: "f2".into()
                        },
                        Some("live-nowin"),
                        Some(MAX_OUTPUT_LIMIT_UNKNOWN_CONTEXT + 1)
                    )
                )
                .await
                .unwrap_err(),
                "invalid_output_limit"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_provider_that_is_not_a_hosted_api_is_refused() {
        with_real(|pool, cat, mut conn| async move {
            let mut local = Catalog {
                provider: cat.provider.clone(),
                models: cat.models.clone(),
                pricing_texts: cat.pricing_texts.clone(),
            };
            local.provider.api_dialect = None;
            assert_eq!(
                set_selection_impl(
                    &pool,
                    &mut conn,
                    USER,
                    &local,
                    &req(&local, SelectionScope::Account, None, None)
                )
                .await
                .unwrap_err(),
                "unknown_provider"
            );
        })
        .await;
    }
}
