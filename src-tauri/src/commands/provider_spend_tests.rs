// src-tauri/src/commands/provider_spend_tests.rs
//
// Tests for the spend IPC logic (items.id=710). The `*_impl` functions hold
// the behavior; the #[tauri::command] wrappers only resolve the session.
// Neutral fixtures, a real encrypted store, no network.

use chrono::{Duration, FixedOffset, TimeZone, Utc};
use serde_json::json;

use super::*;
use crate::persistence::provider_spend_store::{LimitSpec, NewReservation};
use crate::spend::fixtures::{model, provider};

const TEST_KEY_HEX: &str = "0303030303030303030303030303030303030303030303030303030303030303";

const PAID: &str =
    r#"{"input_per_mtok":0.50,"output_per_mtok":1.50,"as_of":"2026-10-11","source":"curated"}"#;
const FREE: &str =
    r#"{"input_per_mtok":0,"output_per_mtok":0,"as_of":"2026-10-11","source":"curated"}"#;

fn catalog(price: Option<&str>) -> Catalog {
    let mut m = model("fixture-model-a");
    m.is_default = true;
    m.request_options = Some(json!({"max_completion_tokens": 1000}));
    Catalog {
        provider: provider(),
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
    let conn = store::open("spend-user", TEST_KEY_HEX).await.expect("open");
    f(conn).await;
    if let Some(v) = saved_root {
        std::env::set_var("QR_DATA_ROOT", v);
    } else {
        std::env::remove_var("QR_DATA_ROOT");
    }
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn consent_req(cat: &Catalog, cap: Option<CapChoice>) -> RecordConsentRequest {
    RecordConsentRequest {
        provider_id: cat.provider.id.clone(),
        action: ConsentAction::Consent,
        attributes_hash: Some(attributes_hash(
            &cat.provider,
            &cat.models,
            &cat.pricing_texts,
        )),
        cap,
    }
}

fn limit_req(
    metric: LimitMetric,
    period: LimitPeriod,
    scope: ScopeKind,
    id: Option<&str>,
    value: &str,
) -> SetLimitRequest {
    SetLimitRequest {
        metric,
        period,
        scope_kind: scope,
        scope_id: id.map(str::to_owned),
        limit_value: value.to_owned(),
        warn_at_pct: 80,
        approves_unattended: false,
        remove: false,
    }
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

#[test]
fn warning_params_become_a_typed_map() {
    let m =
        to_params(&json!({"a": true, "b": 30, "c": "text", "d": null, "e": [1], "f": {"x": 1}}));
    assert_eq!(m["a"], Some(ParamValue::Flag(true)));
    assert_eq!(m["b"], Some(ParamValue::Count(30)));
    assert_eq!(m["c"], Some(ParamValue::Text("text".to_owned())));
    assert_eq!(m["d"], None);
    assert!(
        !m.contains_key("e") && !m.contains_key("f"),
        "nested values are not carried"
    );
    assert!(to_params(&json!("not an object")).is_empty());
}

// ---------------------------------------------------------------------------
// get_provider_disclosure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disclosure_reports_attributes_warnings_hash_and_consent_state() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        let d = disclosure_impl(&mut conn, &cat, None, false).await.unwrap();
        assert_eq!(d.provider_id, "fixture-host-a");
        assert_eq!(
            d.attributes_hash,
            attributes_hash(&cat.provider, &cat.models, &cat.pricing_texts)
        );
        assert_eq!(d.consent_state, "consent_state.none");
        assert_eq!(d.cost_class, CostClass::Metered);
        assert!(d.can_cost_money);
        assert!(d.cap_required, "a metered provider needs a cap to consent");
        assert_eq!(d.suggested_cap_micros, "5000000");
        assert_eq!(d.price_state, "price_state.known");
        assert_eq!(d.price_as_of.as_deref(), Some("2026-10-11"));
        assert!(d.warnings.iter().any(|w| w.code == "warning.cost_metered"));
        assert!(d.attributes.is_anonymous);
        assert_eq!(d.attributes.verification_status, "verified");
        assert_eq!(
            d.attributes.commitment_basis.as_deref(),
            Some("contractual")
        );
        assert_eq!(
            d.attributes.model_overrides,
            vec!["Batch features retain data."]
        );
        // Credentials and prices-as-floats do not appear anywhere in the DTO.
        let wire = serde_json::to_string(&d).unwrap();
        assert!(!wire.contains("api_key") && !wire.contains("credential"));
    })
    .await;
}

#[tokio::test]
async fn disclosure_follows_consent_through_stale_and_revoked() {
    with_conn(|mut conn| async move {
        let mut cat = catalog(Some(FREE));
        // Free provider: consent needs no cap.
        consent_impl(&mut conn, &cat, &consent_req(&cat, None), now())
            .await
            .unwrap();
        let d = disclosure_impl(&mut conn, &cat, None, false).await.unwrap();
        assert_eq!(d.consent_state, "consent_state.current");
        assert!(!d.can_cost_money && !d.cap_required);

        // A verified attribute changes: the consent is stale.
        cat.provider.trains_on_data_by_default = true;
        let d = disclosure_impl(&mut conn, &cat, None, false).await.unwrap();
        assert_eq!(d.consent_state, "consent_state.stale");
        assert_eq!(d.overall_tier, WarningTier::Strong);

        // Re-consent to the new attributes, then revoke.
        consent_impl(
            &mut conn,
            &cat,
            &consent_req(&cat, None),
            now() + Duration::seconds(1),
        )
        .await
        .unwrap();
        assert_eq!(
            disclosure_impl(&mut conn, &cat, None, false)
                .await
                .unwrap()
                .consent_state,
            "consent_state.current"
        );
        consent_impl(
            &mut conn,
            &cat,
            &RecordConsentRequest {
                provider_id: cat.provider.id.clone(),
                action: ConsentAction::Revoke,
                attributes_hash: None,
                cap: None,
            },
            now() + Duration::seconds(2),
        )
        .await
        .unwrap();
        assert_eq!(
            disclosure_impl(&mut conn, &cat, None, false)
                .await
                .unwrap()
                .consent_state,
            "consent_state.revoked"
        );
    })
    .await;
}

#[tokio::test]
async fn disclosure_escalates_for_sensitive_personas_and_unknown_prices() {
    with_conn(|mut conn| async move {
        let cat = catalog(None);
        let d = disclosure_impl(&mut conn, &cat, None, false).await.unwrap();
        assert_eq!(d.price_state, "price_state.unknown");
        assert_eq!(d.cost_class, CostClass::Unknown);
        assert!(d.warnings.iter().any(|w| w.code == "warning.price_unknown"));
        assert_eq!(d.overall_tier, WarningTier::Strong);

        let free = catalog(Some(FREE));
        let calm = disclosure_impl(&mut conn, &free, None, false)
            .await
            .unwrap();
        assert_eq!(calm.overall_tier, WarningTier::None);
        let sensitive = disclosure_impl(&mut conn, &free, None, true).await.unwrap();
        assert_eq!(sensitive.overall_tier, WarningTier::Notice);
        assert!(sensitive
            .warnings
            .iter()
            .any(|w| w.code == "warning.sensitive_persona_content"));
    })
    .await;
}

#[tokio::test]
async fn a_user_price_override_is_reflected_in_the_disclosure() {
    with_conn(|mut conn| async move {
        let cat = catalog(None);
        sqlx::query("INSERT INTO price_overrides (provider_id, model_id, pricing, updated_at) VALUES ('fixture-host-a', 'fixture-model-a', ?, 't')")
            .bind(r#"{"input_per_mtok":1,"output_per_mtok":2,"as_of":"2026-10-12"}"#)
            .execute(&mut conn)
            .await
            .unwrap();
        let d = disclosure_impl(&mut conn, &cat, Some("fixture-model-a"), false).await.unwrap();
        assert_eq!(d.price_state, "price_state.known");
        assert_eq!(d.price_as_of.as_deref(), Some("2026-10-12"));
    })
    .await;
}

// ---------------------------------------------------------------------------
// record_provider_consent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn consent_must_quote_the_current_attributes_hash() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(FREE));
        let mut req = consent_req(&cat, None);
        req.attributes_hash = Some("not-the-hash".to_owned());
        assert_eq!(
            consent_impl(&mut conn, &cat, &req, now())
                .await
                .unwrap_err(),
            "attributes_changed"
        );
        req.attributes_hash = None;
        assert_eq!(
            consent_impl(&mut conn, &cat, &req, now())
                .await
                .unwrap_err(),
            "attributes_hash_required"
        );
        let standing = store::consent_standing_conn(&mut conn, "fixture-host-a")
            .await
            .unwrap();
        assert!(!standing.any_row, "a refused consent records nothing");
    })
    .await;
}

#[tokio::test]
async fn a_metered_provider_cannot_be_consented_to_without_a_cap_choice() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        assert_eq!(
            consent_impl(&mut conn, &cat, &consent_req(&cat, None), now())
                .await
                .unwrap_err(),
            "spend_block.cap_required"
        );
        // An unconfirmed "no cap" is not a confirmation.
        assert_eq!(
            consent_impl(
                &mut conn,
                &cat,
                &consent_req(&cat, Some(CapChoice::NoCap { confirmed: false })),
                now()
            )
            .await
            .unwrap_err(),
            "spend_block.cap_required"
        );
        let standing = store::consent_standing_conn(&mut conn, "fixture-host-a")
            .await
            .unwrap();
        assert!(!standing.any_row, "the refusal rolled back");
    })
    .await;
}

#[tokio::test]
async fn setting_a_cap_at_consent_creates_the_monthly_cost_limit() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        let r = consent_impl(
            &mut conn,
            &cat,
            &consent_req(
                &cat,
                Some(CapChoice::Set {
                    micros: "5000000".to_owned(),
                }),
            ),
            now(),
        )
        .await
        .unwrap();
        assert_eq!(r.consent_state, "consent_state.current");
        assert!(r.cap_limit_id.is_some());
        let limits = store::list_limits_conn(&mut conn).await.unwrap();
        assert_eq!(limits.len(), 1);
        let l = &limits[0].spec;
        assert_eq!(
            (l.metric, l.period, l.scope_kind),
            (LimitMetric::CostUsd, LimitPeriod::Month, ScopeKind::All)
        );
        assert_eq!(l.limit_value, 5_000_000);
        assert_eq!(l.warn_at_pct, 80);
        assert!(!l.approves_unattended);
        let standing = store::consent_standing_conn(&mut conn, "fixture-host-a")
            .await
            .unwrap();
        assert!(!standing.current.unwrap().no_cap_confirmed);
    })
    .await;
}

#[tokio::test]
async fn an_explicitly_confirmed_no_cap_is_recorded_on_the_consent() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        consent_impl(
            &mut conn,
            &cat,
            &consent_req(&cat, Some(CapChoice::NoCap { confirmed: true })),
            now(),
        )
        .await
        .unwrap();
        assert!(store::list_limits_conn(&mut conn).await.unwrap().is_empty());
        let standing = store::consent_standing_conn(&mut conn, "fixture-host-a")
            .await
            .unwrap();
        assert!(standing.current.unwrap().no_cap_confirmed);
    })
    .await;
}

#[tokio::test]
async fn an_existing_cap_satisfies_the_consent_and_a_new_choice_replaces_it() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        store::upsert_limit_conn(
            &mut conn,
            &LimitSpec {
                metric: LimitMetric::CostUsd,
                period: LimitPeriod::Day,
                scope_kind: ScopeKind::Provider,
                scope_id: Some("fixture-host-a".to_owned()),
                limit_value: 1_000_000,
                warn_at_pct: 80,
                approves_unattended: false,
            },
            now(),
        )
        .await
        .unwrap();
        // The provider-scoped daily cap is enough; no choice needed.
        consent_impl(&mut conn, &cat, &consent_req(&cat, None), now())
            .await
            .unwrap();
        // Choosing a monthly cap later replaces the monthly limit in place.
        consent_impl(
            &mut conn,
            &cat,
            &consent_req(
                &cat,
                Some(CapChoice::Set {
                    micros: "2000000".to_owned(),
                }),
            ),
            now() + Duration::seconds(1),
        )
        .await
        .unwrap();
        consent_impl(
            &mut conn,
            &cat,
            &consent_req(
                &cat,
                Some(CapChoice::Set {
                    micros: "3000000".to_owned(),
                }),
            ),
            now() + Duration::seconds(2),
        )
        .await
        .unwrap();
        let monthly: Vec<_> = store::list_limits_conn(&mut conn)
            .await
            .unwrap()
            .into_iter()
            .filter(|l| l.spec.period == LimitPeriod::Month)
            .collect();
        assert_eq!(monthly.len(), 1);
        assert_eq!(monthly[0].spec.limit_value, 3_000_000);
    })
    .await;
}

#[tokio::test]
async fn invalid_cap_amounts_are_refused() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        for bad in ["0", "-5", "abc", "", "1.5", "99999999999999999999"] {
            let r = consent_impl(
                &mut conn,
                &cat,
                &consent_req(
                    &cat,
                    Some(CapChoice::Set {
                        micros: bad.to_owned(),
                    }),
                ),
                now(),
            )
            .await;
            assert_eq!(r.unwrap_err(), "invalid_amount", "{bad:?}");
        }
        assert!(
            !store::consent_standing_conn(&mut conn, "fixture-host-a")
                .await
                .unwrap()
                .any_row
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// set_limit / list_limits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn limits_are_set_updated_listed_and_removed() {
    with_conn(|mut conn| async move {
        let r = limit_req(
            LimitMetric::Requests,
            LimitPeriod::Day,
            ScopeKind::Provider,
            Some("fixture-host-a"),
            "100",
        );
        let a = set_limit_impl(&mut conn, &r, now()).await.unwrap().unwrap();
        assert_eq!(a.limit_value, "100");
        let mut changed = r.clone();
        changed.limit_value = "250".to_owned();
        let b = set_limit_impl(&mut conn, &changed, now())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(b.limit_value, "250");
        assert_eq!(store::list_limits_conn(&mut conn).await.unwrap().len(), 1);

        let mut remove = r.clone();
        remove.remove = true;
        assert!(set_limit_impl(&mut conn, &remove, now())
            .await
            .unwrap()
            .is_none());
        assert!(store::list_limits_conn(&mut conn).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn invalid_limits_are_refused_with_a_code() {
    with_conn(|mut conn| async move {
        let cases: Vec<(&str, SetLimitRequest)> = vec![
            (
                "zero",
                limit_req(
                    LimitMetric::Requests,
                    LimitPeriod::Day,
                    ScopeKind::All,
                    None,
                    "0",
                ),
            ),
            (
                "negative",
                limit_req(
                    LimitMetric::Requests,
                    LimitPeriod::Day,
                    ScopeKind::All,
                    None,
                    "-1",
                ),
            ),
            (
                "text",
                limit_req(
                    LimitMetric::Requests,
                    LimitPeriod::Day,
                    ScopeKind::All,
                    None,
                    "ten",
                ),
            ),
        ];
        for (name, req) in cases {
            assert_eq!(
                set_limit_impl(&mut conn, &req, now()).await.unwrap_err(),
                "invalid_amount",
                "{name}"
            );
        }
        // A scoped limit without an id.
        let r = limit_req(
            LimitMetric::Requests,
            LimitPeriod::Day,
            ScopeKind::Persona,
            None,
            "5",
        );
        assert!(set_limit_impl(&mut conn, &r, now())
            .await
            .unwrap_err()
            .starts_with("invalid_limit:"));
        // The unattended flag only on a per-run cost limit.
        let mut r = limit_req(
            LimitMetric::CostUsd,
            LimitPeriod::Month,
            ScopeKind::All,
            None,
            "5000000",
        );
        r.approves_unattended = true;
        assert!(set_limit_impl(&mut conn, &r, now())
            .await
            .unwrap_err()
            .starts_with("invalid_limit:"));
        let mut r = limit_req(
            LimitMetric::Requests,
            LimitPeriod::Run,
            ScopeKind::All,
            None,
            "5",
        );
        r.approves_unattended = true;
        assert!(set_limit_impl(&mut conn, &r, now()).await.is_err());
        // Out-of-range warning threshold.
        let mut r = limit_req(
            LimitMetric::Requests,
            LimitPeriod::Day,
            ScopeKind::All,
            None,
            "5",
        );
        r.warn_at_pct = 0;
        assert!(set_limit_impl(&mut conn, &r, now()).await.is_err());
        assert!(store::list_limits_conn(&mut conn).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn the_unattended_flag_round_trips_through_the_commands() {
    with_conn(|mut conn| async move {
        let mut r = limit_req(
            LimitMetric::CostUsd,
            LimitPeriod::Run,
            ScopeKind::Focus,
            Some("focus-1"),
            "250000",
        );
        r.approves_unattended = true;
        let dto = set_limit_impl(&mut conn, &r, now()).await.unwrap().unwrap();
        assert!(dto.approves_unattended);
        let listed: Vec<LimitDto> = store::list_limits_conn(&mut conn)
            .await
            .unwrap()
            .iter()
            .map(limit_dto)
            .collect();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].approves_unattended);
        assert_eq!(listed[0].scope_id.as_deref(), Some("focus-1"));
        // Updating it without the flag clears the approval.
        r.approves_unattended = false;
        let dto = set_limit_impl(&mut conn, &r, now()).await.unwrap().unwrap();
        assert!(!dto.approves_unattended);
    })
    .await;
}

// ---------------------------------------------------------------------------
// get_usage_summary
// ---------------------------------------------------------------------------

async fn spend(
    conn: &mut SqliteConnection,
    provider: &str,
    cost: i64,
    tin: i64,
    tout: i64,
    at: DateTime<Utc>,
) {
    store::insert_reservation_conn(
        conn,
        &NewReservation {
            provider_id: provider,
            model_id: "fixture-model-a",
            persona_id: Some("persona-1"),
            focus_id: Some("focus-1"),
            run_id: Some("run-1"),
            tokens_in: tin,
            tokens_out: tout,
            cost_micros: cost,
            price_source: "curated",
        },
        at,
    )
    .await
    .unwrap();
}

fn utc_tz() -> FixedOffset {
    FixedOffset::east_opt(0).unwrap()
}

#[tokio::test]
async fn the_summary_totals_a_calendar_window_and_grades_each_limit() {
    with_conn(|mut conn| async move {
        // Now is Wednesday 2026-10-14. This month: 4_000 + 1_000; last month: 9_000.
        spend(
            &mut conn,
            "fixture-host-a",
            4_000,
            100,
            200,
            now() - Duration::days(2),
        )
        .await;
        spend(
            &mut conn,
            "fixture-host-b",
            1_000,
            10,
            20,
            now() - Duration::hours(1),
        )
        .await;
        spend(
            &mut conn,
            "fixture-host-a",
            9_000,
            5,
            5,
            Utc.with_ymd_and_hms(2026, 9, 20, 0, 0, 0).unwrap(),
        )
        .await;

        for (metric, period, value) in [
            (LimitMetric::CostUsd, LimitPeriod::Month, 10_000), // 5_000 of 10_000 -> ok
            (LimitMetric::Requests, LimitPeriod::Month, 2),     // 2 of 2 -> reached
            (LimitMetric::TokensIn, LimitPeriod::Month, 137),   // 110 of 137 -> 80.3% -> warn
        ] {
            set_limit_impl(
                &mut conn,
                &limit_req(metric, period, ScopeKind::All, None, &value.to_string()),
                now(),
            )
            .await
            .unwrap();
        }
        // A per-run limit is not summarized.
        set_limit_impl(
            &mut conn,
            &limit_req(
                LimitMetric::CostUsd,
                LimitPeriod::Run,
                ScopeKind::All,
                None,
                "100",
            ),
            now(),
        )
        .await
        .unwrap();

        let s = usage_summary_impl(
            &mut conn,
            &[],
            &UsageSummaryRequest {
                period: LimitPeriod::Month,
                provider_id: None,
            },
            &utc_tz(),
            now(),
        )
        .await
        .unwrap();
        assert!(s.settlement_may_exceed_limits);
        assert_eq!(s.cost_basis, "cost_basis.list_price_estimate");
        assert_eq!(s.since, "2026-10-01T00:00:00.000000Z");
        assert_eq!(s.totals.len(), 2);
        assert_eq!(s.totals[0].provider_id, "fixture-host-a");
        assert_eq!(s.totals[0].cost_micros, "4000");
        assert_eq!(s.totals[1].cost_micros, "1000");

        assert_eq!(s.limits.len(), 3, "the per-run limit is not summarized");
        let state =
            |metric: LimitMetric| s.limits.iter().find(|l| l.limit.metric == metric).unwrap();
        let cost = state(LimitMetric::CostUsd);
        assert_eq!(
            (cost.spent.as_str(), cost.pct, cost.state.as_str()),
            ("5000", 50, "limit_state.ok")
        );
        let reqs = state(LimitMetric::Requests);
        assert_eq!(
            (reqs.spent.as_str(), reqs.pct, reqs.state.as_str()),
            ("2", 100, "limit_state.reached")
        );
        let tin = state(LimitMetric::TokensIn);
        assert_eq!(tin.state, "limit_state.warn");
    })
    .await;
}

#[tokio::test]
async fn the_summary_can_exceed_a_limit_because_settlement_can_overshoot() {
    with_conn(|mut conn| async move {
        spend(&mut conn, "fixture-host-a", 7_000, 0, 0, now()).await;
        set_limit_impl(
            &mut conn,
            &limit_req(
                LimitMetric::CostUsd,
                LimitPeriod::Day,
                ScopeKind::All,
                None,
                "5000",
            ),
            now(),
        )
        .await
        .unwrap();
        let s = usage_summary_impl(
            &mut conn,
            &[],
            &UsageSummaryRequest {
                period: LimitPeriod::Day,
                provider_id: None,
            },
            &utc_tz(),
            now(),
        )
        .await
        .unwrap();
        assert_eq!(s.limits[0].state, "limit_state.reached");
        assert_eq!(s.limits[0].pct, 140);
        assert!(s.settlement_may_exceed_limits);
    })
    .await;
}

#[tokio::test]
async fn the_summary_filters_by_provider_and_refuses_the_run_period() {
    with_conn(|mut conn| async move {
        spend(&mut conn, "fixture-host-a", 100, 1, 1, now()).await;
        spend(&mut conn, "fixture-host-b", 200, 1, 1, now()).await;
        set_limit_impl(
            &mut conn,
            &limit_req(
                LimitMetric::Requests,
                LimitPeriod::Day,
                ScopeKind::Provider,
                Some("fixture-host-a"),
                "10",
            ),
            now(),
        )
        .await
        .unwrap();
        set_limit_impl(
            &mut conn,
            &limit_req(
                LimitMetric::Requests,
                LimitPeriod::Day,
                ScopeKind::Provider,
                Some("fixture-host-b"),
                "10",
            ),
            now(),
        )
        .await
        .unwrap();
        let s = usage_summary_impl(
            &mut conn,
            &[],
            &UsageSummaryRequest {
                period: LimitPeriod::Day,
                provider_id: Some("fixture-host-b".to_owned()),
            },
            &utc_tz(),
            now(),
        )
        .await
        .unwrap();
        assert_eq!(s.totals.len(), 1);
        assert_eq!(s.totals[0].provider_id, "fixture-host-b");
        assert_eq!(s.limits.len(), 1);
        assert_eq!(
            s.limits[0].limit.scope_id.as_deref(),
            Some("fixture-host-b")
        );

        let r = usage_summary_impl(
            &mut conn,
            &[],
            &UsageSummaryRequest {
                period: LimitPeriod::Run,
                provider_id: None,
            },
            &utc_tz(),
            now(),
        )
        .await;
        assert_eq!(r.unwrap_err(), "invalid_period");
    })
    .await;
}

#[tokio::test]
async fn the_summary_shows_published_free_tier_limits_beside_the_latest_headers() {
    with_conn(|mut conn| async move {
        let mut p = provider();
        p.free_tier = Some(json!({
            "available": true,
            "limits": {"rpm": 30, "rpd": 1000, "tpm": null, "tpd": 200000},
            "as_of": "2026-10-11"
        }));
        let mut unpublished = provider();
        unpublished.id = "fixture-host-b".to_owned();
        unpublished.free_tier = Some(json!({"available": false}));
        let mut no_dialect = provider();
        no_dialect.id = "fixture-host-c".to_owned();
        no_dialect.api_dialect = None;
        no_dialect.free_tier = p.free_tier.clone();

        let info = crate::providers::hosted_client::RateLimitInfo::from_pairs(&[
            ("x-ratelimit-remaining-requests", "21"),
            ("x-ratelimit-reset-requests", "42s"),
        ]);
        store::upsert_rate_snapshot_conn(&mut conn, "fixture-host-a", &info, now())
            .await
            .unwrap();

        let s = usage_summary_impl(
            &mut conn,
            &[p, unpublished, no_dialect],
            &UsageSummaryRequest {
                period: LimitPeriod::Day,
                provider_id: None,
            },
            &utc_tz(),
            now(),
        )
        .await
        .unwrap();
        assert_eq!(
            s.free_tiers.len(),
            1,
            "only an available free tier on a callable provider"
        );
        let t = &s.free_tiers[0];
        assert_eq!(t.provider_id, "fixture-host-a");
        assert_eq!(t.requests_per_minute, Some(30));
        assert_eq!(t.requests_per_day, Some(1000));
        assert_eq!(t.tokens_per_minute, None);
        assert_eq!(t.tokens_per_day, Some(200_000));
        assert_eq!(t.as_of.as_deref(), Some("2026-10-11"));
        let seen = t.observed.as_ref().expect("the latest headers");
        assert_eq!(seen.requests_remaining.as_deref(), Some("21"));
        assert_eq!(seen.requests_reset.as_deref(), Some("42s"));
    })
    .await;
}

// ---------------------------------------------------------------------------
// estimate_cost
// ---------------------------------------------------------------------------

fn est_req(max_out: Option<u32>, expected: Option<u32>) -> EstimateCostRequest {
    EstimateCostRequest {
        provider_id: "fixture-host-a".to_owned(),
        model_id: "fixture-model-a".to_owned(),
        input_bytes: 4000,
        max_output_tokens: max_out,
        expected_output_tokens: expected,
    }
}

fn today() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(2026, 10, 14).unwrap()
}

#[tokio::test]
async fn the_estimate_prices_the_worst_case_from_the_row_limit() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        let e = estimate_impl(&mut conn, &cat, &est_req(None, None), today())
            .await
            .unwrap();
        assert_eq!(e.price_state, "price_state.known");
        assert_eq!(e.price_source.as_deref(), Some("curated"));
        assert_eq!(e.price_as_of.as_deref(), Some("2026-10-11"));
        assert_eq!(e.price_age_days, Some(3));
        assert_eq!(e.cost_basis, "cost_basis.list_price_estimate");
        assert_eq!(e.input_tokens, "1000");
        assert_eq!(e.output_limit_tokens, "1000");
        // 1000 x 0.5 + 1000 x 1.5 micro-USD.
        assert_eq!(e.input_micros.as_deref(), Some("500"));
        assert_eq!(e.worst_case_micros.as_deref(), Some("2000"));
        assert_eq!(
            e.expected_micros, None,
            "no expected output was given, none is invented"
        );
    })
    .await;
}

#[tokio::test]
async fn a_caller_budget_and_an_expected_output_change_the_figures() {
    with_conn(|mut conn| async move {
        let mut cat = catalog(Some(PAID));
        cat.models[0].output_overhead = 2.0;
        let e = estimate_impl(&mut conn, &cat, &est_req(Some(200), Some(100)), today())
            .await
            .unwrap();
        assert_eq!(e.output_limit_tokens, "200");
        // Worst: 500 + 200 x 1.5 = 800.
        assert_eq!(e.worst_case_micros.as_deref(), Some("800"));
        // Expected: 500 + (100 x 2.0 overhead = 200 tokens) x 1.5 = 800.
        assert_eq!(e.expected_micros.as_deref(), Some("800"));
    })
    .await;
}

#[tokio::test]
async fn the_estimate_applies_the_tokenizer_factor_and_calibration() {
    with_conn(|mut conn| async move {
        let mut cat = catalog(Some(PAID));
        cat.models[0].token_factor = 1.3;
        sqlx::query("INSERT INTO calibration (provider_id, model_id, input_factor, output_factor, sample_count, updated_at) VALUES ('fixture-host-a', 'fixture-model-a', 2.0, 1.0, 5, 't')")
            .execute(&mut conn)
            .await
            .unwrap();
        let e = estimate_impl(&mut conn, &cat, &est_req(None, None), today()).await.unwrap();
        // 1000 bytes/4... 4000 bytes -> 1000 tokens x 1.3 x 2.0 = 2600.
        assert_eq!(e.input_tokens, "2600");
    })
    .await;
}

#[tokio::test]
async fn an_unknown_price_gives_no_amounts_and_a_stale_one_says_so() {
    with_conn(|mut conn| async move {
        let unknown = estimate_impl(&mut conn, &catalog(None), &est_req(None, None), today()).await.unwrap();
        assert_eq!(unknown.price_state, "price_state.unknown");
        assert!(unknown.worst_case_micros.is_none() && unknown.input_micros.is_none());
        assert_eq!(unknown.price_source, None);

        let old_price = r#"{"input_per_mtok":0.50,"output_per_mtok":1.50,"as_of":"2026-01-01","source":"curated"}"#;
        let stale = estimate_impl(&mut conn, &catalog(Some(old_price)), &est_req(None, None), today()).await.unwrap();
        assert_eq!(stale.price_state, "price_state.stale");
        assert!(stale.worst_case_micros.is_some(), "a stale price still estimates");
    })
    .await;
}

#[tokio::test]
async fn a_user_override_price_is_used_and_labelled() {
    with_conn(|mut conn| async move {
        let cat = catalog(Some(PAID));
        sqlx::query("INSERT INTO price_overrides (provider_id, model_id, pricing, updated_at) VALUES ('fixture-host-a', 'fixture-model-a', ?, 't')")
            .bind(r#"{"input_per_mtok":1,"output_per_mtok":2,"as_of":"2026-10-13"}"#)
            .execute(&mut conn)
            .await
            .unwrap();
        let e = estimate_impl(&mut conn, &cat, &est_req(None, None), today()).await.unwrap();
        assert_eq!(e.price_source.as_deref(), Some("user"));
        assert_eq!(e.worst_case_micros.as_deref(), Some("3000"));
    })
    .await;
}

#[tokio::test]
async fn the_estimate_needs_a_known_model_and_an_output_limit() {
    with_conn(|mut conn| async move {
        let mut cat = catalog(Some(PAID));
        let mut req = est_req(None, None);
        req.model_id = "no-such-model".to_owned();
        assert_eq!(
            estimate_impl(&mut conn, &cat, &req, today())
                .await
                .unwrap_err(),
            "unknown_model"
        );

        cat.models[0].request_options = Some(json!({}));
        assert_eq!(
            estimate_impl(&mut conn, &cat, &est_req(None, None), today())
                .await
                .unwrap_err(),
            "no_output_limit"
        );
        // A caller-supplied limit is enough.
        assert!(
            estimate_impl(&mut conn, &cat, &est_req(Some(10), None), today())
                .await
                .is_ok()
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

/// The wire types hold no float and no 64-bit integer: amounts are strings.
#[test]
fn money_fields_are_decimal_strings_on_the_wire() {
    let dto = limit_dto(&LimitRow {
        id: "l".to_owned(),
        spec: LimitSpec {
            metric: LimitMetric::CostUsd,
            period: LimitPeriod::Month,
            scope_kind: ScopeKind::All,
            scope_id: None,
            limit_value: 9_007_199_254_740_993, // beyond 2^53
            warn_at_pct: 80,
            approves_unattended: false,
        },
        created_at: "t".to_owned(),
        updated_at: "t".to_owned(),
    });
    let wire = serde_json::to_value(&dto).unwrap();
    assert_eq!(wire["limit_value"], "9007199254740993");
    assert_eq!(wire["metric"], "cost_usd");
    assert_eq!(wire["period"], "month");
    assert_eq!(wire["scope_kind"], "all");
}

#[test]
fn this_module_names_no_provider() {
    let lower = include_str!("provider_spend.rs").to_ascii_lowercase();
    for word in [
        "groq",
        "mistral",
        "anthropic",
        "openrouter",
        "gemini",
        "claude",
        "llama",
        "ollama",
        "cloudflare",
        "deepinfra",
        "fireworks",
        "sambanova",
    ] {
        assert!(!lower.contains(word), "provider_spend.rs names {word}");
    }
}
